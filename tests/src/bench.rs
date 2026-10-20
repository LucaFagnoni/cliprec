//! Statistiche da CSV di PresentMon 2.x (colonna `MsBetweenPresents`, in alternativa `FrameTime`).

use std::path::Path;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub struct BenchStats {
    pub frames: usize,
    /// 1000 / media dei tempi di frame.
    pub avg_fps: f64,
    /// fps medi del 1% di frame più lenti.
    pub low1_fps: f64,
    pub p99_ms: f64,
}

/// Calcola le statistiche da una serie di tempi di frame in ms.
pub fn stats(mut ms: Vec<f64>) -> Result<BenchStats> {
    ms.retain(|x| x.is_finite() && *x > 0.0);
    if ms.is_empty() {
        bail!("nessun frame valido");
    }
    ms.sort_by(f64::total_cmp);
    let n = ms.len();
    let worst = (n / 100).max(1);
    let slow = &ms[n - worst..];
    Ok(BenchStats {
        frames: n,
        avg_fps: 1000.0 / (ms.iter().sum::<f64>() / n as f64),
        low1_fps: 1000.0 / (slow.iter().sum::<f64>() / worst as f64),
        p99_ms: ms[(n * 99 / 100).min(n - 1)],
    })
}

pub fn stats_from_csv(path: &Path) -> Result<BenchStats> {
    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
    let mut lines = text.lines();
    let header = lines.next().context("CSV vuoto")?;
    let col = header
        .trim_start_matches('\u{feff}')
        .split(',')
        .position(|c| c.trim() == "MsBetweenPresents" || c.trim() == "FrameTime")
        .context("colonna MsBetweenPresents/FrameTime assente")?;
    let ms = lines.filter_map(|l| l.split(',').nth(col)?.trim().parse::<f64>().ok()).collect();
    stats(ms)
}

/// Report testuale: una riga per CSV; con due CSV anche le differenze (il secondo rispetto al primo).
pub fn report(paths: &[&Path]) -> Result<String> {
    let mut out = String::new();
    let mut all = Vec::new();
    for p in paths {
        let s = stats_from_csv(p)?;
        out += &format!(
            "{}: {} frame, {:.1} fps medi, 1% low {:.1} fps, p99 {:.2} ms\n",
            p.display(), s.frames, s.avg_fps, s.low1_fps, s.p99_ms
        );
        all.push(s);
    }
    if let [a, b] = all.as_slice() {
        out += &format!(
            "differenza: fps medi {:+.2}%, 1% low {:+.2}% (obiettivo SPEC: perdita sotto il 2%)\n",
            (b.avg_fps / a.avg_fps - 1.0) * 100.0,
            (b.low1_fps / a.low1_fps - 1.0) * 100.0
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_on_synthetic_csv() {
        let dir = std::env::temp_dir().join("cliprec-bench-test");
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str, slow: &str| {
            let mut s = String::from("Application,ProcessID,MsBetweenPresents,Other\n");
            for i in 0..1000 {
                let ms = if i % 100 == 0 { slow } else { "10" };
                s += &format!("game.exe,1,{ms},x\n");
            }
            let p = dir.join(name);
            std::fs::write(&p, s).unwrap();
            p
        };
        let (a, b) = (mk("a.csv", "20"), mk("b.csv", "40"));
        let s = stats_from_csv(&a).unwrap();
        assert!((s.avg_fps - 1000.0 / 10.1).abs() < 0.01, "{s:?}");
        assert!((s.low1_fps - 50.0).abs() < 0.01, "{s:?}");
        let r = report(&[&a, &b]).unwrap();
        assert!(r.contains("differenza"), "{r}");
    }
}
