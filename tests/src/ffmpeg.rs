//! Localizzazione ed esecuzione di ffmpeg/ffprobe (build LGPL completa in `third_party/ffmpeg/bin`,
//! solo per i test: la libreria che ClipRec linka e distribuisce è `FFMPEG_DIR`, senza programmi).

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use std::os::windows::process::CommandExt as _;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const DEFAULT_DIR: &str = r"D:\Repositories\cliprec\third_party\ffmpeg";

/// Cartella `bin` degli strumenti FFmpeg: `FFMPEG_TOOLS_DIR` (impostata da `.cargo/config.toml`) oppure il percorso fisso.
pub fn bin_dir() -> PathBuf {
    let base = std::env::var_os("FFMPEG_TOOLS_DIR").map_or_else(|| PathBuf::from(DEFAULT_DIR), PathBuf::from);
    base.join("bin")
}

pub fn ffmpeg() -> Command {
    let mut c = Command::new(bin_dir().join("ffmpeg.exe"));
    // Niente finestra di console quando il chiamante non ne ha una (test, xtask).
    c.creation_flags(CREATE_NO_WINDOW);
    c.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    c
}

pub fn ffprobe() -> Command {
    let mut c = Command::new(bin_dir().join("ffprobe.exe"));
    // Niente finestra di console quando il chiamante non ne ha una (test, xtask).
    c.creation_flags(CREATE_NO_WINDOW);
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
