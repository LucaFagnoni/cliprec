//! `cargo xtask dist`: compila in release e prepara `target/dist/ClipRec/` (niente zip, niente download).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::run;

/// Limite di SPEC.md (tabella delle prestazioni), con la build FFmpeg ridotta.
const LIMIT_BYTES: u64 = 40 * 1024 * 1024;
const EXES: [&str; 2] = ["cliprec-app.exe", "cliprec-settings.exe"];
/// DLL FFmpeg davvero importate (`dumpbin /dependents` sugli exe e sulle DLL stesse).
/// Restano fuori avfilter, avdevice e swscale (nessuno la importa) (e ffmpeg/ffplay/ffprobe).
const FFMPEG_DLLS: [&str; 4] = ["avcodec-63.dll", "avformat-63.dll", "avutil-61.dll", "swresample-7.dll"];

pub fn run_dist(root: &Path) -> Result<()> {
    run(Command::new("cargo").current_dir(root).args(["build", "--release", "-p", "cliprec-app", "-p", "cliprec-settings"]))?;
    let ffmpeg = std::env::var_os("FFMPEG_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("third_party").join("ffmpeg"));
    let out = root.join("target").join("dist").join("ClipRec");
    if out.exists() {
        std::fs::remove_dir_all(&out)?;
    }
    std::fs::create_dir_all(&out)?;
    let copy = |from: PathBuf, to: &str| -> Result<()> {
        std::fs::copy(&from, out.join(to)).with_context(|| format!("copia di {}", from.display()))?;
        Ok(())
    };
    for exe in EXES {
        copy(root.join("target").join("release").join(exe), exe)?;
    }
    for dll in FFMPEG_DLLS {
        copy(ffmpeg.join("bin").join(dll), dll)?;
    }
    copy(root.join("THIRD_PARTY_NOTICES.md"), "THIRD_PARTY_NOTICES.md")?;
    copy(ffmpeg.join("LICENSE.txt"), "FFmpeg-LICENSE.txt")?;
    std::fs::write(out.join("FFmpeg-BUILD.txt"), build_info(&ffmpeg)?)?;

    let total = dir_size(&out)?;
    let mb = total as f64 / (1024.0 * 1024.0);
    let verdict = if total <= LIMIT_BYTES { "entro" } else { "OLTRE" };
    println!("{}: {mb:.1} MB, {verdict} il limite di 40 MB (con la build FFmpeg completa; vedi installer/FFMPEG-RIDOTTA.md)", out.display());
    Ok(())
}

/// Versione e riga di configure della build FFmpeg, più il link al sorgente (LGPL 6).
fn build_info(ffmpeg: &Path) -> Result<String> {
    let o = Command::new(ffmpeg.join("bin").join("ffmpeg.exe")).arg("-version").output().context("ffmpeg -version")?;
    if !o.status.success() {
        bail!("ffmpeg -version fallito");
    }
    let text = String::from_utf8_lossy(&o.stdout);
    Ok(format!(
        "Libreria FFmpeg inclusa in ClipRec (LGPL, vedi FFmpeg-LICENSE.txt).\n\
         Build: BtbN FFmpeg-Builds, https://github.com/BtbN/FFmpeg-Builds\n\
         Sorgente: https://ffmpeg.org/download.html (tag n9.0.2) e gli script di build nel repository sopra.\n\n{}\n",
        text.lines().take(2).chain(text.lines().filter(|l| l.starts_with("configuration:"))).collect::<Vec<_>>().join("\n")
    ))
}

fn dir_size(dir: &Path) -> Result<u64> {
    let mut sum = 0;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let m = e.metadata()?;
        sum += if m.is_dir() { dir_size(&e.path())? } else { m.len() };
    }
    Ok(sum)
}
