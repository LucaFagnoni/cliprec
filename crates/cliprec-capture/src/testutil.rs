//! Aiuti per i test: lettura su CPU di texture sintetiche (mai dello schermo dell'utente).

use cliprec_core::GpuDevice;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
    SHOW_WINDOW_CMD, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetTimer, SetWindowPos, ShowWindow,
    ShowWindowAsync, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WM_PAINT, WM_TIMER, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, w};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

/// Piccola finestra di prova (non a schermo intero, non attivata) disegnata con GDI che cambia
/// colore ogni 15 ms; si chiude al drop.
pub struct TestWindow {
    hwnd: isize,
    thread: Option<std::thread::JoinHandle<()>>,
}

static TICKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

unsafe extern "system" fn test_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: chiamate Win32 sulla finestra che riceve il messaggio, dal suo thread.
    unsafe {
        match msg {
            WM_TIMER => {
                TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let t = TICKS.load(std::sync::atomic::Ordering::Relaxed);
                let brush = CreateSolidBrush(COLORREF((t * 37 % 256) | ((t * 11 % 256) << 8) | (0x40 << 16)));
                FillRect(hdc, &ps.rcPaint, brush);
                let _ = DeleteObject(brush.into());
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

impl TestWindow {
    pub fn open(title: &str, width: i32, height: i32) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let title = HSTRING::from(title);
        let thread = std::thread::spawn(move || {
            // SAFETY: registrazione della classe e creazione della finestra con stringhe che vivono
            // per tutta la chiamata; il ciclo dei messaggi gira sul thread che possiede la finestra.
            unsafe {
                let instance = GetModuleHandleW(None).unwrap();
                let class = w!("cliprec-test-window");
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(test_wndproc),
                    hInstance: instance.into(),
                    lpszClassName: class,
                    ..Default::default()
                };
                RegisterClassW(&wc); // 0 se già registrata: va bene
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    class,
                    &title,
                    WS_OVERLAPPEDWINDOW,
                    40,
                    40,
                    width,
                    height,
                    None,
                    None,
                    Some(instance.into()),
                    None,
                )
                .unwrap();
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                SetTimer(Some(hwnd), 1, 15, None);
                tx.send(hwnd.0 as isize).unwrap();
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        });
        let hwnd = rx.recv().unwrap();
        Self { hwnd, thread: Some(thread) }
    }

    pub fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut _)
    }

    pub fn resize(&self, width: i32, height: i32) {
        // SAFETY: finestra valida finché vive `self`.
        unsafe {
            SetWindowPos(self.hwnd(), None, 0, 0, width, height, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE).unwrap();
        }
    }

    pub fn show(&self, cmd: SHOW_WINDOW_CMD) {
        // SAFETY: finestra valida finché vive `self`.
        let _ = unsafe { ShowWindowAsync(self.hwnd(), cmd) };
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        // SAFETY: finestra valida; WM_CLOSE la distrugge e termina il ciclo dei messaggi.
        let _ = unsafe { PostMessageW(Some(self.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub struct Readback {
    device: GpuDevice,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
}

// SAFETY: solo test; il contesto immediato è protetto per l'uso da più thread.
unsafe impl Send for Readback {}

pub struct Pixels {
    data: Vec<u8>,
    pitch: usize,
}

impl Pixels {
    /// Byte BGRA del pixel (x, y).
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let o = y as usize * self.pitch + x as usize * 4;
        [self.data[o], self.data[o + 1], self.data[o + 2], self.data[o + 3]]
    }
}

impl Readback {
    pub fn new(device: &GpuDevice, width: u32, height: u32) -> Self {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        // SAFETY: descrittore e puntatore d'uscita locali.
        unsafe { device.device.CreateTexture2D(&desc, None, Some(&mut tex)).unwrap() };
        Self { device: device.clone(), staging: tex.unwrap(), width, height }
    }

    /// Copia `tex` (stessa dimensione) e ne legge i pixel.
    pub fn read(&mut self, tex: &ID3D11Texture2D) -> Pixels {
        let ctx = &self.device.context;
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: texture valide della stessa dimensione e formato; la mappatura è letta entro i
        // limiti RowPitch × altezza e chiusa con Unmap prima di tornare.
        unsafe {
            ctx.CopyResource(&self.staging, tex);
            ctx.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut map)).unwrap();
            let pitch = map.RowPitch as usize;
            let len = pitch * self.height as usize;
            let data = std::slice::from_raw_parts(map.pData as *const u8, len).to_vec();
            ctx.Unmap(&self.staging, 0);
            assert!(pitch >= self.width as usize * 4);
            Pixels { data, pitch }
        }
    }
}
