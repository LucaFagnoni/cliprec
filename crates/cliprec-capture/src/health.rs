//! Rilevamento di frame neri o fermi a lungo, tipici del fullscreen esclusivo che WGC e DDA non
//! catturano. Una volta al secondo copia 16 pixel dell'ultimo frame in una texture di staging 16×1
//! e legge la copia del giro precedente senza attendere la GPU (64 byte, fuori dal percorso caldo).

use std::time::{Duration, Instant};

use cliprec_core::GpuDevice;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use crate::pacer::FrameSlots;

pub(crate) const BLACK_AFTER: Duration = Duration::from_secs(3);
pub(crate) const FROZEN_AFTER: Duration = Duration::from_secs(5);
const PROBE_EVERY: Duration = Duration::from_secs(1);
pub(crate) const BLACK_MSG: &str =
    "frame neri da alcuni secondi: se è un gioco a schermo intero esclusivo, usa la modalità finestra senza bordi";
pub(crate) const FROZEN_MSG: &str =
    "nessun frame nuovo da alcuni secondi: se è un gioco a schermo intero esclusivo, usa la modalità finestra senza bordi";

const GRID: u32 = 4;
const SAMPLES: u32 = GRID * GRID;
/// Canale massimo (0..255) perché un pixel conti come nero.
const BLACK_LEVEL: u8 = 8;

pub(crate) struct Health {
    probe: Option<Probe>,
    last_probe: Option<Instant>,
    black_since: Option<Instant>,
    last_arrived: u64,
    arrived_at: Instant,
}

impl Health {
    pub fn new(device: &GpuDevice) -> Self {
        let probe = Probe::new(device).map_err(|e| tracing::warn!("rilevamento frame neri non disponibile: {e}")).ok();
        Self { probe, last_probe: None, black_since: None, last_arrived: 0, arrived_at: Instant::now() }
    }

    /// Da chiamare quando la cattura cambia o riparte (finestra ripristinata, nuova sessione).
    pub fn reset(&mut self, now: Instant) {
        self.black_since = None;
        self.arrived_at = now;
    }

    /// Frame neri all'ultima sonda o nessun frame nuovo da `after`: con il segnale di Windows sullo
    /// schermo intero esclusivo bastano tempi molto più corti delle soglie degli avvisi.
    pub fn failing(&self, now: Instant, after: Duration) -> bool {
        self.black_since.is_some() || now.duration_since(self.arrived_at) >= after
    }

    /// Avviso da mostrare, se c'è. `watch_frozen` va dato solo per una finestra in primo piano: un
    /// desktop fermo non produce frame nuovi da 24H2 ed è normale.
    pub fn check(&mut self, slots: &FrameSlots, arrived: u64, watch_frozen: bool, now: Instant) -> Option<&'static str> {
        if self.last_probe.is_none_or(|t| now.duration_since(t) >= PROBE_EVERY) {
            self.last_probe = Some(now);
            if let Some(black) = self.probe.as_mut().and_then(|p| p.sample(slots)) {
                self.black_since = if black { Some(self.black_since.unwrap_or(now)) } else { None };
            }
        }
        if arrived != self.last_arrived {
            self.last_arrived = arrived;
            self.arrived_at = now;
        }
        verdict(now, self.black_since, self.arrived_at, watch_frozen)
    }
}

fn verdict(now: Instant, black_since: Option<Instant>, arrived_at: Instant, watch_frozen: bool) -> Option<&'static str> {
    if black_since.is_some_and(|t| now.duration_since(t) >= BLACK_AFTER) {
        Some(BLACK_MSG)
    } else if watch_frozen && now.duration_since(arrived_at) >= FROZEN_AFTER {
        Some(FROZEN_MSG)
    } else {
        None
    }
}

struct Probe {
    device: GpuDevice,
    staging: ID3D11Texture2D,
    pending: bool,
}

impl Probe {
    fn new(device: &GpuDevice) -> windows::core::Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: SAMPLES,
            Height: 1,
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
        unsafe { device.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        let staging = tex.ok_or(windows::core::Error::from(windows::Win32::Foundation::E_POINTER))?;
        Ok(Self { device: device.clone(), staging, pending: false })
    }

    /// Legge la copia precedente (se la GPU l'ha finita) e accoda la prossima.
    /// `Some(true)` = tutti i campioni neri.
    fn sample(&mut self, slots: &FrameSlots) -> Option<bool> {
        let ctx = &self.device.context;
        let mut result = None;
        if self.pending {
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            // SAFETY: texture di staging valida; DO_NOT_WAIT restituisce errore invece di bloccare.
            let mapped = unsafe {
                ctx.Map(&self.staging, 0, D3D11_MAP_READ, D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32, Some(&mut map))
            };
            if mapped.is_err() {
                return None; // copia ancora in corso: si riprova al prossimo giro
            }
            // SAFETY: la riga mappata contiene SAMPLES pixel BGRA (4 byte); Unmap subito dopo la lettura.
            let black = unsafe {
                let px = std::slice::from_raw_parts(map.pData as *const u8, (SAMPLES * 4) as usize);
                let black = px.chunks_exact(4).all(|p| p[..3].iter().all(|&c| c <= BLACK_LEVEL));
                ctx.Unmap(&self.staging, 0);
                black
            };
            result = Some(black);
            self.pending = false;
        }
        let queued = slots.with_latest(|tex, w, h| {
            for i in 0..SAMPLES {
                let x = (2 * (i % GRID) + 1) * w / (2 * GRID);
                let y = (2 * (i / GRID) + 1) * h / (2 * GRID);
                let region = D3D11_BOX { left: x, top: y, front: 0, right: x + 1, bottom: y + 1, back: 1 };
                // SAFETY: (x, y) è dentro la texture (x < w, y < h); stesso formato; contesto protetto.
                unsafe { ctx.CopySubresourceRegion(&self.staging, 0, i, 0, 0, tex, 0, Some(&region)) };
            }
        });
        self.pending = queued.is_some();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_needs_time() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_secs(n);
        assert_eq!(verdict(s(2), Some(t0), t0, false), None);
        assert_eq!(verdict(s(3), Some(t0), t0, false), Some(BLACK_MSG));
        assert_eq!(verdict(s(4), None, t0, false), None);
        assert_eq!(verdict(s(4), None, t0, true), None);
        assert_eq!(verdict(s(5), None, t0, true), Some(FROZEN_MSG));
        assert_eq!(verdict(s(9), None, s(5), true), None);
    }
}
