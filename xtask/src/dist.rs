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

const VC_REDIST_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";

/// `cargo xtask redist`: scarica il redistributable VC++ da Microsoft in `third_party/vc_redist/`.
pub fn redist(root: &Path) -> Result<()> {
    let dir = root.join("third_party").join("vc_redist");
    std::fs::create_dir_all(&dir)?;
    let exe = dir.join("vc_redist.x64.exe");
    run(Command::new("curl.exe").args(["-sSfL", "-o"]).arg(&exe).arg(VC_REDIST_URL))?;
    let size = std::fs::metadata(&exe)?.len();
    if size < 1_000_000 {
        bail!("{}: file troppo piccolo ({size} byte), scaricamento non valido", exe.display());
    }
    println!("{} ({:.1} MB)", exe.display(), size as f64 / 1048576.0);
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

    /// Regressione M1: l'installer include il redistributable e lo lancia solo se manca.
    #[test]
    fn installer_ships_vc_redist_with_check() {
        let iss = include_str!("../../installer/cliprec.iss");
        let line = |needle: &str| iss.lines().find(|l| l.contains(needle) && !l.trim_start().starts_with(';')).unwrap_or("");
        let files = line("Source: \"..\\third_party\\vc_redist\\vc_redist.x64.exe\"");
        assert!(files.contains("deleteafterinstall") && files.contains("Check: VCRedistNeeded"), "[Files]: {files:?}");
        let run = line("Filename: \"{tmp}\\vc_redist.x64.exe\"");
        assert!(run.contains("/install /quiet /norestart") && run.contains("Check: VCRedistNeeded"), "[Run]: {run:?}");
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
