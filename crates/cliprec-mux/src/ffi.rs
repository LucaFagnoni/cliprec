//! FFI: scrittura dei file con libavformat/libavcodec e spazio libero con `GetDiskFreeSpaceExW`.
//!
//! Tutto l'`unsafe` del crate sta qui. Ogni oggetto FFmpeg ha un proprietario RAII che lo
//! libera anche sui percorsi d'errore.

use std::ffi::{CStr, CString, c_char, c_int};
use std::path::Path;
use std::ptr;

use cliprec_core::{CHANNELS, HNS_PER_SEC, QpcTime, SAMPLE_RATE, VideoCodec, VideoSnapshot};
use ffmpeg_the_third::ffi::{
    AV_CODEC_FLAG_GLOBAL_HEADER, AV_DISPOSITION_DEFAULT, AV_PKT_FLAG_KEY, AVCodecContext, AVCodecID, AVDictionary,
    AVERROR, AVERROR_EOF, AVFMT_GLOBALHEADER, AVFMT_NOFILE, AVFormatContext, AVFrame, AVIO_FLAG_WRITE, AVMediaType,
    AVPacket, AVRational, AVSampleFormat, AVStream, av_channel_layout_copy, av_channel_layout_default, av_dict_free,
    av_dict_set, av_frame_alloc, av_frame_free, av_frame_get_buffer, av_frame_make_writable,
    av_interleaved_write_frame, av_mallocz, av_new_packet, av_packet_alloc, av_packet_free, av_packet_rescale_ts,
    av_rescale_q, av_strerror, av_write_trailer, avcodec_alloc_context3, avcodec_find_encoder_by_name,
    avcodec_free_context, avcodec_open2, avcodec_parameters_from_context, avcodec_receive_packet, avcodec_send_frame,
    avformat_alloc_output_context2, avformat_free_context, avformat_new_stream, avformat_write_header, avio_closep,
    avio_open,
};
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, MOVE_FILE_FLAGS, MoveFileExW};
use windows::core::HSTRING;

use crate::MuxError;

const EAGAIN: c_int = 11;
const HNS: AVRational = AVRational { num: 1, den: HNS_PER_SEC as c_int };
const AUDIO_TB: AVRational = AVRational { num: 1, den: SAMPLE_RATE as c_int };
const S24_MAX: f32 = 8_388_607.0;

/// Codifica delle tracce audio nel file.
#[derive(Clone, Copy)]
pub(crate) enum AudioCodec {
    /// AAC-LC nativo di FFmpeg, planare float.
    Aac { bit_rate: i64 },
    /// FLAC a 24 bit.
    Flac,
    /// PCM 24 bit little-endian (WAV).
    PcmS24,
}

pub(crate) struct Track<'a> {
    pub name: &'a str,
    /// PCM della traccia già allineato all'inizio del file e lungo quanto il file.
    pub source: crate::Aligned<'a>,
}

/// Pacchetti codificati in anticipo per traccia: abbastanza per l'interleaving, pochi kB.
const AUDIO_QUEUE: usize = 32;

/// Video da copiare senza ricodifica; `start` diventa il tempo zero del file.
pub(crate) struct VideoIn<'a> {
    pub snapshot: &'a VideoSnapshot,
    pub start: QpcTime,
}

fn ffmpeg_error(op: &'static str, code: c_int) -> MuxError {
    let mut buf = [0 as c_char; 256];
    // SAFETY: il buffer è locale e la dimensione passata è la sua; av_strerror lo termina con NUL.
    let message = unsafe {
        av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    };
    MuxError::Ffmpeg { op, code, message }
}

fn check(op: &'static str, code: c_int) -> Result<c_int, MuxError> {
    if code < 0 { Err(ffmpeg_error(op, code)) } else { Ok(code) }
}

fn cstring(s: &str) -> Result<CString, MuxError> {
    CString::new(s).map_err(|_| MuxError::InvalidName(s.to_owned()))
}

struct Output(*mut AVFormatContext);

impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: il contesto è stato allocato da avformat_alloc_output_context2 ed è di nostra
        // proprietà; pb è aperto da avio_open solo per i formati con file (o è nullo).
        unsafe {
            let ctx = self.0;
            if ctx.is_null() {
                return;
            }
            if !(*ctx).oformat.is_null() && ((*(*ctx).oformat).flags & AVFMT_NOFILE) == 0 {
                avio_closep(&mut (*ctx).pb);
            }
            avformat_free_context(ctx);
        }
    }
}

struct Encoder(*mut AVCodecContext);

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: contesto allocato da avcodec_alloc_context3 e di nostra proprietà.
        unsafe { avcodec_free_context(&mut self.0) };
    }
}

// SAFETY: un AVCodecContext si può usare da un altro thread purché mai in contemporanea; ogni
// `Encoder` ha un solo proprietario e passa di thread solo per intero.
unsafe impl Send for Encoder {}

struct Frame(*mut AVFrame);

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: frame allocato da av_frame_alloc e di nostra proprietà.
        unsafe { av_frame_free(&mut self.0) };
    }
}

struct Packet(*mut AVPacket);

impl Packet {
    fn new() -> Result<Self, MuxError> {
        // SAFETY: allocazione senza precondizioni; il nullo è gestito.
        let p = unsafe { av_packet_alloc() };
        if p.is_null() { Err(MuxError::OutOfMemory) } else { Ok(Self(p)) }
    }
}

// SAFETY: come per `Encoder`: un solo proprietario, nessun accesso concorrente.
unsafe impl Send for Packet {}

impl Drop for Packet {
    fn drop(&mut self) {
        // SAFETY: pacchetto allocato da av_packet_alloc e di nostra proprietà.
        unsafe { av_packet_free(&mut self.0) };
    }
}

/// Pacchetto pronto da scrivere con il suo istante (in secondi) per l'interleaving.
struct Pending {
    packet: Packet,
    secs: f64,
}

fn secs(ts: i64, tb: AVRational) -> f64 {
    ts as f64 * f64::from(tb.num) / f64::from(tb.den)
}

/// Scrive `path` nel formato FFmpeg `format` (es. "mp4", "matroska", "flac", "wav").
pub(crate) fn write(
    path: &Path,
    format: &str,
    video: Option<VideoIn<'_>>,
    tracks: Vec<Track<'_>>,
    codec: AudioCodec,
    faststart: bool,
) -> Result<(), MuxError> {
    let url = cstring(path.to_str().ok_or_else(|| MuxError::InvalidName(path.display().to_string()))?)?;
    let format_c = cstring(format)?;
    let mut out = Output(ptr::null_mut());
    // SAFETY: puntatori a CString vive per tutta la chiamata; il contesto va in `out` (RAII).
    check("avformat_alloc_output_context2", unsafe {
        avformat_alloc_output_context2(&mut out.0, ptr::null(), format_c.as_ptr(), url.as_ptr())
    })?;
    // SAFETY: `out.0` è un contesto valido appena allocato.
    let global_header = unsafe { ((*(*out.0).oformat).flags & AVFMT_GLOBALHEADER) != 0 };

    let video_stream = match &video {
        Some(v) => Some(add_video_stream(&out, v.snapshot)?),
        None => None,
    };
    // File di una sola traccia senza video (FLAC/WAV per traccia): il nome anche come titolo del file.
    if let ([t], None) = (tracks.as_slice(), &video) {
        let name = cstring(t.name)?;
        // SAFETY: contesto valido; av_dict_set copia la stringa.
        unsafe { av_dict_set(&mut (*out.0).metadata, c"title".as_ptr(), name.as_ptr(), 0) };
    }
    let mut encoders = Vec::with_capacity(tracks.len());
    for (i, t) in tracks.iter().enumerate() {
        let enc = open_audio_encoder(codec, global_header)?;
        let st = add_audio_stream(&out, &enc, t.name, i == 0)?;
        encoders.push((enc, st));
    }

    // SAFETY: contesto valido; pb viene chiuso dal Drop di `out`.
    unsafe {
        if ((*(*out.0).oformat).flags & AVFMT_NOFILE) == 0 {
            check("avio_open", avio_open(&mut (*out.0).pb, url.as_ptr(), AVIO_FLAG_WRITE))?;
        }
    }
    let mut opts: *mut AVDictionary = ptr::null_mut();
    if faststart {
        // SAFETY: chiavi e valori sono letterali terminati da NUL.
        unsafe { av_dict_set(&mut opts, c"movflags".as_ptr(), c"+faststart".as_ptr(), 0) };
    }
    // SAFETY: contesto valido con gli stream configurati; il dizionario viene liberato subito dopo.
    let header = unsafe {
        let r = avformat_write_header(out.0, &mut opts);
        av_dict_free(&mut opts);
        r
    };
    check("avformat_write_header", header)?;

    // Ogni traccia si codifica a pezzi su un thread proprio, leggendo il PCM in streaming e
    // consegnando i pacchetti su una coda corta; qui si fondono col video in ordine di tempo.
    // La base dei tempi si legge dopo l'header, che può cambiarla.
    let jobs: Vec<(Encoder, AVRational, c_int)> = encoders
        .into_iter()
        // SAFETY: stream del contesto ancora vivo.
        .map(|(enc, st)| unsafe { (enc, (*st).time_base, (*st).index) })
        .collect();
    let mut receivers = Vec::with_capacity(jobs.len());
    let mut senders = Vec::with_capacity(jobs.len());
    for _ in 0..jobs.len() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Pending, MuxError>>(AUDIO_QUEUE);
        senders.push(tx);
        receivers.push(rx);
    }
    std::thread::scope(|scope| {
        for (((enc, tb, index), t), tx) in jobs.into_iter().zip(tracks).zip(senders) {
            scope.spawn(move || {
                let run = std::panic::AssertUnwindSafe(|| encode_track(enc, tb, index, codec, t, &tx));
                let result = std::panic::catch_unwind(run).unwrap_or(Err(MuxError::EncoderPanicked));
                if let Err(e) = result {
                    let _ = tx.send(Err(e));
                }
            });
        }
        // `receivers` vive solo qui: su errore si chiudono le code e i thread escono subito.
        let receivers = receivers;
        let mut heads: Vec<Option<Pending>> = receivers.iter().map(|_| None).collect();
        let mut open = vec![true; receivers.len()];
        let mut video_iter = match (&video, video_stream) {
            (Some(v), Some(st)) => Some((v, st, v.snapshot.packets.iter().peekable())),
            _ => None,
        };
        loop {
            for (i, rx) in receivers.iter().enumerate() {
                if heads[i].is_none() && open[i] {
                    match rx.recv() {
                        Ok(Ok(p)) => heads[i] = Some(p),
                        Ok(Err(e)) => return Err(e),
                        Err(_) => open[i] = false,
                    }
                }
            }
            // Prossimo pacchetto video, convertito solo quando tocca a lui.
            let next_video = match &mut video_iter {
                Some((v, st, it)) => it.peek().map(|p| {
                    // SAFETY: stream valido del contesto.
                    let tb = unsafe { (**st).time_base };
                    secs(av_rescale_q_safe(p.dts.hns_since(v.start), HNS, tb), tb)
                }),
                None => None,
            };
            let next_audio = heads
                .iter()
                .enumerate()
                .filter_map(|(i, h)| h.as_ref().map(|p| (i, p.secs)))
                .min_by(|a, b| a.1.total_cmp(&b.1));
            let packet = match (next_video, next_audio) {
                (Some(vs), a) if a.is_none_or(|(_, s)| vs <= s) => {
                    let Some((v, st, it)) = &mut video_iter else { break };
                    let Some(p) = it.next() else { break };
                    video_packet(p, v.start, *st)?
                }
                (_, Some((i, _))) => match heads[i].take() {
                    Some(p) => p.packet,
                    None => break,
                },
                _ => break,
            };
            // SAFETY: contesto con header scritto; il pacchetto è valido e ne cediamo il contenuto.
            check("av_interleaved_write_frame", unsafe { av_interleaved_write_frame(out.0, packet.0) })?;
        }
        Ok(())
    })?;
    // SAFETY: contesto con header scritto.
    check("av_write_trailer", unsafe { av_write_trailer(out.0) })?;
    // SAFETY: chiusura esplicita per far emergere gli errori di scrittura finale.
    unsafe {
        if ((*(*out.0).oformat).flags & AVFMT_NOFILE) == 0 {
            check("avio_closep", avio_closep(&mut (*out.0).pb))?;
        }
    }
    Ok(())
}

fn av_rescale_q_safe(a: i64, from: AVRational, to: AVRational) -> i64 {
    // SAFETY: funzione pura su interi.
    unsafe { av_rescale_q(a, from, to) }
}

fn add_video_stream(out: &Output, snap: &VideoSnapshot) -> Result<*mut AVStream, MuxError> {
    let p = &snap.params;
    let codec_id = match p.codec {
        VideoCodec::H264 => AVCodecID::H264,
        VideoCodec::Hevc => AVCodecID::HEVC,
        VideoCodec::Av1 => AVCodecID::AV1,
    };
    let fps = c_int::try_from(p.fps.max(1)).unwrap_or(60);
    // SAFETY: contesto valido; lo stream appartiene al contesto. L'extradata è allocata con
    // av_mallocz (con il padding richiesto) e passa in proprietà ai parametri dello stream.
    unsafe {
        let st = avformat_new_stream(out.0, ptr::null());
        if st.is_null() {
            return Err(MuxError::OutOfMemory);
        }
        let par = (*st).codecpar;
        (*par).codec_type = AVMediaType::VIDEO;
        (*par).codec_id = codec_id;
        (*par).width = c_int::try_from(p.width).unwrap_or(0);
        (*par).height = c_int::try_from(p.height).unwrap_or(0);
        (*par).bit_rate = i64::try_from(p.bit_rate).unwrap_or(0);
        if !p.extradata.is_empty() {
            let size = p.extradata.len();
            let buf = av_mallocz(size + 64).cast::<u8>();
            if buf.is_null() {
                return Err(MuxError::OutOfMemory);
            }
            ptr::copy_nonoverlapping(p.extradata.as_ptr(), buf, size);
            (*par).extradata = buf;
            (*par).extradata_size = c_int::try_from(size).unwrap_or(0);
        }
        // Base dei tempi multipla degli fps: ogni frame della griglia CFR cade su un tick intero.
        (*st).time_base = AVRational { num: 1, den: fps.saturating_mul(1000) };
        (*st).avg_frame_rate = AVRational { num: fps, den: 1 };
        (*st).r_frame_rate = AVRational { num: fps, den: 1 };
        (*st).disposition = AV_DISPOSITION_DEFAULT;
        Ok(st)
    }
}

fn video_packet(p: &cliprec_core::EncodedPacket, start: QpcTime, st: *mut AVStream) -> Result<Packet, MuxError> {
    let pkt = Packet::new()?;
    let size = c_int::try_from(p.data.len()).map_err(|_| MuxError::OutOfMemory)?;
    // SAFETY: pacchetto appena allocato; av_new_packet alloca `size` byte (più padding) in cui
    // copiamo i dati; lo stream appartiene al contesto ancora vivo.
    unsafe {
        check("av_new_packet", av_new_packet(pkt.0, size))?;
        ptr::copy_nonoverlapping(p.data.as_ptr(), (*pkt.0).data, p.data.len());
        (*pkt.0).pts = p.pts.hns_since(start);
        (*pkt.0).dts = p.dts.hns_since(start);
        (*pkt.0).duration = p.duration;
        if p.keyframe {
            (*pkt.0).flags |= AV_PKT_FLAG_KEY;
        }
        (*pkt.0).stream_index = (*st).index;
        av_packet_rescale_ts(pkt.0, HNS, (*st).time_base);
    }
    Ok(pkt)
}

fn open_audio_encoder(codec: AudioCodec, global_header: bool) -> Result<Encoder, MuxError> {
    let (name, fmt, bit_rate) = match codec {
        AudioCodec::Aac { bit_rate } => (c"aac", AVSampleFormat::FLTP, bit_rate),
        AudioCodec::Flac => (c"flac", AVSampleFormat::S32, 0),
        AudioCodec::PcmS24 => (c"pcm_s24le", AVSampleFormat::S32, 0),
    };
    // SAFETY: nome letterale; il contesto va subito nel proprietario RAII.
    unsafe {
        let c = avcodec_find_encoder_by_name(name.as_ptr());
        if c.is_null() {
            return Err(MuxError::EncoderMissing(name.to_string_lossy().into_owned()));
        }
        let enc = Encoder(avcodec_alloc_context3(c));
        if enc.0.is_null() {
            return Err(MuxError::OutOfMemory);
        }
        let ctx = enc.0;
        (*ctx).sample_rate = SAMPLE_RATE as c_int;
        av_channel_layout_default(&mut (*ctx).ch_layout, CHANNELS as c_int);
        (*ctx).sample_fmt = fmt;
        (*ctx).bit_rate = bit_rate;
        (*ctx).time_base = AUDIO_TB;
        if fmt == AVSampleFormat::S32 {
            (*ctx).bits_per_raw_sample = 24;
        }
        if global_header {
            (*ctx).flags |= AV_CODEC_FLAG_GLOBAL_HEADER as c_int;
        }
        check("avcodec_open2", avcodec_open2(ctx, c, ptr::null_mut()))?;
        Ok(enc)
    }
}

fn add_audio_stream(out: &Output, enc: &Encoder, name: &str, default: bool) -> Result<*mut AVStream, MuxError> {
    let name = cstring(name)?;
    // SAFETY: contesto ed encoder validi e aperti; le stringhe vengono copiate da av_dict_set.
    unsafe {
        let st = avformat_new_stream(out.0, ptr::null());
        if st.is_null() {
            return Err(MuxError::OutOfMemory);
        }
        check("avcodec_parameters_from_context", avcodec_parameters_from_context((*st).codecpar, enc.0))?;
        (*st).time_base = AUDIO_TB;
        (*st).disposition = if default { AV_DISPOSITION_DEFAULT } else { 0 };
        av_dict_set(&mut (*st).metadata, c"title".as_ptr(), name.as_ptr(), 0);
        av_dict_set(&mut (*st).metadata, c"handler_name".as_ptr(), name.as_ptr(), 0);
        Ok(st)
    }
}

/// Codifica tutta la traccia; restituisce i pacchetti nella base dei tempi `st_tb` dello stream `index`.
fn encode_track(
    enc: Encoder,
    st_tb: AVRational,
    index: c_int,
    codec: AudioCodec,
    mut track: Track<'_>,
    tx: &std::sync::mpsc::SyncSender<Result<Pending, MuxError>>,
) -> Result<(), MuxError> {
    let ctx = enc.0;
    // SAFETY: encoder aperto.
    let frame_size = match usize::try_from(unsafe { (*ctx).frame_size }) {
        Ok(0) | Err(_) => 1024,
        Ok(n) => n,
    };
    let frame = Frame(
        // SAFETY: allocazione senza precondizioni; il nullo è gestito sotto.
        unsafe { av_frame_alloc() },
    );
    if frame.0.is_null() {
        return Err(MuxError::OutOfMemory);
    }
    // SAFETY: frame valido; formato, layout e dimensione presi dall'encoder aperto.
    unsafe {
        (*frame.0).nb_samples = frame_size as c_int;
        (*frame.0).format = (*ctx).sample_fmt.0;
        (*frame.0).sample_rate = (*ctx).sample_rate;
        check("av_channel_layout_copy", av_channel_layout_copy(&mut (*frame.0).ch_layout, &(*ctx).ch_layout))?;
        check("av_frame_get_buffer", av_frame_get_buffer(frame.0, 0))?;
    }
    // `false` se il lettore ha chiuso la coda (errore altrove): si smette di codificare.
    let drain = || -> Result<bool, MuxError> {
        loop {
            let pkt = Packet::new()?;
            // SAFETY: encoder aperto e pacchetto valido.
            let r = unsafe { avcodec_receive_packet(ctx, pkt.0) };
            if r == AVERROR(EAGAIN) || r == AVERROR_EOF {
                return Ok(true);
            }
            check("avcodec_receive_packet", r)?;
            // SAFETY: pacchetto appena riempito dall'encoder.
            let dts = unsafe {
                av_packet_rescale_ts(pkt.0, AUDIO_TB, st_tb);
                (*pkt.0).stream_index = index;
                (*pkt.0).dts
            };
            if tx.send(Ok(Pending { packet: pkt, secs: secs(dts, st_tb) })).is_err() {
                return Ok(false);
            }
        }
    };
    let mut buf = vec![0.0f32; frame_size * CHANNELS];
    let mut pos = 0usize;
    loop {
        let count = track.source.read(&mut buf);
        if count == 0 {
            break;
        }
        let input = buf[..count * CHANNELS].chunks_exact(CHANNELS);
        // SAFETY: frame con buffer di `frame_size` campioni per canale (count ≤ frame_size);
        // make_writable garantisce che l'encoder non ne tenga più un riferimento.
        unsafe {
            check("av_frame_make_writable", av_frame_make_writable(frame.0))?;
            (*frame.0).nb_samples = count as c_int;
            (*frame.0).pts = pos as i64;
            match codec {
                AudioCodec::Aac { .. } => {
                    let l = std::slice::from_raw_parts_mut((*frame.0).data[0].cast::<f32>(), count);
                    let r = std::slice::from_raw_parts_mut((*frame.0).data[1].cast::<f32>(), count);
                    for ((l, r), s) in l.iter_mut().zip(r.iter_mut()).zip(input) {
                        *l = s[0];
                        *r = s[1];
                    }
                }
                AudioCodec::Flac | AudioCodec::PcmS24 => {
                    let d = std::slice::from_raw_parts_mut((*frame.0).data[0].cast::<i32>(), count * CHANNELS);
                    for (dst, &x) in d.iter_mut().zip(input.flatten()) {
                        *dst = ((x.clamp(-1.0, 1.0) * S24_MAX).round() as i32) << 8;
                    }
                }
            }
            check("avcodec_send_frame", avcodec_send_frame(ctx, frame.0))?;
        }
        if !drain()? {
            return Ok(());
        }
        pos += count;
    }
    // SAFETY: frame nullo = fine dello stream.
    check("avcodec_send_frame", unsafe { avcodec_send_frame(ctx, ptr::null()) })?;
    drain()?;
    Ok(())
}

/// Byte liberi per l'utente sul volume di `dir`, o `None` se non si riesce a saperlo.
pub(crate) fn free_space(dir: &Path) -> Option<u64> {
    let mut free = 0u64;
    let dir = verbatim(dir);
    // SAFETY: stringa wide terminata da NUL viva per la chiamata; il puntatore d'uscita è locale.
    unsafe { GetDiskFreeSpaceExW(&dir, Some(&mut free), None, None) }.ok()?;
    Some(free)
}

/// Rinomina file o cartella senza mai sovrascrivere: se `to` esiste dà `AlreadyExists`.
pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let (from, to) = (verbatim(from), verbatim(to));
    // SAFETY: stringhe wide terminate da NUL vive per la chiamata; nessun flag = niente sostituzione.
    unsafe { MoveFileExW(&from, &to, MOVE_FILE_FLAGS(0)) }
        .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
}

/// Percorso assoluto con prefisso `\\?\` (o `\\?\UNC\`), per superare MAX_PATH nelle API
/// Win32 chiamate direttamente (std e FFmpeg lo fanno da soli).
fn verbatim(path: &Path) -> HSTRING {
    use std::os::windows::ffi::OsStrExt;
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let wide: Vec<u16> = abs.as_os_str().encode_wide().collect();
    let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    let full = if wide.starts_with(&w(r"\\?\")) {
        wide
    } else if let Some(unc) = wide.strip_prefix(&w(r"\\")[..]) {
        [w(r"\\?\UNC\").as_slice(), unc].concat()
    } else {
        [w(r"\\?\").as_slice(), &wide].concat()
    };
    HSTRING::from_wide(&full)
}
