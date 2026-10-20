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
/// Estensioni delle clip, per `cleanup_stale`.
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

/// Rinomina `part` sul primo nome libero, ritentando per circa mezzo secondo se il file è
/// bloccato. Non cancella mai `part`.
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
            Err(source) => return Err(MuxError::RenameFailed { part: part.to_path_buf(), source }),
        }
    }
}

/// Pulizia all'avvio di `dir` (la cartella delle clip): toglie i `.part` più vecchi di un'ora
/// (scritture interrotte) e i file di clip da 0 byte. Restituisce quanti elementi ha tolto.
pub fn cleanup_stale(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let hour_ago = SystemTime::now().checked_sub(Duration::from_secs(3600));
    let mut removed = 0;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
        let old = matches!((meta.modified(), hour_ago), (Ok(m), Some(h)) if m < h);
        let gone = if ext == "part" && old {
            if meta.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) }.is_ok()
        } else if meta.is_file() && meta.len() == 0 && CLIP_EXTENSIONS.contains(&ext.as_str()) {
            std::fs::remove_file(&path).is_ok()
        } else {
            false
        };
        removed += usize::from(gone);
    }
    removed
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
pub fn write_video_clip(
    path: &Path,
    video: &VideoSnapshot,
    audio: Option<&AudioSnapshot>,
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
            let skip = hns_to_frames(start.hns_since(a.start));
            a.tracks.iter().map(|t| Track { name: &t.name, samples: &t.samples, skip, frames }).collect()
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
        ffi::write(part, format, Some(VideoIn { snapshot: video, start }), &tracks, codec, faststart)
    })?;
    Ok(SavedClip {
        kind: ClipKind::Video,
        bytes: size_of(&path),
        path,
        duration_secs: duration_hns as f64 / HNS_PER_SEC as f64,
    })
}

/// Frame stereo `i` dell'uscita, cioè il frame `i + skip` di `samples`; silenzio fuori dai dati.
pub(crate) fn frame_at(samples: &[f32], skip: i64, i: usize) -> [f32; CHANNELS] {
    usize::try_from(i as i64 + skip)
        .ok()
        .and_then(|src| samples.get(src * CHANNELS..(src + 1) * CHANNELS))
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0.0; CHANNELS])
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
pub fn write_audio_clip(path: &Path, audio: &AudioSnapshot, format: AudioClipFormat) -> Result<SavedClip, MuxError> {
    let first = audio.tracks.first().ok_or(MuxError::Empty("nessuna traccia audio"))?;
    let frames = first.samples.len() / CHANNELS;
    let duration_secs = frames as f64 / f64::from(SAMPLE_RATE);
    // Al massimo PCM 24 bit: anche il FLAC non supera questa dimensione.
    let needed = (frames * CHANNELS * 3 * audio.tracks.len()) as u64;
    ensure_space(path, needed)?;

    let (ffmt, codec, ext) = match format {
        AudioClipFormat::FlacPerTrack => ("flac", AudioCodec::Flac, ".flac"),
        AudioClipFormat::WavPerTrack => ("wav", AudioCodec::PcmS24, ".wav"),
        AudioClipFormat::Mka => ("matroska", AudioCodec::Flac, ".mka"),
    };
    let tracks: Vec<Track<'_>> = audio
        .tracks
        .iter()
        .map(|t| Track { name: &t.name, samples: &t.samples, skip: 0, frames: t.samples.len() / CHANNELS })
        .collect();

    let saved = if format == AudioClipFormat::Mka || tracks.len() == 1 {
        write_reserved(path, ext, false, |part| ffi::write(part, ffmt, None, &tracks, codec, false))?
    } else {
        write_reserved(path, "", true, |part| {
            std::fs::create_dir(part).map_err(io("creazione della cartella", part))?;
            for (i, t) in tracks.iter().enumerate() {
                let file = part.join(format!("{:02} - {}{ext}", i + 1, file_name(t.name)));
                ffi::write(&file, ffmt, None, std::slice::from_ref(t), codec, false)?;
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
        // Audio iniziato 2 frame prima del video: si saltano.
        assert_eq!(frame_at(&s, 2, 0), [2.0, -2.0]);
        assert_eq!(frame_at(&s, 2, 2), [4.0, -4.0]);
        // Audio iniziato 2 frame dopo: silenzio davanti, silenzio in coda oltre i dati.
        assert_eq!(frame_at(&s, -2, 1), [0.0, 0.0]);
        assert_eq!(frame_at(&s, -2, 11), [9.0, -9.0]);
        assert_eq!(frame_at(&s, -2, 12), [0.0, 0.0]);
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
        std::fs::create_dir_all(dir.join("old.123-0.part")).unwrap();
        std::fs::write(dir.join("old.mp4.1-0.part"), b"x").unwrap();
        std::fs::write(dir.join("new.mp4.1-1.part"), b"x").unwrap();
        std::fs::write(dir.join("vuota.mp4"), b"").unwrap();
        std::fs::write(dir.join("buona.mp4"), b"x").unwrap();
        std::fs::write(dir.join("vuoto.txt"), b"").unwrap();
        let two_hours_ago = SystemTime::now() - Duration::from_secs(7200);
        for p in ["old.123-0.part", "old.mp4.1-0.part"] {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_BACKUP_SEMANTICS: serve per aprire anche una cartella.
            let f = std::fs::File::options().write(true).custom_flags(0x0200_0000).open(dir.join(p)).unwrap();
            f.set_modified(two_hours_ago).unwrap();
        }
        assert_eq!(cleanup_stale(&dir), 3);
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["buona.mp4", "new.mp4.1-1.part", "vuoto.txt"]);
    }

    #[test]
    fn disk_space_is_checked() {
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/mux/space");
        assert!(ensure_space(&target, 1 << 20).is_ok());
        assert!(matches!(ensure_space(&target, u64::MAX / 4), Err(MuxError::DiskFull { .. })));
    }

    #[test]
    fn safe_file_names() {
        assert_eq!(file_name("Mic: 1/2?"), "Mic_ 1_2_");
        assert_eq!(file_name("  ..  "), "Traccia");
        assert_eq!(with_suffix(Path::new("a/Half-Life 2.5"), ".mp4"), PathBuf::from("a/Half-Life 2.5.mp4"));
    }
}
