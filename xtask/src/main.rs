//! `cargo xtask <comando>`: automazione del progetto.
//!
//! - `probe <file|cartella> [--duration S] [--tracks N] [--names "A,B"] [--fps F]`: validazione ffprobe.
//! - `sync <file|cartella> [--tolerance-ms 20]`: misura della sincronia (lampi e bip).
//! - `gen-test-clip <out.mp4> [--av-offset-ms X]`: clip di riferimento.
//! - `bench-report <csv...>`: fps medi e 1% low da CSV PresentMon, confronto di due run.
//! - `testwindow [--seconds N]`: finestra 640x360 con timecode QPC e lampo a ogni secondo.
//! - `mem <pid|nome.exe> [--top N]`: memoria di un processo per categoria (immagini, heap, stack, privata).
//! - `dist`: build release e cartella `target/dist/ClipRec`. `notices`: rigenera THIRD_PARTY_NOTICES.md.
//! - `ffmpeg`: scarica in `third_party/ffmpeg` la build FFmpeg 9 LGPL condivisa di BtbN
//!   (header, librerie di importazione, DLL). Usa `curl.exe` e `tar.exe` di Windows.

mod dist;
mod mem;
mod notices;
mod testwindow;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use cliprec_tests::generate::{self, GenOptions};
use cliprec_tests::probe::{self, Expect};
use cliprec_tests::{bench, sync};

/// Build fissata: release `autobuild-2026-10-19-13-06` di BtbN (FFmpeg n9.0.2-28-g669f5ab083).
/// Lo SHA-256 è quello dichiarato da GitHub per l'asset ed è identico alle DLL in uso (avutil verificata).
/// Se la release sparisce, serve una nuova release fissata e un nuovo hash (vedi docs/DECISIONS.md).
const FFMPEG_SHA256: &str = "557513674c1628d82cad40515d75bc28d26c4730582b4029b018c4a1ff77a49c";

const FFMPEG_URL: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-10-19-13-06/ffmpeg-n9.0.2-28-g669f5ab083-win64-lgpl-shared-9.0.zip";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("ffmpeg") => ffmpeg(&root()?),
        Some("redist") => dist::redist(&root()?),
        Some("dist") => dist::run_dist(&root()?),
        Some("notices") => notices::run(&root()?),
        Some("probe") => finish("probe", probe_cmd(&args[1..])?),
        Some("sync") => finish("sync", sync_cmd(&args[1..])?),
        Some("gen-test-clip") => gen_cmd(&args[1..]),
        Some("mem") => mem::run(&args[1..]),
        Some("testwindow") => testwindow::run(opt(&args, "--seconds")?),
        Some("bench-report") => {
            let paths: Vec<&Path> = args[1..].iter().map(Path::new).collect();
            print!("{}", bench::report(&paths)?);
            Ok(())
        }
        _ => {
            eprintln!("uso: cargo xtask ffmpeg | redist | dist | notices | probe | sync | mem | gen-test-clip | bench-report (vedi la testata di xtask/src/main.rs)");
            std::process::exit(2);
        }
    }
}

/// Stampa le violazioni ed esce con codice 1 se ce ne sono.
fn finish(what: &str, violations: Vec<String>) -> Result<()> {
    if violations.is_empty() {
        println!("{what}: ok");
        return Ok(());
    }
    for v in &violations {
        eprintln!("{what}: VIOLAZIONE: {v}");
    }
    std::process::exit(1);
}

/// Valore di `--nome`, se presente.
fn opt<T: std::str::FromStr>(args: &[String], name: &str) -> Result<Option<T>> {
    match args.iter().position(|a| a == name) {
        None => Ok(None),
        Some(i) => {
            let v = args.get(i + 1).with_context(|| format!("{name} senza valore"))?;
            v.parse().map(Some).map_err(|_| anyhow::anyhow!("valore non valido per {name}: {v}"))
        }
    }
}

fn probe_cmd(args: &[String]) -> Result<Vec<String>> {
    let path = Path::new(args.first().context("manca il file")?);
    let expect = Expect {
        min_duration: opt(args, "--duration")?,
        audio_tracks: opt(args, "--tracks")?,
        track_names: opt::<String>(args, "--names")?.map(|n| n.split(',').map(str::to_owned).collect()),
        fps: opt(args, "--fps")?,
        ..Expect::default()
    };
    if path.is_dir() { probe::check_audio_dir(path, &expect) } else { probe::check(path, &expect) }
}

fn sync_cmd(args: &[String]) -> Result<Vec<String>> {
    let path = Path::new(args.first().context("manca il file")?);
    let tol = opt(args, "--tolerance-ms")?.unwrap_or(20.0);
    let mut r = sync::measure_sync(path)?;
    println!("lampi: {}, bip per traccia: {:?}", r.flashes, r.bips);
    for (i, (a, t)) in r.av.iter().zip(&r.inter_track).enumerate() {
        println!(
            "traccia {i}: A/V medio {:+.2} ms, max {:.2} ms ({} abbinati); vs traccia 0 medio {:+.2} ms, max {:.2} ms",
            a.mean_ms, a.max_abs_ms, a.matched, t.mean_ms, t.max_abs_ms
        );
    }
    // Senza video non c'è scarto A/V da giudicare.
    if r.flashes == 0 {
        r.av.clear();
    }
    Ok(r.violations(tol))
}

fn gen_cmd(args: &[String]) -> Result<()> {
    let out = Path::new(args.first().context("manca il file di uscita")?);
    let o = GenOptions { av_offset_ms: opt(args, "--av-offset-ms")?.unwrap_or(0.0), ..GenOptions::default() };
    generate::gen_test_clip(out, &o)?;
    println!("clip di prova in {}", out.display());
    Ok(())
}

fn root() -> Result<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.parent().map(Path::to_path_buf).context("xtask senza cartella padre")
}

pub(crate) fn run(cmd: &mut Command) -> Result<()> {
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
    dist::verify_sha256(&zip, FFMPEG_SHA256)?;
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
