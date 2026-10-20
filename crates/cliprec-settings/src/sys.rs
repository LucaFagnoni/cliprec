//! Elenchi d'aiuto da Windows (monitor, finestre, app in esecuzione, dispositivi audio) e
//! selettore di cartelle. Ogni errore degrada a un elenco vuoto: sono solo suggerimenti.

use std::ffi::c_void;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, EDataFlow, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::UI::Shell::{FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::{BOOL, PWSTR};

/// Voce di un elenco d'aiuto: etichetta mostrata e valore da scrivere nella configurazione.
#[derive(Clone, Debug, Default)]
pub struct Choice {
    pub label: String,
    pub value: String,
}

/// COM a thread singolo sul thread della UI (serve a dialogo cartelle e dispositivi audio).
pub fn init_com() {
    // SAFETY: chiamata FFI senza puntatori; un esito diverso da S_OK (già inizializzato con un
    // altro modello) non è un errore per noi.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
}

fn take_pwstr(p: PWSTR) -> String {
    // SAFETY: `p` è una stringa terminata da NUL restituita da COM; la liberiamo una sola volta.
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

unsafe extern "system" fn monitor_cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    // SAFETY: `data` è il puntatore al Vec passato da `monitors()`, vivo per tutta l'enumerazione.
    let list = unsafe { &mut *(data.0 as *mut Vec<(bool, String)>) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: MONITORINFOEXW inizia con un MONITORINFO e cbSize dichiara la dimensione estesa.
    if unsafe { GetMonitorInfoW(h, std::ptr::from_mut(&mut info).cast::<MONITORINFO>()) }.as_bool() {
        let r = info.monitorInfo.rcMonitor;
        let name = String::from_utf16_lossy(&info.szDevice).trim_end_matches('\0').to_string();
        list.push((info.monitorInfo.dwFlags & 1 != 0, format!("{name} {}x{}", r.right - r.left, r.bottom - r.top)));
    }
    TRUE
}

/// Monitor con il principale per primo (indice 0), poi gli altri nell'ordine di Windows.
pub fn monitors() -> Vec<Choice> {
    let mut list: Vec<(bool, String)> = Vec::new();
    // SAFETY: il puntatore a `list` resta valido finché EnumDisplayMonitors non ritorna.
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(monitor_cb), LPARAM(std::ptr::from_mut(&mut list) as isize)) };
    list.sort_by_key(|(primary, _)| !*primary);
    list.into_iter().enumerate().map(|(i, (_, label))| Choice { label: format!("{i}: {label}"), value: i.to_string() }).collect()
}

struct WindowInfo {
    title: String,
    exe: String,
}

unsafe extern "system" fn window_cb(hwnd: HWND, data: LPARAM) -> BOOL {
    // SAFETY: `data` è il puntatore al Vec passato da `open_windows()`.
    let list = unsafe { &mut *(data.0 as *mut Vec<WindowInfo>) };
    // SAFETY: chiamate FFI con handle ricevuto da EnumWindows e buffer locali di dimensione corretta.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }
        let mut buf = [0u16; 256];
        let n = usize::try_from(GetWindowTextW(hwnd, &mut buf)).unwrap_or(0);
        if n == 0 {
            return TRUE;
        }
        let title = String::from_utf16_lossy(&buf[..n]);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let mut exe = String::new();
        if let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let mut path = [0u16; 520];
            let mut len = path.len() as u32;
            if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut len).is_ok() {
                let full = String::from_utf16_lossy(&path[..len as usize]);
                exe = full.rsplit(['\\', '/']).next().unwrap_or_default().to_string();
            }
            let _ = CloseHandle(h);
        }
        list.push(WindowInfo { title, exe });
    }
    TRUE
}

fn open_windows() -> Vec<WindowInfo> {
    let mut list: Vec<WindowInfo> = Vec::new();
    // SAFETY: il puntatore a `list` resta valido finché EnumWindows non ritorna.
    let _ = unsafe { EnumWindows(Some(window_cb), LPARAM(std::ptr::from_mut(&mut list) as isize)) };
    list
}

/// Etichetta dell'elenco finestre: il titolo (che è il valore salvato) più l'eseguibile.
pub fn window_label(title: &str, exe: &str) -> String {
    if exe.is_empty() { title.to_string() } else { format!("{title}  ·  {exe}") }
}

/// Titoli delle finestre visibili.
pub fn window_titles() -> Vec<Choice> {
    let mut v: Vec<Choice> = open_windows()
        .into_iter()
        .map(|w| Choice { label: window_label(&w.title, &w.exe), value: w.title })
        .collect();
    v.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    v.dedup_by(|a, b| a.label == b.label);
    v
}

/// Nomi degli eseguibili con almeno una finestra visibile.
pub fn running_apps() -> Vec<Choice> {
    let mut v: Vec<Choice> = open_windows()
        .into_iter()
        .filter(|w| !w.exe.is_empty())
        .map(|w| Choice { label: w.exe.clone(), value: w.exe })
        .collect();
    v.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    v.dedup_by(|a, b| a.value.eq_ignore_ascii_case(&b.value));
    v
}

fn endpoints(flow: EDataFlow) -> windows::core::Result<Vec<Choice>> {
    // SAFETY: chiamate COM su interfacce ottenute qui; le stringhe restituite sono liberate da take_pwstr.
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let coll = en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..coll.GetCount()? {
            let dev = coll.Item(i)?;
            let id = take_pwstr(dev.GetId()?);
            let label = dev
                .OpenPropertyStore(STGM_READ)
                .and_then(|s| s.GetValue(&PKEY_Device_FriendlyName))
                .and_then(|pv| PropVariantToStringAlloc(&raw const pv))
                .map(take_pwstr)
                .unwrap_or_else(|_| id.clone());
            out.push(Choice { label, value: id });
        }
        Ok(out)
    }
}

/// Dispositivi d'ingresso attivi.
pub fn input_devices() -> Vec<Choice> {
    endpoints(eCapture).unwrap_or_default()
}

/// Dispositivi d'uscita attivi (per il loopback).
pub fn output_devices() -> Vec<Choice> {
    endpoints(eRender).unwrap_or_default()
}

/// Selettore di cartelle di Windows; `None` se annullato o in caso di errore.
pub fn pick_folder() -> Option<String> {
    // SAFETY: chiamate COM su un'interfaccia creata qui, sul thread della UI (STA).
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS).ok()?;
        dlg.Show(None).ok()?;
        let item = dlg.GetResult().ok()?;
        item.GetDisplayName(SIGDN_FILESYSPATH).ok().map(take_pwstr)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn window_label_shows_the_executable() {
        assert_eq!(super::window_label("Documento - Blocco note", "notepad.exe"), "Documento - Blocco note  ·  notepad.exe");
        assert_eq!(super::window_label("Senza exe", ""), "Senza exe");
        // Due finestre con lo stesso titolo si distinguono dall'eseguibile.
        assert_ne!(super::window_label("Nuova scheda", "chrome.exe"), super::window_label("Nuova scheda", "msedge.exe"));
    }

    #[test]
    fn helper_lists_work_on_this_machine() {
        super::init_com();
        let m = super::monitors();
        println!("monitor {m:?}
finestre {}
app {}
ingressi {:?}
uscite {:?}", super::window_titles().len(), super::running_apps().len(), super::input_devices(), super::output_devices());
        assert!(!m.is_empty());
        assert!(m[0].label.starts_with("0: "));
    }
}
