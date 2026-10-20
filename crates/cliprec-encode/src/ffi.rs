//! Modulo FFI FFmpeg: hwdevice/hwframes D3D11, encoder hardware e conversione dei pacchetti.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

use cliprec_core::{EncodeError, GpuDevice, GpuFrame, HNS_PER_SEC, PacketRef, QpcTime, VideoEncoder, VideoStreamParams};
use ffmpeg_the_third::ffi;
use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_DECODER, D3D11_BIND_RENDER_TARGET};
use windows::core::Interface;

use crate::d3d::{Converter, Output};
use crate::yuv::{Plane, nv12_to_i420};
use crate::{EncoderSettings, Vendor};
use cliprec_core::config::RateControl;

/// Texture del pool fisso per QSV (`async_depth` 4 + B-frame + margine). D3D11 rifiuta array NV12
/// con `BIND_RENDER_TARGET` oltre 2 elementi (misurato su RTX 3080 e UHD 770), e FFmpeg con
/// `initial_pool_size > 0` crea un array: per NVENC/AMF il pool è quindi dinamico (texture singole
/// render target), per QSV, che richiede un pool fisso da derivare, è un array `BIND_DECODER`
/// riempito con una copia GPU da una texture render target intermedia.
const QSV_POOL_SIZE: c_int = 16;

const EAGAIN: c_int = 11;
const ENOENT: c_int = 2;
const EINVAL: c_int = 22;
const ENOSYS: c_int = 40;

/// `AVD3D11VADeviceContext` di `libavutil/hwcontext_d3d11va.h` (non coperto dai binding).
#[repr(C)]
struct AVD3D11VADeviceContext {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
}

/// `AVD3D11VAFramesContext` di `libavutil/hwcontext_d3d11va.h`.
#[repr(C)]
struct AVD3D11VAFramesContext {
    texture: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
    texture_infos: *mut c_void,
}

fn av_error_string(code: c_int) -> String {
    let mut buf = [0 as c_char; 128];
    // SAFETY: il buffer è locale e la sua lunghezza è passata; av_strerror lo termina con NUL.
    unsafe {
        ffi::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

/// Errore all'apertura: «non supportato» se l'encoder o il device non possono funzionare con
/// queste opzioni, altrimenti «sessione rifiutata» (es. sessioni NVENC esaurite, memoria).
fn open_error(name: &str, step: &str, code: c_int) -> EncodeError {
    let msg = format!("{name}: {step}: {}", av_error_string(code));
    if [ffi::AVERROR(ENOSYS), ffi::AVERROR(ENOENT), ffi::AVERROR(EINVAL)].contains(&code) {
        EncodeError::Unsupported(msg)
    } else {
        EncodeError::SessionRejected(msg)
    }
}

/// Callback di av_log: inoltra a `tracing` (target `ffmpeg`) i messaggi entro il livello di
/// av_log. ERROR diventa `warn` perché molti errori sono attesi (es. il probe degli encoder).
unsafe extern "C" fn log_to_tracing(avcl: *mut c_void, level: c_int, fmt: *const c_char, vl: ffi::va_list) {
    // SAFETY: FFmpeg chiama la callback con argomenti validi; av_log_format_line2 scrive al più
    // `line.len()` byte terminati da NUL nel buffer locale e consuma `vl` come vsnprintf.
    unsafe {
        if level > ffi::av_log_get_level() {
            return;
        }
        let mut line = [0 as c_char; 1024];
        let mut prefix: c_int = 1;
        ffi::av_log_format_line2(avcl, level, fmt, vl, line.as_mut_ptr(), line.len() as c_int, &mut prefix);
        let text = CStr::from_ptr(line.as_ptr()).to_string_lossy();
        let text = text.trim_end();
        match level {
            _ if text.is_empty() => {}
            l if l <= ffi::AV_LOG_FATAL => tracing::error!(target: "ffmpeg", "{text}"),
            l if l <= ffi::AV_LOG_ERROR => tracing::warn!(target: "ffmpeg", "{text}"),
            l if l <= ffi::AV_LOG_WARNING => tracing::info!(target: "ffmpeg", "{text}"),
            _ => tracing::debug!(target: "ffmpeg", "{text}"),
        }
    }
}

/// Livello di av_log a ERROR e messaggi inoltrati a `tracing` invece che su stderr.
pub(crate) fn quiet_logs() {
    // SAFETY: funzioni globali di libavutil senza precondizioni; la callback è thread-safe.
    unsafe {
        ffi::av_log_set_level(ffi::AV_LOG_ERROR);
        ffi::av_log_set_callback(Some(log_to_tracing));
    }
}

/// Preset di SVT-AV1. Non 10-12: in modalità random access SVT avvisa che da M10 in su sono
/// «per l'automazione» e a M10 su 1080p sono usciti 3 frame interamente grigi (82-84 nel test con
/// colori pieni, confermato decodificando con dav1d e con libaom). M9 in release: 1080p 389 fps su
/// colori pieni, 1440p 108 fps su rumore (caso peggiore), oltre i 60 fps richiesti.
const SOFTWARE_PRESET: u32 = 9;

/// Superficie QSV mappata una volta all'apertura su una fetta del pool D3D11 e riusata: niente
/// `av_hwframe_get_buffer`/`av_hwframe_map` per frame. È libera quando l'encoder ha rilasciato
/// il suo riferimento (`buf[0]` con un solo riferimento, il nostro).
struct QsvSlot {
    frame: *mut ffi::AVFrame,
    texture: *mut c_void,
    index: usize,
}

/// Encoder FFmpeg su frame D3D11 (NV12) del [`GpuDevice`] di sessione.
pub(crate) struct HwEncoder {
    ctx: *mut ffi::AVCodecContext,
    device_ref: *mut ffi::AVBufferRef,
    /// Pool NV12 D3D11 in cui il video processor scrive.
    frames_ref: *mut ffi::AVBufferRef,
    /// Solo QSV: device e pool QSV derivati da quelli D3D11 (stesse texture).
    qsv_device_ref: *mut ffi::AVBufferRef,
    qsv_frames_ref: *mut ffi::AVBufferRef,
    frame: *mut ffi::AVFrame,
    qsv_slots: Vec<QsvSlot>,
    next_slot: usize,
    pkt: *mut ffi::AVPacket,
    converter: Converter,
    vendor: Vendor,
    params: VideoStreamParams,
    /// Indice e pts QPC del primo frame: pts dell'encoder = indice − primo indice (time_base 1/fps).
    first: Option<(u64, QpcTime)>,
    last_pts: i64,
    /// Ultimo frame mandato all'encoder (`frame` o una superficie QSV), nullo prima del primo: un
    /// frame ripetuto lo rimanda senza rifare la conversione.
    last_sent: *mut ffi::AVFrame,
}

// SAFETY: i contesti FFmpeg non sono legati al thread che li ha creati; `HwEncoder` è usato da un
// thread alla volta (`&mut self`) e le interfacce COM del Converter vengono da un device
// free-threaded con contesto immediato protetto da SetMultithreadProtected.
unsafe impl Send for HwEncoder {}

impl Drop for HwEncoder {
    fn drop(&mut self) {
        // SAFETY: ogni puntatore è nullo o di proprietà esclusiva di `self`; le funzioni di
        // rilascio di FFmpeg accettano puntatori nulli e li azzerano.
        unsafe {
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_frame_free(&mut self.frame);
            for slot in &mut self.qsv_slots {
                ffi::av_frame_free(&mut slot.frame);
            }
            ffi::av_packet_free(&mut self.pkt);
            ffi::av_buffer_unref(&mut self.qsv_frames_ref);
            ffi::av_buffer_unref(&mut self.qsv_device_ref);
            ffi::av_buffer_unref(&mut self.frames_ref);
            ffi::av_buffer_unref(&mut self.device_ref);
        }
    }
}

struct Dict(*mut ffi::AVDictionary);

impl Dict {
    fn set(&mut self, key: &str, value: impl ToString) {
        let (Ok(k), Ok(v)) = (CString::new(key), CString::new(value.to_string())) else { return };
        // SAFETY: chiave e valore sono stringhe C valide; av_dict_set le copia.
        unsafe { ffi::av_dict_set(&mut self.0, k.as_ptr(), v.as_ptr(), 0) };
    }

    /// Chiavi rimaste: dopo avcodec_open2 sono le opzioni che l'encoder non conosce.
    fn keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        let mut e: *const ffi::AVDictionaryEntry = ptr::null();
        // SAFETY: iterazione documentata di un dizionario valido (o nullo); le voci restano valide
        // finché il dizionario non cambia, e le copiamo subito.
        unsafe {
            loop {
                e = ffi::av_dict_iterate(self.0, e);
                if e.is_null() {
                    break;
                }
                keys.push(CStr::from_ptr((*e).key).to_string_lossy().into_owned());
            }
        }
        keys
    }
}

impl Drop for Dict {
    fn drop(&mut self) {
        // SAFETY: il dizionario è nullo o allocato da av_dict_set.
        unsafe { ffi::av_dict_free(&mut self.0) };
    }
}

impl HwEncoder {
    /// Apre `name` con al più `b_frames` B-frame (0 li spegne).
    pub(crate) fn open(
        gpu: &GpuDevice,
        s: &EncoderSettings,
        name: &str,
        codec: cliprec_core::VideoCodec,
        vendor: Vendor,
        b_frames: u32,
    ) -> Result<Self, EncodeError> {
        let cname = CString::new(name).map_err(|_| EncodeError::Unsupported(name.to_owned()))?;
        // SAFETY: stringa C valida; la funzione restituisce un puntatore statico o nullo.
        let av_codec = unsafe { ffi::avcodec_find_encoder_by_name(cname.as_ptr()) };
        if av_codec.is_null() {
            return Err(EncodeError::Unsupported(format!("{name}: assente in questa build di FFmpeg")));
        }
        if s.width == 0 || s.height == 0 || !s.width.is_multiple_of(2) || !s.height.is_multiple_of(2) || s.fps == 0 {
            return Err(EncodeError::Failed(format!("{name}: dimensione {}×{} @ {} fps non valida", s.width, s.height, s.fps)));
        }
        let output = match vendor {
            Vendor::Qsv => Output::PoolViaCopy,
            Vendor::Software => Output::Cpu,
            Vendor::Nvenc | Vendor::Amf => Output::Pool,
        };
        // 4:4:4: frame BGRA all'encoder, che converte in YUV 4:4:4 (solo NVENC, `supports_444`).
        let rgb = s.chroma444_for(name);
        let converter = Converter::new(gpu, s.width, s.height, s.fps, output, rgb)
            .map_err(|e| EncodeError::Unsupported(format!("{name}: video processor D3D11: {e}")))?;

        let max_kbps = s.max_bitrate_kbps_for(codec);
        let mut enc = HwEncoder {
            ctx: ptr::null_mut(),
            device_ref: ptr::null_mut(),
            frames_ref: ptr::null_mut(),
            qsv_device_ref: ptr::null_mut(),
            qsv_frames_ref: ptr::null_mut(),
            frame: ptr::null_mut(),
            qsv_slots: Vec::new(),
            next_slot: 0,
            pkt: ptr::null_mut(),
            converter,
            vendor,
            params: VideoStreamParams {
                codec,
                width: s.width,
                height: s.height,
                fps: s.fps,
                extradata: Vec::new(),
                bit_rate: u64::from(max_kbps) * 1000,
                encoder_name: name.to_owned(),
            },
            first: None,
            last_pts: -1,
            last_sent: ptr::null_mut(),
        };

        // SAFETY: sequenza documentata di libavutil/libavcodec. Ogni puntatore allocato finisce
        // subito in `enc`, che lo libera in Drop anche se un passo successivo fallisce. I campi
        // scritti nei contesti D3D11 seguono hwcontext_d3d11va.h; i riferimenti COM passati a
        // FFmpeg sono AddRef-ati (clone + into_raw) perché FFmpeg li rilascia alla chiusura.
        unsafe {
            let (pix_fmt, enc_frames) = match vendor {
                Vendor::Software => (ffi::AVPixelFormat::YUV420P, ptr::null_mut()),
                _ => enc.init_hw(gpu, s, name, vendor, rgb)?,
            };

            enc.ctx = ffi::avcodec_alloc_context3(av_codec);
            enc.frame = ffi::av_frame_alloc();
            enc.pkt = ffi::av_packet_alloc();
            if enc.ctx.is_null() || enc.frame.is_null() || enc.pkt.is_null() {
                return Err(EncodeError::Failed(format!("{name}: allocazione del contesto")));
            }
            let c = &mut *enc.ctx;
            c.width = s.width as c_int;
            c.height = s.height as c_int;
            c.time_base = ffi::AVRational { num: 1, den: s.fps as c_int };
            c.framerate = ffi::AVRational { num: s.fps as c_int, den: 1 };
            c.pix_fmt = pix_fmt;
            if enc_frames.is_null() {
                // Software: un solo frame yuv420p riusato per tutta la sessione.
                let f = &mut *enc.frame;
                f.format = ffi::AVPixelFormat::YUV420P.0;
                f.width = s.width as c_int;
                f.height = s.height as c_int;
                let ret = ffi::av_frame_get_buffer(enc.frame, 0);
                if ret < 0 {
                    return Err(EncodeError::Failed(format!("{name}: av_frame_get_buffer: {}", av_error_string(ret))));
                }
            } else {
                c.sw_pix_fmt = if rgb { ffi::AVPixelFormat::BGRA } else { ffi::AVPixelFormat::NV12 };
                c.hw_frames_ctx = ffi::av_buffer_ref(enc_frames);
            }
            c.gop_size = (s.fps * s.advanced.keyframe_seconds.clamp(1, 10)) as c_int;
            c.max_b_frames = b_frames.min(16) as c_int;
            c.rc_max_rate = i64::from(max_kbps) * 1000;
            c.rc_buffer_size = (i64::from(max_kbps) * 1000).min(i64::from(c_int::MAX)) as c_int;
            c.flags |= ffi::AV_CODEC_FLAG_GLOBAL_HEADER as c_int | ffi::AV_CODEC_FLAG_CLOSED_GOP as c_int;
            c.color_primaries = ffi::AVColorPrimaries::BT709;
            c.color_trc = ffi::AVColorTransferCharacteristic::BT709;
            c.colorspace = ffi::AVColorSpace::BT709;
            c.color_range = ffi::AVColorRange::MPEG;

            let mut opts = Dict(ptr::null_mut());
            let q = s.quality.clamp(1, 51);
            let a = &s.advanced;
            let preset = a.preset as usize;
            // VBR e CBR: obiettivo in bit/s; CBR porta tetto e buffer all'obiettivo.
            let target = i64::from(s.target_bitrate_kbps_for(codec)) * 1000;
            if a.rate_control == RateControl::Cbr {
                c.rc_max_rate = target;
                c.rc_buffer_size = target.min(i64::from(c_int::MAX)) as c_int;
            } else if a.rate_control == RateControl::Vbr {
                c.rc_max_rate = c.rc_max_rate.max(target);
            }
            match vendor {
                Vendor::Nvenc => {
                    opts.set("preset", ["p1", "p3", "p4", "p5", "p7"][preset]);
                    opts.set("tune", "hq");
                    match a.rate_control {
                        RateControl::Quality => {
                            c.bit_rate = 0;
                            opts.set("rc", "vbr");
                            opts.set("cq", q);
                        }
                        RateControl::Vbr => {
                            c.bit_rate = target;
                            opts.set("rc", "vbr");
                        }
                        RateControl::Cbr => {
                            c.bit_rate = target;
                            opts.set("rc", "cbr");
                        }
                    }
                    opts.set("rc-lookahead", if a.lookahead { 20 } else { 0 });
                    if a.adaptive_quantization {
                        opts.set("spatial-aq", 1);
                        // L'AQ temporale richiede il look-ahead.
                        if a.lookahead {
                            opts.set("temporal-aq", 1);
                        }
                    }
                    if rgb {
                        opts.set("rgb_mode", "yuv444");
                        opts.set("profile", if codec == cliprec_core::VideoCodec::H264 { "high444p" } else { "rext" });
                    }
                    opts.set("forced-idr", 1);
                }
                Vendor::Amf => {
                    // ponytail: non verificato (niente GPU AMD qui); livello QVBR = quality.
                    c.bit_rate = target;
                    opts.set("usage", "transcoding");
                    opts.set("quality", ["speed", "speed", "balanced", "quality", "quality"][preset]);
                    match a.rate_control {
                        RateControl::Quality => {
                            opts.set("rc", "qvbr");
                            opts.set("qvbr_quality_level", q);
                        }
                        RateControl::Vbr => opts.set("rc", "vbr_peak"),
                        RateControl::Cbr => opts.set("rc", "cbr"),
                    }
                    if a.lookahead {
                        opts.set("preanalysis", 1);
                    }
                    if a.adaptive_quantization {
                        opts.set("vbaq", 1);
                    }
                    if codec == cliprec_core::VideoCodec::Av1 {
                        // Su RDNA3 il default storico (64x16) codifica 1080p a 1082 righe: "none"
                        // (NO_RESTRICTIONS) lascia al driver padding e ritaglio. Se la GPU non lo
                        // supporta l'apertura fallisce e si passa al candidato successivo.
                        opts.set("align", "none");
                        opts.set("max_b_frames", 0);
                    } else {
                        opts.set("forced_idr", 1);
                    }
                }
                Vendor::Qsv => {
                    // global_quality + maxrate + bitrate ⇒ QVBR in qsvenc; senza global_quality VBR o CBR.
                    c.bit_rate = target;
                    if a.rate_control == RateControl::Quality {
                        c.global_quality = c_int::from(q);
                    }
                    opts.set("preset", ["veryfast", "veryfast", "faster", "faster", "medium"][preset]);
                    if codec == cliprec_core::VideoCodec::H264 {
                        opts.set("look_ahead", i32::from(a.lookahead));
                    }
                    opts.set("forced_idr", 1);
                    opts.set("idr_interval", 0);
                }
                Vendor::Software => {
                    // CRF con tetto (maxrate/bufsize sopra): la scala 0-63 di SVT-AV1 da quella 0-51.
                    c.bit_rate = 0;
                    opts.set("preset", SOFTWARE_PRESET);
                    opts.set("crf", (u32::from(q) * 63 / 51).clamp(1, 63));
                    // KEY_FRAME a GOP chiuso, non «forward keyframe»: punto di partenza valido.
                    // lp=4: al più 4 livelli di parallelismo; con quello automatico (come lp=6 su 28 thread)
                    // l'encoder tiene ~1 GB residente a 1080p, con 4 circa 660 MB e 90 fps a 1440p
                    // sul rumore. pred-struct=1 e rtc=1 scendono sotto 100 MB ma ignorano il tetto
                    // di bitrate e crollano sotto i 20 fps sul rumore (DECISIONS 31).
                    opts.set("svtav1-params", "irefresh-type=2:lp=4");
                }
            }

            let ret = ffi::avcodec_open2(enc.ctx, av_codec, &mut opts.0);
            if ret < 0 {
                return Err(open_error(name, "avcodec_open2", ret));
            }
            let ignored = opts.keys();
            if !ignored.is_empty() {
                tracing::warn!(encoder = name, ?ignored, "opzioni dell'encoder ignorate");
            }
            let c = &*enc.ctx;
            if !c.extradata.is_null() && c.extradata_size > 0 {
                enc.params.extradata = std::slice::from_raw_parts(c.extradata, c.extradata_size as usize).to_vec();
            }
        }
        Ok(enc)
    }

    /// Device D3D11VA sul device di sessione, pool NV12 e, per QSV, device e pool QSV derivati.
    /// Restituisce il formato e il pool da dare all'encoder.
    ///
    /// # Safety
    /// Da chiamare una sola volta, con i campi FFmpeg di `self` ancora nulli.
    unsafe fn init_hw(
        &mut self,
        gpu: &GpuDevice,
        s: &EncoderSettings,
        name: &str,
        vendor: Vendor,
        rgb: bool,
    ) -> Result<(ffi::AVPixelFormat, *mut ffi::AVBufferRef), EncodeError> {
        // SAFETY: sequenza documentata di libavutil. Ogni puntatore allocato finisce subito in
        // `self`, che lo libera in Drop anche se un passo successivo fallisce. I campi scritti nei
        // contesti D3D11 seguono hwcontext_d3d11va.h; i riferimenti COM passati a FFmpeg sono
        // AddRef-ati (clone + into_raw) perché FFmpeg li rilascia alla chiusura.
        unsafe {
            self.device_ref = ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::D3D11VA);
            if self.device_ref.is_null() {
                return Err(EncodeError::Failed(format!("{name}: av_hwdevice_ctx_alloc")));
            }
            let dev = (*self.device_ref).data as *mut ffi::AVHWDeviceContext;
            let d3d = (*dev).hwctx as *mut AVD3D11VADeviceContext;
            (*d3d).device = gpu.device.clone().into_raw();
            (*d3d).device_context = gpu.context.clone().into_raw();
            let ret = ffi::av_hwdevice_ctx_init(self.device_ref);
            if ret < 0 {
                return Err(open_error(name, "av_hwdevice_ctx_init", ret));
            }

            self.frames_ref = ffi::av_hwframe_ctx_alloc(self.device_ref);
            if self.frames_ref.is_null() {
                return Err(EncodeError::Failed(format!("{name}: av_hwframe_ctx_alloc")));
            }
            let fc = (*self.frames_ref).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::D3D11;
            (*fc).sw_format = if rgb { ffi::AVPixelFormat::BGRA } else { ffi::AVPixelFormat::NV12 };
            (*fc).width = s.width as c_int;
            (*fc).height = s.height as c_int;
            let fhw = (*fc).hwctx as *mut AVD3D11VAFramesContext;
            if vendor == Vendor::Qsv {
                (*fc).initial_pool_size = QSV_POOL_SIZE;
                (*fhw).bind_flags = D3D11_BIND_DECODER.0 as u32;
            } else {
                (*fhw).bind_flags = D3D11_BIND_RENDER_TARGET.0 as u32;
            }
            let ret = ffi::av_hwframe_ctx_init(self.frames_ref);
            if ret < 0 {
                return Err(open_error(name, "av_hwframe_ctx_init", ret));
            }

            Ok(if vendor == Vendor::Qsv {
                let ret = ffi::av_hwdevice_ctx_create_derived(&mut self.qsv_device_ref, ffi::AVHWDeviceType::QSV, self.device_ref, 0);
                if ret < 0 {
                    return Err(EncodeError::Unsupported(format!("{name}: device QSV derivato: {}", av_error_string(ret))));
                }
                let ret = ffi::av_hwframe_ctx_create_derived(
                    &mut self.qsv_frames_ref,
                    ffi::AVPixelFormat::QSV,
                    self.qsv_device_ref,
                    self.frames_ref,
                    0,
                );
                if ret < 0 {
                    return Err(EncodeError::Unsupported(format!("{name}: frame QSV derivati: {}", av_error_string(ret))));
                }
                self.map_qsv_slots(name)?;
                (ffi::AVPixelFormat::QSV, self.qsv_frames_ref)
            } else {
                (ffi::AVPixelFormat::D3D11, self.frames_ref)
            })
        }
    }

    /// Mappa ogni texture del pool D3D11 fisso in un frame QSV che resta vivo fino a Drop.
    ///
    /// # Safety
    /// `frames_ref` e `qsv_frames_ref` devono essere inizializzati, il secondo derivato dal primo.
    unsafe fn map_qsv_slots(&mut self, name: &str) -> Result<(), EncodeError> {
        // SAFETY: vedi il contratto della funzione; ogni frame allocato finisce subito in
        // `qsv_slots` (liberato in Drop) o viene liberato qui.
        unsafe {
            for _ in 0..QSV_POOL_SIZE {
                let mut d3d = ffi::av_frame_alloc();
                if d3d.is_null() || ffi::av_hwframe_get_buffer(self.frames_ref, d3d, 0) < 0 {
                    ffi::av_frame_free(&mut d3d);
                    break;
                }
                let frame = ffi::av_frame_alloc();
                if frame.is_null() {
                    ffi::av_frame_free(&mut d3d);
                    return Err(EncodeError::Failed(format!("{name}: av_frame_alloc")));
                }
                self.qsv_slots.push(QsvSlot { frame, texture: (*d3d).data[0] as *mut c_void, index: (*d3d).data[1] as usize });
                (*frame).format = ffi::AVPixelFormat::QSV.0;
                (*frame).hw_frames_ctx = ffi::av_buffer_ref(self.qsv_frames_ref);
                // La mappatura tiene un riferimento al frame D3D11 sorgente (e alla texture).
                let ret = ffi::av_hwframe_map(frame, d3d, 0);
                ffi::av_frame_free(&mut d3d);
                if ret < 0 || (*frame).buf[0].is_null() {
                    return Err(EncodeError::Unsupported(format!("{name}: mappatura D3D11→QSV: {}", av_error_string(ret))));
                }
            }
        }
        if self.qsv_slots.len() < 4 {
            return Err(EncodeError::Failed(format!("{name}: pool QSV di {} superfici", self.qsv_slots.len())));
        }
        Ok(())
    }

    /// Errore a runtime: device perso se D3D11 lo dice, altrimenti errore dell'encoder.
    fn runtime_error(&self, step: &str, detail: String) -> EncodeError {
        if self.converter.device_removed() {
            EncodeError::DeviceLost
        } else {
            EncodeError::Failed(format!("{}: {step}: {detail}", self.params.encoder_name))
        }
    }

    fn to_qpc(&self, ts: i64) -> QpcTime {
        let (_, base) = self.first.unwrap_or_default();
        let hns = i128::from(ts) * i128::from(HNS_PER_SEC) / i128::from(self.params.fps.max(1));
        base.add_hns(hns as i64)
    }

    /// Manda all'encoder `send` (un frame di `self` già riempito) con il pts dato, poi svuota.
    fn send(&mut self, send: *mut ffi::AVFrame, pts: i64, sink: &mut dyn FnMut(PacketRef<'_>)) -> Result<(), EncodeError> {
        // SAFETY: `send` è un frame di proprietà di `self` con un buffer valido; l'encoder ne
        // prende un riferimento proprio in avcodec_send_frame, quindi rimandarlo è lecito.
        let ret = unsafe {
            let f = &mut *send;
            f.pts = pts;
            f.duration = 1;
            f.color_primaries = ffi::AVColorPrimaries::BT709;
            f.color_trc = ffi::AVColorTransferCharacteristic::BT709;
            f.colorspace = ffi::AVColorSpace::BT709;
            f.color_range = ffi::AVColorRange::MPEG;
            ffi::avcodec_send_frame(self.ctx, send)
        };
        if ret < 0 {
            return Err(self.runtime_error("avcodec_send_frame", av_error_string(ret)));
        }
        self.drain(sink)
    }

    /// Consegna a `sink` tutti i pacchetti pronti.
    fn drain(&mut self, sink: &mut dyn FnMut(PacketRef<'_>)) -> Result<(), EncodeError> {
        let duration = HNS_PER_SEC / i64::from(self.params.fps.max(1));
        loop {
            // SAFETY: contesto aperto e pacchetto allocato, entrambi di proprietà di `self`.
            let ret = unsafe { ffi::avcodec_receive_packet(self.ctx, self.pkt) };
            if ret == ffi::AVERROR(EAGAIN) || ret == ffi::AVERROR_EOF {
                return Ok(());
            }
            if ret < 0 {
                return Err(self.runtime_error("avcodec_receive_packet", av_error_string(ret)));
            }
            // SAFETY: dopo un receive riuscito `pkt` contiene dati validi per `size` byte fino a
            // av_packet_unref; la fetta non sopravvive alla chiamata di `sink`.
            unsafe {
                let p = &*self.pkt;
                let data = if p.data.is_null() || p.size <= 0 { &[][..] } else { std::slice::from_raw_parts(p.data, p.size as usize) };
                let dts = if p.dts == ffi::AV_NOPTS_VALUE { p.pts } else { p.dts };
                sink(PacketRef {
                    data,
                    pts: self.to_qpc(p.pts),
                    dts: self.to_qpc(dts),
                    duration,
                    // Solo gli IDR aprono un GOP chiuso: un I-frame non IDR o un CRA non bastano.
                    keyframe: p.flags & ffi::AV_PKT_FLAG_KEY != 0
                        && crate::nal::first_vcl_is_idr(self.params.codec, data).unwrap_or(true),
                });
                ffi::av_packet_unref(self.pkt);
            }
        }
    }
}

impl VideoEncoder for HwEncoder {
    fn params(&self) -> &VideoStreamParams {
        &self.params
    }

    fn encode(&mut self, frame: &GpuFrame, sink: &mut dyn FnMut(PacketRef<'_>)) -> Result<(), EncodeError> {
        let (first_index, _) = *self.first.get_or_insert((frame.index, frame.pts));
        let pts = (frame.index.saturating_sub(first_index) as i64).max(self.last_pts + 1);
        self.last_pts = pts;

        // Frame ripetuto (stesso contenuto del precedente, o riempitivo di un tick perso): si rimanda
        // la superficie già convertita. Niente conversione sulla GPU, che sotto carico compete col
        // gioco e fa perdere tick; come i frame duplicati di `ffmpeg -r`, che gli encoder accettano.
        if frame.repeated && !self.last_sent.is_null() {
            return self.send(self.last_sent, pts, sink);
        }
        // Finché la conversione non riesce, nessuna superficie è valida da rimandare.
        self.last_sent = ptr::null_mut();
        let (tex, slice, send) = if self.vendor == Vendor::Software {
            // SAFETY: frame yuv420p allocato in open; alloca un buffer nuovo solo se l'encoder ne
            // tiene ancora un riferimento (non succede dopo il drain del frame precedente).
            let ret = unsafe { ffi::av_frame_make_writable(self.frame) };
            if ret < 0 {
                return Err(self.runtime_error("av_frame_make_writable", av_error_string(ret)));
            }
            (ptr::null_mut(), 0, self.frame)
        } else if self.qsv_slots.is_empty() {
            // SAFETY: frame e pool sono di proprietà di `self`; av_hwframe_get_buffer riempie
            // data[0] (ID3D11Texture2D*) e data[1] (indice nell'array) secondo hwcontext_d3d11va.h.
            unsafe {
                ffi::av_frame_unref(self.frame);
                let ret = ffi::av_hwframe_get_buffer(self.frames_ref, self.frame, 0);
                if ret < 0 {
                    return Err(self.runtime_error("av_hwframe_get_buffer", av_error_string(ret)));
                }
                ((*self.frame).data[0] as *mut c_void, (*self.frame).data[1] as usize, self.frame)
            }
        } else {
            let n = self.qsv_slots.len();
            // SAFETY: i frame delle superfici sono mappati e vivi fino a Drop; buf[0] non è nullo
            // (verificato in map_qsv_slots).
            let free = (0..n)
                .map(|k| (self.next_slot + k) % n)
                .find(|&i| unsafe { ffi::av_buffer_get_ref_count((*self.qsv_slots[i].frame).buf[0]) } == 1);
            let Some(i) = free else {
                return Err(self.runtime_error("superficie QSV", format!("tutte le {n} superfici sono occupate")));
            };
            self.next_slot = (i + 1) % n;
            let slot = &self.qsv_slots[i];
            (slot.texture, slot.index, slot.frame)
        };
        if let Err(e) = self.converter.blit(&frame.texture, frame.width, frame.height, tex, slice) {
            return Err(self.runtime_error("VideoProcessorBlt", e.to_string()));
        }
        if self.vendor == Vendor::Software {
            let (w, h) = (self.params.width as usize, self.params.height as usize);
            // SAFETY: i piani di un frame yuv420p scrivibile allocato da av_frame_get_buffer hanno
            // `linesize` byte per riga, `h` righe per Y e `h/2` per U e V; nessun altro li tocca
            // finché non chiamiamo avcodec_send_frame.
            let (dy, du, dv) = unsafe {
                let f = &*self.frame;
                let plane = |i: usize, rows: usize| Plane {
                    data: std::slice::from_raw_parts_mut(f.data[i], f.linesize[i].max(0) as usize * rows),
                    stride: f.linesize[i].max(0) as usize,
                };
                (plane(0, h), plane(1, h / 2), plane(2, h / 2))
            };
            if let Err(e) = self.converter.read_nv12(|y, yp, uv, uvp| nv12_to_i420(w, h, (y, yp), (uv, uvp), dy, du, dv)) {
                return Err(self.runtime_error("lettura del frame dalla GPU", e.to_string()));
            }
        }

        self.last_sent = send;
        self.send(send, pts, sink)
    }

    fn flush(&mut self, sink: &mut dyn FnMut(PacketRef<'_>)) -> Result<(), EncodeError> {
        // SAFETY: contesto aperto; un frame nullo chiede lo svuotamento.
        let ret = unsafe { ffi::avcodec_send_frame(self.ctx, ptr::null()) };
        if ret < 0 && ret != ffi::AVERROR_EOF {
            return Err(self.runtime_error("flush", av_error_string(ret)));
        }
        self.drain(sink)
    }
}
