//! Sessione Windows.Graphics.Capture: frame pool free-threaded a 2 buffer sul device condiviso;
//! ogni frame arrivato viene copiato in uno slot del pacer e rilasciato subito.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cliprec_core::{GpuDevice, HNS_PER_SEC, QpcTime};
use windows::Foundation::Metadata::ApiInformation;
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, D3D11_TEXTURE2D_DESC, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{HSTRING, IInspectable, Interface};

use crate::pacer::{FrameSlots, Stats};
use crate::targets::{is_minimized, window_rect};

const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
const BUFFERS: i32 = 2;
const SESSION_CLASS: &str = "Windows.Graphics.Capture.GraphicsCaptureSession";

/// `true` se il sistema supporta Windows.Graphics.Capture.
pub(crate) fn is_supported() -> bool {
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

/// `true` se `GraphicsCaptureSession.MinUpdateInterval` esiste (Windows 11 24H2, build 26100+).
pub(crate) fn has_min_update_interval() -> bool {
    property_present("MinUpdateInterval")
}

/// Una volta per processo: chiede a Windows il permesso di catturare senza il bordo giallo. Per
/// un'app desktop non mostra dialoghi; nei test e in modalità headless non lo si chiede.
fn borderless_allowed() -> bool {
    use windows::Graphics::Capture::{GraphicsCaptureAccess, GraphicsCaptureAccessKind};
    use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;
    static ALLOWED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ALLOWED.get_or_init(|| {
        if cfg!(test) || std::env::var_os("CLIPREC_HEADLESS").is_some_and(|v| v != "0") {
            return false;
        }
        let status = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless).and_then(|op| op.join());
        match status {
            Ok(s) if s == AppCapabilityAccessStatus::Allowed => true,
            Ok(s) => {
                tracing::warn!("Windows non concede la cattura senza bordo giallo: {s:?}");
                false
            }
            Err(e) => {
                tracing::warn!("permesso senza bordo giallo non disponibile: {e}");
                false
            }
        }
    })
}

fn property_present(name: &str) -> bool {
    ApiInformation::IsPropertyPresent(&HSTRING::from(SESSION_CLASS), &HSTRING::from(name)).unwrap_or(false)
}

fn interop() -> windows::core::Result<IGraphicsCaptureItemInterop> {
    windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
}

pub(crate) fn item_for_monitor(h: HMONITOR) -> windows::core::Result<GraphicsCaptureItem> {
    // SAFETY: HMONITOR ottenuto da EnumDisplayMonitors/MonitorFrom*; un handle non valido dà errore.
    unsafe { interop()?.CreateForMonitor(h) }
}

pub(crate) fn item_for_window(h: HWND) -> windows::core::Result<GraphicsCaptureItem> {
    // SAFETY: un HWND non valido dà errore, non comportamento indefinito.
    unsafe { interop()?.CreateForWindow(h) }
}

fn winrt_device(device: &GpuDevice) -> windows::core::Result<IDirect3DDevice> {
    let dxgi: IDXGIDevice = device.device.cast()?;
    // SAFETY: device DXGI valido; la funzione restituisce un nuovo riferimento WinRT.
    unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()
}

fn pack(s: SizeInt32) -> u64 {
    (u64::from(s.Width as u32) << 32) | u64::from(s.Height as u32)
}

/// Una sessione WGC attiva. Il drop la chiude.
pub(crate) struct WgcCapture {
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    arrived_token: i64,
    closed_token: i64,
    closed: Arc<AtomicBool>,
}

/// `MinUpdateInterval` per `fps` in uscita: tre quarti di periodo, almeno 1 ms. Un periodo intero
/// perde frame quando il monitor va alla stessa frequenza (misurato: a 60 fps su 60 Hz ~50 frame
/// nuovi/s invece di 60, perché le composizioni di DWM oscillano attorno ai 16,7 ms); con 3/4 si
/// evitano comunque le catture e le copie doppie su monitor a 120-240 Hz.
pub(crate) fn min_update_interval(fps: u32) -> i64 {
    (HNS_PER_SEC * 3 / (4 * i64::from(fps.max(1)))).max(10_000)
}

impl WgcCapture {
    /// `window` è la finestra catturata, se l'elemento è una finestra: mentre è minimizzata i suoi
    /// frame (la sola barra del titolo, ~146×28) vengono scartati e resta l'ultimo frame buono.
    pub fn start(
        item: GraphicsCaptureItem,
        window: Option<HWND>,
        slots: Arc<FrameSlots>,
        stats: Arc<Stats>,
        fps: u32,
        cursor: bool,
    ) -> windows::core::Result<Self> {
        let window = window.map(|h| h.0 as isize);
        // Nessun frame nero qui: finché WGC non consegna, il pacer ripete l'ultimo frame (niente
        // lampi neri quando la modalità automatica passa dal monitor al gioco e viceversa).
        let device = winrt_device(&slots.device)?;
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&device, FORMAT, BUFFERS, size)?;
        let pool_size = AtomicU64::new(pack(size));
        let arrived_token = pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
            move |pool, _| {
                if let Some(pool) = pool.as_ref()
                    && let Err(e) = on_frame_arrived(pool, window, &slots, &stats, &pool_size)
                {
                    tracing::debug!("frame WGC scartato: {e}");
                }
                Ok(())
            },
        ))?;
        let closed = Arc::new(AtomicBool::new(false));
        let flag = closed.clone();
        let closed_token = item.Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            flag.store(true, Ordering::Release);
            Ok(())
        }))?;
        let session = pool.CreateCaptureSession(&item)?;
        if let Err(e) = session.SetIsCursorCaptureEnabled(cursor) {
            tracing::debug!("IsCursorCaptureEnabled non disponibile: {e}");
        }
        // Il bordo giallo si toglie solo dopo il permesso «senza bordo», chiesto una volta (m16).
        if property_present("IsBorderRequired")
            && borderless_allowed()
            && let Err(e) = session.SetIsBorderRequired(false)
        {
            tracing::warn!("bordo giallo non disattivato: {e}");
        }
        if has_min_update_interval() {
            // Mai il default: su 24H2 a 144 Hz il default può fermarsi intorno ai 50 fps.
            let interval = min_update_interval(fps);
            if let Err(e) = session.SetMinUpdateInterval(TimeSpan { Duration: interval }) {
                tracing::warn!("MinUpdateInterval non impostato: {e}");
            }
        }
        session.StartCapture()?;
        Ok(Self { item, pool, session, arrived_token, closed_token, closed })
    }

    /// `true` se l'elemento catturato non esiste più (finestra chiusa, monitor scollegato).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.arrived_token);
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// `true` se il contenuto è meno di metà della finestra in larghezza o in altezza: non è la finestra
/// com'è adesso (frame residuo della riduzione a icona, o un ingrandimento brusco di cui arriverà
/// subito il frame giusto). I bordi invisibili e il DPI rendono le due misure solo simili.
fn is_stale(content: SizeInt32, window: RECT) -> bool {
    let (ww, wh) = (window.right - window.left, window.bottom - window.top);
    2 * content.Width < ww || 2 * content.Height < wh
}

fn on_frame_arrived(
    pool: &Direct3D11CaptureFramePool,
    window: Option<isize>,
    slots: &FrameSlots,
    stats: &Stats,
    pool_size: &AtomicU64,
) -> windows::core::Result<()> {
    let frame = pool.TryGetNextFrame()?;
    let content = frame.ContentSize()?;
    // Finestra minimizzata, o frame della finestra minimizzata (~146×28) che arriva quando
    // IsIconic è già tornato falso (ripristino) o non lo è ancora (inizio della riduzione): si
    // scarta e resta l'ultimo frame buono.
    if let Some(h) = window {
        let hwnd = HWND(h as *mut _);
        if is_minimized(hwnd) || window_rect(hwnd).is_some_and(|r| is_stale(content, r)) {
            return frame.Close();
        }
    }
    // Stesso orologio e stessa unità di QpcTime (QPC in 100 ns).
    let captured = QpcTime(frame.SystemRelativeTime()?.Duration);
    stats.arrival.record(QpcTime::now().hns_since(captured));
    let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
    // SAFETY: la superficie di un frame WGC è sempre una ID3D11Texture2D del nostro device.
    let src: ID3D11Texture2D = unsafe { access.GetInterface()? };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: puntatore locale.
    unsafe { src.GetDesc(&mut desc) };
    // Se il contenuto supera la superficie (finestra ingrandita o ripristinata, pool non ancora
    // ricreato) il frame sarebbe un ritaglio, magari della dimensione minimizzata: si scarta e si
    // aspetta il primo frame del pool ricreato.
    let width = content.Width.max(1) as u32;
    let height = content.Height.max(1) as u32;
    if width <= desc.Width && height <= desc.Height {
        slots.publish(width, height, captured, |dst| {
            let region = D3D11_BOX { left: 0, top: 0, front: 0, right: width, bottom: height, back: 1 };
            // SAFETY: regione dentro entrambe le texture (stesso formato BGRA); contesto immediato protetto.
            unsafe { slots.device.context.CopySubresourceRegion(dst, 0, 0, 0, 0, &src, 0, Some(&region)) };
            Ok(())
        })?;
    }
    // Nessun riferimento alla superficie oltre il rilascio del frame.
    drop(src);
    drop(access);
    frame.Close()?;
    stats.arrived.fetch_add(1, Ordering::Relaxed);
    if content.Width > 0 && content.Height > 0 && pack(content) != pool_size.load(Ordering::Relaxed) {
        // Raro (ridimensionamento): ricrea il device WinRT invece di tenerlo nel gestore.
        pool.Recreate(&winrt_device(&slots.device)?, FORMAT, BUFFERS, content)?;
        pool_size.store(pack(content), Ordering::Relaxed);
        tracing::debug!(w = content.Width, h = content.Height, "frame pool WGC ricreato");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimized_leftover_frames_are_stale() {
        let window = |w, h| RECT { left: 40, top: 40, right: 40 + w, bottom: 40 + h };
        let size = |w, h| SizeInt32 { Width: w, Height: h };
        // Frame della finestra minimizzata arrivato dopo il ripristino.
        assert!(is_stale(size(146, 28), window(480, 360)));
        // Contenuto con i bordi invisibili tolti, o in ritardo di un frame su un ridimensionamento.
        assert!(!is_stale(size(306, 233), window(320, 240)));
        assert!(!is_stale(size(466, 353), window(320, 240)));
        assert!(!is_stale(size(306, 233), window(480, 360)));
    }
}
