//! Pacer a frequenza costante, triplo buffer dell'ultima texture arrivata e contatori.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use cliprec_core::{CaptureStats, FrameCallback, GpuDevice, GpuFrame, HNS_PER_SEC, QpcTime, SourceError, SourceState};
use windows::Win32::Foundation::{CloseHandle, E_POINTER, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11RenderTargetView,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentThread, INFINITE, SetThreadPriority,
    SetWaitableTimer, THREAD_PRIORITY_HIGHEST, TIMER_ALL_ACCESS, WaitForSingleObject,
};

/// Frequenze accettate dal pacer.
pub const SUPPORTED_FPS: [u32; 4] = [30, 60, 120, 144];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Texture BGRA della dimensione data, leggibile dagli shader e usabile come render target.
pub(crate) fn create_bgra(device: &GpuDevice, width: u32, height: u32) -> windows::core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width.max(1),
        Height: height.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    // SAFETY: descrittore e puntatore d'uscita sono variabili locali valide per tutta la chiamata.
    unsafe { device.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or_else(|| E_POINTER.into())
}

pub(crate) fn create_rtv(device: &GpuDevice, tex: &ID3D11Texture2D) -> windows::core::Result<ID3D11RenderTargetView> {
    let mut rtv = None;
    // SAFETY: texture valida creata con D3D11_BIND_RENDER_TARGET; puntatore d'uscita locale.
    unsafe { device.device.CreateRenderTargetView(tex, None, Some(&mut rtv))? };
    rtv.ok_or_else(|| E_POINTER.into())
}

/// Riempie la texture di nero (usato per il primo frame di ogni sorgente).
pub(crate) fn clear_black(device: &GpuDevice, tex: &ID3D11Texture2D) -> windows::core::Result<()> {
    let rtv = create_rtv(device, tex)?;
    // SAFETY: vista valida; il contesto immediato è protetto per l'uso da più thread.
    unsafe { device.context.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]) };
    Ok(())
}

struct Slot {
    tex: ID3D11Texture2D,
    width: u32,
    height: u32,
}

#[derive(Default)]
struct SlotsInner {
    slots: [Option<Slot>; 3],
    /// Slot dell'ultimo frame pubblicato.
    latest: Option<usize>,
    /// Slot che il pacer sta consegnando alla callback: la cattura non ci scrive.
    in_use: Option<usize>,
    /// Numero di frame pubblicati: se non cambia tra due tick, il frame è ripetuto.
    seq: u64,
}

/// Triplo buffer tra cattura e pacer: la cattura scrive sempre in uno slot che non è né l'ultimo
/// pubblicato né quello in mano alla callback, quindi nessuna copia in più e nessuna sovrascrittura.
pub(crate) struct FrameSlots {
    pub device: GpuDevice,
    inner: Mutex<SlotsInner>,
}

// SAFETY: le texture D3D11 sono oggetti free-threaded e il contesto immediato del GpuDevice è
// protetto da SetMultithreadProtected(true); lo stato degli slot è sotto Mutex.
unsafe impl Send for FrameSlots {}
// SAFETY: vedi sopra.
unsafe impl Sync for FrameSlots {}

impl FrameSlots {
    pub fn new(device: GpuDevice) -> Self {
        Self { device, inner: Mutex::new(SlotsInner::default()) }
    }

    /// Pubblica un nuovo frame `width`×`height`: `fill` riceve la texture di uno slot libero
    /// (ricreata solo se la dimensione cambia) e vi accoda la scrittura sul contesto immediato.
    pub fn publish(
        &self,
        width: u32,
        height: u32,
        fill: impl FnOnce(&ID3D11Texture2D) -> windows::core::Result<()>,
    ) -> windows::core::Result<()> {
        let mut g = lock(&self.inner);
        let (latest, in_use) = (g.latest, g.in_use);
        // Con tre slot e al massimo due occupati, uno libero c'è sempre.
        let i = (0..3).find(|&i| Some(i) != latest && Some(i) != in_use).unwrap_or(0);
        let reuse = matches!(&g.slots[i], Some(s) if s.width == width && s.height == height);
        if !reuse {
            g.slots[i] = Some(Slot { tex: create_bgra(&self.device, width, height)?, width, height });
        }
        if let Some(s) = &g.slots[i] {
            fill(&s.tex)?;
        }
        g.latest = Some(i);
        g.seq += 1;
        Ok(())
    }

    /// Prende in prestito l'ultimo frame fino a [`Self::release`]: (texture, larghezza, altezza, seq).
    fn acquire(&self) -> Option<(ID3D11Texture2D, u32, u32, u64)> {
        let mut g = lock(&self.inner);
        let i = g.latest?;
        g.in_use = Some(i);
        let seq = g.seq;
        g.slots[i].as_ref().map(|s| (s.tex.clone(), s.width, s.height, seq))
    }

    fn release(&self) {
        lock(&self.inner).in_use = None;
    }

    /// Esegue `f` sull'ultimo frame tenendo il lock: la cattura non può riscriverlo finché `f` non
    /// ha accodato i suoi comandi sul contesto immediato.
    pub fn with_latest<R>(&self, f: impl FnOnce(&ID3D11Texture2D, u32, u32) -> R) -> Option<R> {
        let g = lock(&self.inner);
        let s = g.slots[g.latest?].as_ref()?;
        Some(f(&s.tex, s.width, s.height))
    }
}

/// Contatori condivisi tra pacer, cattura e chi chiama `stats()`.
#[derive(Default)]
pub(crate) struct Stats {
    delivered: AtomicU64,
    repeated: AtomicU64,
    dropped: AtomicU64,
    /// Frame arrivati dalla cattura (non esposto in `CaptureStats`, utile ai test).
    pub arrived: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
    state: Mutex<SourceState>,
}

impl Stats {
    pub fn snapshot(&self) -> CaptureStats {
        CaptureStats {
            frames_delivered: self.delivered.load(Ordering::Relaxed),
            frames_repeated: self.repeated.load(Ordering::Relaxed),
            frames_dropped: self.dropped.load(Ordering::Relaxed),
            source_width: self.width.load(Ordering::Relaxed),
            source_height: self.height.load(Ordering::Relaxed),
            state: self.state(),
        }
    }

    pub fn state(&self) -> SourceState {
        lock(&self.state).clone()
    }

    pub fn set_state(&self, s: SourceState) {
        let mut g = lock(&self.state);
        if *g != s {
            tracing::info!(state = ?s, "stato della sorgente video");
            *g = s;
        }
    }
}

/// Istante del tick `index` sulla griglia: `start + index * HNS_PER_SEC / fps`.
pub(crate) fn grid_pts(start: QpcTime, index: u64, fps: u32) -> QpcTime {
    let offset = i128::from(index) * i128::from(HNS_PER_SEC) / i128::from(fps.max(1));
    start.add_hns(i64::try_from(offset).unwrap_or(i64::MAX))
}

/// Ultimo tick con istante `<= now` (0 se `now` precede `start`).
fn last_due_tick(start: QpcTime, now: QpcTime, fps: u32) -> u64 {
    let elapsed = now.hns_since(start).max(0);
    let mut j = u64::try_from(i128::from(elapsed) * i128::from(fps) / i128::from(HNS_PER_SEC)).unwrap_or(u64::MAX);
    // La divisione intera di grid_pts può spostare il confine di un'unità: correggi.
    while j > 0 && grid_pts(start, j, fps) > now {
        j -= 1;
    }
    while grid_pts(start, j + 1, fps) <= now {
        j += 1;
    }
    j
}

/// Chiamata dal pacer prima di ogni consegna, con pts e indice del tick: le sorgenti sintetiche
/// ci disegnano il frame.
pub(crate) type Prepare = Box<dyn FnMut(&FrameSlots, QpcTime, u64) + Send>;

/// Thread che a ogni tick consegna l'ultima texture pubblicata nei [`FrameSlots`].
pub(crate) struct Pacer {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Pacer {
    pub fn start(
        fps: u32,
        slots: Arc<FrameSlots>,
        stats: Arc<Stats>,
        prepare: Option<Prepare>,
        on_frame: FrameCallback,
    ) -> Result<Self, SourceError> {
        if !SUPPORTED_FPS.contains(&fps) {
            return Err(SourceError::Unsupported(format!("{fps} fps (ammessi: 30, 60, 120, 144)")));
        }
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let thread = std::thread::Builder::new()
            .name("cliprec-pacer".into())
            .spawn(move || run(fps, &slots, &stats, prepare, on_frame, &flag))
            .map_err(|e| SourceError::Failed(format!("thread del pacer: {e}")))?;
        Ok(Self { running, thread: Some(thread) })
    }

    /// Ferma il thread; al ritorno la callback non viene più chiamata.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(
    fps: u32,
    slots: &FrameSlots,
    stats: &Stats,
    mut prepare: Option<Prepare>,
    mut on_frame: FrameCallback,
    running: &AtomicBool,
) {
    // SAFETY: GetCurrentThread restituisce una pseudo-handle sempre valida per il thread corrente.
    if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) } {
        tracing::debug!("priorità del pacer non alzata: {e}");
    }
    let timer = Timer::new();
    let start = QpcTime::now();
    let mut tick: u64 = 0;
    let mut last_seq = 0u64;
    while running.load(Ordering::Acquire) {
        timer.wait_until(grid_pts(start, tick, fps));
        if !running.load(Ordering::Acquire) {
            break;
        }
        // Se la callback (o lo scheduler) ci ha fatto saltare uno slot intero, quei tick sono persi:
        // si consegna solo l'ultimo tick scaduto, senza mai accumulare.
        let due = last_due_tick(start, QpcTime::now(), fps);
        if due > tick {
            stats.dropped.fetch_add(due - tick, Ordering::Relaxed);
            tick = due;
        }
        let pts = grid_pts(start, tick, fps);
        if let Some(p) = prepare.as_mut() {
            p(slots, pts, tick);
        }
        let Some((texture, width, height, seq)) = slots.acquire() else {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            tick += 1;
            continue;
        };
        let repeated = seq == last_seq;
        last_seq = seq;
        let frame = GpuFrame { texture, width, height, pts, index: tick, repeated };
        let result = catch_unwind(AssertUnwindSafe(|| on_frame(&frame)));
        drop(frame);
        slots.release();
        if result.is_err() {
            tracing::error!("panic nella callback dei frame: pacer fermato");
            stats.set_state(SourceState::Error("panic nella callback dei frame".into()));
            break;
        }
        stats.delivered.fetch_add(1, Ordering::Relaxed);
        if repeated {
            stats.repeated.fetch_add(1, Ordering::Relaxed);
        }
        stats.width.store(width, Ordering::Relaxed);
        stats.height.store(height, Ordering::Relaxed);
        tick += 1;
    }
}

/// Attesa precisa: waitable timer ad alta risoluzione, poi `yield` sugli ultimi 200 µs.
struct Timer(Option<HANDLE>);

impl Timer {
    fn new() -> Self {
        for flags in [CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, 0] {
            // SAFETY: nessun attributo né nome; i flag sono costanti documentate.
            if let Ok(h) = unsafe { CreateWaitableTimerExW(None, None, flags, TIMER_ALL_ACCESS.0) } {
                return Self(Some(h));
            }
        }
        tracing::warn!("waitable timer non disponibile: uso thread::sleep");
        Self(None)
    }

    fn wait_until(&self, deadline: QpcTime) {
        const SPIN_HNS: i64 = 2_000;
        loop {
            let remaining = deadline.hns_since(QpcTime::now());
            if remaining <= 0 {
                return;
            }
            if remaining <= SPIN_HNS {
                std::thread::yield_now();
                continue;
            }
            let wait = remaining - SPIN_HNS / 2;
            match self.0 {
                Some(h) => {
                    let due = -wait; // negativo = relativo, in unità da 100 ns
                    // SAFETY: handle del timer valida finché vive `self`; `due` è una variabile locale.
                    let armed = unsafe { SetWaitableTimer(h, &due, 0, None, None, false) };
                    if armed.is_ok() {
                        // SAFETY: handle valida.
                        unsafe { WaitForSingleObject(h, INFINITE) };
                    } else {
                        std::thread::sleep(std::time::Duration::from_micros(u64::try_from(wait / 10).unwrap_or(0)));
                    }
                }
                None => std::thread::sleep(std::time::Duration::from_micros(u64::try_from(wait / 10).unwrap_or(0))),
            }
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            // SAFETY: handle creata da CreateWaitableTimerExW e chiusa una sola volta.
            let _ = unsafe { CloseHandle(h) };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn slot_in_use_is_never_written() {
        let slots = FrameSlots::new(GpuDevice::create(None).unwrap());
        slots.publish(64, 64, |_| Ok(())).unwrap();
        let (held, ..) = slots.acquire().unwrap();
        for _ in 0..5 {
            slots.publish(64, 64, |t| {
                assert_ne!(t, &held);
                Ok(())
            })
            .unwrap();
        }
        // Cambio di dimensione: lo slot libero viene ricreato, quello in uso resta intatto.
        slots.publish(32, 16, |_| Ok(())).unwrap();
        let (now, w, h, seq) = slots.acquire().unwrap();
        assert_ne!(now, held);
        assert_eq!((w, h, seq), (32, 16, 7));
        slots.release();
    }

    #[test]
    fn repeats_slow_capture_and_drops_busy_ticks_without_accumulating() {
        let device = GpuDevice::create(None).unwrap();
        let slots = Arc::new(FrameSlots::new(device));
        let stats = Arc::new(Stats::default());
        let feeding = Arc::new(AtomicBool::new(true));
        let feeder = {
            let (slots, feeding) = (slots.clone(), feeding.clone());
            std::thread::spawn(move || {
                while feeding.load(Ordering::Relaxed) {
                    slots.publish(64, 64, |t| clear_black(&slots.device, t)).unwrap();
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };
        let (tx, rx) = mpsc::channel();
        let mut pacer = Pacer::start(
            60,
            slots,
            stats.clone(),
            None,
            Box::new(move |f| {
                tx.send((f.index, f.repeated)).unwrap();
                if f.index % 10 == 5 {
                    // Occupata per ~2,4 periodi: un tick perso, il successivo consegnato in ritardo.
                    std::thread::sleep(Duration::from_millis(40));
                }
            }),
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(2));
        pacer.stop();
        feeding.store(false, Ordering::Relaxed);
        feeder.join().unwrap();
        let s = stats.snapshot();
        let frames: Vec<_> = rx.try_iter().collect();
        let last = frames.last().unwrap().0;
        assert_eq!(s.frames_delivered, frames.len() as u64);
        assert_eq!(s.frames_delivered + s.frames_dropped, last + 1);
        assert_eq!(s.frames_repeated, frames.iter().filter(|f| f.1).count() as u64);
        assert!(s.frames_repeated > s.frames_delivered / 2, "{s:?}");
        assert!(s.frames_repeated < s.frames_delivered, "{s:?}");
        assert!((5..=30).contains(&s.frames_dropped), "{s:?}");
        assert!((110..=122).contains(&(last + 1)), "{s:?}");
    }

    #[test]
    fn panic_in_callback_stops_pacer_with_error() {
        let slots = Arc::new(FrameSlots::new(GpuDevice::create(None).unwrap()));
        slots.publish(16, 16, |_| Ok(())).unwrap();
        let stats = Arc::new(Stats::default());
        let mut pacer = Pacer::start(
            60,
            slots,
            stats.clone(),
            None,
            Box::new(|f| assert!(f.index < 3, "panic voluto nel test")),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        pacer.stop();
        let s = stats.snapshot();
        assert_eq!(s.frames_delivered, 3);
        assert!(matches!(s.state, SourceState::Error(_)), "{s:?}");
    }

    #[test]
    fn rejects_unsupported_fps() {
        let slots = Arc::new(FrameSlots::new(GpuDevice::create(None).unwrap()));
        assert!(Pacer::start(50, slots, Arc::default(), None, Box::new(|_| ())).is_err());
    }

    #[test]
    fn grid_and_last_due_tick_agree() {
        let start = QpcTime(123_456_789);
        for fps in SUPPORTED_FPS {
            for j in [0u64, 1, 2, 59, 143, 144, 1_000_003] {
                let t = grid_pts(start, j, fps);
                assert_eq!(last_due_tick(start, t, fps), j, "fps {fps} tick {j}");
                assert_eq!(last_due_tick(start, t.add_hns(-1), fps), j.saturating_sub(1));
            }
        }
        assert_eq!(grid_pts(start, 144, 144), start.add_hns(HNS_PER_SEC));
        assert_eq!(last_due_tick(start, QpcTime(0), 60), 0);
    }
}
