//! Sessione Windows.Graphics.Capture: frame pool free-threaded a 2 buffer sul device condiviso;
//! ogni frame arrivato viene copiato in uno slot del pacer e rilasciato subito.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cliprec_core::{GpuDevice, HNS_PER_SEC};
use windows::Foundation::Metadata::ApiInformation;
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, D3D11_TEXTURE2D_DESC, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{HSTRING, IInspectable, Interface};

use crate::pacer::{FrameSlots, Stats};

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

impl WgcCapture {
    pub fn start(
        item: GraphicsCaptureItem,
        slots: Arc<FrameSlots>,
        stats: Arc<Stats>,
        fps: u32,
        cursor: bool,
    ) -> windows::core::Result<Self> {
        // Nessun frame nero qui: finché WGC non consegna, il pacer ripete l'ultimo frame (niente
        // lampi neri quando la modalità automatica passa dal monitor al gioco e viceversa).
        let device = winrt_device(&slots.device)?;
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&device, FORMAT, BUFFERS, size)?;
        let pool_size = AtomicU64::new(pack(size));
        let arrived_token = pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
            move |pool, _| {
                if let Some(pool) = pool.as_ref()
                    && let Err(e) = on_frame_arrived(pool, &slots, &stats, &pool_size)
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
        // Senza RequestAccessAsync può essere rifiutato: il bordo giallo resta, non è un errore.
        if property_present("IsBorderRequired")
            && let Err(e) = session.SetIsBorderRequired(false)
        {
            tracing::debug!("bordo giallo non disattivato: {e}");
        }
        if has_min_update_interval() {
            // Mai il default: su 24H2 a 144 Hz il default può fermarsi intorno ai 50 fps.
            let interval = (HNS_PER_SEC / (2 * i64::from(fps.max(1)))).max(10_000);
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

fn on_frame_arrived(
    pool: &Direct3D11CaptureFramePool,
    slots: &FrameSlots,
    stats: &Stats,
    pool_size: &AtomicU64,
) -> windows::core::Result<()> {
    let frame = pool.TryGetNextFrame()?;
    let content = frame.ContentSize()?;
    let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
    // SAFETY: la superficie di un frame WGC è sempre una ID3D11Texture2D del nostro device.
    let src: ID3D11Texture2D = unsafe { access.GetInterface()? };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: puntatore locale.
    unsafe { src.GetDesc(&mut desc) };
    // Dopo un ridimensionamento il contenuto può superare la superficie finché il pool non è ricreato.
    let width = (content.Width.max(1) as u32).min(desc.Width);
    let height = (content.Height.max(1) as u32).min(desc.Height);
    slots.publish(width, height, |dst| {
        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: width, bottom: height, back: 1 };
        // SAFETY: regione dentro entrambe le texture (stesso formato BGRA); contesto immediato protetto.
        unsafe { slots.device.context.CopySubresourceRegion(dst, 0, 0, 0, 0, &src, 0, Some(&region)) };
        Ok(())
    })?;
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
