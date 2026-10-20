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
    copy_licenses(root, &out)?;
    bundle_runtime(&out)?;
    let missing = missing_imports(&out)?;
    if !missing.is_empty() {
        bail!("DLL importate ma assenti dalla cartella: {missing:?}");
    }

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
    Ok(build_info_text(&String::from_utf8_lossy(&o.stdout)))
}

fn build_info_text(text: &str) -> String {
    let version = text.lines().next().unwrap_or("");
    // "ffmpeg version n9.0.2-28-g669f5ab083-20261019 Copyright ..."
    let full = version.split_whitespace().nth(2).unwrap_or("");
    let tag = full.split('-').next().unwrap_or("");
    let commit = full.split("-g").nth(1).and_then(|s| s.split('-').next()).unwrap_or("");
    let release = crate::FFMPEG_URL.split("/download/").nth(1).and_then(|s| s.split('/').next()).unwrap_or("");
    format!(
        "Libreria FFmpeg inclusa in ClipRec: LGPL 3 (FFmpeg-LICENSE.txt, licenses/LGPL-3.0.txt, licenses/GPL-3.0.txt).\n\n\
         Versione esatta: {full}\n\
         Commit FFmpeg: {commit}\n\
         Sorgente di FFmpeg: https://github.com/FFmpeg/FFmpeg/commit/{commit} (base: https://github.com/FFmpeg/FFmpeg/tree/{tag})\n\
         Binari: BtbN FFmpeg-Builds, release https://github.com/BtbN/FFmpeg-Builds/releases/tag/{release}\n\
         Archivio: {url}\n\
         SHA-256: {sha}\n\
         Script di build (sorgenti delle librerie incluse e opzioni): https://github.com/BtbN/FFmpeg-Builds\n\n\
         {info}\n",
        url = crate::FFMPEG_URL,
        sha = crate::FFMPEG_SHA256,
        info = text.lines().take(2).chain(text.lines().filter(|l| l.starts_with("configuration:"))).collect::<Vec<_>>().join("\n")
    )
}

/// Copia in `licenses/` i testi generici di `installer/licenses` e, per ogni crate Rust, i file di
/// licenza del suo sorgente in `licenses/crates/<nome>-<versione>/`.
fn copy_licenses(root: &Path, out: &Path) -> Result<()> {
    let dest = out.join("licenses");
    std::fs::create_dir_all(&dest)?;
    for e in std::fs::read_dir(root.join("installer").join("licenses"))? {
        let p = e?.path();
        if let Some(name) = p.file_name() {
            std::fs::copy(&p, dest.join(name))?;
        }
    }
    let meta = crate::notices::load_meta(root)?;
    let missing = crate::notices::uncovered(&meta, root)?;
    if !missing.is_empty() {
        bail!("crate senza testo di licenza: {missing:?}");
    }
    for c in crate::notices::crates(&meta)? {
        let files = crate::notices::license_files(&c.dir);
        if files.is_empty() {
            continue; // coperto da un testo generico (controllato da `uncovered`)
        }
        let d = dest.join("crates").join(format!("{}-{}", c.name, c.version));
        std::fs::create_dir_all(&d)?;
        for f in files {
            if let Some(name) = f.file_name() {
                std::fs::copy(&f, d.join(name))?;
            }
        }
    }
    Ok(())
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

/// SHA-256 di un file (`certutil -hashfile`, già in Windows) confrontato con quello atteso.
pub fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let o = Command::new("certutil.exe").arg("-hashfile").arg(path).arg("SHA256").output().context("certutil")?;
    let text = String::from_utf8_lossy(&o.stdout);
    let got = text.lines().nth(1).map(|l| l.replace(' ', "").to_ascii_lowercase()).unwrap_or_default();
    if !o.status.success() || got != expected.to_ascii_lowercase() {
        bail!("{}: SHA-256 {got:?}, atteso {expected:?}", path.display());
    }
    Ok(())
}

/// Cartella `Microsoft.VC143.CRT` più recente dei Build Tools / di Visual Studio 2022.
fn crt_dir() -> Result<PathBuf> {
    let pf = std::env::var_os("ProgramFiles(x86)").map(PathBuf::from).unwrap_or_else(|| r"C:\Program Files (x86)".into());
    let vs = pf.join("Microsoft Visual Studio").join("2022");
    let mut found = Vec::new();
    for edition in std::fs::read_dir(&vs).with_context(|| format!("{} non trovata", vs.display()))? {
        let msvc = edition?.path().join("VC").join("Redist").join("MSVC");
        for ver in std::fs::read_dir(&msvc).into_iter().flatten().flatten() {
            let crt = ver.path().join("x64").join("Microsoft.VC143.CRT");
            if crt.is_dir() {
                found.push(crt);
            }
        }
    }
    found.sort();
    found.pop().context("Microsoft.VC143.CRT non trovata nei Build Tools di Visual Studio 2022")
}

/// Copia accanto agli exe le DLL del runtime VC++ importate (distribuzione app-local, consentita
/// da Microsoft), ripetendo finché le copiate non ne importano altre.
fn bundle_runtime(out: &Path) -> Result<()> {
    let crt = crt_dir()?;
    loop {
        let wanted: Vec<String> = missing_imports(out)?.into_iter().filter(|n| crt.join(n).exists()).collect();
        if wanted.is_empty() {
            return Ok(());
        }
        for n in wanted {
            std::fs::copy(crt.join(&n), out.join(&n)).with_context(|| format!("copia di {n}"))?;
        }
    }
}

/// Nomi delle DLL importate da un eseguibile PE (tabella degli import).
pub fn imports(path: &Path) -> Result<Vec<String>> {
    let b = std::fs::read(path).with_context(|| format!("lettura di {}", path.display()))?;
    let rd = |o: usize, n: usize| -> Result<usize> {
        let s = b.get(o..o + n).with_context(|| format!("{}: PE troncato", path.display()))?;
        Ok(s.iter().rev().fold(0usize, |a, &x| (a << 8) | usize::from(x)))
    };
    if rd(0, 2)? != 0x5a4d {
        bail!("{}: non è un PE", path.display());
    }
    let pe = rd(0x3c, 4)?;
    if rd(pe, 4)? != 0x0000_4550 {
        bail!("{}: firma PE assente", path.display());
    }
    let (nsec, opt_len, opt) = (rd(pe + 6, 2)?, rd(pe + 20, 2)?, pe + 24);
    let dirs = opt + if rd(opt, 2)? == 0x20b { 112 } else { 96 };
    let imp_rva = rd(dirs + 8, 4)?;
    if imp_rva == 0 {
        return Ok(Vec::new());
    }
    let sections = opt + opt_len;
    let to_off = |rva: usize| -> Result<usize> {
        for i in 0..nsec {
            let s = sections + i * 40;
            let (va, raw, ptr) = (rd(s + 12, 4)?, rd(s + 16, 4)?, rd(s + 20, 4)?);
            if rva >= va && rva < va + raw {
                return Ok(rva - va + ptr);
            }
        }
        bail!("{}: RVA {rva:#x} fuori dalle sezioni", path.display())
    };
    let mut names = Vec::new();
    let mut d = to_off(imp_rva)?;
    while rd(d + 12, 4)? != 0 {
        let start = to_off(rd(d + 12, 4)?)?;
        let len = b.get(start..).and_then(|s| s.iter().position(|&c| c == 0)).context("nome di DLL senza terminatore")?;
        names.push(String::from_utf8_lossy(&b[start..start + len]).into_owned());
        d += 20;
    }
    Ok(names)
}

/// DLL di Windows (o in System32)? Il runtime VC++ non lo è mai: può esserci in System32,
/// ma su un PC pulito no.
fn is_system(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if n.starts_with("api-ms-") || n.starts_with("ext-ms-") {
        return true;
    }
    if ["vcruntime", "msvcp", "concrt", "vccorlib", "vcomp"].iter().any(|p| n.starts_with(p)) {
        return false;
    }
    let sys = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join("System32");
    sys.join(&n).exists()
}

/// DLL importate da exe e DLL della cartella che non sono di sistema e non stanno nella cartella.
pub fn missing_imports(dir: &Path) -> Result<Vec<String>> {
    let mut missing = std::collections::BTreeSet::new();
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if !matches!(p.extension().and_then(|x| x.to_str()), Some("exe" | "dll")) {
            continue;
        }
        for name in imports(&p)? {
            if !is_system(&name) && !dir.join(&name).exists() {
                missing.insert(name);
            }
        }
    }
    Ok(missing.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regressione M1: l'installer scarica il redistributable e lo lancia solo se manca.
    const SAMPLE: &str = include_str!("../../installer/ffmpeg-version.txt");

    #[test]
    fn sha256_is_verified() {
        let f = std::env::temp_dir().join("cliprec-xtask-sha.txt");
        std::fs::write(&f, "abc").unwrap();
        verify_sha256(&f, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").unwrap();
        assert!(verify_sha256(&f, &"0".repeat(64)).is_err());
        std::fs::remove_file(&f).unwrap();
    }

    #[test]
    fn ffmpeg_download_is_pinned() {
        assert!(!crate::FFMPEG_URL.contains("latest"), "URL non fissato: {}", crate::FFMPEG_URL);
        assert!(crate::FFMPEG_SHA256.len() == 64 && crate::FFMPEG_SHA256.bytes().all(|b| b.is_ascii_hexdigit()), "SHA-256 mancante");
    }

    #[test]
    fn build_info_names_exact_build() {
        let t = build_info_text(SAMPLE);
        for needle in [
            "n9.0.2-28-g669f5ab083-20261019",
            "https://github.com/FFmpeg/FFmpeg/commit/669f5ab083",
            "https://github.com/BtbN/FFmpeg-Builds/releases/tag/autobuild-2026-10-19-13-06",
            "--enable-version3",
        ] {
            assert!(t.contains(needle), "FFmpeg-BUILD.txt senza {needle:?}:
{t}");
        }
    }

    #[test]
    fn dist_folder_has_gpl_text() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/dist/ClipRec");
        if dir.exists() {
            assert!(dir.join("licenses").join("GPL-3.0.txt").exists(), "licenses/GPL-3.0.txt assente in {}", dir.display());
        }
    }

    #[test]
    fn installer_downloads_vc_redist_only_if_missing() {
        let iss = include_str!("../../installer/cliprec.iss");
        let line = |needle: &str| iss.lines().find(|l| l.contains(needle) && !l.trim_start().starts_with(';')).unwrap_or("");
        // Il runtime non è nel pacchetto: lo si scarica da Microsoft solo se il registro dice che manca.
        assert!(line("vc_redist.x64.exe\"; DestDir").is_empty(), "il redistributable non va incluso nel pacchetto");
        assert!(iss.contains("https://aka.ms/vs/17/release/vc_redist.x64.exe") && iss.contains("CreateDownloadPage"), "download assente");
        assert!(iss.contains("if (CurPageID <> wpReady) or (not VCRedistNeeded) then"), "il download deve dipendere dal controllo");
        let run = line("Filename: \"{tmp}\\vc_redist.x64.exe\"");
        assert!(run.contains("/install /quiet /norestart") && run.contains("Check: VCRedistDownloaded"), "[Run]: {run:?}");
        assert!(iss.contains("Runtimes\\x64") && iss.contains("WOW6432Node"), "controllo del registro assente");
    }

    /// Regressione M1: la cartella di distribuzione deve portare con sé ogni DLL non di sistema
    /// importata (prima mancava VCRUNTIME140.dll). Si salta se `cargo xtask dist` non è mai stato eseguito.
    #[test]
    fn dist_folder_is_self_contained() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/dist/ClipRec");
        if !dir.exists() {
            eprintln!("nessuna cartella dist: test saltato");
            return;
        }
        assert_eq!(missing_imports(&dir).unwrap(), Vec::<String>::new());
    }
}
