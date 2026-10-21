//! Pacer a frequenza costante, triplo buffer dell'ultima texture arrivata e contatori.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cliprec_core::{CaptureStats, FrameCallback, GpuDevice, GpuFrame, HNS_PER_SEC, QpcTime, SourceError, SourceState};
use windows::Win32::Foundation::{CloseHandle, E_POINTER, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11RenderTargetView,
    ID3D11Texture2D,
};
use windows::Graphics::Capture::Direct3D11CaptureFrame;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW,
    GetCurrentThread, INFINITE, SetThreadPriority, SetWaitableTimer, THREAD_PRIORITY_HIGHEST, TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::w;

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
    /// Istante QPC in cui il contenuto è stato prodotto (`SystemRelativeTime` di WGC,
    /// `LastPresentTime` di DDA, il pts per la sintetica).
    captured: QpcTime,
    /// Frame WGC di cui `tex` è la superficie (zero-copy): torna al pool quando lo slot si libera.
    frame: Option<Direct3D11CaptureFrame>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(f) = self.frame.take() {
            let _ = f.Close();
        }
    }
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
    /// `true` dopo il primo frame vero (non segnaposto).
    real: bool,
}

impl SlotsInner {
    /// Toglie dagli slot i frame WGC che non sono né l'ultimo né quello in consegna (si chiudono
    /// quando `out` viene distrutto, fuori dal lock).
    fn take_stale_frames(&mut self, out: &mut [Option<Slot>]) {
        let (latest, in_use) = (self.latest, self.in_use);
        for ((i, s), o) in self.slots.iter_mut().enumerate().zip(out) {
            if Some(i) != latest && Some(i) != in_use && s.as_ref().is_some_and(|s| s.frame.is_some()) {
                *o = s.take();
            }
        }
    }
}

/// L'ultimo frame preso in prestito dal pacer.
struct Acquired {
    tex: ID3D11Texture2D,
    width: u32,
    height: u32,
    seq: u64,
    captured: QpcTime,
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

    /// Pubblica un nuovo frame `width`×`height` prodotto all'istante `captured`: `fill` riceve la
    /// texture di uno slot libero (ricreata solo se la dimensione cambia) e vi accoda la scrittura
    /// sul contesto immediato.
    pub fn publish(
        &self,
        width: u32,
        height: u32,
        captured: QpcTime,
        fill: impl FnOnce(&ID3D11Texture2D) -> windows::core::Result<()>,
    ) -> windows::core::Result<()> {
        self.put(width, height, captured, true, fill)
    }

    /// Frame nero della dimensione del bersaglio, consegnato solo se il primo frame vero tarda.
    /// Se un frame vero è già arrivato (la cattura parte prima del segnaposto) non fa nulla.
    pub fn publish_placeholder(&self, width: u32, height: u32) -> windows::core::Result<()> {
        self.put(width, height, QpcTime::now(), false, |t| clear_black(&self.device, t))
    }

    fn put(
        &self,
        width: u32,
        height: u32,
        captured: QpcTime,
        real: bool,
        fill: impl FnOnce(&ID3D11Texture2D) -> windows::core::Result<()>,
    ) -> windows::core::Result<()> {
        let mut g = lock(&self.inner);
        if !real && g.real {
            return Ok(()); // il segnaposto non copre mai un frame vero
        }
        let (latest, in_use) = (g.latest, g.in_use);
        // Con tre slot e al massimo due occupati, uno libero c'è sempre.
        let i = (0..3).find(|&i| Some(i) != latest && Some(i) != in_use).unwrap_or(0);
        // Mai la superficie di un frame WGC: è del pool, non nostra.
        let reuse = matches!(&g.slots[i], Some(s) if s.width == width && s.height == height && s.frame.is_none());
        if !reuse {
            let tex = create_bgra(&self.device, width, height)?;
            g.slots[i] = Some(Slot { tex, width, height, captured, frame: None });
        }
        if let Some(s) = g.slots[i].as_mut() {
            fill(&s.tex)?;
            s.captured = captured;
        }
        g.latest = Some(i);
        g.seq += 1;
        g.real |= real;
        Ok(())
    }

    /// Pubblica un frame WGC senza copiarlo: `tex` (la sua superficie, con il contenuto `width`×`height`
    /// in alto a sinistra) resta valida finché il frame non torna al pool, cioè quando ne arriva uno
    /// più nuovo e il pacer non lo sta consegnando. Così restano in mano al più due frame del pool.
    pub fn publish_frame(&self, frame: Direct3D11CaptureFrame, tex: ID3D11Texture2D, width: u32, height: u32, captured: QpcTime) {
        // I frame che tornano al pool si chiudono dopo aver lasciato il lock: `Close()` è una
        // chiamata WinRT e il pacer non deve aspettarla per prendere il frame successivo.
        let mut spent: [Option<Slot>; 4] = Default::default();
        {
            let mut g = lock(&self.inner);
            let (latest, in_use) = (g.latest, g.in_use);
            let i = (0..3).find(|&i| Some(i) != latest && Some(i) != in_use).unwrap_or(0);
            spent[3] = g.slots[i].replace(Slot { tex, width, height, captured, frame: Some(frame) });
            g.latest = Some(i);
            g.seq += 1;
            g.real = true;
            g.take_stale_frames(&mut spent[..3]);
        }
        drop(spent);
    }

    fn has_real(&self) -> bool {
        lock(&self.inner).real
    }

    /// Prende in prestito l'ultimo frame fino a [`Self::release`].
    fn acquire(&self) -> Option<Acquired> {
        let mut g = lock(&self.inner);
        let i = g.latest?;
        g.in_use = Some(i);
        let seq = g.seq;
        g.slots[i].as_ref().map(|s| Acquired {
            tex: s.tex.clone(),
            width: s.width,
            height: s.height,
            seq,
            captured: s.captured,
        })
    }

    /// Il frame consegnato, se nel frattempo ne è arrivato uno nuovo, torna al pool al prossimo
    /// `publish_frame`, sul thread della cattura: niente chiamate WinRT sul thread del pacer.
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
    /// Panic della callback presi e superati.
    pub panics: AtomicU64,
    /// `pts − istante di cattura` dei frame nuovi consegnati.
    pub latency: LatencyLog,
    /// `arrivo nel gestore − istante di cattura` dei frame della cattura (diagnostica).
    pub arrival: LatencyLog,
}

const LATENCY_SAMPLES: usize = 1024;

/// Ultimi 1024 valori (unità da 100 ns) in un anello preallocato.
pub(crate) struct LatencyLog(Mutex<LatencyRing>);

struct LatencyRing {
    samples: [i64; LATENCY_SAMPLES],
    len: usize,
    next: usize,
}

impl Default for LatencyLog {
    fn default() -> Self {
        Self(Mutex::new(LatencyRing { samples: [0; LATENCY_SAMPLES], len: 0, next: 0 }))
    }
}

impl LatencyLog {
    pub fn record(&self, hns: i64) {
        let mut g = lock(&self.0);
        let n = g.next;
        g.samples[n] = hns;
        g.next = (n + 1) % LATENCY_SAMPLES;
        g.len = (g.len + 1).min(LATENCY_SAMPLES);
    }

    /// Mediana e 95° percentile in ms (alloca: solo diagnostica).
    pub fn percentiles_ms(&self) -> Option<(f64, f64)> {
        let mut v = {
            let g = lock(&self.0);
            g.samples[..g.len].to_vec()
        };
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        let ms = |i: usize| v[i.min(v.len() - 1)] as f64 / 1e4;
        Some((ms(v.len() / 2), ms(v.len() * 95 / 100)))
    }
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

/// Opzioni del pacer; il default (nessuna attesa, nessuno spostamento) è quello della sintetica.
#[derive(Default)]
pub(crate) struct PacerOptions {
    pub prepare: Option<Prepare>,
    /// Prima del primo tick aspetta fino a tanto il primo frame vero; scaduto il tempo consegna
    /// il segnaposto. Il primo tick (e quindi `start`) è l'istante in cui l'attesa finisce.
    pub first_frame_wait: Duration,
    /// Spostamento costante della griglia dei pts all'indietro (unità da 100 ns): compensa il
    /// ritardo medio tra la cattura del contenuto e il tick che lo consegna.
    pub pts_offset: i64,
}

/// Dopo tanti panic della callback in [`PANIC_WINDOW`] il pacer si ferma in `Error`.
pub(crate) const MAX_PANICS: usize = 10;
pub(crate) const PANIC_WINDOW: Duration = Duration::from_secs(10);

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
        options: PacerOptions,
        on_frame: FrameCallback,
    ) -> Result<Self, SourceError> {
        if !SUPPORTED_FPS.contains(&fps) {
            return Err(SourceError::Unsupported(format!("{fps} fps (ammessi: 30, 60, 120, 144)")));
        }
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let thread = std::thread::Builder::new()
            .name("cliprec-pacer".into())
            .spawn(move || run(fps, &slots, &stats, options, on_frame, &flag))
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
    options: PacerOptions,
    mut on_frame: FrameCallback,
    running: &AtomicBool,
) {
    let PacerOptions { mut prepare, first_frame_wait, pts_offset } = options;
    // MMCSS «Capture»: Windows schedula il thread come quelli audio, con priorità alta anche quando
    // un gioco satura la CPU (i tick persi vengono da risvegli in ritardo o da callback lente).
    // Senza MMCSS (servizio fermo) resta THREAD_PRIORITY_HIGHEST.
    struct Mmcss(Option<windows::Win32::Foundation::HANDLE>);
    impl Drop for Mmcss {
        fn drop(&mut self) {
            if let Some(h) = self.0 {
                // SAFETY: handle restituita da AvSetMmThreadCharacteristicsW su questo thread.
                let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
            }
        }
    }
    let mut task = 0u32;
    // SAFETY: nome statico terminato da zero; `task` è un'uscita valida. La registrazione si annulla sotto.
    let mmcss = Mmcss(unsafe { AvSetMmThreadCharacteristicsW(w!("Capture"), &mut task) }.ok());
    if mmcss.0.is_none() {
        // SAFETY: GetCurrentThread restituisce una pseudo-handle sempre valida per il thread corrente.
        if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) } {
            tracing::debug!("priorità del pacer non alzata: {e}");
        }
    }
    // Il primo frame consegnato decide la dimensione dell'encoder: meglio quello vero del segnaposto.
    let give_up = Instant::now() + first_frame_wait;
    while running.load(Ordering::Acquire) && !slots.has_real() && Instant::now() < give_up {
        std::thread::sleep(Duration::from_millis(1));
    }
    let timer = Timer::new();
    let start = QpcTime::now();
    let pts_start = start.add_hns(-pts_offset);
    let mut tick: u64 = 0;
    let mut last_seq = 0u64;
    // Istanti degli ultimi MAX_PANICS panic, in un anello.
    let mut panics = [None::<Instant>; MAX_PANICS];
    let mut panic_count = 0usize;
    let mut last_callback = Duration::ZERO;
    let mut drops = DropReport::default();
    while running.load(Ordering::Acquire) {
        timer.wait_until(grid_pts(start, tick, fps));
        if !running.load(Ordering::Acquire) {
            break;
        }
        // Se la callback (o lo scheduler) ci ha fatto saltare uno slot intero, quei tick sono persi:
        // si consegna solo l'ultimo tick scaduto, senza mai accumulare.
        let now = QpcTime::now();
        let due = last_due_tick(start, now, fps);
        if due > tick {
            stats.dropped.fetch_add(due - tick, Ordering::Relaxed);
            drops.add(due - tick, now.hns_since(grid_pts(start, tick, fps)), last_callback);
            tick = due;
        }
        drops.log_if_due();
        let pts = grid_pts(pts_start, tick, fps);
        if let Some(p) = prepare.as_mut() {
            p(slots, pts, tick);
        }
        let Some(a) = slots.acquire() else {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            tick += 1;
            continue;
        };
        let repeated = a.seq == last_seq;
        last_seq = a.seq;
        if !repeated {
            stats.latency.record(pts.hns_since(a.captured));
        }
        let (width, height) = (a.width, a.height);
        let frame = GpuFrame { texture: a.tex, width, height, pts, index: tick, repeated };
        let began = Instant::now();
        let result = catch_unwind(AssertUnwindSafe(|| on_frame(&frame)));
        last_callback = began.elapsed();
        drop(frame);
        slots.release();
        stats.delivered.fetch_add(1, Ordering::Relaxed);
        if repeated {
            stats.repeated.fetch_add(1, Ordering::Relaxed);
        }
        stats.width.store(width, Ordering::Relaxed);
        stats.height.store(height, Ordering::Relaxed);
        tick += 1;
        if result.is_err() {
            // Il panic è già stato stampato dall'hook; si prosegue col tick successivo, a meno che
            // la callback non sia rotta in modo stabile.
            let now = Instant::now();
            panics[panic_count % MAX_PANICS] = Some(now);
            panic_count += 1;
            stats.panics.fetch_add(1, Ordering::Relaxed);
            tracing::error!(panics = panic_count, "panic nella callback dei frame");
            // Il prossimo da sovrascrivere è il più vecchio degli ultimi MAX_PANICS.
            let oldest = panics[panic_count % MAX_PANICS];
            if panic_count >= MAX_PANICS && oldest.is_some_and(|t| now.duration_since(t) < PANIC_WINDOW) {
                tracing::error!("{MAX_PANICS} panic in {PANIC_WINDOW:?}: pacer fermato");
                stats.set_state(SourceState::Error(format!(
                    "la callback dei frame è andata in panic {MAX_PANICS} volte in {} s",
                    PANIC_WINDOW.as_secs()
                )));
                break;
            }
        }
    }
}

/// Tick persi raccolti e scritti nel log al più ogni 10 s, con quanto tardi si è svegliato il pacer e
/// quanto era durata la callback precedente: callback lunga = conversione o encoder lenti, callback
/// breve e risveglio tardi = thread del pacer non schedulato in tempo.
#[derive(Default)]
struct DropReport {
    lost: u64,
    events: u32,
    worst_late_hns: i64,
    worst_callback: Duration,
    since: Option<Instant>,
}

impl DropReport {
    fn add(&mut self, lost: u64, late_hns: i64, previous_callback: Duration) {
        self.lost += lost;
        self.events += 1;
        self.worst_late_hns = self.worst_late_hns.max(late_hns);
        self.worst_callback = self.worst_callback.max(previous_callback);
        self.since.get_or_insert_with(Instant::now);
    }

    fn log_if_due(&mut self) {
        if self.since.is_some_and(|t| t.elapsed() >= Duration::from_secs(10)) {
            tracing::warn!(
                tick_persi = self.lost,
                episodi = self.events,
                ritardo_max_ms = self.worst_late_hns as f64 / 1e4,
                callback_max_ms = self.worst_callback.as_secs_f64() * 1000.0,
                "tick persi negli ultimi 10 s"
            );
            *self = Self::default();
        }
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

    fn start(
        fps: u32,
        slots: Arc<FrameSlots>,
        stats: Arc<Stats>,
        on_frame: impl FnMut(&GpuFrame) + Send + 'static,
    ) -> Pacer {
        Pacer::start(fps, slots, stats, PacerOptions::default(), Box::new(on_frame)).unwrap()
    }

    #[test]
    fn slot_in_use_is_never_written() {
        let slots = FrameSlots::new(GpuDevice::create(None).unwrap());
        slots.publish_placeholder(64, 64).unwrap();
        assert!(!slots.has_real());
        let held = slots.acquire().unwrap().tex;
        for _ in 0..5 {
            slots
                .publish(64, 64, QpcTime(7), |t| {
                    assert_ne!(t, &held);
                    Ok(())
                })
                .unwrap();
        }
        assert!(slots.has_real());
        // Cambio di dimensione: lo slot libero viene ricreato, quello in uso resta intatto.
        slots.publish(32, 16, QpcTime(9), |_| Ok(())).unwrap();
        slots.publish_placeholder(8, 8).unwrap(); // ignorato: c'è già un frame vero
        let a = slots.acquire().unwrap();
        assert_ne!(a.tex, held);
        assert_eq!((a.width, a.height, a.seq, a.captured), (32, 16, 7, QpcTime(9)));
        slots.release();
    }

    /// Il pacer aspetta il primo frame vero (fino a `first_frame_wait`) invece di consegnare subito
    /// il segnaposto, e i pts seguono la griglia spostata di `pts_offset`.
    #[test]
    fn waits_for_first_real_frame_and_shifts_grid() {
        let slots = Arc::new(FrameSlots::new(GpuDevice::create(None).unwrap()));
        slots.publish_placeholder(640, 360).unwrap();
        let (tx, rx) = mpsc::channel();
        let options =
            PacerOptions { first_frame_wait: Duration::from_millis(500), pts_offset: 50_000, ..Default::default() };
        let t0 = QpcTime::now();
        let mut pacer = Pacer::start(
            60,
            slots.clone(),
            Arc::default(),
            options,
            Box::new(move |f| tx.send((QpcTime::now(), f.pts, f.width, f.repeated)).unwrap()),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(rx.try_recv().is_err(), "niente consegne prima del primo frame vero");
        slots.publish(320, 180, QpcTime::now(), |t| clear_black(&slots.device, t)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        pacer.stop();
        let frames: Vec<_> = rx.try_iter().collect();
        let (at, pts, w, repeated) = frames[0];
        assert_eq!((w, repeated), (320, false));
        assert!(at.hns_since(t0) >= 1_400_000, "primo frame troppo presto");
        // Il primo tick è all'istante `at` (meno il ritardo di consegna): pts = tick − 5 ms.
        let shift = at.hns_since(pts);
        assert!((50_000..80_000).contains(&shift), "spostamento {shift}");

        // Senza frame veri il segnaposto arriva dopo l'attesa.
        let slots = Arc::new(FrameSlots::new(GpuDevice::create(None).unwrap()));
        slots.publish_placeholder(640, 360).unwrap();
        let (tx, rx) = mpsc::channel();
        let options = PacerOptions { first_frame_wait: Duration::from_millis(200), ..Default::default() };
        let mut pacer =
            Pacer::start(60, slots, Arc::default(), options, Box::new(move |f| tx.send(f.width).unwrap())).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        pacer.stop();
        let n = rx.try_iter().filter(|&w| w == 640).count();
        assert!((8..=14).contains(&n), "{n} segnaposto");
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
                    slots.publish(64, 64, QpcTime::now(), |t| clear_black(&slots.device, t)).unwrap();
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        };
        let (tx, rx) = mpsc::channel();
        let mut pacer = start(60, slots, stats.clone(), move |f| {
            tx.send((f.index, f.repeated)).unwrap();
            if f.index % 10 == 5 {
                // Occupata per ~2,4 periodi: un tick perso, il successivo consegnato in ritardo.
                std::thread::sleep(Duration::from_millis(40));
            }
        });
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
    fn panics_in_callback_are_survived_until_ten_in_ten_seconds() {
        let slots = Arc::new(FrameSlots::new(GpuDevice::create(None).unwrap()));
        slots.publish(16, 16, QpcTime::now(), |_| Ok(())).unwrap();

        // Qualche panic sparso: il pacer prosegue.
        let stats = Arc::new(Stats::default());
        let (tx, rx) = mpsc::channel();
        let mut pacer = start(60, slots.clone(), stats.clone(), move |f| {
            tx.send(f.index).unwrap();
            assert!(!(3..6).contains(&f.index), "panic voluto nel test");
        });
        std::thread::sleep(Duration::from_millis(300));
        pacer.stop();
        let s = stats.snapshot();
        assert_eq!(stats.panics.load(Ordering::Relaxed), 3);
        assert!(rx.try_iter().any(|i| i > 6), "il pacer non è andato avanti");
        assert!(!matches!(s.state, SourceState::Error(_)), "{s:?}");

        // Panic a ogni frame: al decimo il pacer si ferma in Error.
        let stats = Arc::new(Stats::default());
        let mut pacer = start(60, slots, stats.clone(), |_| panic!("panic voluto nel test"));
        std::thread::sleep(Duration::from_millis(400));
        pacer.stop();
        let s = stats.snapshot();
        assert_eq!(s.frames_delivered, MAX_PANICS as u64);
        assert!(matches!(s.state, SourceState::Error(_)), "{s:?}");
    }

    #[test]
    fn rejects_unsupported_fps() {
        let slots = Arc::new(FrameSlots::new(GpuDevice::create(None).unwrap()));
        assert!(Pacer::start(50, slots, Arc::default(), PacerOptions::default(), Box::new(|_| ())).is_err());
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
