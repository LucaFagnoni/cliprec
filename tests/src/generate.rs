//! Clip di riferimento che rispetta le convenzioni delle sorgenti sintetiche di ClipRec:
//! lampo bianco a ogni secondo intero, bip di 50 ms che inizia a ogni secondo intero.

use std::path::Path;

use anyhow::Result;

use crate::ffmpeg;

#[derive(Debug, Clone)]
pub struct GenOptions {
    pub duration: u32,
    /// Ritardo dei bip rispetto ai lampi (positivo = audio in ritardo).
    pub av_offset_ms: f64,
    /// Un nome per traccia; frequenza del bip = 1000 Hz + 500 Hz per ogni traccia successiva.
    pub track_names: Vec<String>,
    /// Ritardo aggiuntivo per traccia (oltre a `av_offset_ms`), per provare lo scarto tra tracce.
    pub track_offsets_ms: Vec<f64>,
}

impl Default for GenOptions {
    fn default() -> Self {
        Self {
            duration: 5,
            av_offset_ms: 0.0,
            track_names: vec!["Mix".into(), "Discord".into()],
            track_offsets_ms: Vec::new(),
        }
    }
}

/// Genera un MP4 1280x720 60 fps (GOP di 1 s) con `libopenh264` + AAC.
pub fn gen_test_clip(out: &Path, o: &GenOptions) -> Result<()> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let d = o.duration;
    let mut cmd = ffmpeg::ffmpeg();
    cmd.arg("-y").args(["-f", "lavfi", "-i"]).arg(format!(
        "color=c=0x101010:s=1280x720:r=60:d={d},drawbox=x=0:y=0:w=iw:h=ih:color=white:t=fill:enable='lt(mod(t+0.001,1),0.01)'"
    ));
    for i in 0..o.track_names.len() {
        let off = (o.av_offset_ms + o.track_offsets_ms.get(i).copied().unwrap_or(0.0)) / 1000.0;
        let f = 1000 + 500 * i;
        cmd.args(["-f", "lavfi", "-i"]).arg(format!(
            "aevalsrc='0.5*sin(2*PI*{f}*mod(t-({off:.6}),1))*lt(mod(t-({off:.6}),1),0.05)*gte(t,{off:.6})':s=48000:d={d}"
        ));
    }
    cmd.args(["-map", "0:v"]);
    for i in 0..o.track_names.len() {
        cmd.arg("-map").arg(format!("{}:a", i + 1));
    }
    cmd.args(["-c:v", "libopenh264", "-b:v", "6M", "-g", "60", "-pix_fmt", "yuv420p", "-r", "60"]);
    cmd.args(["-c:a", "aac", "-b:a", "128k", "-t"]).arg(d.to_string());
    for (i, n) in o.track_names.iter().enumerate() {
        cmd.arg(format!("-metadata:s:a:{i}")).arg(format!("title={n}"));
        cmd.arg(format!("-metadata:s:a:{i}")).arg(format!("handler_name={n}"));
        cmd.arg(format!("-disposition:a:{i}")).arg(if i == 0 { "default" } else { "0" });
    }
    cmd.arg(out);
    ffmpeg::run(&mut cmd)
}
