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
    #[error("memoria esaurita in FFmpeg")]
    OutOfMemory,
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

/// Rinomina `.part` nel nome finale; se il nome esiste già aggiunge « (2)», « (3)»...
fn finish(part: &Path, base: &Path, suffix: &str) -> Result<PathBuf, MuxError> {
    let mut target = with_suffix(base, suffix);
    let mut n = 2;
    while target.exists() {
        target = with_suffix(base, &format!(" ({n}){suffix}"));
        n += 1;
    }
    std::fs::rename(part, &target).map_err(io("rinomina", part))?;
    Ok(target)
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

    let aligned: Vec<(String, Vec<f32>)> = audio
        .map(|a| {
            // Campioni da saltare (>0) o silenzio da anteporre (<0) per partire dal primo frame.
            let skip = hns_to_frames(start.hns_since(a.start));
            a.tracks.iter().map(|t| (t.name.clone(), align(&t.samples, skip, frames))).collect()
        })
        .unwrap_or_default();

    let (format, suffix, faststart) = match opts.container {
        VideoContainer::Mp4 => ("mp4", ".mp4", true),
        VideoContainer::Mkv => ("matroska", ".mkv", false),
    };
    let video_bytes: u64 = video.packets.iter().map(|p| p.data.len() as u64).sum();
    let audio_bytes = u64::from(opts.audio_bit_rate) / 8 * (duration_hns / HNS_PER_SEC + 1) as u64;
    ensure_space(path, video_bytes + audio_bytes * aligned.len() as u64)?;

    let part = with_suffix(path, &format!("{suffix}.part"));
    let tracks: Vec<Track<'_>> = aligned.iter().map(|(name, s)| Track { name, samples: s }).collect();
    let codec = AudioCodec::Aac { bit_rate: i64::from(opts.audio_bit_rate) };
    let written = ffi::write(&part, format, Some(VideoIn { snapshot: video, start }), &tracks, codec, faststart);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    let path = finish(&part, path, suffix)?;
    Ok(SavedClip {
        kind: ClipKind::Video,
        bytes: size_of(&path),
        path,
        duration_secs: duration_hns as f64 / HNS_PER_SEC as f64,
    })
}

/// `frames` frame stereo a partire dal frame `skip` di `samples` (negativo = silenzio prima).
fn align(samples: &[f32], skip: i64, frames: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; frames * CHANNELS];
    let src_frames = samples.len() / CHANNELS;
    for (i, dst) in out.chunks_exact_mut(CHANNELS).enumerate() {
        let Ok(src) = usize::try_from(i as i64 + skip) else { continue };
        if src >= src_frames {
            break;
        }
        dst.copy_from_slice(&samples[src * CHANNELS..(src + 1) * CHANNELS]);
    }
    out
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
    let tracks: Vec<Track<'_>> = audio.tracks.iter().map(|t| Track { name: &t.name, samples: &t.samples }).collect();

    let saved = if format == AudioClipFormat::Mka || tracks.len() == 1 {
        let part = with_suffix(path, &format!("{ext}.part"));
        if let Err(e) = ffi::write(&part, ffmt, None, &tracks, codec, false) {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        finish(&part, path, ext)?
    } else {
        let part = with_suffix(path, ".part");
        std::fs::create_dir_all(&part).map_err(io("creazione della cartella", &part))?;
        for (i, t) in tracks.iter().enumerate() {
            let file = part.join(format!("{:02} - {}{ext}", i + 1, file_name(t.name)));
            if let Err(e) = ffi::write(&file, ffmt, None, std::slice::from_ref(t), codec, false) {
                let _ = std::fs::remove_dir_all(&part);
                return Err(e);
            }
        }
        finish(&part, path, "")?
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
        assert_eq!(align(&s, 2, 3), vec![2.0, -2.0, 3.0, -3.0, 4.0, -4.0]);
        // Audio iniziato 2 frame dopo: silenzio davanti, silenzio in coda oltre i dati.
        let a = align(&s, -2, 13);
        assert_eq!(&a[..6], &[0.0, 0.0, 0.0, 0.0, 0.0, -0.0]);
        assert_eq!(a[2 * 11], 9.0);
        assert_eq!(a[2 * 12], 0.0);
    }

    #[test]
    fn safe_file_names() {
        assert_eq!(file_name("Mic: 1/2?"), "Mic_ 1_2_");
        assert_eq!(file_name("  ..  "), "Traccia");
        assert_eq!(with_suffix(Path::new("a/Half-Life 2.5"), ".mp4"), PathBuf::from("a/Half-Life 2.5.mp4"));
    }
}
