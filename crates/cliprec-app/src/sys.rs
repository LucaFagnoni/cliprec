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
    fn sid_and_elevation_are_readable() {
        assert!(current_user_sid().unwrap().starts_with("S-1-5-"));
        let _ = self_elevated();
    }
}
