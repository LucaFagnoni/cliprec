//! Scrittura delle clip: MP4/MKV multitraccia con AAC, clip audio FLAC/WAV/MKA, file .part rinominato a fine scrittura.
//!
//! Contratti in `cliprec-core`; vedi SPEC.md, sezioni «Buffer in RAM e salvataggio clip» e
//! «Clip solo audio».
//!
//! - Il video è copiato senza ricodifica; i tempi partono da zero sul primo frame.
//! - Ogni traccia audio è codificata a parte (AAC nativo di FFmpeg nelle clip video), con il nome
//!   in `title` e `handler_name`; solo la prima traccia audio è predefinita.
//! - Si scrive sempre su `<nome>.part` (file o cartella), rinominato solo a scrittura completa;
//!   prima si controlla lo spazio libero.
//! - WAV a 24 bit intero: i campioni del buffer sono già quantizzati a 24 bit (FLAC), quindi il
//!   float a 32 bit non aggiungerebbe nulla e occuperebbe un terzo in più; il PCM 24 bit si apre
//!   ovunque, anche dove il WAV float non è supportato.

mod ffi;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use cliprec_core::config::{AudioClipFormat, VideoContainer};
use cliprec_core::time::hns_to_frames;
use cliprec_core::{AudioSnapshot, CHANNELS, ClipKind, HNS_PER_SEC, SAMPLE_RATE, SavedClip, VideoSnapshot};

use cliprec_buffer::{AudioRefs, TrackReader};
use cliprec_core::QpcTime;

use crate::ffi::{AudioCodec, Track, VideoIn};

#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("FFmpeg, {op}: {message} ({code})")]
    Ffmpeg { op: &'static str, code: i32, message: String },
    #[error("encoder {0} non presente nella build di FFmpeg")]
    EncoderMissing(String),
    #[error("spazio su disco insufficiente: servono circa {needed} MB, liberi {available} MB")]
    DiskFull { needed: u64, available: u64 },
    #[error("nome non valido: {0}")]
    InvalidName(String),
    #[error("niente da scrivere: {0}")]
    Empty(&'static str),
    #[error("thread di codifica audio terminato in modo anomalo")]
    EncoderPanicked,
    #[error("memoria esaurita in FFmpeg")]
    OutOfMemory,
    #[error("clip scritta ma non rinominata (resta in {part}): {source}")]
    RenameFailed { part: PathBuf, source: std::io::Error },
    #[error("{op} {path}: {source}")]
    Io { op: &'static str, path: PathBuf, source: std::io::Error },
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoClipOptions {
    pub container: VideoContainer,
    /// Bitrate AAC per traccia, in bit/s.
    pub audio_bit_rate: u32,
}

impl Default for VideoClipOptions {
    fn default() -> Self {
        Self { container: VideoContainer::Mp4, audio_bit_rate: 192_000 }
    }
}

/// `base` + `suffix` (es. ".mp4"), senza toccare eventuali punti già nel nome.
fn with_suffix(base: &Path, suffix: &str) -> PathBuf {
    let mut s = OsString::from(base.as_os_str());
    s.push(suffix);
    PathBuf::from(s)
}

fn io(op: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> MuxError {
    let path = path.to_path_buf();
    move |source| MuxError::Io { op, path, source }
}

/// Controlla che sul volume di `target` ci siano almeno `needed` byte (più 64 MB di margine).
fn ensure_space(target: &Path, needed: u64) -> Result<(), MuxError> {
    let dir = target.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(io("creazione della cartella", dir))?;
    let needed = needed + (64 << 20);
    match ffi::free_space(dir) {
        Some(available) if available < needed => {
            Err(MuxError::DiskFull { needed: needed >> 20, available: available >> 20 })
        }
        _ => Ok(()),
    }
}

static PART_SEQ: AtomicU64 = AtomicU64::new(0);
/// Tentativi di rinomina quando il file è tenuto aperto da altri (anteprima, indicizzatore,
/// antivirus, OneDrive): 10 × 50 ms.
const RENAME_TRIES: u32 = 10;
const RENAME_WAIT: Duration = Duration::from_millis(50);
/// Estensioni dei file di clip, per riconoscere i `.part` in `cleanup_stale`.
const CLIP_EXTENSIONS: [&str; 5] = ["mp4", "mkv", "flac", "wav", "mka"];

/// Scrive con `write` su un `.part` univoco (`<base><suffix>.<PID>-<n>.part`) e solo a scrittura
/// completa lo rinomina, senza mai sovrascrivere, sul primo nome libero tra `base+suffix`,
/// `base (2)+suffix`... Finché la clip non è completa non esiste nessun file con il nome finale.
/// Se la scrittura fallisce il `.part` viene tolto; se fallisce solo la rinomina il `.part` (una
/// clip completa) resta e l'errore ne riporta il percorso.
fn write_reserved(
    base: &Path,
    suffix: &str,
    dir: bool,
    write: impl FnOnce(&Path) -> Result<(), MuxError>,
) -> Result<PathBuf, MuxError> {
    let seq = PART_SEQ.fetch_add(1, Ordering::Relaxed);
    let part = with_suffix(base, &format!("{suffix}.{}-{seq}.part", std::process::id()));
    if let Err(e) = write(&part) {
        let _ = if dir { std::fs::remove_dir_all(&part) } else { std::fs::remove_file(&part) };
        return Err(e);
    }
    publish(&part, base, suffix, ffi::rename_no_replace)
}

/// Rinomina `part` (una clip completa) sul primo nome libero, ritentando per circa mezzo secondo
/// se il file è bloccato. Non cancella mai `part`: se non riesce lo segna con `.ok`, così
/// `cleanup_stale` lo rinomina al prossimo avvio invece di cancellarlo.
fn publish(
    part: &Path,
    base: &Path,
    suffix: &str,
    rename: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<PathBuf, MuxError> {
    let mut n = 1u32;
    let mut tries = 0;
    loop {
        let target = if n == 1 { with_suffix(base, suffix) } else { with_suffix(base, &format!(" ({n}){suffix}")) };
        match rename(part, &target) {
            Ok(()) => return Ok(target),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 10_000 => n += 1,
            Err(_) if tries + 1 < RENAME_TRIES => {
                tries += 1;
                std::thread::sleep(RENAME_WAIT);
            }
            Err(source) => {
                let _ = std::fs::write(with_suffix(part, COMPLETE_MARK), b"");
                return Err(MuxError::RenameFailed { part: part.to_path_buf(), source });
            }
        }
    }
}

/// Segno accanto a un `.part` completo che non si è potuto rinominare.
const COMPLETE_MARK: &str = ".ok";

/// Se `name` è un `.part` scritto da ClipRec (`<nome><ext>.<PID>-<n>.part`, PID e contatore
/// numerici) restituisce `<nome>` ed `<ext>` (vuota per le cartelle delle clip audio).
fn parse_part(name: &str) -> Option<(&str, &str)> {
    let (head, id) = name.strip_suffix(".part")?.rsplit_once('.')?;
    let (pid, n) = id.split_once('-')?;
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !numeric(pid) || !numeric(n) {
        return None;
    }
    for ext in CLIP_EXTENSIONS {
        if let Some(base) = head.strip_suffix(ext).and_then(|h| h.strip_suffix('.')) {
            return Some((base, &head[base.len()..]));
        }
    }
    Some((head, ""))
}

/// `true` se la cartella contiene solo file di traccia di ClipRec (`NN - Nome.flac`/`.wav`).
fn is_clip_folder(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else { return false };
    entries.filter_map(Result::ok).all(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        let b = name.as_bytes();
        e.file_type().is_ok_and(|t| t.is_file())
            && b.len() > 5
            && b[..2].iter().all(u8::is_ascii_digit)
            && name[2..].starts_with(" - ")
            && (name.ends_with(".flac") || name.ends_with(".wav"))
    })
}

/// Pulizia all'avvio di `dir` (la cartella delle clip). Tocca solo i `.part` con lo schema di
/// ClipRec: quelli completi ma non rinominati (segnati con `.ok`) vengono rinominati sul nome
/// libero; quelli più vecchi di un'ora senza segno (scritture interrotte) vengono tolti. Le
/// cartelle `.part` si tolgono solo se contengono soltanto file di traccia. Restituisce quanti
/// elementi ha sistemato.
pub fn cleanup_stale(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let hour_ago = SystemTime::now().checked_sub(Duration::from_secs(3600));
    let mut done = 0;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        // Segno rimasto senza il suo `.part`.
        if let Some(part) = name.strip_suffix(COMPLETE_MARK).filter(|p| parse_part(p).is_some()) {
            if !dir.join(part).exists() && std::fs::remove_file(&path).is_ok() {
                done += 1;
            }
            continue;
        }
        let Some((base, ext)) = parse_part(&name) else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        let is_dir = meta.is_dir();
        if ext.is_empty() != is_dir || (is_dir && !is_clip_folder(&path)) {
            continue;
        }
        let mark = with_suffix(&path, COMPLETE_MARK);
        if mark.exists() {
            if publish(&path, &dir.join(base), ext, ffi::rename_no_replace).is_ok() {
                let _ = std::fs::remove_file(&mark);
                done += 1;
            }
            continue;
        }
        let old = matches!((meta.modified(), hour_ago), (Ok(m), Some(h)) if m < h);
        if old && if is_dir { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) }.is_ok() {
            done += 1;
        }
    }
    done
}

fn size_of(path: &Path) -> u64 {
    if path.is_dir() {
        std::fs::read_dir(path)
            .map(|d| d.filter_map(|e| e.ok()?.metadata().ok()).map(|m| m.len()).sum())
            .unwrap_or(0)
    } else {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }
}

/// Scrive una clip video in `<path>.mp4` (o `.mkv`). `path` è senza estensione.
///
/// L'audio viene allineato al primo frame video (tagliato o preceduto da silenzio) e portato
/// alla stessa durata del video.
pub fn write_video_clip<A: ClipAudio + ?Sized>(
    path: &Path,
    video: &VideoSnapshot,
    audio: Option<&A>,
    opts: &VideoClipOptions,
) -> Result<SavedClip, MuxError> {
    let start = video.start().ok_or(MuxError::Empty("nessun pacchetto video"))?;
    let end = video.packets.iter().map(|p| p.pts.add_hns(p.duration.max(0))).max().unwrap_or(start);
    let duration_hns = end.hns_since(start);
    let frames = usize::try_from(hns_to_frames(duration_hns)).unwrap_or(0);

    // Campioni da saltare (>0) o silenzio da anteporre (<0) per partire dal primo frame: un
    // offset, senza copiare il PCM.
    let tracks: Vec<Track<'_>> = audio
        .map(|a| {
            let skip = hns_to_frames(start.hns_since(a.start()));
            (0..a.track_count())
                .map(|i| Track { name: a.track_name(i), source: Aligned::new(a.reader(i), skip, frames) })
                .collect()
        })
        .unwrap_or_default();

    let (format, suffix, faststart) = match opts.container {
        VideoContainer::Mp4 => ("mp4", ".mp4", true),
        VideoContainer::Mkv => ("matroska", ".mkv", false),
    };
    let video_bytes: u64 = video.packets.iter().map(|p| p.data.len() as u64).sum();
    let audio_bytes = u64::from(opts.audio_bit_rate) / 8 * (duration_hns / HNS_PER_SEC + 1) as u64;
    ensure_space(path, video_bytes + audio_bytes * tracks.len() as u64)?;

    let codec = AudioCodec::Aac { bit_rate: i64::from(opts.audio_bit_rate) };
    let path = write_reserved(path, suffix, false, |part| {
        ffi::write(part, format, Some(VideoIn { snapshot: video, start }), tracks, codec, faststart)
    })?;
    Ok(SavedClip {
        kind: ClipKind::Video,
        bytes: size_of(&path),
        path,
        duration_secs: duration_hns as f64 / HNS_PER_SEC as f64,
    })
}

/// Lettore di PCM f32 stereo interleaved a 48 kHz.
pub trait PcmReader {
    /// Riempie `out` con i frame successivi e restituisce quanti ne ha scritti (0 alla fine).
    fn read(&mut self, out: &mut [f32]) -> usize;
}

/// Audio di una clip, letto a pezzi: [`AudioRefs`] del buffer (decompresso in streaming, il
/// modo previsto per salvare) o un [`AudioSnapshot`] già in memoria.
pub trait ClipAudio: Sync {
    /// Istante QPC del primo campione di ogni traccia.
    fn start(&self) -> QpcTime;
    /// Lunghezza di ogni traccia in frame.
    fn frames(&self) -> usize;
    fn track_count(&self) -> usize;
    fn track_name(&self, track: usize) -> &str;
    fn reader(&self, track: usize) -> Box<dyn PcmReader + Send + '_>;
}

impl PcmReader for TrackReader<'_> {
    fn read(&mut self, out: &mut [f32]) -> usize {
        TrackReader::read(self, out)
    }
}

impl ClipAudio for AudioRefs {
    fn start(&self) -> QpcTime {
        AudioRefs::start(self)
    }
    fn frames(&self) -> usize {
        AudioRefs::frames(self)
    }
    fn track_count(&self) -> usize {
        AudioRefs::track_count(self)
    }
    fn track_name(&self, track: usize) -> &str {
        AudioRefs::track_name(self, track).unwrap_or_default()
    }
    fn reader(&self, track: usize) -> Box<dyn PcmReader + Send + '_> {
        match AudioRefs::reader(self, track) {
            Some(r) => Box::new(r),
            None => Box::new(SliceReader { samples: &[] }),
        }
    }
}

struct SliceReader<'a> {
    samples: &'a [f32],
}

impl PcmReader for SliceReader<'_> {
    fn read(&mut self, out: &mut [f32]) -> usize {
        let n = out.len().min(self.samples.len()) / CHANNELS * CHANNELS;
        let (head, rest) = self.samples.split_at(n);
        out[..n].copy_from_slice(head);
        self.samples = rest;
        n / CHANNELS
    }
}

impl ClipAudio for AudioSnapshot {
    fn start(&self) -> QpcTime {
        self.start
    }
    fn frames(&self) -> usize {
        self.tracks.first().map_or(0, |t| t.samples.len() / CHANNELS)
    }
    fn track_count(&self) -> usize {
        self.tracks.len()
    }
    fn track_name(&self, track: usize) -> &str {
        self.tracks.get(track).map_or("", |t| t.name.as_str())
    }
    fn reader(&self, track: usize) -> Box<dyn PcmReader + Send + '_> {
        Box::new(SliceReader { samples: self.tracks.get(track).map_or(&[][..], |t| t.samples.as_slice()) })
    }
}

/// PCM allineato all'inizio del file: salta `skip` frame della sorgente (se positivo) o
/// antepone silenzio (se negativo), poi dà esattamente `left` frame, con silenzio oltre i dati.
pub(crate) struct Aligned<'a> {
    inner: Box<dyn PcmReader + Send + 'a>,
    skip: i64,
    left: usize,
}

impl<'a> Aligned<'a> {
    pub(crate) fn new(inner: Box<dyn PcmReader + Send + 'a>, skip: i64, frames: usize) -> Self {
        Self { inner, skip, left: frames }
    }

    pub(crate) fn read(&mut self, out: &mut [f32]) -> usize {
        let want = (out.len() / CHANNELS).min(self.left);
        let out = &mut out[..want * CHANNELS];
        while self.skip > 0 && want > 0 {
            let n = usize::try_from(self.skip).unwrap_or(usize::MAX).min(want);
            let got = self.inner.read(&mut out[..n * CHANNELS]);
            if got == 0 {
                self.skip = 0;
            }
            self.skip -= got as i64;
        }
        let mut w = 0;
        if self.skip < 0 {
            let pad = usize::try_from(-self.skip).unwrap_or(usize::MAX).min(want);
            out[..pad * CHANNELS].fill(0.0);
            self.skip += pad as i64;
            w = pad;
        }
        while w < want {
            let got = self.inner.read(&mut out[w * CHANNELS..]);
            if got == 0 {
                out[w * CHANNELS..].fill(0.0);
                break;
            }
            w += got;
        }
        self.left -= want;
        want
    }
}

/// Nome di file sicuro per Windows a partire dal nome di una traccia.
fn file_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
    let s = s.trim().trim_end_matches('.').to_owned();
    if s.is_empty() { "Traccia".into() } else { s }
}

/// Scrive una clip solo audio. `path` è senza estensione:
/// - `FlacPerTrack`/`WavPerTrack`: cartella `path` con `01 - Nome.flac`... (stesso istante
///   d'inizio); con una sola traccia direttamente `path.flac`/`path.wav`;
/// - `Mka`: `path.mka` multitraccia FLAC con i nomi.
pub fn write_audio_clip<A: ClipAudio + ?Sized>(
    path: &Path,
    audio: &A,
    format: AudioClipFormat,
) -> Result<SavedClip, MuxError> {
    let count = audio.track_count();
    if count == 0 {
        return Err(MuxError::Empty("nessuna traccia audio"));
    }
    let frames = audio.frames();
    let duration_secs = frames as f64 / f64::from(SAMPLE_RATE);
    // Al massimo PCM 24 bit: anche il FLAC non supera questa dimensione.
    let needed = (frames * CHANNELS * 3 * count) as u64;
    ensure_space(path, needed)?;

    let (ffmt, codec, ext) = match format {
        AudioClipFormat::FlacPerTrack => ("flac", AudioCodec::Flac, ".flac"),
        AudioClipFormat::WavPerTrack => ("wav", AudioCodec::PcmS24, ".wav"),
        AudioClipFormat::Mka => ("matroska", AudioCodec::Flac, ".mka"),
        // Formati nuovi del contratto: li implementa replay-buffer (m27).
        AudioClipFormat::PolyWav | AudioClipFormat::Opus | AudioClipFormat::Aac => {
            return Err(MuxError::EncoderMissing(format!("{format:?} non ancora supportato")));
        }
    };
    let track = |i: usize| Track { name: audio.track_name(i), source: Aligned::new(audio.reader(i), 0, frames) };

    let saved = if format == AudioClipFormat::Mka || count == 1 {
        write_reserved(path, ext, false, |part| ffi::write(part, ffmt, None, (0..count).map(track).collect(), codec, false))?
    } else {
        write_reserved(path, "", true, |part| {
            std::fs::create_dir(part).map_err(io("creazione della cartella", part))?;
            for i in 0..count {
                let file = part.join(format!("{:02} - {}{ext}", i + 1, file_name(audio.track_name(i))));
                ffi::write(&file, ffmt, None, vec![track(i)], codec, false)?;
            }
            Ok(())
        })?
    };
    Ok(SavedClip { kind: ClipKind::Audio, bytes: size_of(&saved), path: saved, duration_secs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_trims_or_pads() {
        let s: Vec<f32> = (0..10).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let read_all = |skip: i64, frames: usize| {
            let mut a = Aligned::new(Box::new(SliceReader { samples: &s }), skip, frames);
            let mut out = Vec::new();
            let mut buf = [9.9f32; 6]; // pezzi da 3 frame
            loop {
                let n = a.read(&mut buf);
                if n == 0 {
                    break out;
                }
                out.extend_from_slice(&buf[..n * 2]);
            }
        };
        // Audio iniziato 2 frame prima del video: si saltano.
        assert_eq!(read_all(2, 3), vec![2.0, -2.0, 3.0, -3.0, 4.0, -4.0]);
        // Audio iniziato 2 frame dopo: silenzio davanti, silenzio in coda oltre i dati.
        let a = read_all(-2, 13);
        assert_eq!(a.len(), 26);
        assert_eq!(&a[..4], &[0.0, 0.0, 0.0, 0.0]);
        assert_eq!(&a[4..6], &[0.0, -0.0]);
        assert_eq!(&a[22..24], &[9.0, -9.0]);
        assert_eq!(&a[24..26], &[0.0, 0.0]);
    }

    #[test]
    fn locked_rename_keeps_the_part() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/mux/rename");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("clip");
        let part = dir.join("clip.mp4.1-1.part");
        std::fs::write(&part, b"clip").unwrap();
        let locked = |_: &Path, _: &Path| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        let t = std::time::Instant::now();
        match publish(&part, &base, ".mp4", locked) {
            Err(MuxError::RenameFailed { part: p, .. }) => assert_eq!(p, part),
            other => panic!("atteso RenameFailed, avuto {other:?}"),
        }
        assert!(t.elapsed() >= Duration::from_millis(400), "ritenta per circa mezzo secondo");
        assert!(part.exists(), "la clip non va cancellata");
        let mark = dir.join("clip.mp4.1-1.part.ok");
        assert!(mark.exists(), "segno di clip completa per cleanup_stale");
        std::fs::remove_file(mark).unwrap();
        // Bloccato per due tentativi, poi libero: riesce.
        let calls = std::cell::Cell::new(0);
        let flaky = |a: &Path, b: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() <= 2 { Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)) } else { ffi::rename_no_replace(a, b) }
        };
        assert_eq!(publish(&part, &base, ".mp4", flaky).unwrap(), dir.join("clip.mp4"));
        // Nome già preso: non sovrascrive, passa a « (2)».
        std::fs::write(&part, b"altra").unwrap();
        assert_eq!(publish(&part, &base, ".mp4", ffi::rename_no_replace).unwrap(), dir.join("clip (2).mp4"));
        assert_eq!(std::fs::read(dir.join("clip.mp4")).unwrap(), b"clip");
    }

    #[test]
    fn final_name_appears_only_when_complete() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/mux/reserve");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("clip");
        let saved = write_reserved(&base, ".mp4", false, |part| {
            assert!(!dir.join("clip.mp4").exists(), "nessun file col nome finale durante la scrittura");
            std::fs::write(part, b"clip").map_err(io("scrittura", part))
        })
        .unwrap();
        assert_eq!(saved, dir.join("clip.mp4"));
        // Scrittura fallita: niente .part né nome finale.
        let failed = write_reserved(&base, ".mkv", false, |part| {
            std::fs::write(part, b"mezza").unwrap();
            Err(MuxError::Empty("prova"))
        });
        assert!(failed.is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn stale_files_are_cleaned() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/mux/stale");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Cartella audio interrotta (schema ClipRec, solo file di traccia): da togliere.
        std::fs::create_dir_all(dir.join("audio.123-0.part")).unwrap();
        std::fs::write(dir.join("audio.123-0.part").join("01 - Gioco.flac"), b"x").unwrap();
        // Cartella con lo schema ma con altro dentro: mai remove_dir_all.
        std::fs::create_dir_all(dir.join("altro.123-1.part")).unwrap();
        std::fs::write(dir.join("altro.123-1.part").join("documento.txt"), b"x").unwrap();
        std::fs::write(dir.join("old.mp4.1-0.part"), b"x").unwrap();
        std::fs::write(dir.join("new.mp4.1-1.part"), b"x").unwrap();
        // Download di Firefox in pausa e simili: fuori schema, intoccabili.
        std::fs::write(dir.join("film.mp4.part"), b"x").unwrap();
        std::fs::write(dir.join("serie.mkv.abc-1.part"), b"x").unwrap();
        std::fs::write(dir.join("vuota.mp4"), b"").unwrap();
        // Clip completa non rinominata: va rinominata, non cancellata.
        std::fs::write(dir.join("salvata.mp4.7-3.part"), b"clip").unwrap();
        std::fs::write(dir.join("salvata.mp4.7-3.part.ok"), b"").unwrap();
        let two_hours_ago = SystemTime::now() - Duration::from_secs(7200);
        for p in ["audio.123-0.part", "altro.123-1.part", "old.mp4.1-0.part", "film.mp4.part", "serie.mkv.abc-1.part", "salvata.mp4.7-3.part"] {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_BACKUP_SEMANTICS: serve per aprire anche una cartella.
            let f = std::fs::File::options().write(true).custom_flags(0x0200_0000).open(dir.join(p)).unwrap();
            f.set_modified(two_hours_ago).unwrap();
        }
        assert_eq!(cleanup_stale(&dir), 3);
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(
            left,
            ["altro.123-1.part", "film.mp4.part", "new.mp4.1-1.part", "salvata.mp4", "serie.mkv.abc-1.part", "vuota.mp4"]
        );
        assert_eq!(std::fs::read(dir.join("salvata.mp4")).unwrap(), b"clip");
    }

    #[test]
    fn part_names_follow_the_schema() {
        assert_eq!(parse_part("Gioco_2026.mp4.1234-0.part"), Some(("Gioco_2026", ".mp4")));
        assert_eq!(parse_part("Gioco 2.5.mkv.1-22.part"), Some(("Gioco 2.5", ".mkv")));
        assert_eq!(parse_part("audio.1-2.part"), Some(("audio", "")));
        assert_eq!(parse_part("film.mp4.part"), None);
        assert_eq!(parse_part("x.mp4.12-.part"), None);
        assert_eq!(parse_part("x.mp4.a-1.part"), None);
    }

    #[test]
    fn disk_space_is_checked() {
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/mux/space");
        assert!(ensure_space(&target, 1 << 20).is_ok());
        assert!(matches!(ensure_space(&target, u64::MAX / 4), Err(MuxError::DiskFull { .. })));
    }

    /// m8: i thread di codifica audio girano sotto la priorità normale.
    #[test]
    fn encoder_threads_run_at_low_priority() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/mux/priority");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tracks = vec![cliprec_core::AudioTrackPcm { name: "A".into(), samples: vec![0.1; 48_000 * 2] }];
        let audio = AudioSnapshot { start: QpcTime(0), tracks };
        write_audio_clip(&dir.join("clip"), &audio, AudioClipFormat::FlacPerTrack).unwrap();
        let p = ffi::ENCODER_PRIORITY.load(Ordering::Relaxed);
        assert!(p < 0, "priorità del thread di codifica: {p} (0 = normale)");
    }

    #[test]
    fn safe_file_names() {
        assert_eq!(file_name("Mic: 1/2?"), "Mic_ 1_2_");
        assert_eq!(file_name("  ..  "), "Traccia");
        assert_eq!(with_suffix(Path::new("a/Half-Life 2.5"), ".mp4"), PathBuf::from("a/Half-Life 2.5.mp4"));
    }
}
