//! Aggiornamenti senza reinstallare. ClipRec legge l'ultima release del repository su GitHub, scarica
//! lo zip portable, ne verifica lo SHA-256 (file `.sha256` della stessa release), lo estrae e
//! sostituisce i propri file. Gli eseguibili e le DLL in uso si possono rinominare ma non
//! sovrascrivere: ogni file vecchio diventa `<nome>.old`, il nuovo prende il suo posto e ClipRec si
//! riavvia; i `.old` spariscono all'avvio successivo. Usa curl, tar e certutil di Windows, senza
//! finestre, e nessun permesso di amministratore (la cartella è quella dell'utente).

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use cliprec_core::ipc::{UpdateInfo, UpdatePhase};

/// Repository delle release.
pub const REPO: &str = "LucaFagnoni/cliprec";
/// Versione in esecuzione; `CLIPREC_UPDATE_AS` la sostituisce nelle prove dell'aggiornamento
/// (una copia che si crede vecchia trova e installa l'ultima release vera).
pub fn current() -> String {
    std::env::var("CLIPREC_UPDATE_AS").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_owned())
}
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Release pubblicata con lo zip portable e il suo SHA-256.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub zip_url: String,
    pub sha_url: String,
    /// Note della release (Markdown).
    pub notes: String,
}

fn version_parts(v: &str) -> Option<[u64; 3]> {
    let core = v.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next()?;
    let mut parts = core.split('.').map(str::parse::<u64>);
    let mut next = || parts.next().unwrap_or(Ok(0)).ok();
    Some([next()?, next()?, next()?])
}

/// `remote` è una versione più recente di `current` (confronto numerico, «v» iniziale ammessa).
pub fn is_newer(remote: &str, current: &str) -> bool {
    matches!((version_parts(remote), version_parts(current)), (Some(r), Some(c)) if r > c)
}

/// Risposta di `GET /repos/{REPO}/releases/latest`: versione e URL di zip portable e `.sha256`.
/// `None` per bozze, pre-release o release senza i due file.
pub fn parse_latest(json: &str) -> Option<Release> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let assets = v["assets"].as_array()?;
    let url = |suffix: &str| {
        assets
            .iter()
            .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(suffix)))
            .and_then(|a| a["browser_download_url"].as_str())
            .map(str::to_owned)
    };
    Some(Release {
        version: v["tag_name"].as_str()?.trim_start_matches(['v', 'V']).to_owned(),
        zip_url: url("-portable.zip")?,
        sha_url: url("-portable.zip.sha256")?,
        notes: v["body"].as_str().unwrap_or_default().trim().to_owned(),
    })
}

/// Strumento di sistema (`System32`), mai una copia trovata nel PATH (es. il tar di Git).
fn system_tool(name: &str) -> Command {
    let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let mut c = Command::new(root.join("System32").join(name));
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

/// Scarica `url` (in `out`, oppure restituisce il corpo).
fn fetch(url: &str, out: Option<&Path>) -> anyhow::Result<Vec<u8>> {
    let mut c = system_tool("curl.exe");
    c.args(["-sSfL", "--max-time", "900", "-H", "User-Agent: ClipRec", "-H", "Accept: application/vnd.github+json"]).arg(url);
    if let Some(p) = out {
        c.arg("-o").arg(p);
    }
    let o = c.output().context("curl.exe")?;
    if !o.status.success() {
        bail!("{url}: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(o.stdout)
}

/// L'ultima release, se è più recente di questa versione.
pub fn check() -> anyhow::Result<Option<Release>> {
    let body = fetch(&format!("https://api.github.com/repos/{REPO}/releases/latest"), None)?;
    let release = parse_latest(&String::from_utf8_lossy(&body)).context("la release non contiene lo zip portable")?;
    Ok(is_newer(&release.version, &current()).then_some(release))
}

fn sha256(path: &Path) -> anyhow::Result<String> {
    let o = system_tool("certutil.exe").arg("-hashfile").arg(path).arg("SHA256").output().context("certutil.exe")?;
    let text = String::from_utf8_lossy(&o.stdout);
    let hash = text.lines().nth(1).map(|l| l.replace(' ', "").to_ascii_lowercase()).unwrap_or_default();
    if !o.status.success() || hash.len() != 64 {
        bail!("certutil: {}", text.trim());
    }
    Ok(hash)
}

/// Scarica, verifica ed estrae `release`, poi ne mette i file in `app_dir` (`installing` segna
/// il passaggio dal download alla sostituzione). Al ritorno con `Ok` va solo riavviato ClipRec.
pub fn install(release: &Release, app_dir: &Path, installing: impl FnOnce()) -> anyhow::Result<()> {
    let work = std::env::temp_dir().join(format!("cliprec-update-{}", release.version));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let zip = work.join("ClipRec.zip");
    fetch(&release.zip_url, Some(&zip))?;
    let expected = String::from_utf8_lossy(&fetch(&release.sha_url, None)?).split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    if expected.len() != 64 || sha256(&zip)? != expected {
        bail!("lo zip scaricato non corrisponde allo SHA-256 pubblicato");
    }
    let o = system_tool("tar.exe").arg("-x").arg("-f").arg(&zip).arg("-C").arg(&work).output().context("tar.exe")?;
    if !o.status.success() {
        bail!("estrazione dello zip: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    let staging = work.join("ClipRec");
    if !staging.join("cliprec-app.exe").exists() {
        bail!("lo zip non contiene ClipRec\\cliprec-app.exe");
    }
    installing();
    replace_files(&staging, app_dir)?;
    let _ = std::fs::remove_dir_all(&work);
    Ok(())
}

fn old_name(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".old");
    s.into()
}

/// File sotto `dir`, ricorsivamente, in ordine.
fn files_under(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let p = e?.path();
            if p.is_dir() { stack.push(p) } else { out.push(p) }
        }
    }
    out.sort();
    Ok(out)
}

fn same_content(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else { return false };
    ma.len() == mb.len() && std::fs::read(a).ok() == std::fs::read(b).ok()
}

/// Mette i file di `new_dir` in `app_dir`: un file esistente diventa `<nome>.old` (si può anche se
/// è in uso), poi il nuovo prende il suo posto. Al primo errore rimette tutto com'era.
pub fn replace_files(new_dir: &Path, app_dir: &Path) -> anyhow::Result<()> {
    let mut done: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    let mut step = || -> anyhow::Result<()> {
        for src in files_under(new_dir)? {
            let dst = app_dir.join(src.strip_prefix(new_dir)?);
            // File identici (licenze, DLL non cambiate): nessuna copia e nessun `.old`.
            if same_content(&src, &dst) {
                continue;
            }
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).with_context(|| format!("{}", parent.display()))?;
            }
            let old = if dst.exists() {
                let old = old_name(&dst);
                let _ = std::fs::remove_file(&old);
                std::fs::rename(&dst, &old).with_context(|| format!("{} non rinominabile", dst.display()))?;
                Some(old)
            } else {
                None
            };
            done.push((dst.clone(), old));
            std::fs::copy(&src, &dst).with_context(|| format!("{} non scrivibile", dst.display()))?;
        }
        Ok(())
    };
    let result = step();
    if result.is_err() {
        for (dst, old) in done.into_iter().rev() {
            let _ = std::fs::remove_file(&dst);
            if let Some(old) = old {
                let _ = std::fs::rename(&old, &dst);
            }
        }
    }
    result
}

/// Toglie i `.old` lasciati dall'ultimo aggiornamento (all'avvio, quando non sono più in uso).
pub fn cleanup_old(app_dir: &Path) {
    for f in files_under(app_dir).unwrap_or_default() {
        if f.extension().is_some_and(|e| e == "old") {
            let _ = std::fs::remove_file(&f);
        }
    }
}

/// Aggiornamenti dell'app in esecuzione: stato per la finestra di aggiornamento (via IPC),
/// controllo e installazione su thread a parte, riavvio sulla versione nuova.
pub struct Updater {
    info: Mutex<UpdateInfo>,
    release: Mutex<Option<Release>>,
    /// Apre la finestra di aggiornamento (`cliprec-settings --aggiornamento`).
    open_dialog: Box<dyn Fn() + Send + Sync>,
    /// Chiude questa istanza dopo aver avviato quella aggiornata.
    quit: Box<dyn Fn() + Send + Sync>,
}

impl Updater {
    pub fn new(open_dialog: impl Fn() + Send + Sync + 'static, quit: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            info: Mutex::new(UpdateInfo { current: current(), ..UpdateInfo::default() }),
            release: Mutex::new(None),
            open_dialog: Box::new(open_dialog),
            quit: Box::new(quit),
        })
    }

    pub fn info(&self) -> UpdateInfo {
        self.info.lock().map(|i| i.clone()).unwrap_or_default()
    }

    fn set_phase(&self, phase: UpdatePhase) {
        if let Ok(mut i) = self.info.lock() {
            i.phase = phase;
        }
    }

    /// Passa a `phase` se non c'è già un controllo o un'installazione in corso.
    fn start(&self, phase: UpdatePhase) -> bool {
        let Ok(mut i) = self.info.lock() else { return false };
        if matches!(i.phase, UpdatePhase::Checking | UpdatePhase::Downloading | UpdatePhase::Installing | UpdatePhase::Restarting) {
            return false;
        }
        i.phase = phase;
        true
    }

    /// Cerca una versione nuova su un thread a parte; con `show` apre la finestra se la trova.
    pub fn check(self: &Arc<Self>, show: bool) {
        if !self.start(UpdatePhase::Checking) {
            return;
        }
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new().name("aggiornamenti".into()).spawn(move || match check() {
            Ok(Some(release)) => {
                tracing::info!(versione = %release.version, "aggiornamento disponibile");
                if let Ok(mut i) = me.info.lock() {
                    i.available = Some(release.version.clone());
                    i.notes.clone_from(&release.notes);
                    i.phase = UpdatePhase::Idle;
                }
                if let Ok(mut r) = me.release.lock() {
                    *r = Some(release);
                }
                if show {
                    (me.open_dialog)();
                }
            }
            Ok(None) => me.set_phase(UpdatePhase::UpToDate),
            Err(e) => {
                tracing::warn!("controllo aggiornamenti: {e:#}");
                me.set_phase(UpdatePhase::Failed(format!("{e:#}")));
            }
        });
        if spawned.is_err() {
            self.set_phase(UpdatePhase::Failed("thread non avviato".into()));
        }
    }

    /// Scarica e installa la versione trovata, poi riavvia ClipRec su quella.
    pub fn install(self: &Arc<Self>) {
        let Some(release) = self.release.lock().ok().and_then(|r| r.clone()) else {
            self.set_phase(UpdatePhase::Failed("nessuna versione da installare".into()));
            return;
        };
        if !self.start(UpdatePhase::Downloading) {
            return;
        }
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new().name("aggiornamento".into()).spawn(move || {
            let result = app_dir()
                .context("cartella di ClipRec sconosciuta")
                .and_then(|dir| install(&release, &dir, || me.set_phase(UpdatePhase::Installing)).map(|()| dir));
            match result {
                Ok(dir) => {
                    tracing::info!(versione = %release.version, "aggiornamento installato, riavvio");
                    me.set_phase(UpdatePhase::Restarting);
                    // La nuova istanza aspetta che questa liberi il mutex dell'istanza unica.
                    let started = Command::new(dir.join("cliprec-app.exe"))
                        .arg(crate::sys::RESTART_ARG)
                        .arg("--tray")
                        .env_remove("CLIPREC_UPDATE_AS")
                        .spawn();
                    match started {
                        Ok(_) => (me.quit)(),
                        Err(e) => me.set_phase(UpdatePhase::Failed(format!("riavvio: {e}"))),
                    }
                }
                Err(e) => {
                    tracing::warn!("aggiornamento: {e:#}");
                    me.set_phase(UpdatePhase::Failed(format!("{e:#}")));
                }
            }
        });
        if spawned.is_err() {
            self.set_phase(UpdatePhase::Failed("thread non avviato".into()));
        }
    }
}

/// Cartella dell'eseguibile in esecuzione.
pub fn app_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/update").join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("pippo", "0.1.0"));
    }

    #[test]
    fn latest_release_needs_zip_and_checksum() {
        let json = r#"{"tag_name":"v0.2.0","draft":false,"prerelease":false,"assets":[
            {"name":"ClipRec-0.2.0-Setup.exe","browser_download_url":"https://x/setup.exe"},
            {"name":"ClipRec-0.2.0-portable.zip","browser_download_url":"https://x/p.zip"},
            {"name":"ClipRec-0.2.0-portable.zip.sha256","browser_download_url":"https://x/p.zip.sha256"}]}"#;
        assert_eq!(
            parse_latest(json),
            Some(Release {
                version: "0.2.0".into(),
                zip_url: "https://x/p.zip".into(),
                sha_url: "https://x/p.zip.sha256".into(),
                notes: String::new(),
            })
        );
        assert_eq!(parse_latest(&json.replace(".zip.sha256", ".txt")), None, "senza checksum non si aggiorna");
        assert_eq!(parse_latest(&json.replace("\"prerelease\":false", "\"prerelease\":true")), None);
    }

    #[test]
    fn files_are_replaced_and_old_ones_cleaned_up() {
        let (app, new) = (temp("app"), temp("nuovi"));
        std::fs::write(app.join("a.txt"), "vecchio").unwrap();
        std::fs::write(new.join("a.txt"), "nuovo").unwrap();
        std::fs::create_dir_all(new.join("licenses")).unwrap();
        std::fs::write(new.join("licenses").join("b.txt"), "b").unwrap();
        std::fs::write(app.join("uguale.txt"), "uguale").unwrap();
        std::fs::write(new.join("uguale.txt"), "uguale").unwrap();
        replace_files(&new, &app).unwrap();
        assert!(!app.join("uguale.txt.old").exists(), "un file identico non si tocca");
        assert_eq!(std::fs::read_to_string(app.join("a.txt")).unwrap(), "nuovo");
        assert_eq!(std::fs::read_to_string(app.join("a.txt.old")).unwrap(), "vecchio");
        assert_eq!(std::fs::read_to_string(app.join("licenses").join("b.txt")).unwrap(), "b");
        cleanup_old(&app);
        assert!(!app.join("a.txt.old").exists());
    }

    #[test]
    fn failed_update_restores_the_old_files() {
        let (app, new) = (temp("app-errore"), temp("nuovi-errore"));
        std::fs::write(app.join("a.txt"), "vecchio").unwrap();
        // «sub» è un file nella cartella dell'app: «sub\b.txt» non si può creare.
        std::fs::write(app.join("sub"), "x").unwrap();
        std::fs::write(new.join("a.txt"), "nuovo").unwrap();
        std::fs::create_dir_all(new.join("sub")).unwrap();
        std::fs::write(new.join("sub").join("b.txt"), "b").unwrap();
        assert!(replace_files(&new, &app).is_err());
        assert_eq!(std::fs::read_to_string(app.join("a.txt")).unwrap(), "vecchio");
        assert!(!app.join("a.txt.old").exists());
    }

    /// Un eseguibile in esecuzione si sostituisce: è il caso di cliprec-app.exe durante l'aggiornamento.
    #[test]
    fn running_executable_is_replaced() {
        let (app, new) = (temp("app-in-uso"), temp("nuovi-in-uso"));
        let exe = app.join("prova.exe");
        std::fs::copy(r"C:\Windows\System32\PING.EXE", &exe).unwrap();
        let mut child = Command::new(&exe)
            .args(["-n", "5", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        std::fs::write(new.join("prova.exe"), "versione nuova").unwrap();
        let result = replace_files(&new, &app);
        let _ = child.kill();
        let _ = child.wait();
        result.unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "versione nuova");
        assert!(app.join("prova.exe.old").exists());
    }
}
