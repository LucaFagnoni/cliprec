//! Raw Input: registrazione di tastiera e mouse in modalità passiva (`RIDEV_INPUTSINK`) e
//! decodifica di `WM_INPUT` negli eventi di [`crate::hotkey`].

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MBUTTON, VK_MENU, VK_RWIN, VK_SHIFT, VK_XBUTTON1, VK_XBUTTON2,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEKEYBOARD,
    RIM_TYPEMOUSE, RegisterRawInputDevices,
};

use crate::hotkey::{InputEvent, Mods, Trigger, mouse_events};

const RI_KEY_BREAK: u16 = 1;
const RI_KEY_E0: u16 = 2;

/// Registra la tastiera (1/6) e, solo se `mouse`, il mouse (1/2) sulla finestra nascosta, con
/// `RIDEV_INPUTSINK`: gli eventi arrivano anche quando un altro programma ha il focus, senza
/// sottrarli a nessuno. Con `mouse == false` toglie un'eventuale registrazione precedente del mouse
/// (si richiama alla ricarica della configurazione).
pub fn register(hwnd: HWND, mouse: bool) -> anyhow::Result<()> {
    let dev = |usage| RAWINPUTDEVICE { usUsagePage: 1, usUsage: usage, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
    let mouse_dev = if mouse {
        dev(2)
    } else {
        RAWINPUTDEVICE { usUsagePage: 1, usUsage: 2, dwFlags: RIDEV_REMOVE, hwndTarget: HWND::default() }
    };
    let devices = [dev(6), mouse_dev];
    // SAFETY: `devices` è un array valido della dimensione dichiarata.
    unsafe { RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32) }?;
    Ok(())
}

/// Decodifica un `WM_INPUT`. Vuoto se il messaggio non è leggibile o non ci interessa.
pub fn decode(lparam: LPARAM) -> Vec<InputEvent> {
    let mut raw = RAWINPUT::default();
    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
    // SAFETY: `raw` è un RAWINPUT della dimensione indicata in `size`; l'handle viene da WM_INPUT.
    let n = unsafe {
        GetRawInputData(
            HRAWINPUT(lparam.0 as *mut _),
            RID_INPUT,
            Some((&mut raw as *mut RAWINPUT).cast()),
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if n == u32::MAX || n == 0 {
        return Vec::new();
    }
    if raw.header.dwType == RIM_TYPEKEYBOARD.0 {
        // SAFETY: dwType indica che il membro valido dell'unione è `keyboard`.
        let kb = unsafe { raw.data.keyboard };
        if kb.VKey == 0xFF {
            return Vec::new(); // overrun / tasto fittizio
        }
        vec![InputEvent::Key { vk: kb.VKey, e0: kb.Flags & RI_KEY_E0 != 0, down: kb.Flags & RI_KEY_BREAK == 0 }]
    } else if raw.header.dwType == RIM_TYPEMOUSE.0 {
        // SAFETY: dwType indica che il membro valido dell'unione è `mouse`.
        let flags = unsafe { raw.data.mouse.Anonymous.Anonymous.usButtonFlags };
        mouse_events(flags)
    } else {
        Vec::new()
    }
}

fn async_down(vk: u16) -> bool {
    // SAFETY: GetAsyncKeyState non ha precondizioni.
    let state = unsafe { GetAsyncKeyState(i32::from(vk)) };
    state < 0
}

/// Modificatori realmente premuti adesso (per riallineare lo stato se un «su» è andato perso).
pub fn async_mods() -> Mods {
    Mods {
        ctrl: async_down(VK_CONTROL.0),
        alt: async_down(VK_MENU.0),
        shift: async_down(VK_SHIFT.0),
        win: async_down(VK_LWIN.0) || async_down(VK_RWIN.0),
    }
}

/// Il trigger è fisicamente premuto adesso?
pub fn async_trigger_down(t: Trigger) -> bool {
    match t {
        Trigger::Key(vk) => async_down(vk),
        Trigger::Mouse(3) => async_down(VK_MBUTTON.0),
        Trigger::Mouse(4) => async_down(VK_XBUTTON1.0),
        Trigger::Mouse(_) => async_down(VK_XBUTTON2.0),
    }
}
