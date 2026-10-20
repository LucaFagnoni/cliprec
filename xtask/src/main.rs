//! `cargo xtask <comando>`: automazione del progetto.
//!
//! - `ffmpeg`: scarica in `third_party/ffmpeg` la build FFmpeg 9 LGPL condivisa di BtbN
//!   (header, librerie di importazione, DLL). Usa `curl.exe` e `tar.exe` di Windows.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const FFMPEG_URL: &str =
    "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("ffmpeg") => ffmpeg(&root()?),
        _ => {
            eprintln!("uso: cargo xtask ffmpeg");
            Ok(())
        }
    }
}

fn root() -> Result<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.parent().map(Path::to_path_buf).context("xtask senza cartella padre")
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("avvio di {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} è terminato con {status}");
    }
    Ok(())
}

fn ffmpeg(root: &Path) -> Result<()> {
    let third = root.join("third_party");
    let dest = third.join("ffmpeg");
    if dest.join("lib").join("avcodec.lib").exists() {
        println!("FFmpeg già presente in {}", dest.display());
        return Ok(());
    }
    let tmp = third.join("ffmpeg-download");
    std::fs::create_dir_all(&tmp)?;
    let zip = tmp.join("ffmpeg.zip");
    run(Command::new("curl.exe").args(["-sSfL", "-o"]).arg(&zip).arg(FFMPEG_URL))?;
    let system_tar = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
        .join("System32")
        .join("tar.exe");
    run(Command::new(system_tar).arg("-xf").arg(&zip).arg("-C").arg(&tmp))?;
    let extracted = std::fs::read_dir(&tmp)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_dir())
        .context("archivio FFmpeg senza cartella")?;
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::rename(&extracted, &dest)?;
    std::fs::remove_dir_all(&tmp)?;
    println!("FFmpeg pronto in {}", dest.display());
    Ok(())
}
