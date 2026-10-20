//! Piccole chiamate Win32: istanza unica, elevazione dei processi, apertura di file.

use std::ffi::c_void;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, LocalFree, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenUser};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowRect, FindWindowExW, GetWindowThreadProcessId, HWND_MESSAGE, PostMessageW, RegisterWindowMessageW, SW_SHOWNORMAL,
};
use windows::core::{PCWSTR, PWSTR};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Nome del mutex di istanza unica: `Local\ClipRec-<utente>`.
pub fn mutex_name() -> String {
    format!(r"Local\ClipRec-{}", std::env::var("USERNAME").unwrap_or_default())
}

/// Tiene vivo il mutex nominato finché esiste.
pub struct InstanceGuard(HANDLE);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: l'handle è stato creato da CreateMutexW e non è mai copiato.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// `Ok(Some(guard))` se siamo la prima istanza, `Ok(None)` se ne esiste già un'altra.
pub fn acquire_single_instance(name: &str) -> anyhow::Result<Option<InstanceGuard>> {
    let wname = wide(name);
    // SAFETY: `wname` è terminata da NUL e vive per tutta la chiamata.
    let handle = unsafe { CreateMutexW(None, true, PCWSTR(wname.as_ptr())) }?;
    // SAFETY: GetLastError legge solo lo stato del thread, subito dopo CreateMutexW.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // SAFETY: handle valido appena restituito.
        let _ = unsafe { CloseHandle(handle) };
        return Ok(None);
    }
    Ok(Some(InstanceGuard(handle)))
}

fn token_elevated(process: HANDLE) -> Option<bool> {
    let mut token = HANDLE::default();
    // SAFETY: `token` è un'uscita valida; un errore (anche ACCESS_DENIED) lo gestiamo sotto.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.ok()?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: il buffer è un TOKEN_ELEVATION della dimensione dichiarata.
    let r = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast::<c_void>()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    // SAFETY: handle aperto sopra.
    let _ = unsafe { CloseHandle(token) };
    r.ok()?;
    Some(elevation.TokenIsElevated != 0)
}

/// ClipRec stesso gira elevato?
pub fn self_elevated() -> bool {
    // SAFETY: pseudo-handle del processo corrente, sempre valido.
    token_elevated(unsafe { GetCurrentProcess() }).unwrap_or(false)
}

/// Stringa SDDL del SID dell'utente corrente, per limitare la named pipe a lui.
pub fn current_user_sid() -> anyhow::Result<String> {
    let mut token = HANDLE::default();
    // SAFETY: uscita valida.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }?;
    let mut needed = 0u32;
    // SAFETY: prima chiamata per conoscere la dimensione; fallisce di proposito con buffer nullo.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) };
    let mut buf = vec![0u64; (needed as usize).div_ceil(8).max(1)];
    // SAFETY: buf è allineato a 8 byte e grande almeno `needed`.
    let r = unsafe { GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast::<c_void>()), needed, &mut needed) };
    // SAFETY: handle aperto sopra.
    let _ = unsafe { CloseHandle(token) };
    r?;
    // SAFETY: GetTokenInformation ha scritto un TOKEN_USER valido all'inizio di buf.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut out = PWSTR::null();
    // SAFETY: `sid` punta dentro buf, vivo; `out` è allocato da Windows e liberato sotto.
    unsafe { ConvertSidToStringSidW(sid, &mut out) }?;
    // SAFETY: `out` è una stringa NUL-terminata valida appena restituita.
    let s = unsafe { out.to_string() };
    // SAFETY: la stringa va liberata con LocalFree.
    let _ = unsafe { LocalFree(Some(HLOCAL(out.0.cast()))) };
    Ok(s?)
}

/// Gioco in primo piano avviato come amministratore mentre ClipRec no: Raw Input non arriverebbe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElevatedForeground {
    pub pid: u32,
    pub exe: String,
}

/// Processo in primo piano se è elevato (o non ispezionabile per accesso negato), a schermo
/// intero oppure presente in `games`; `None` altrimenti.
pub fn elevated_foreground_game(games: &[String]) -> Option<ElevatedForeground> {
    // SAFETY: nessun argomento; restituisce HWND nullo se non c'è finestra attiva.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return None;
    }
    let mut pid = 0u32;
    // SAFETY: `pid` è un'uscita valida.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 || pid == std::process::id() {
        return None;
    }
    // SAFETY: handle chiuso più sotto; se fallisce il processo è protetto.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let exe = image_name(process).unwrap_or_default();
    let elevated = token_elevated(process).unwrap_or(true); // accesso negato al token = elevato
    // SAFETY: handle aperto sopra.
    let _ = unsafe { CloseHandle(process) };
    if !elevated || exe.eq_ignore_ascii_case("explorer.exe") {
        return None;
    }
    let listed = games.iter().any(|g| g.eq_ignore_ascii_case(&exe));
    (listed || is_fullscreen(hwnd)).then_some(ElevatedForeground { pid, exe })
}

fn image_name(process: HANDLE) -> Option<String> {
    let mut buf = [0u16; 520];
    let mut len = buf.len() as u32;
    // SAFETY: buf e len descrivono lo stesso buffer.
    unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.ok()?;
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    path.rsplit('\\').next().map(str::to_owned)
}

fn is_fullscreen(hwnd: windows::Win32::Foundation::HWND) -> bool {
    let mut r = RECT::default();
    // SAFETY: uscite valide.
    if unsafe { GetWindowRect(hwnd, &mut r) }.is_err() {
        return false;
    }
    // SAFETY: hwnd valido per la chiamata.
    let mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: cbSize impostato.
    if !unsafe { GetMonitorInfoW(mon, &mut mi) }.as_bool() {
        return false;
    }
    let m = mi.rcMonitor;
    r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
}

/// Argomento con cui l'istanza riavviata come amministratore sa di dover aspettare la vecchia.
pub const RESTART_ARG: &str = "--dopo-riavvio";

/// Quanto la nuova istanza aspetta la vecchia: il motore chiude in al più 15 s (salvataggi in corso).
pub const RESTART_WAIT: std::time::Duration = std::time::Duration::from_secs(25);

/// La voce «Riavvia come amministratore» ha senso solo se ClipRec non lo è già.
pub fn restart_menu_visible(self_elevated: bool) -> bool {
    !self_elevated
}

/// Come [`acquire_single_instance`], ma riprova ogni `step` fino a `timeout`: la vecchia istanza
/// sta chiudendo e rilascia il mutex all'uscita.
pub fn acquire_single_instance_wait(
    name: &str,
    timeout: std::time::Duration,
    step: std::time::Duration,
) -> anyhow::Result<Option<InstanceGuard>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(g) = acquire_single_instance(name)? {
            return Ok(Some(g));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(step);
    }
}

/// Rilancia questo exe con il verbo «runas» (dialogo UAC). Se l'utente rifiuta restituisce un
/// errore e non cambia nulla: il chiamante decide se uscire solo a `Ok`.
pub fn restart_elevated() -> anyhow::Result<()> {
    use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
    let exe = wide(&std::env::current_exe()?.to_string_lossy());
    let args = wide(RESTART_ARG);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        lpVerb: windows::core::w!("runas"),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(args.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` è completa e le stringhe terminate da NUL vivono per tutta la chiamata.
    unsafe { ShellExecuteExW(&mut info) }.map_err(|e| {
        if e.code() == windows::Win32::Foundation::ERROR_CANCELLED.to_hresult() {
            anyhow::anyhow!("richiesta di amministratore annullata dall'utente")
        } else {
            anyhow::anyhow!("ShellExecuteExW: {e}")
        }
    })
}

/// Esegue `file params` come amministratore (dialogo UAC), senza finestra, e ne aspetta l'uscita
/// (al più 60 s). Restituisce il codice d'uscita; errore se l'utente rifiuta.
pub fn run_elevated_wait(file: &str, params: &str) -> anyhow::Result<u32> {
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let (file, params) = (wide(file), wide(params));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_NOCLOSEPROCESS,
        lpVerb: windows::core::w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: `info` è completa e le stringhe terminate da NUL vivono per tutta la chiamata.
    unsafe { ShellExecuteExW(&mut info) }.map_err(|e| {
        if e.code() == windows::Win32::Foundation::ERROR_CANCELLED.to_hresult() {
            anyhow::anyhow!("richiesta di amministratore annullata")
        } else {
            anyhow::anyhow!("ShellExecuteExW: {e}")
        }
    })?;
    let process = info.hProcess;
    let mut code = 1u32;
    // SAFETY: `process` è l'handle restituito da ShellExecuteExW con NOCLOSEPROCESS: lo si usa e lo si chiude qui.
    unsafe {
        WaitForSingleObject(process, 60_000);
        let _ = GetExitCodeProcess(process, &mut code);
        let _ = CloseHandle(process);
    }
    Ok(code)
}

/// Messaggio registrato con cui una seconda istanza avvisa la prima.
pub fn second_instance_msg() -> u32 {
    // SAFETY: stringa statica terminata da NUL; il valore è lo stesso in tutti i processi.
    unsafe { RegisterWindowMessageW(windows::core::w!("ClipRec.SecondInstance")) }
}

/// Dice alla prima istanza (finestra nascosta della shell) che ne è partita un'altra.
pub fn ping_first_instance() {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    // SAFETY: ricerca tra le finestre solo-messaggi; PostMessage verso un HWND trovato è sempre sicuro.
    unsafe {
        if let Ok(hwnd) = FindWindowExW(Some(HWND_MESSAGE), None, windows::core::w!("ClipRecShell"), None) {
            let _ = PostMessageW(Some(hwnd), second_instance_msg(), WPARAM(0), LPARAM(0));
        }
    }
}

/// Titoli delle finestre di cliprec-settings (impostazioni e aggiornamenti, nelle due lingue):
/// uguali a `window-title` in cliprec-settings/src/i18n.rs e a `update_window::TITLES`.
const SETTINGS_TITLES: [&str; 4] = ["ClipRec · Impostazioni", "ClipRec · Settings", "ClipRec · Aggiornamenti", "ClipRec · Updates"];

/// Chiude le finestre di cliprec-settings (con «Esci» dalla tray non devono restare aperte su un
/// core che non c'è più). WM_CLOSE come la X della finestra.
pub fn close_settings_windows() {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, WM_CLOSE};
    for title in SETTINGS_TITLES {
        let title = wide(title);
        // SAFETY: stringa terminata da NUL valida per la chiamata; PostMessage verso un HWND trovato è sicuro.
        unsafe {
            if let Ok(hwnd) = FindWindowW(None, PCWSTR(title.as_ptr())) {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// Apre un file o una cartella con l'app predefinita di Windows.
pub fn open_path(path: &Path) {
    let wpath = wide(&path.to_string_lossy());
    // SAFETY: stringhe terminate da NUL vive durante la chiamata.
    let r = unsafe { ShellExecuteW(None, windows::core::w!("open"), PCWSTR(wpath.as_ptr()), None, None, SW_SHOWNORMAL) };
    // ShellExecute segnala l'errore con un valore <= 32.
    if r.0 as usize <= 32 {
        tracing::warn!(path = %path.display(), code = r.0 as usize, "apertura fallita");
    }
}

const INSTALLER_KEY: windows::core::PCWSTR = windows::core::w!(r"Software\ClipRec");
const INSTALLER_VALUE: windows::core::PCWSTR = windows::core::w!("InstallerLanguage");

/// Lingua scelta nell'installer (`italian`/`english`, valore `HKCU\Software\ClipRec\InstallerLanguage`),
/// letta una sola volta: il valore viene cancellato, così una scelta successiva nelle impostazioni resta.
pub fn take_installer_language() -> Option<String> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW};
    let mut buf = [0u16; 32];
    let mut size = std::mem::size_of_val(&buf) as u32;
    // SAFETY: buffer e dimensione validi per tutta la chiamata; chiave e valore sono stringhe statiche.
    let read = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, INSTALLER_KEY, INSTALLER_VALUE, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut size))
    };
    if read.is_err() {
        return None;
    }
    // SAFETY: come sopra.
    let _ = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, INSTALLER_KEY, INSTALLER_VALUE) };
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_instance_second_acquire_fails() {
        let name = format!(r"Local\ClipRec-test-{}", std::process::id());
        let first = acquire_single_instance(&name).unwrap();
        assert!(first.is_some());
        assert!(acquire_single_instance(&name).unwrap().is_none());
        drop(first);
        assert!(acquire_single_instance(&name).unwrap().is_some());
    }

    #[test]
    fn restart_logic_without_uac() {
        assert!(restart_menu_visible(false) && !restart_menu_visible(true));
        let name = format!(r"Local\ClipRec-test-wait-{}", std::process::id());
        let (t, step) = (std::time::Duration::from_millis(300), std::time::Duration::from_millis(10));
        let held = acquire_single_instance(&name).unwrap().unwrap();
        // finché la vecchia istanza tiene il mutex l'attesa scade
        assert!(acquire_single_instance_wait(&name, t, step).unwrap().is_none());
        // la vecchia rilascia a metà attesa: la nuova lo prende
        let n = name.clone();
        let waiter = std::thread::spawn(move || {
            acquire_single_instance_wait(&n, std::time::Duration::from_secs(5), step).unwrap().is_some()
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        drop(held);
        assert!(waiter.join().unwrap());
        assert_eq!(RESTART_ARG, "--dopo-riavvio");
    }

    #[test]
    fn m13_restart_waits_longer_than_the_engine_shutdown() {
        assert!(RESTART_WAIT >= std::time::Duration::from_secs(20), "attesa di {RESTART_WAIT:?}: la vecchia istanza può impiegare 15 s");
    }

    #[test]
    fn sid_and_elevation_are_readable() {
        assert!(current_user_sid().unwrap().starts_with("S-1-5-"));
        let _ = self_elevated();
    }
}
