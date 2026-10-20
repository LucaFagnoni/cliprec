//! Desktop Duplication: fallback per i monitor (oltre 60 fps prima di Windows 11 24H2, o se WGC
//! fallisce). Il puntatore del mouse non viene disegnato.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use cliprec_core::time::qpc_frequency;
use cliprec_core::{GpuDevice, QpcTime, SourceError};
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::core::Interface;

use crate::pacer::{FrameSlots, Stats};

/// Duplicazione dell'uscita che mostra `monitor`, sull'adattatore del device (deve essere quello).
fn duplicate(device: &GpuDevice, monitor: HMONITOR) -> windows::core::Result<IDXGIOutputDuplication> {
    let dxgi: IDXGIDevice = device.device.cast()?;
    // SAFETY: chiamate COM su interfacce valide; EnumOutputs fallisce oltre l'ultima uscita.
    let adapter = unsafe { dxgi.GetAdapter()? };
    for i in 0.. {
        // SAFETY: vedi sopra.
        let output = unsafe { adapter.EnumOutputs(i)? };
        // SAFETY: vedi sopra.
        if unsafe { output.GetDesc()? }.Monitor != monitor {
            continue;
        }
        // DuplicateOutput1 converte in BGRA anche con HDR attivo, ma richiede un processo DPI-aware
        // per monitor: se fallisce si usa DuplicateOutput.
        if let Ok(o5) = output.cast::<IDXGIOutput5>() {
            // SAFETY: device valido; l'elenco dei formati vive per tutta la chiamata.
            if let Ok(d) = unsafe { o5.DuplicateOutput1(&device.device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) } {
                return Ok(d);
            }
        }
        // SAFETY: device valido.
        return unsafe { output.cast::<IDXGIOutput1>()?.DuplicateOutput(&device.device) };
    }
    Err(windows::Win32::Graphics::Dxgi::DXGI_ERROR_NOT_FOUND.into())
}

/// Thread che duplica un monitor e pubblica ogni nuova immagine del desktop negli slot del pacer.
/// Se la duplicazione si interrompe (desktop sicuro, cambio di modalità) il thread termina e
/// [`DdaCapture::is_closed`] diventa vero: la sorveglianza la ricrea.
pub(crate) struct DdaCapture {
    running: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DdaCapture {
    pub fn start(monitor: HMONITOR, slots: Arc<FrameSlots>, stats: Arc<Stats>) -> Result<Self, SourceError> {
        let running = Arc::new(AtomicBool::new(true));
        let closed = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let monitor_id = monitor.0 as isize;
        let (run, done) = (running.clone(), closed.clone());
        let thread = std::thread::Builder::new()
            .name("cliprec-dda".into())
            .spawn(move || {
                let monitor = HMONITOR(monitor_id as *mut _);
                let dup = match duplicate(&slots.device, monitor) {
                    Ok(d) => d,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                let _ = tx.send(Ok(()));
                if let Err(e) = pump(&dup, &slots, &stats, &run) {
                    tracing::info!("Desktop Duplication interrotta: {e}");
                }
                done.store(true, Ordering::Release);
            })
            .map_err(|e| SourceError::Failed(format!("thread Desktop Duplication: {e}")))?;
        let this = Self { running, closed, thread: Some(thread) };
        match rx.recv() {
            Ok(Ok(())) => Ok(this),
            Ok(Err(e)) => Err(SourceError::Failed(format!("Desktop Duplication: {e}"))),
            Err(_) => Err(SourceError::Failed("Desktop Duplication: thread terminato".into())),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

impl Drop for DdaCapture {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn pump(dup: &IDXGIOutputDuplication, slots: &FrameSlots, stats: &Stats, running: &AtomicBool) -> windows::core::Result<()> {
    while running.load(Ordering::Acquire) {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // SAFETY: puntatori d'uscita locali; ogni frame acquisito è rilasciato sotto.
        match unsafe { dup.AcquireNextFrame(100, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
            Err(e) => return Err(e),
        }
        // LastPresentTime 0 = solo il puntatore è cambiato: nessuna immagine nuova.
        let copied = match resource {
            Some(r) if info.LastPresentTime != 0 => r.cast::<ID3D11Texture2D>().and_then(|src| {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                // SAFETY: puntatore locale.
                unsafe { src.GetDesc(&mut desc) };
                if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                    return Err(windows::Win32::Graphics::Dxgi::DXGI_ERROR_UNSUPPORTED.into());
                }
                // LastPresentTime è in tick QPC grezzi.
                let captured = QpcTime::from_ticks(info.LastPresentTime, qpc_frequency());
                stats.arrival.record(QpcTime::now().hns_since(captured));
                slots.publish(desc.Width, desc.Height, captured, |dst| {
                    // SAFETY: stesse dimensioni e formato BGRA; contesto immediato protetto.
                    unsafe { slots.device.context.CopyResource(dst, &src) };
                    Ok(())
                })
            }),
            _ => Ok(()),
        };
        // SAFETY: c'è un frame acquisito da rilasciare.
        unsafe { dup.ReleaseFrame()? };
        copied?;
        if info.LastPresentTime != 0 {
            stats.arrived.fetch_add(1, Ordering::Relaxed);
        }
    }
    Ok(())
}
