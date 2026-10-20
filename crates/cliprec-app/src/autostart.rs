//! Avvio con Windows (valore `ClipRec` nella chiave HKCU Run, come l'installer, oppure attività pianificata con
//! privilegi massimi per l'avvio come amministratore) e collegamento nel menu Start con
//! AppUserModelID, necessario ai toast. Scrive nel registro, nel menu Start e nell'Utilità di
//! pianificazione: non va chiamato in test né di notte (la shell lo salta con `CLIPREC_HEADLESS`).

use std::path::PathBuf;

use cliprec_core::Config;
use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_BINARY, REG_SZ, RRF_RT_ANY, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::core::{Interface, PCWSTR, w};

use crate::notify::APP_ID;
use crate::sys::wide;

/// Nome dell'attività pianificata per l'avvio come amministratore.
const TASK: &str = "ClipRec";

/// Cosa fare per allineare l'avvio automatico alla configurazione.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plan {
    /// Valore `Run` (avvio normale).
    run: bool,
    /// Attività pianificata da creare o da togliere (entrambe chiedono la conferma UAC).
    create_task: bool,
    delete_task: bool,
}

/// L'attività si crea o si toglie solo quando cambia: `task_present` viene dal segnaposto scritto
/// dopo l'ultima modifica riuscita, così non si chiede il permesso a ogni avvio.
fn plan(start_with_windows: bool, start_as_admin: bool, task_present: bool) -> Plan {
    let admin = start_with_windows && start_as_admin;
    Plan { run: start_with_windows && !admin, create_task: admin && !task_present, delete_task: !admin && task_present }
}

/// Allinea l'avvio automatico alla configurazione e crea il collegamento del menu Start se manca.
/// Restituisce un problema da mostrare all'utente (es. conferma di amministratore rifiutata).
pub fn apply(config: &Config) -> Option<String> {
    let g = &config.general;
    let marker = task_marker();
    let p = plan(g.start_with_windows, g.start_as_admin, marker.as_ref().is_some_and(|m| m.exists()));
    if let Err(e) = apply_autostart(p.run) {
        tracing::warn!("avvio con Windows: {e}");
    }
    if let Err(e) = ensure_start_menu_shortcut() {
        tracing::warn!("collegamento menu Start: {e}");
    }
    let result = if p.create_task {
        create_task().map(|()| marker.as_ref().map(|m| std::fs::write(m, TASK)))
    } else if p.delete_task {
        delete_task().map(|()| marker.as_ref().map(std::fs::remove_file))
    } else {
        return None;
    };
    match result {
        Ok(_) => {
            tracing::info!(creata = p.create_task, "attività di avvio come amministratore aggiornata");
            None
        }
        Err(e) => {
            tracing::warn!("avvio come amministratore: {e}");
            Some(format!("Avvio come amministratore non aggiornato: {e}"))
        }
    }
}

/// Segnaposto accanto a config.toml: c'è finché esiste l'attività pianificata creata da ClipRec.
fn task_marker() -> Option<PathBuf> {
    Config::default_path().parent().map(|d| d.join("avvio-amministratore"))
}

/// Attività all'accesso dell'utente, con privilegi massimi, priorità normale, senza limite di
/// durata e anche a batteria (i predefiniti di Windows fermerebbero ClipRec dopo 3 giorni).
fn task_xml(exe: &str, user: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let (exe, user) = (esc(exe), esc(user));
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>ClipRec: avvio come amministratore all'accesso</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>--tray</Arguments></Exec></Actions>
</Task>
"#
    )
}

fn create_task() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(d), Ok(u)) => format!("{d}\\{u}"),
        (_, Ok(u)) => u,
        _ => anyhow::bail!("utente corrente sconosciuto"),
    };
    // L'XML va in UTF-16 con BOM, come lo vuole schtasks.
    let xml = std::env::temp_dir().join("cliprec-avvio.xml");
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(task_xml(&exe, &user).encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&xml, bytes)?;
    let code = crate::sys::run_elevated_wait("schtasks.exe", &format!("/Create /TN {TASK} /XML \"{}\" /F", xml.display()));
    let _ = std::fs::remove_file(&xml);
    match code? {
        0 => Ok(()),
        c => anyhow::bail!("schtasks /Create ha restituito {c}"),
    }
}

fn delete_task() -> anyhow::Result<()> {
    match crate::sys::run_elevated_wait("schtasks.exe", &format!("/Delete /TN {TASK} /F"))? {
        0 => Ok(()),
        c => anyhow::bail!("schtasks /Delete ha restituito {c}"),
    }
}

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
/// Dove Gestione attività → App di avvio segna le app disattivate.
const APPROVED_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run");
const RUN_NAME: PCWSTR = w!("ClipRec");
/// Valore di StartupApproved per un'app attiva.
const APPROVED_ON: [u8; 12] = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// Valore `ClipRec` sotto `key`, se c'è (al più 1 KB: i valori più lunghi risultano presenti ma vuoti).
fn reg_value(root: HKEY, key: PCWSTR) -> Option<Vec<u8>> {
    let mut buf = [0u8; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: chiave e nome sono stringhe wide statiche; `buf` e `len` descrivono lo stesso buffer.
    let r = unsafe { RegGetValueW(root, key, RUN_NAME, RRF_RT_ANY, None, Some(buf.as_mut_ptr().cast()), Some(&mut len)) };
    match r {
        ERROR_SUCCESS => Some(buf[..(len as usize).min(buf.len())].to_vec()),
        ERROR_MORE_DATA => Some(Vec::new()),
        _ => None,
    }
}

/// Gestione attività ha disattivato ClipRec tra le app di avvio.
fn disabled_by_task_manager(root: HKEY) -> bool {
    reg_value(root, APPROVED_KEY).is_some_and(|v| approved_off(&v))
}

/// Valore di StartupApproved per un'app disattivata: gli ultimi 8 byte (l'ora della
/// disattivazione) non sono tutti zero.
fn approved_off(v: &[u8]) -> bool {
    v.len() >= 8 && v[v.len() - 8..].iter().any(|&b| b != 0)
}

/// Il valore `Run` di ClipRec esiste (scritto dall'installer o da ClipRec, in HKCU o in HKLM dalle
/// versioni precedenti) e Gestione attività non l'ha disattivato. Sola lettura del registro.
pub fn run_value_present() -> bool {
    let roots = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE];
    roots.iter().any(|&r| reg_value(r, RUN_KEY).is_some()) && !roots.iter().any(|&r| disabled_by_task_manager(r))
}

fn apply_autostart(enabled: bool) -> anyhow::Result<()> {
    if run_value_present() == enabled {
        return Ok(());
    }
    if enabled {
        let data = wide(&format!("\"{}\" --tray", std::env::current_exe()?.display()));
        // SAFETY: chiave e nome statici; `data` è una stringa wide terminata da NUL di `2 * len` byte.
        unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_NAME, REG_SZ.0, Some(data.as_ptr().cast()), (data.len() * 2) as u32) }
            .ok()?;
        // Chiedere l'avvio con Windows annulla la disattivazione fatta da Gestione attività.
        if disabled_by_task_manager(HKEY_CURRENT_USER) {
            // SAFETY: come sopra; APPROVED_ON vive per tutta la chiamata.
            unsafe {
                RegSetKeyValueW(HKEY_CURRENT_USER, APPROVED_KEY, RUN_NAME, REG_BINARY.0, Some(APPROVED_ON.as_ptr().cast()), 12)
            }
            .ok()?;
        }
    } else {
        // HKLM: residuo delle versioni che da amministratore scrivevano lì; senza permessi fallisce, va bene.
        for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
            // SAFETY: chiave e nome statici.
            let _ = unsafe { RegDeleteKeyValueW(root, RUN_KEY, RUN_NAME) };
        }
    }
    Ok(())
}

fn shortcut_path() -> PathBuf {
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    appdata.join(r"Microsoft\Windows\Start Menu\Programs\ClipRec.lnk")
}

fn ensure_start_menu_shortcut() -> anyhow::Result<()> {
    let lnk = shortcut_path();
    if lnk.exists() {
        return Ok(());
    }
    let exe = wide(&std::env::current_exe()?.to_string_lossy());
    let lnk_w = wide(&lnk.to_string_lossy());
    // SAFETY: COM inizializzato e rilasciato in questa funzione; tutte le stringhe sono
    // terminate da NUL e vivono per l'intera sequenza di chiamate.
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = (|| -> windows::core::Result<()> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(PCWSTR(exe.as_ptr()))?;
            let store: IPropertyStore = link.cast()?;
            let value = windows::Win32::System::Com::StructuredStorage::PROPVARIANT::from(APP_ID);
            store.SetValue(&PKEY_AppUserModel_ID, &value)?;
            store.Commit()?;
            let file: IPersistFile = link.cast()?;
            file.Save(PCWSTR(lnk_w.as_ptr()), true)
        })();
        if init.is_ok() {
            CoUninitialize();
        }
        result?;
    }
    tracing::info!(path = %lnk.display(), "collegamento nel menu Start creato");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_manager_disable_is_read_from_startup_approved() {
        assert!(!approved_off(&APPROVED_ON));
        assert!(approved_off(&[3, 0, 0, 0, 0x9a, 0x3f, 0x11, 0x4e, 0x2d, 0x41, 0xdc, 0x01]));
        assert!(!approved_off(&[3, 0, 0]));
        // Sola lettura del registro: non deve andare in panic qualunque sia lo stato.
        println!("avvio con Windows tramite Run: {}", run_value_present());
    }

    #[test]
    fn admin_start_uses_the_task_and_asks_only_on_change() {
        // Normale: valore Run, nessuna attività.
        assert_eq!(plan(true, false, false), Plan { run: true, create_task: false, delete_task: false });
        // Come amministratore: niente Run, l'attività si crea una volta sola.
        assert_eq!(plan(true, true, false), Plan { run: false, create_task: true, delete_task: false });
        assert_eq!(plan(true, true, true), Plan { run: false, create_task: false, delete_task: false });
        // «Avvia con Windows» spento: l'amministratore non conta e l'attività si toglie.
        assert_eq!(plan(false, true, true), Plan { run: false, create_task: false, delete_task: true });
        assert_eq!(plan(false, true, false), Plan { run: false, create_task: false, delete_task: false });
        assert_eq!(plan(true, false, true), Plan { run: true, create_task: false, delete_task: true });
    }

    #[test]
    fn task_runs_elevated_without_time_limit_and_on_battery() {
        let xml = task_xml(r"C:\Program Files\R&D <x>\cliprec-app.exe", r"PC\Luca");
        for want in [
            "<RunLevel>HighestAvailable</RunLevel>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<Priority>4</Priority>",
            "<LogonTrigger><Enabled>true</Enabled><UserId>PC\\Luca</UserId>",
            r"<Command>C:\Program Files\R&amp;D &lt;x&gt;\cliprec-app.exe</Command>",
            "<Arguments>--tray</Arguments>",
        ] {
            assert!(xml.contains(want), "manca {want}");
        }
    }
}
