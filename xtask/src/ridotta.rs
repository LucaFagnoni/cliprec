//! `cargo xtask ffmpeg-ridotta`: compila in `FFMPEG_DIR` la build FFmpeg LGPL ridotta che ClipRec
//! linka e distribuisce (solo encoder, muxer e hwcontext usati; vedi installer/FFMPEG-RIDOTTA.md).
//!
//! Toolchain: MSYS2 UCRT64 in `third_party/toolchain/msys64`, scaricata da repo.msys2.org e con i
//! pacchetti fissati per versione (pacman ne verifica la firma). Sorgente: commit fissato di FFmpeg.
//! La compilazione vera è in `scripts/ffmpeg-ridotta.sh`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::run;

pub const MSYS2_URL: &str = "https://repo.msys2.org/distrib/x86_64/msys2-base-x86_64-20260927.tar.xz";
pub const MSYS2_SHA256: &str = "ea2f31a0b6ade63914ce441ffb022f0f6aa96982bfefa2326460a26d5fb01322";

/// Stesso commit della build BtbN usata prima (n9.0.2-28-g669f5ab083, ramo release/9.0).
pub const FFMPEG_COMMIT: &str = "669f5ab083c7732c10f1ce9e4f1915960dd88457";
pub const FFMPEG_SRC_URL: &str =
    "https://github.com/FFmpeg/FFmpeg/archive/669f5ab083c7732c10f1ce9e4f1915960dd88457.tar.gz";
pub const FFMPEG_SRC_SHA256: &str = "e5b24d5779f736b3ef8a985d52051d48fece48712f810af7037283ad707ecd67";

/// Pacchetti MSYS2 fissati: compilatore, assembler e le librerie che finiscono nelle DLL.
/// Le altre dipendenze di gcc (gmp, isl, zstd...) vengono dal repository al momento della build.
pub const PACKAGES: [&str; 18] = [
    "make-4.4.1-3",
    "diffutils-3.12-1",
    "mingw-w64-ucrt-x86_64-gcc-16.2.0-6",
    "mingw-w64-ucrt-x86_64-cc-libs-16.2.0-6",
    "mingw-w64-ucrt-x86_64-libgcc-16.2.0-6",
    "mingw-w64-ucrt-x86_64-libstdc++-16.2.0-6",
    "mingw-w64-ucrt-x86_64-binutils-2.47-4",
    "mingw-w64-ucrt-x86_64-crt-14.0.0.r495.g63ebe6cdb-1",
    "mingw-w64-ucrt-x86_64-headers-14.0.0.r495.g63ebe6cdb-1",
    "mingw-w64-ucrt-x86_64-winpthreads-14.0.0.r495.g63ebe6cdb-2",
    "mingw-w64-ucrt-x86_64-libwinpthread-14.0.0.r495.g63ebe6cdb-2",
    "mingw-w64-ucrt-x86_64-nasm-3.02-1",
    "mingw-w64-ucrt-x86_64-pkgconf-1~3.0.10-1",
    "mingw-w64-ucrt-x86_64-svt-av1-4.2.0-1",
    "mingw-w64-ucrt-x86_64-opus-1.6.1-2",
    "mingw-w64-ucrt-x86_64-libvpl-2.17.0-1",
    "mingw-w64-ucrt-x86_64-amf-headers-1.5.3-1",
    "mingw-w64-ucrt-x86_64-ffnvcodec-headers-13.0.19.0-1",
];

fn package_url(p: &str) -> String {
    if p.starts_with("mingw-w64-") {
        format!("https://repo.msys2.org/mingw/ucrt64/{p}-any.pkg.tar.zst")
    } else {
        format!("https://repo.msys2.org/msys/x86_64/{p}-x86_64.pkg.tar.zst")
    }
}

pub fn run_build(root: &Path) -> Result<()> {
    let dest = std::env::var_os("FFMPEG_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("third_party").join("ffmpeg-ridotta"));
    let tool = dest.parent().context("FFMPEG_DIR senza cartella padre")?.join("toolchain");
    let bash = tool.join("msys64").join("usr").join("bin").join("bash.exe");
    std::fs::create_dir_all(&tool)?;

    if !bash.exists() {
        let archive = download(&tool, MSYS2_URL, MSYS2_SHA256)?;
        extract(&archive, &tool)?;
    }
    // Primo avvio (chiavi di pacman), aggiornamento del sistema base, poi i pacchetti fissati.
    let msys = |script: &str| -> Result<()> {
        run(Command::new(&bash).args(["-lc", script]).env("MSYSTEM", "UCRT64").env("CHERE_INVOKING", "1").current_dir(&tool))
    };
    msys("pacman -Syuu --noconfirm")?;
    msys("pacman -Syuu --noconfirm")?;
    let urls: Vec<String> = PACKAGES.iter().map(|p| package_url(p)).collect();
    msys(&format!("pacman -U --needed --noconfirm {}", urls.join(" ")))?;

    let src_archive = download(&tool, FFMPEG_SRC_URL, FFMPEG_SRC_SHA256)?;
    let build = tool.join("build");
    if build.exists() {
        std::fs::remove_dir_all(&build)?;
    }
    std::fs::create_dir_all(&build)?;
    extract(&src_archive, &build)?;
    let src = build.join(format!("FFmpeg-{FFMPEG_COMMIT}"));
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    let script = root.join("scripts").join("ffmpeg-ridotta.sh");
    let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
    msys(&format!("bash '{}' '{}' '{}'", posix(&script), posix(&src), posix(&dest)))?;
    println!("FFmpeg ridotta pronta in {}", dest.display());
    Ok(())
}

/// Scarica `url` in `dir` (se non c'è già) e ne verifica lo SHA-256.
fn download(dir: &Path, url: &str, sha256: &str) -> Result<PathBuf> {
    let name = url.rsplit('/').next().context("URL senza nome di file")?;
    let path = dir.join(name);
    if !path.exists() {
        run(Command::new("curl.exe").args(["-sSfL", "-o"]).arg(&path).arg(url))?;
    }
    crate::dist::verify_sha256(&path, sha256)?;
    Ok(path)
}

fn extract(archive: &Path, into: &Path) -> Result<()> {
    run(Command::new(crate::system_tar()).arg("-xf").arg(archive).arg("-C").arg(into))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolchain_and_source_are_pinned() {
        for (url, sha) in [(MSYS2_URL, MSYS2_SHA256), (FFMPEG_SRC_URL, FFMPEG_SRC_SHA256)] {
            assert!(!url.contains("latest"), "URL non fissato: {url}");
            assert!(sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()), "SHA-256 mancante per {url}");
        }
        assert!(FFMPEG_SRC_URL.contains(FFMPEG_COMMIT));
        // Ogni pacchetto ha una versione esplicita (nome-versione-rilascio).
        for p in PACKAGES {
            assert!(p.rsplit('-').next().is_some_and(|r| r.parse::<u32>().is_ok()), "versione mancante: {p}");
        }
        assert_eq!(package_url("make-4.4.1-3"), "https://repo.msys2.org/msys/x86_64/make-4.4.1-3-x86_64.pkg.tar.zst");
    }

    /// Niente GPL né nonfree: la build resta LGPL 3.
    #[test]
    fn build_script_is_lgpl() {
        let s = include_str!("../../scripts/ffmpeg-ridotta.sh");
        assert!(s.contains("--enable-version3") && s.contains("--disable-everything"));
        for bad in ["--enable-gpl", "--enable-nonfree", "libx264", "libx265", "libfdk"] {
            assert!(!s.contains(bad), "opzione non LGPL nello script: {bad}");
        }
    }
}
