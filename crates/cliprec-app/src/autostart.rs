//! Avvio con Windows (chiave HKCU Run, via `auto-launch`) e collegamento nel menu Start con
//! AppUserModelID, necessario ai toast. Scrive nel registro e nel menu Start dell'utente: non
//! va chiamato in test né di notte (la shell lo salta con `CLIPREC_HEADLESS`).

use std::path::PathBuf;

use auto_launch::AutoLaunchBuilder;
use cliprec_core::Config;
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile,
};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::core::{Interface, PCWSTR};

use crate::notify::APP_ID;
use crate::sys::wide;

/// Allinea l'avvio automatico a `config.general.start_with_windows` e crea il collegamento
/// del menu Start se manca.
pub fn apply(config: &Config) {
    if let Err(e) = apply_autostart(config.general.start_with_windows) {
        tracing::warn!("avvio con Windows: {e}");
    }
    if let Err(e) = ensure_start_menu_shortcut() {
        tracing::warn!("collegamento menu Start: {e}");
    }
}

/// Il valore `Run` di ClipRec esiste già (scritto dall'installer)? Sola lettura del registro.
pub fn run_value_present() -> bool {
    autolaunch().and_then(|a| Ok(a.is_enabled()?)).unwrap_or(false)
}

fn autolaunch() -> anyhow::Result<auto_launch::AutoLaunch> {
    let exe = std::env::current_exe()?;
    Ok(AutoLaunchBuilder::new().set_app_name("ClipRec").set_app_path(&exe.to_string_lossy()).set_args(&["--tray"]).build()?)
}

fn apply_autostart(enabled: bool) -> anyhow::Result<()> {
    let auto = autolaunch()?;
    if auto.is_enabled()? != enabled {
        if enabled { auto.enable()? } else { auto.disable()? }
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
