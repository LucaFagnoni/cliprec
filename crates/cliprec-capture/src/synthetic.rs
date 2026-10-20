//! Sorgente sintetica per test e misura della sincronia: sfondo scuro, lampo bianco a tutto schermo
//! nel primo frame con pts pari o successivo a ogni secondo intero di QPC, indice del frame in
//! binario (32 blocchi in alto a sinistra, il bit più significativo per primo). Niente shader:
//! solo ClearRenderTargetView e ClearView su rettangoli.

use std::sync::Arc;

use cliprec_core::{CaptureStats, FrameCallback, GpuDevice, HNS_PER_SEC, QpcTime, SourceError, SourceState, VideoSource};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{ID3D11DeviceContext1, ID3D11RenderTargetView, ID3D11Texture2D};
use windows::core::Interface;

use crate::pacer::{FrameSlots, Pacer, PacerOptions, Stats, create_bgra, create_rtv};

/// Bit del contatore disegnati.
pub const COUNTER_BITS: usize = 32;
/// Sfondo (RGBA lineare 0..1).
pub const BACKGROUND: [f32; 4] = [0.08, 0.08, 0.1, 1.0];
pub const FLASH: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
/// Blocco del contatore per un bit a 1 (verde) e a 0 (grigio).
pub const BIT_ON: [f32; 4] = [0.0, 0.9, 0.3, 1.0];
pub const BIT_OFF: [f32; 4] = [0.3, 0.3, 0.3, 1.0];

/// `true` se tra `prev` (escluso) e `pts` (incluso) cade un secondo intero di QPC: è il frame del lampo.
pub fn is_flash(prev: QpcTime, pts: QpcTime) -> bool {
    pts.0.div_euclid(HNS_PER_SEC) != prev.0.div_euclid(HNS_PER_SEC)
}

/// Lato in pixel di un blocco del contatore per una texture larga `width`.
pub fn counter_block(width: u32) -> u32 {
    (width / (COUNTER_BITS as u32 + 8)).max(1)
}

/// Rettangolo del bit `bit` (0 = il più significativo): riga in alto, a un blocco dal bordo.
pub fn counter_rect(width: u32, bit: usize) -> RECT {
    let b = counter_block(width) as i32;
    let left = b * (1 + bit as i32);
    let gap = i32::from(b >= 4);
    RECT { left, top: b, right: left + b - gap, bottom: 2 * b - gap }
}

pub struct SyntheticSource {
    device: GpuDevice,
    width: u32,
    height: u32,
    stats: Arc<Stats>,
    pacer: Option<Pacer>,
}

impl SyntheticSource {
    /// Sorgente `width`×`height` sul device dato. Il primo frame è disegnato al primo tick.
    pub fn new(device: GpuDevice, width: u32, height: u32) -> Self {
        Self { device, width: width.max(1), height: height.max(1), stats: Arc::default(), pacer: None }
    }
}

impl VideoSource for SyntheticSource {
    fn start(&mut self, fps: u32, on_frame: FrameCallback) -> Result<(), SourceError> {
        self.stop();
        let mut painter = Painter::new(&self.device, self.width, self.height, fps)?;
        let stats = self.stats.clone();
        let prepare = Box::new(move |slots: &FrameSlots, pts: QpcTime, index: u64| {
            if let Err(e) = painter.paint(slots, pts, index)
                && !painter.failed
            {
                painter.failed = true;
                stats.set_state(SourceState::Error(format!("disegno del frame sintetico: {e}")));
            }
        });
        let slots = Arc::new(FrameSlots::new(self.device.clone()));
        // Il contenuto è disegnato esattamente per il pts del tick: nessuna attesa né spostamento.
        let options = PacerOptions { prepare: Some(prepare), ..Default::default() };
        self.pacer = Some(Pacer::start(fps, slots, self.stats.clone(), options, on_frame)?);
        self.stats.set_state(SourceState::Running);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut p) = self.pacer.take() {
            p.stop();
            self.stats.set_state(SourceState::Stopped);
        }
    }

    fn stats(&self) -> CaptureStats {
        self.stats.snapshot()
    }

    fn describe(&self) -> String {
        format!("Sorgente sintetica ({}×{})", self.width, self.height)
    }

    fn native_size(&self) -> Option<(u32, u32)> {
        Some((self.width, self.height))
    }
}

impl Drop for SyntheticSource {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Painter {
    ctx: ID3D11DeviceContext1,
    canvas: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    width: u32,
    height: u32,
    step: i64,
    prev: Option<QpcTime>,
    ones: [RECT; COUNTER_BITS],
    zeros: [RECT; COUNTER_BITS],
    failed: bool,
}

// SAFETY: oggetti D3D11 free-threaded; il contesto immediato è protetto da SetMultithreadProtected.
// Il Painter è usato da un solo thread alla volta (il pacer).
unsafe impl Send for Painter {}

impl Painter {
    fn new(device: &GpuDevice, width: u32, height: u32, fps: u32) -> windows::core::Result<Self> {
        let canvas = create_bgra(device, width, height)?;
        let rtv = create_rtv(device, &canvas)?;
        Ok(Self {
            ctx: device.context.cast()?,
            canvas,
            rtv,
            width,
            height,
            step: HNS_PER_SEC / i64::from(fps.max(1)),
            prev: None,
            ones: [RECT::default(); COUNTER_BITS],
            zeros: [RECT::default(); COUNTER_BITS],
            failed: false,
        })
    }

    fn paint(&mut self, slots: &FrameSlots, pts: QpcTime, index: u64) -> windows::core::Result<()> {
        let prev = self.prev.unwrap_or(pts.add_hns(-self.step));
        self.prev = Some(pts);
        let bg = if is_flash(prev, pts) { &FLASH } else { &BACKGROUND };
        let (mut n1, mut n0) = (0, 0);
        for bit in 0..COUNTER_BITS {
            let r = counter_rect(self.width, bit);
            if (index >> (COUNTER_BITS - 1 - bit)) & 1 == 1 {
                self.ones[n1] = r;
                n1 += 1;
            } else {
                self.zeros[n0] = r;
                n0 += 1;
            }
        }
        // SAFETY: viste e texture valide create su questo device; il contesto immediato è protetto
        // per l'uso da più thread. ClearView con zero rettangoli pulirebbe tutto: si salta.
        unsafe {
            self.ctx.ClearRenderTargetView(&self.rtv, bg);
            if n1 > 0 {
                self.ctx.ClearView(&self.rtv, &BIT_ON, Some(&self.ones[..n1]));
            }
            if n0 > 0 {
                self.ctx.ClearView(&self.rtv, &BIT_OFF, Some(&self.zeros[..n0]));
            }
        }
        slots.publish(self.width, self.height, pts, |dst| {
            // SAFETY: stesse dimensioni e formato; contesto immediato protetto.
            unsafe { self.ctx.CopyResource(dst, &self.canvas) };
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::pacer::grid_pts;
    use crate::testutil::Readback;

    #[test]
    fn pacer_60fps_two_seconds_on_grid() {
        let device = GpuDevice::create(None).unwrap();
        let mut src = SyntheticSource::new(device, 320, 240);
        let (tx, rx) = mpsc::channel();
        src.start(
            60,
            Box::new(move |f| {
                let late = QpcTime::now().hns_since(f.pts);
                tx.send((f.pts, f.index, f.repeated, f.width, f.height, late)).unwrap();
            }),
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(2));
        src.stop();
        let stats = src.stats();
        let frames: Vec<_> = rx.try_iter().collect();
        // Ritardo della consegna rispetto al pts (comprende il disegno del frame sintetico).
        let mut late: Vec<i64> = frames.iter().map(|f| f.5).collect();
        late.sort_unstable();
        let p50 = late[late.len() / 2] as f64 / 1e4;
        let p95 = late[late.len() * 95 / 100] as f64 / 1e4;
        let max = late[late.len() - 1] as f64 / 1e4;
        println!("ritardo consegna: mediana {p50:.3} ms, p95 {p95:.3} ms, max {max:.3} ms");
        assert!(p95 < 4.0, "consegna in ritardo: p95 {p95} ms");
        assert!((114..=122).contains(&frames.len()), "{} frame", frames.len());
        let (pts0, idx0, ..) = frames[0];
        let start = pts0.add_hns(-grid_pts(QpcTime(0), idx0, 60).0);
        for w in frames.windows(2) {
            assert!(w[1].1 > w[0].1, "indici crescenti");
        }
        for &(pts, index, _, w, h, _) in &frames {
            assert_eq!(pts, grid_pts(start, index, 60), "pts fuori griglia al frame {index}");
            assert_eq!((w, h), (320, 240));
        }
        let repeated = frames.iter().filter(|f| f.2).count() as u64;
        assert_eq!(stats.frames_delivered, frames.len() as u64);
        assert_eq!(stats.frames_repeated, repeated);
        assert_eq!(repeated, 0, "la sintetica disegna un frame nuovo a ogni tick");
        let last = frames.last().unwrap().1;
        assert_eq!(stats.frames_delivered + stats.frames_dropped, last + 1);
        assert_eq!(stats.state, SourceState::Stopped);
        assert_eq!((stats.source_width, stats.source_height), (320, 240));
    }

    #[test]
    fn flash_on_second_boundaries_and_counter_matches_index() {
        let device = GpuDevice::create(None).unwrap();
        let mut rb = Readback::new(&device, 320, 240);
        let mut src = SyntheticSource::new(device, 320, 240);
        let (tx, rx) = mpsc::channel();
        src.start(
            30,
            Box::new(move |f| {
                let px = rb.read(&f.texture);
                let center = px.pixel(160, 200);
                let white = center.iter().take(3).all(|&c| c > 240);
                let mut counter = 0u64;
                for bit in 0..COUNTER_BITS {
                    let r = counter_rect(320, bit);
                    let [_, g, ..] = px.pixel(((r.left + r.right) / 2) as u32, ((r.top + r.bottom) / 2) as u32);
                    counter = (counter << 1) | u64::from(g > 200);
                }
                tx.send((f.pts, f.index, white, counter)).unwrap();
            }),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(2600));
        src.stop();
        let frames: Vec<_> = rx.try_iter().collect();
        assert!(frames.len() > 60, "{} frame", frames.len());
        let mut prev = frames[0].0.add_hns(-HNS_PER_SEC / 30);
        let mut flashes = 0;
        for &(pts, index, white, counter) in &frames {
            assert_eq!(white, is_flash(prev, pts), "lampo sbagliato al frame {index}");
            assert_eq!(counter, index, "contatore");
            flashes += usize::from(white);
            prev = pts;
        }
        assert!(flashes >= 2, "{flashes} lampi");
    }

    #[test]
    fn flash_rule() {
        let s = HNS_PER_SEC;
        assert!(is_flash(QpcTime(s - 1), QpcTime(s)));
        assert!(is_flash(QpcTime(s - 1), QpcTime(s + 5)));
        assert!(!is_flash(QpcTime(s), QpcTime(s + 1)));
        assert!(!is_flash(QpcTime(s + 1), QpcTime(2 * s - 1)));
    }
}
