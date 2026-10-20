//! Localizzazione ed esecuzione di ffmpeg/ffprobe (build LGPL in `third_party/ffmpeg/bin`).

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

const DEFAULT_DIR: &str = r"D:\Repositories\cliprec\third_party\ffmpeg";

/// Cartella `bin` di FFmpeg: `FFMPEG_DIR` (impostata da `.cargo/config.toml`) oppure il percorso fisso.
pub fn bin_dir() -> PathBuf {
    let base = std::env::var_os("FFMPEG_DIR").map_or_else(|| PathBuf::from(DEFAULT_DIR), PathBuf::from);
    base.join("bin")
}

pub fn ffmpeg() -> Command {
    let mut c = Command::new(bin_dir().join("ffmpeg.exe"));
    c.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    c
}

pub fn ffprobe() -> Command {
    let mut c = Command::new(bin_dir().join("ffprobe.exe"));
    c.args(["-hide_banner", "-loglevel", "error"]);
    c
}

/// Esegue il comando e restituisce stdout; errore se il codice di uscita non è 0.
pub fn output(cmd: &mut Command) -> Result<Vec<u8>> {
    let out = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("avvio di {cmd:?}"))?;
    if !out.status.success() {
        bail!("{cmd:?} è terminato con {}: {}", out.status, String::from_utf8_lossy(&out.stderr));
    }
    Ok(out.stdout)
}

/// Esegue il comando scartando l'output.
pub fn run(cmd: &mut Command) -> Result<()> {
    output(cmd).map(drop)
}
