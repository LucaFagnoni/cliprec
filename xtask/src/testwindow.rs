//! `cargo xtask testwindow [--seconds N]`: finestra GDI 640x360 (mai a schermo intero, nessun suono)
//! con il timecode QPC e un lampo bianco di 50 ms a ogni secondo intero di QPC, da catturare con la
//! cattura di finestra. Il lampo comincia nel primo disegno dopo il secondo intero (ritardo < 4 ms).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC, SetBkMode, SetTextColor, TextOutW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, IsWindow, PM_REMOVE,
    PeekMessageW, PostQuitMessage, RegisterClassW, SW_SHOWNOACTIVATE, ShowWindow, TranslateMessage, WM_DESTROY,
    WM_ERASEBKGND, WM_QUIT, WNDCLASSW, WS_CAPTION, WS_EX_NOACTIVATE, WS_SYSMENU, MSG,
};
use windows::core::w;

const FLASH_MS: f64 = 50.0;

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_DESTROY => {
            // SAFETY: chiamata senza puntatori, dal thread proprietario della finestra.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        // SAFETY: inoltra gli stessi argomenti ricevuti dal sistema.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn qpc() -> Result<i64> {
    let mut t = 0i64;
    // SAFETY: puntatore a una variabile locale valida.
    unsafe { QueryPerformanceCounter(&mut t)? };
    Ok(t)
}

pub fn run(seconds: Option<f64>) -> Result<()> {
    let mut freq = 0i64;
    // SAFETY: puntatore a una variabile locale valida.
    unsafe { QueryPerformanceFrequency(&mut freq)? };
    let freq = freq as f64;
    // SAFETY: tutte le chiamate Win32 sotto usano handle creati qui, sullo stesso thread.
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: w!("ClipRecTestWindow"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            anyhow::bail!("RegisterClassW fallita");
        }
        let style = WS_CAPTION | WS_SYSMENU;
        let mut rc = RECT { left: 0, top: 0, right: 640, bottom: 360 };
        AdjustWindowRect(&mut rc, style, false)?;
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE,
            w!("ClipRecTestWindow"),
            w!("ClipRec testwindow"),
            style,
            200,
            200,
            rc.right - rc.left,
            rc.bottom - rc.top,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .context("CreateWindowExW")?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        let white = CreateSolidBrush(COLORREF(0x00FF_FFFF));
        let dark = CreateSolidBrush(COLORREF(0x0010_1010));
        let start = Instant::now();
        let mut msg = MSG::default();
        'outer: loop {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'outer;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if !IsWindow(Some(hwnd)).as_bool() || seconds.is_some_and(|s| start.elapsed().as_secs_f64() > s) {
                break;
            }
            let now = qpc()? as f64 / freq;
            let flash = (now.fract() * 1000.0) < FLASH_MS;
            let mut client = RECT::default();
            GetClientRect(hwnd, &mut client)?;
            let hdc = GetDC(Some(hwnd));
            FillRect(hdc, &client, if flash { white } else { dark });
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(if flash { 0 } else { 0x00FF_FFFF }));
            let text: Vec<u16> = format!("QPC {now:.3} s").encode_utf16().collect();
            let _ = TextOutW(hdc, 20, 20, &text);
            ReleaseDC(Some(hwnd), hdc);
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = DeleteObject(white.into());
        let _ = DeleteObject(dark.into());
    }
    Ok(())
}
