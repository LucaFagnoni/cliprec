//! Monitor, finestre e adattatori: enumerazione e ricerca (nessuna allocazione nel percorso caldo,
//! queste funzioni girano solo all'avvio e nel thread di sorveglianza).

use cliprec_core::gpu::luid_to_i64;
use cliprec_core::{GpuDevice, SourceError};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromPoint, MonitorFromWindow,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetForegroundWindow, GetShellWindow, GetWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, MONITORINFOF_PRIMARY,
    WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, PWSTR};

/// Monitor attivi: il principale per primo, poi gli altri nell'ordine di Windows.
pub(crate) fn monitors() -> Vec<HMONITOR> {
    unsafe extern "system" fn push(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` è il puntatore al Vec passato da `monitors`, vivo per tutta EnumDisplayMonitors.
        unsafe { (*(data.0 as *mut Vec<HMONITOR>)).push(h) };
        BOOL(1)
    }
    let mut list: Vec<HMONITOR> = Vec::new();
    // SAFETY: la callback usa `list` solo durante la chiamata sincrona.
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(push), LPARAM(&mut list as *mut _ as isize)) };
    list.sort_by_key(|&h| monitor_info(h).is_none_or(|mi| mi.dwFlags & MONITORINFOF_PRIMARY == 0));
    list
}

pub(crate) fn primary_monitor() -> HMONITOR {
    // SAFETY: nessun puntatore; con DEFAULTTOPRIMARY restituisce sempre un monitor.
    unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
}

fn monitor_info(h: HMONITOR) -> Option<MONITORINFO> {
    let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: `mi` è locale con cbSize impostato.
    unsafe { GetMonitorInfoW(h, &mut mi) }.as_bool().then_some(mi)
}

/// Rettangolo del monitor in coordinate desktop.
pub(crate) fn monitor_rect(h: HMONITOR) -> Option<RECT> {
    monitor_info(h).map(|mi| mi.rcMonitor)
}

pub(crate) fn rect_size(r: RECT) -> (u32, u32) {
    ((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32)
}

/// LUID dell'adattatore DXGI che pilota il monitor.
fn adapter_luid_for_monitor(h: HMONITOR) -> Option<i64> {
    // SAFETY: CreateDXGIFactory1 non ha precondizioni.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    for i in 0.. {
        // SAFETY: chiamate COM su interfacce valide; gli Enum* falliscono oltre l'ultimo elemento.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else { break };
        for j in 0.. {
            // SAFETY: vedi sopra.
            let Ok(output) = (unsafe { adapter.EnumOutputs(j) }) else { break };
            // SAFETY: vedi sopra.
            if unsafe { output.GetDesc() }.is_ok_and(|d| d.Monitor == h) {
                // SAFETY: vedi sopra.
                return unsafe { adapter.GetDesc1() }.ok().map(|d| luid_to_i64(d.AdapterLuid));
            }
        }
    }
    None
}

/// Crea il device sull'adattatore che mostra il monitor (sui portatili ibridi è quello giusto per
/// WGC e l'unico che funziona con Desktop Duplication); se non lo trova usa il predefinito.
pub(crate) fn device_for_hmonitor(h: HMONITOR) -> Result<GpuDevice, SourceError> {
    let luid = adapter_luid_for_monitor(h);
    if luid.is_none() {
        tracing::warn!("adattatore del monitor non trovato: uso quello predefinito");
    }
    GpuDevice::create(luid)
        .or_else(|e| {
            tracing::warn!("device sull'adattatore del monitor non creato ({e}): uso quello predefinito");
            GpuDevice::create(None)
        })
        .map_err(|e| SourceError::Failed(e.to_string()))
}

/// Device D3D11 sull'adattatore che mostra il monitor `index` (0 = principale).
pub fn device_for_monitor(index: u32) -> Result<GpuDevice, SourceError> {
    let h = monitors()
        .get(index as usize)
        .copied()
        .ok_or_else(|| SourceError::NotFound(format!("monitor {}", index + 1)))?;
    device_for_hmonitor(h)
}

pub(crate) fn window_monitor(hwnd: HWND) -> Option<HMONITOR> {
    // SAFETY: nessun puntatore; HWND non valido restituisce un monitor nullo.
    let h = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) };
    (!h.is_invalid()).then_some(h)
}

pub(crate) fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: buffer locale; la funzione scrive al massimo buf.len() caratteri.
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.clamp(0, 256) as usize])
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    // SAFETY: buffer locale.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.clamp(0, 128) as usize])
}

/// Nome del file eseguibile del processo che possiede la finestra, es. `osu!.exe`.
pub(crate) fn window_exe(hwnd: HWND) -> Option<String> {
    let mut pid = 0u32;
    // SAFETY: puntatore locale.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    // SAFETY: nessun puntatore; l'handle è chiusa sotto.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: buffer locale di `len` caratteri; l'handle è valida.
    let ok = unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) };
    // SAFETY: handle aperta sopra, chiusa una volta.
    let _ = unsafe { CloseHandle(process) };
    ok.ok()?;
    let path = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
    path.rsplit(['\\', '/']).next().map(str::to_owned)
}

pub(crate) fn is_window(hwnd: HWND) -> bool {
    // SAFETY: nessun puntatore.
    unsafe { IsWindow(Some(hwnd)) }.as_bool()
}

pub(crate) fn is_foreground(hwnd: HWND) -> bool {
    // SAFETY: nessun puntatore.
    let fg = unsafe { GetForegroundWindow() };
    fg == hwnd
}

pub(crate) fn is_minimized(hwnd: HWND) -> bool {
    // SAFETY: nessun puntatore.
    unsafe { IsIconic(hwnd) }.as_bool()
}

fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    // SAFETY: puntatore locale.
    unsafe { GetWindowRect(hwnd, &mut r) }.ok().map(|()| r)
}

/// Finestre di primo livello candidate alla cattura: visibili, non nascoste da DWM, non strumenti,
/// senza proprietario.
fn capturable_windows() -> Vec<HWND> {
    unsafe extern "system" fn push(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` è il puntatore al Vec passato da `capturable_windows`, vivo per tutta EnumWindows.
        unsafe { (*(data.0 as *mut Vec<HWND>)).push(hwnd) };
        BOOL(1)
    }
    let mut all: Vec<HWND> = Vec::new();
    // SAFETY: la callback usa `all` solo durante la chiamata sincrona.
    let _ = unsafe { EnumWindows(Some(push), LPARAM(&mut all as *mut _ as isize)) };
    all.retain(|&h| is_capturable(h));
    all
}

fn is_capturable(hwnd: HWND) -> bool {
    // SAFETY: nessun puntatore.
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return false;
    }
    // SAFETY: nessun puntatore.
    if unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.is_invalid()) {
        return false;
    }
    // SAFETY: nessun puntatore.
    let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    if ex & WS_EX_TOOLWINDOW.0 != 0 {
        return false;
    }
    let mut cloaked = 0u32;
    // SAFETY: `cloaked` è locale e della dimensione dichiarata.
    let cloak = unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, size_of::<u32>() as u32)
    };
    !(cloak.is_ok() && cloaked != 0)
}

fn area(hwnd: HWND) -> i64 {
    window_rect(hwnd).map_or(0, |r| i64::from(r.right - r.left) * i64::from(r.bottom - r.top))
}

/// La finestra più grande il cui titolo contiene `part` (senza distinguere maiuscole).
pub(crate) fn find_window_by_title(part: &str) -> Option<HWND> {
    let part = part.to_lowercase();
    capturable_windows()
        .into_iter()
        .filter(|&h| window_title(h).to_lowercase().contains(&part))
        .max_by_key(|&h| area(h))
}

/// `true` se `exe` corrisponde a `wanted` senza distinguere maiuscole e con «.exe» facoltativo.
pub(crate) fn exe_matches(exe: &str, wanted: &str) -> bool {
    let strip = |s: &str| {
        let s = s.trim().to_lowercase();
        s.strip_suffix(".exe").map(str::to_owned).unwrap_or(s)
    };
    strip(exe) == strip(wanted)
}

/// La finestra principale (la più grande) del processo con questo eseguibile.
pub(crate) fn find_window_by_exe(exe: &str) -> Option<HWND> {
    capturable_windows()
        .into_iter()
        .filter(|&h| window_exe(h).is_some_and(|e| exe_matches(&e, exe)))
        .max_by_key(|&h| area(h))
}

/// Finestra in primo piano se copre tutto il suo monitor (e non è il desktop), con il suo eseguibile
/// (vuoto se il processo è protetto e non si lascia interrogare).
pub(crate) fn fullscreen_foreground() -> Option<(HWND, String)> {
    // SAFETY: nessun puntatore.
    let hwnd = unsafe { GetForegroundWindow() };
    // SAFETY: nessun puntatore.
    if hwnd.is_invalid() || hwnd == unsafe { GetShellWindow() } {
        return None;
    }
    if matches!(class_name(hwnd).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") || !is_capturable(hwnd) {
        return None;
    }
    let r = window_rect(hwnd)?;
    let m = monitor_rect(window_monitor(hwnd)?)?;
    let covers = r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom;
    if !covers {
        return None;
    }
    Some((hwnd, window_exe(hwnd).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_monitor_comes_first() {
        let list = monitors();
        assert!(!list.is_empty());
        assert_eq!(list[0], primary_monitor());
        let (w, h) = rect_size(monitor_rect(list[0]).unwrap());
        assert!(w >= 640 && h >= 480, "{w}×{h}");
        let dev = device_for_monitor(0).unwrap();
        println!("monitor principale {w}×{h} su {dev:?}");
        assert_ne!(dev.adapter_luid, 0);
        assert!(device_for_monitor(99).is_err());
    }

    #[test]
    fn exe_names_match_loosely() {
        assert!(exe_matches("osu!.exe", "OSU!"));
        assert!(exe_matches("Game.EXE", "game.exe"));
        assert!(!exe_matches("game2.exe", "game.exe"));
    }
}
