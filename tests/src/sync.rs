//! Misura della sincronia: lampi nel video, inizi dei bip in ogni traccia audio.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::{ffmpeg, probe};

const RATE: usize = 48_000;

/// Scarti di una serie di eventi rispetto a un riferimento, in millisecondi (positivo = in ritardo).
#[derive(Debug, Clone, Default)]
pub struct Skew {
    pub mean_ms: f64,
    pub max_abs_ms: f64,
    pub matched: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub flashes: usize,
    /// Bip trovati per traccia.
    pub bips: Vec<usize>,
    /// Bip di ogni traccia rispetto ai lampi.
    pub av: Vec<Skew>,
    /// Bip di ogni traccia rispetto alla traccia 0 (la 0 stessa è sempre zero).
    pub inter_track: Vec<Skew>,
}

impl SyncReport {
    /// Violazioni oltre `tol_ms` (scarto massimo assoluto) o mancanza di eventi da confrontare.
    pub fn violations(&self, tol_ms: f64) -> Vec<String> {
        let mut v = Vec::new();
        for (i, s) in self.av.iter().enumerate() {
            if s.matched == 0 {
                v.push(format!("traccia {i}: nessun bip abbinato a un lampo"));
            } else if s.max_abs_ms > tol_ms {
                v.push(format!("traccia {i}: scarto A/V massimo {:.2} ms > {tol_ms} ms", s.max_abs_ms));
            }
        }
        for (i, s) in self.inter_track.iter().enumerate().skip(1) {
            if s.matched == 0 {
                v.push(format!("traccia {i}: nessun bip abbinato alla traccia 0"));
            } else if s.max_abs_ms > tol_ms {
                v.push(format!("traccia {i}: scarto dalla traccia 0 massimo {:.2} ms > {tol_ms} ms", s.max_abs_ms));
            }
        }
        v
    }
}

/// Inizi dei lampi (s): primo frame di ogni gruppo con luma sopra la metà tra minimo e massimo.
pub fn detect_flashes(luma: &[f64], pts: &[f64]) -> Vec<f64> {
    let n = luma.len().min(pts.len());
    let (lo, hi) = luma[..n].iter().fold((f64::MAX, f64::MIN), |(l, h), &x| (l.min(x), h.max(x)));
    if n == 0 || hi - lo < 50.0 {
        return Vec::new();
    }
    let thr = (lo + hi) / 2.0;
    (0..n).filter(|&i| luma[i] > thr && (i == 0 || luma[i - 1] <= thr)).map(|i| pts[i]).collect()
}

/// Inizi dei bip (s dall'inizio dei campioni): primo campione sopra il 10% del picco,
/// con 0,3 s di rifrazione dopo ogni rilevamento.
pub fn detect_bips(samples: &[f32], rate: usize) -> Vec<f64> {
    let peak = samples.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    if peak < 0.05 {
        return Vec::new();
    }
    let thr = peak * 0.1;
    let refractory = rate * 3 / 10;
    let mut out = Vec::new();
    let mut i = 0;
    while i < samples.len() {
        if samples[i].abs() > thr {
            out.push(i as f64 / rate as f64);
            i += refractory;
        } else {
            i += 1;
        }
    }
    out
}

/// Scarti `b - a` per ogni `b` con un `a` entro 0,5 s (il più vicino).
pub fn skew(reference: &[f64], events: &[f64]) -> Skew {
    let d: Vec<f64> = events
        .iter()
        .filter_map(|&e| {
            reference.iter().copied().min_by(|x, y| (e - x).abs().total_cmp(&(e - y).abs())).map(|r| e - r)
        })
        .filter(|d| d.abs() < 0.5)
        .map(|d| d * 1e3)
        .collect();
    if d.is_empty() {
        return Skew::default();
    }
    Skew {
        mean_ms: d.iter().sum::<f64>() / d.len() as f64,
        max_abs_ms: d.iter().fold(0.0, |m, x| f64::max(m, x.abs())),
        matched: d.len(),
    }
}

fn decode_audio(path: &Path, stream: usize) -> Result<Vec<f32>> {
    let raw = ffmpeg::output(
        ffmpeg::ffmpeg()
            .arg("-i")
            .arg(path)
            .args(["-map", &format!("0:a:{stream}"), "-f", "f32le", "-ac", "1", "-ar", "48000", "-"]),
    )?;
    Ok(raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
}

fn flash_times(path: &Path) -> Result<Vec<f64>> {
    let raw = ffmpeg::output(ffmpeg::ffmpeg().arg("-i").arg(path).args([
        "-map", "0:v:0", "-vf", "scale=16:16,format=gray", "-fps_mode", "passthrough", "-f", "rawvideo", "-",
    ]))?;
    let luma: Vec<f64> =
        raw.chunks_exact(256).map(|f| f.iter().map(|&b| f64::from(b)).sum::<f64>() / 256.0).collect();
    Ok(detect_flashes(&luma, &probe::video_pts(path)?))
}

/// Misura la sincronia di una clip (video + tracce), di un file solo audio multitraccia
/// o di una cartella con un file audio per traccia.
pub fn measure_sync(path: &Path) -> Result<SyncReport> {
    let (flashes, tracks): (Vec<f64>, Vec<Vec<f64>>) = if path.is_dir() {
        let mut files: Vec<PathBuf> =
            std::fs::read_dir(path)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file()).collect();
        files.sort();
        let mut t = Vec::new();
        for f in &files {
            t.push(detect_bips(&decode_audio(f, 0)?, RATE));
        }
        (Vec::new(), t)
    } else {
        let info = probe::probe(path)?;
        let flashes = if info.video.is_some() { flash_times(path)? } else { Vec::new() };
        let mut t = Vec::new();
        for i in 0..info.audio.len() {
            t.push(detect_bips(&decode_audio(path, i)?, RATE));
        }
        (flashes, t)
    };
    if tracks.is_empty() {
        bail!("nessuna traccia audio in {}", path.display());
    }
    Ok(SyncReport {
        flashes: flashes.len(),
        bips: tracks.iter().map(Vec::len).collect(),
        av: tracks.iter().map(|b| skew(&flashes, b)).collect(),
        inter_track: tracks.iter().map(|b| skew(&tracks[0], b)).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_flashes_and_bips() {
        let luma: Vec<f64> = (0..180).map(|i| if i % 60 == 0 { 235.0 } else { 16.0 }).collect();
        let pts: Vec<f64> = (0..180).map(|i| f64::from(i) / 60.0).collect();
        assert_eq!(detect_flashes(&luma, &pts), vec![0.0, 1.0, 2.0]);
        let mut s = vec![0.0f32; RATE * 3];
        for sec in 0..3 {
            for k in 0..2400 {
                s[sec * RATE + 480 + k] = 0.5 * (k as f32 * 0.131).sin();
            }
        }
        let b = detect_bips(&s, RATE);
        assert_eq!(b.len(), 3);
        assert!((b[1] - 1.01).abs() < 0.001);
        let k = skew(&[0.0, 1.0], &[0.01, 1.03]);
        assert!((k.mean_ms - 20.0).abs() < 1e-9 && (k.max_abs_ms - 30.0).abs() < 1e-9);
    }
}
