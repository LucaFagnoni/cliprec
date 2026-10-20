//! Validazione delle clip con ffprobe: durata, tracce audio, nomi, disposition, fps costanti.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::ffmpeg;

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub r_frame_rate: f64,
    pub avg_frame_rate: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioInfo {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    /// `title`, altrimenti `handler_name`.
    pub name: Option<String>,
    pub default: bool,
    /// Campioni totali se `duration_ts` è in unità di campione (FLAC/WAV).
    pub samples: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClipInfo {
    pub format: String,
    /// Durata del contenitore in secondi.
    pub duration: f64,
    pub video: Option<VideoInfo>,
    pub audio: Vec<AudioInfo>,
}

/// Aspettative di `check`. `None` = non controllare.
#[derive(Debug, Clone)]
pub struct Expect {
    pub min_duration: Option<f64>,
    /// Eccesso massimo oltre `min_duration` (default 1 GOP = 1 s).
    pub max_extra: f64,
    pub audio_tracks: Option<usize>,
    pub track_names: Option<Vec<String>>,
    pub fps: Option<f64>,
}

impl Default for Expect {
    fn default() -> Self {
        Self { min_duration: None, max_extra: 1.0, audio_tracks: None, track_names: None, fps: None }
    }
}

#[derive(Deserialize)]
struct RawProbe {
    #[serde(default)]
    streams: Vec<RawStream>,
    format: RawFormat,
}

#[derive(Deserialize)]
struct RawFormat {
    format_name: Option<String>,
    duration: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u32>,
    time_base: Option<String>,
    duration_ts: Option<i64>,
    #[serde(default)]
    disposition: RawDisposition,
    #[serde(default)]
    tags: RawTags,
}

#[derive(Deserialize, Default)]
struct RawDisposition {
    #[serde(default)]
    default: u8,
}

#[derive(Deserialize, Default)]
struct RawTags {
    // ffprobe emette i nomi dei tag con la capitalizzazione del contenitore.
    #[serde(alias = "TITLE")]
    title: Option<String>,
    #[serde(alias = "HANDLER_NAME")]
    handler_name: Option<String>,
}

fn parse_rate(s: &str) -> f64 {
    let (n, d) = s.split_once('/').unwrap_or((s, "1"));
    let (n, d) = (n.parse::<f64>().unwrap_or(0.0), d.parse::<f64>().unwrap_or(0.0));
    if d == 0.0 { 0.0 } else { n / d }
}

/// Esegue ffprobe e riassume file video, audio o solo audio.
pub fn probe(path: &Path) -> Result<ClipInfo> {
    let json = ffmpeg::output(
        ffmpeg::ffprobe().args(["-show_streams", "-show_format", "-of", "json"]).arg(path),
    )?;
    let raw: RawProbe = serde_json::from_slice(&json).context("JSON di ffprobe")?;
    let mut video = None;
    let mut audio = Vec::new();
    for s in raw.streams {
        match s.codec_type.as_deref() {
            Some("video") if video.is_none() => {
                video = Some(VideoInfo {
                    width: s.width.unwrap_or(0),
                    height: s.height.unwrap_or(0),
                    r_frame_rate: parse_rate(s.r_frame_rate.as_deref().unwrap_or("0/1")),
                    avg_frame_rate: parse_rate(s.avg_frame_rate.as_deref().unwrap_or("0/1")),
                });
            }
            Some("audio") => {
                let sample_rate = s.sample_rate.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0);
                let in_samples = s.time_base.as_deref() == Some(format!("1/{sample_rate}").as_str());
                audio.push(AudioInfo {
                    codec: s.codec_name.unwrap_or_default(),
                    sample_rate,
                    channels: s.channels.unwrap_or(0),
                    name: s.tags.title.or(s.tags.handler_name),
                    default: s.disposition.default == 1,
                    samples: s.duration_ts.filter(|_| in_samples).and_then(|v| u64::try_from(v).ok()),
                });
            }
            _ => {}
        }
    }
    Ok(ClipInfo {
        format: raw.format.format_name.unwrap_or_default(),
        duration: raw.format.duration.and_then(|d| d.parse().ok()).unwrap_or(0.0),
        video,
        audio,
    })
}

/// Tempi di presentazione (s) dei pacchetti del primo stream video, ordinati.
pub fn video_pts(path: &Path) -> Result<Vec<f64>> {
    #[derive(Deserialize)]
    struct P {
        pts_time: Option<String>,
    }
    #[derive(Deserialize)]
    struct R {
        #[serde(default)]
        packets: Vec<P>,
    }
    let json = ffmpeg::output(
        ffmpeg::ffprobe()
            .args(["-select_streams", "v:0", "-show_entries", "packet=pts_time", "-of", "json"])
            .arg(path),
    )?;
    let r: R = serde_json::from_slice(&json).context("JSON dei pacchetti")?;
    let mut v: Vec<f64> = r.packets.iter().filter_map(|p| p.pts_time.as_deref()?.parse().ok()).collect();
    v.sort_by(f64::total_cmp);
    Ok(v)
}

/// Controlla una clip; restituisce l'elenco delle violazioni (vuoto = ok).
pub fn check(path: &Path, expect: &Expect) -> Result<Vec<String>> {
    let info = probe(path)?;
    let mut v = Vec::new();
    if let Some(min) = expect.min_duration {
        if info.duration < min - 0.001 {
            v.push(format!("durata {:.3} s inferiore al minimo {min} s", info.duration));
        }
        if info.duration > min + expect.max_extra + 0.001 {
            v.push(format!(
                "durata {:.3} s oltre {min} s + {} s (un GOP)",
                info.duration, expect.max_extra
            ));
        }
    }
    if let Some(n) = expect.audio_tracks
        && info.audio.len() != n {
            v.push(format!("{} tracce audio, attese {n}", info.audio.len()));
        }
    if let Some(names) = &expect.track_names {
        let got: Vec<&str> = info.audio.iter().map(|a| a.name.as_deref().unwrap_or("")).collect();
        if got != names.iter().map(String::as_str).collect::<Vec<_>>() {
            v.push(format!("nomi tracce {got:?}, attesi {names:?}"));
        }
    }
    // Un FLAC/WAV con una sola traccia non porta la disposition: la controlliamo solo dove conta.
    let check_default = info.video.is_some() || info.audio.len() > 1;
    for (i, a) in info.audio.iter().enumerate() {
        if check_default && a.default != (i == 0) {
            v.push(format!("traccia audio {i}: disposition default = {}, atteso {}", a.default, i == 0));
        }
    }
    if let Some(vi) = &info.video {
        if (vi.r_frame_rate - vi.avg_frame_rate).abs() > 0.01 {
            v.push(format!("fps non costanti: r_frame_rate {} != avg_frame_rate {}", vi.r_frame_rate, vi.avg_frame_rate));
        }
        if let Some(fps) = expect.fps
            && (vi.avg_frame_rate - fps).abs() > 0.01 {
                v.push(format!("fps {} invece di {fps}", vi.avg_frame_rate));
            }
        let pts = video_pts(path)?;
        let gaps: Vec<f64> = pts.windows(2).map(|w| w[1] - w[0]).collect();
        if let (Some(min), Some(max)) = (
            gaps.iter().copied().min_by(f64::total_cmp),
            gaps.iter().copied().max_by(f64::total_cmp),
        ) {
            // 2 ms: tollera l'arrotondamento a 1 ms dei contenitori con base tempi 1/1000.
            if max - min > 0.002 {
                v.push(format!("intervalli tra pacchetti non costanti: da {:.2} a {:.2} ms", min * 1e3, max * 1e3));
            }
        }
    } else if expect.fps.is_some() {
        v.push("fps richiesti ma nessun video".into());
    }
    Ok(v)
}

/// Clip audio in cartella (un file per traccia): stessa durata e stesso numero di campioni.
pub fn check_audio_dir(dir: &Path, expect: &Expect) -> Result<Vec<String>> {
    let mut files: Vec<_> =
        std::fs::read_dir(dir)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file()).collect();
    files.sort();
    let mut v = Vec::new();
    if let Some(n) = expect.audio_tracks
        && files.len() != n {
            v.push(format!("{} file, attesi {n}", files.len()));
        }
    let mut first: Option<(f64, Option<u64>)> = None;
    for f in &files {
        let info = probe(f)?;
        let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(min) = expect.min_duration
            && (info.duration < min - 0.001 || info.duration > min + expect.max_extra + 0.001) {
                v.push(format!("{name}: durata {:.3} s fuori da {min} s (+{} s)", info.duration, expect.max_extra));
            }
        let samples = info.audio.first().and_then(|a| a.samples);
        match first {
            None => first = Some((info.duration, samples)),
            Some((d, s)) => {
                if (info.duration - d).abs() > 0.0005 {
                    v.push(format!("{name}: durata {:.4} s diversa da {d:.4} s", info.duration));
                }
                if samples != s {
                    v.push(format!("{name}: {samples:?} campioni diversi da {s:?}"));
                }
            }
        }
    }
    Ok(v)
}
