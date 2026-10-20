//! Sorgente reale (monitor, finestra, gioco, automatica): pacer + thread di sorveglianza che trova
//! il bersaglio, apre o riapre la sessione di cattura e aggiorna lo stato. Il pacer non si ferma mai
//! quando la cattura cambia: consegna l'ultimo frame (o nero) finché non ne arriva uno nuovo.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cliprec_core::config::{VideoConfig, VideoSourceConfig};
use cliprec_core::{CaptureStats, FrameCallback, GpuDevice, SourceError, SourceState, VideoSource};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::HMONITOR;

use crate::dda::DdaCapture;
use crate::health::Health;
use crate::pacer::{FrameSlots, Pacer, PacerOptions, Stats};
use crate::targets::{
    adapter_luid_for_monitor, covers_monitor, exe_matches, find_window_by_exe, find_window_by_title, fullscreen_foreground,
    is_foreground, is_minimized, is_window, monitor_pixels, monitors, primary_monitor, window_exe, window_monitor,
    window_title,
};
use crate::wgc::{self, WgcCapture};

/// Cosa catturare.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Monitor per indice (0 = principale).
    Monitor(u32),
    /// Finestra il cui titolo contiene questo testo.
    Window(String),
    /// Finestra principale del processo con questo eseguibile, ritrovata se si riavvia.
    Process(String),
    /// App a schermo intero in primo piano (in `games`, o qualsiasi se `any_fullscreen`),
    /// altrimenti il monitor principale.
    Auto { games: Vec<String>, any_fullscreen: bool },
}

impl Target {
    pub fn from_config(cfg: &VideoConfig) -> Self {
        match &cfg.source {
            VideoSourceConfig::Auto => Self::Auto { games: cfg.games.clone(), any_fullscreen: cfg.any_fullscreen_app },
            VideoSourceConfig::Monitor { index } => Self::Monitor(*index),
            VideoSourceConfig::Window { title } => Self::Window(title.clone()),
            VideoSourceConfig::Process { exe } => Self::Process(exe.clone()),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Monitor(i) => format!("Monitor {}", i + 1),
            Self::Window(t) => format!("Finestra «{t}»"),
            Self::Process(e) => e.clone(),
            Self::Auto { .. } => "Automatica".into(),
        }
    }

    /// Bersaglio concreto adesso; `Ok(None)` = da aspettare (finestra o gioco non ancora aperti).
    fn resolve(&self) -> Result<Option<Resolved>, SourceError> {
        Ok(match self {
            Self::Monitor(i) => Some(Resolved::Monitor(
                *monitors().get(*i as usize).ok_or_else(|| SourceError::NotFound(self.label()))?,
            )),
            Self::Window(t) => find_window_by_title(t).map(Resolved::Window),
            Self::Process(e) => find_window_by_exe(e).map(Resolved::Window),
            Self::Auto { games, any_fullscreen } => {
                Some(auto_pick(fullscreen_foreground(), games, *any_fullscreen, primary_monitor()))
            }
        })
    }

    /// Monitor su cui creare il device: quello del bersaglio se già visibile, altrimenti il principale.
    fn initial_monitor(&self) -> Result<HMONITOR, SourceError> {
        Ok(match self.resolve()? {
            Some(Resolved::Monitor(h)) => h,
            Some(Resolved::Window(w)) => window_monitor(w).unwrap_or_else(primary_monitor),
            None => primary_monitor(),
        })
    }
}

/// Modalità automatica: la finestra a schermo intero in primo piano se è un gioco in lista (o
/// qualsiasi, con `any_fullscreen`), altrimenti il monitor principale.
fn auto_pick(fullscreen: Option<(HWND, String)>, games: &[String], any_fullscreen: bool, primary: HMONITOR) -> Resolved {
    match fullscreen {
        Some((h, exe)) if any_fullscreen || games.iter().any(|g| exe_matches(&exe, g)) => Resolved::Window(h),
        _ => Resolved::Monitor(primary),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resolved {
    Monitor(HMONITOR),
    Window(HWND),
}

impl Resolved {
    fn describe(self) -> String {
        match self {
            Self::Monitor(h) => {
                let index = monitors().iter().position(|&m| m == h).unwrap_or(0) + 1;
                let (w, hh) = monitor_pixels(h).unwrap_or_default();
                format!("Monitor {index} ({w}×{hh})")
            }
            Self::Window(h) => match window_exe(h) {
                Some(exe) => format!("{} ({exe})", window_title(h)),
                None => window_title(h),
            },
        }
    }
}

/// Apre il device sull'adattatore giusto e la sorgente descritta dalla configurazione.
/// Il device va passato anche all'encoder: un solo device per sessione.
pub fn open_source(cfg: &VideoConfig) -> Result<(GpuDevice, Box<dyn VideoSource>), SourceError> {
    let target = Target::from_config(cfg);
    let device = crate::targets::device_for_hmonitor(target.initial_monitor()?)?;
    let source = CaptureSource::new(device.clone(), target, cfg.capture_cursor);
    Ok((device, Box::new(source)))
}

/// Sorgente video reale: Desktop Duplication per i monitor (WGC se non è disponibile, per esempio
/// sulla GPU dedicata dei portatili ibridi), Windows.Graphics.Capture per le finestre.
pub struct CaptureSource {
    device: GpuDevice,
    target: Target,
    cursor: bool,
    prefer_dda: bool,
    stats: Arc<Stats>,
    description: Arc<Mutex<String>>,
    /// Risoluzione del monitor del bersaglio corrente, aggiornata dalla sorveglianza.
    native: Arc<Mutex<Option<(u32, u32)>>>,
    pacer: Option<Pacer>,
    watcher: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl CaptureSource {
    /// `device` deve essere quello della sessione (vedi [`open_source`] e
    /// [`crate::device_for_monitor`]). Nulla parte finché non si chiama `start`.
    pub fn new(device: GpuDevice, target: Target, capture_cursor: bool) -> Self {
        let description = Arc::new(Mutex::new(target.label()));
        Self {
            device,
            target,
            cursor: capture_cursor,
            // Come «Cattura schermo» di OBS sui PC fissi: DDA non mostra il bordo giallo, cattura
            // anche il fullscreen esclusivo e costa meno di WGC sul monitor intero.
            prefer_dda: true,
            stats: Arc::default(),
            description,
            native: Arc::default(),
            pacer: None,
            watcher: None,
        }
    }

    /// Per i monitor prova prima Desktop Duplication (WGC resta il ripiego; è il default). Con `false`
    /// DDA si usa solo se WGC fallisce o se si chiedono più di 60 fps su build precedenti a 26100.
    pub fn prefer_desktop_duplication(mut self, yes: bool) -> Self {
        self.prefer_dda = yes;
        self
    }

    /// Frame arrivati dalla cattura (WGC o DDA) finora, per diagnostica e test.
    pub fn frames_arrived(&self) -> u64 {
        self.stats.arrived.load(Ordering::Relaxed)
    }

    /// Mediana e 95° percentile (ms) di `pts − istante di cattura` sugli ultimi 1024 frame nuovi.
    /// Vicino a 0 se la compensazione dei pts è giusta; positivo = video in ritardo sull'audio.
    pub fn latency_ms(&self) -> Option<(f64, f64)> {
        self.stats.latency.percentiles_ms()
    }
}

impl VideoSource for CaptureSource {
    fn start(&mut self, fps: u32, on_frame: FrameCallback) -> Result<(), SourceError> {
        self.stop();
        if !wgc::is_supported() && !matches!(self.target, Target::Monitor(_)) {
            return Err(SourceError::Unsupported("Windows.Graphics.Capture non disponibile".into()));
        }
        let slots = Arc::new(FrameSlots::new(self.device.clone()));
        let mut watch = Watch {
            target: self.target.clone(),
            cursor: self.cursor,
            prefer_dda: self.prefer_dda || (fps > 60 && !wgc::has_min_update_interval()) || !wgc::is_supported(),
            fps,
            slots: slots.clone(),
            stats: self.stats.clone(),
            description: self.description.clone(),
            native: self.native.clone(),
            current: None,
            health: Health::new(&self.device),
            target_monitor: None,
            other_adapter: false,
            usable_adapters: Vec::new(),
        };
        // Primo giro sincrono: un monitor inesistente o una sessione rifiutata sono errori d'avvio.
        watch.poll()?;
        // Segnaposto nero della dimensione nativa (il monitor del bersaglio), mai quella di una
        // finestra minimizzata; il pacer aspetta fino a FIRST_FRAME_WAIT il primo frame vero.
        let (w, h) = lock(&self.native).unwrap_or((1920, 1080));
        slots.publish_placeholder(w, h)?;
        let options =
            PacerOptions { first_frame_wait: FIRST_FRAME_WAIT, pts_offset: CAPTURE_PTS_OFFSET, ..Default::default() };
        let pacer = Pacer::start(fps, slots, self.stats.clone(), options, on_frame)?;
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        // Se lo spawn fallisce, pacer e cattura si fermano nel drop.
        let thread = std::thread::Builder::new()
            .name("cliprec-capture-watch".into())
            .spawn(move || watch.run(&flag))
            .map_err(|e| SourceError::Failed(format!("thread di sorveglianza: {e}")))?;
        self.pacer = Some(pacer);
        self.watcher = Some((running, thread));
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut p) = self.pacer.take() {
            p.stop();
        }
        if let Some((running, thread)) = self.watcher.take() {
            running.store(false, Ordering::Release);
            let _ = thread.join();
        }
        self.stats.set_state(SourceState::Stopped);
    }

    fn stats(&self) -> CaptureStats {
        self.stats.snapshot()
    }

    fn describe(&self) -> String {
        lock(&self.description).clone()
    }

    /// Risoluzione fisica del monitor del bersaglio: quello della finestra (anche minimizzata, dove
    /// stava prima) o il principale se non c'è; il monitor scelto; per Auto il bersaglio corrente.
    /// Prima di `start` si calcola al momento.
    fn native_size(&self) -> Option<(u32, u32)> {
        if self.watcher.is_some() {
            return *lock(&self.native);
        }
        monitor_pixels(self.target.initial_monitor().ok()?)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Drop for CaptureSource {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Stato del thread di sorveglianza.
struct Watch {
    target: Target,
    cursor: bool,
    prefer_dda: bool,
    fps: u32,
    slots: Arc<FrameSlots>,
    stats: Arc<Stats>,
    description: Arc<Mutex<String>>,
    native: Arc<Mutex<Option<(u32, u32)>>>,
    current: Option<(Resolved, Capture)>,
    health: Health,
    /// Monitor su cui sta il bersaglio, e se è pilotato da un adattatore diverso dal device.
    target_monitor: Option<HMONITOR>,
    other_adapter: bool,
    /// Adattatori già provati: (LUID, si può creare un device lì sopra).
    usable_adapters: Vec<(i64, bool)>,
}

/// `true` se conviene riaprire device e sorgente: il bersaglio è su un adattatore diverso da quello
/// del device *e* su quell'adattatore si può creare un device. Se non si può (adattatore solo
/// display, virtuale, senza video), [`crate::device_for_monitor`] ripiegherebbe di nuovo sul
/// predefinito: segnalarlo farebbe riavviare la sessione all'infinito.
fn should_reopen(target_luid: Option<i64>, device_luid: i64, usable: impl FnOnce(i64) -> bool) -> bool {
    target_luid.is_some_and(|l| l != device_luid && usable(l))
}

enum Capture {
    Wgc(WgcCapture),
    Dda(DdaCapture),
}

impl Capture {
    fn is_closed(&self) -> bool {
        match self {
            Self::Wgc(c) => c.is_closed(),
            Self::Dda(c) => c.is_closed(),
        }
    }

    fn backend(&self) -> &'static str {
        match self {
            Self::Wgc(_) => "WGC",
            Self::Dda(_) => "Desktop Duplication",
        }
    }
}

// SAFETY: HWND/HMONITOR sono identificatori di sistema validi da qualunque thread; le sessioni WGC
// sono oggetti agili; le risorse D3D11 della sonda sono free-threaded e il contesto immediato è
// protetto da SetMultithreadProtected. Watch passa al thread di sorveglianza e da lì non esce.
unsafe impl Send for Watch {}

const POLL: Duration = Duration::from_millis(250);
const RETRY: Duration = Duration::from_secs(2);

/// Quanto il pacer aspetta il primo frame vero prima di consegnare il segnaposto.
pub const FIRST_FRAME_WAIT: Duration = Duration::from_millis(500);

/// Stato `Waiting` con questo testo: il bersaglio è passato a un monitor collegato a un'altra GPU
/// (la cattura prosegue, ma con copie tra adattatori). L'integrazione può riaprire device, sorgente
/// ed encoder con [`open_source`].
pub const ADAPTER_CHANGED: &str =
    "il bersaglio è su un monitor collegato a un'altra GPU: riapri la sorgente per catturare senza copie tra adattatori";

/// Fullscreen esclusivo: la cattura WGC di una finestra a schermo intero esclusivo dà frame neri o
/// fermi, Desktop Duplication del suo monitor no (come «Cattura schermo» di OBS, senza iniettare
/// nulla nel gioco). `Some(true)`: passa a Desktop Duplication; `Some(false)`: torna a WGC appena
/// il gioco non è più a schermo intero in primo piano (DDA vedrebbe il desktop).
fn exclusive_switch(on_dda: bool, frames_bad: bool, fullscreen_foreground: bool) -> Option<bool> {
    match (on_dda, frames_bad, fullscreen_foreground) {
        (false, true, true) => Some(true),
        (true, _, false) => Some(false),
        _ => None,
    }
}

/// Spostamento costante della griglia dei pts per le sorgenti catturate (unità da 100 ns, positivo =
/// pts prima del tick). Manopola di taratura: va regolata con la misura end-to-end lampo/bip.
///
/// Misure su questo PC (60 Hz, finestra GDI e monitor): WGC consegna il frame circa 13-14 ms
/// *prima* del suo `SystemRelativeTime`, che è quindi l'istante di visualizzazione (vblank), non
/// quello di cattura; tick − SystemRelativeTime ha mediana tra −8 e −11 ms a 30/60/144 fps. DDA
/// consegna 0,05 ms dopo `LastPresentTime` e tick − LastPresentTime vale circa mezzo periodo.
/// Con pts = tick il video sta circa 10 ms prima dell'istante di visualizzazione e mezzo periodo
/// dopo l'istante di produzione: in mezzo tra i due riferimenti possibili e nei ±20 ms richiesti
/// in entrambi i casi, mentre ogni spostamento ne favorisce uno a spese dell'altro. Per questo 0.
const CAPTURE_PTS_OFFSET: i64 = 0;

impl Watch {
    fn run(mut self, running: &AtomicBool) {
        let mut wait = POLL;
        while running.load(Ordering::Acquire) {
            let mut slept = Duration::ZERO;
            while slept < wait && running.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(50));
                slept += Duration::from_millis(50);
            }
            if !running.load(Ordering::Acquire) {
                break;
            }
            wait = match self.poll() {
                Ok(()) => POLL,
                Err(e) => {
                    self.stats.set_state(SourceState::Error(e.to_string()));
                    RETRY
                }
            };
        }
        self.current = None;
    }

    /// Apre la cattura del bersaglio: WGC, o per i monitor DDA (preferito o come ripiego).
    fn open(&self, r: Resolved) -> Result<Capture, SourceError> {
        let wgc = || -> Result<Capture, SourceError> {
            let (item, window) = match r {
                Resolved::Monitor(h) => (wgc::item_for_monitor(h)?, None),
                Resolved::Window(h) => (wgc::item_for_window(h)?, Some(h)),
            };
            let (slots, stats) = (self.slots.clone(), self.stats.clone());
            Ok(Capture::Wgc(WgcCapture::start(item, window, slots, stats, self.fps, self.cursor)?))
        };
        let Resolved::Monitor(h) = r else { return wgc() };
        let dda = || DdaCapture::start(h, self.slots.clone(), self.stats.clone(), self.cursor).map(Capture::Dda);
        if self.prefer_dda {
            dda().or_else(|e| {
                tracing::warn!("Desktop Duplication non disponibile ({e}): uso WGC");
                wgc()
            })
        } else {
            wgc().or_else(|e| {
                tracing::warn!("WGC non disponibile ({e}): uso Desktop Duplication");
                dda()
            })
        }
    }

    fn poll(&mut self) -> Result<(), SourceError> {
        // SAFETY: chiamata COM senza argomenti su un device valido.
        if let Err(e) = unsafe { self.slots.device.device.GetDeviceRemovedReason() } {
            self.current = None;
            return Err(SourceError::Failed(format!("device GPU perso: {e}")));
        }
        let now = Instant::now();
        let want = self.target.resolve()?;
        let gone = self
            .current
            .as_ref()
            .is_some_and(|(r, c)| c.is_closed() || matches!(r, Resolved::Window(h) if !is_window(*h)));
        if gone || self.current.as_ref().map(|(r, _)| *r) != want {
            self.current = None;
            self.health.reset(now);
            if let Some(r) = want {
                let cap = self.open(r)?;
                let desc = format!("{} · {}", r.describe(), cap.backend());
                tracing::info!(source = %desc, "cattura avviata");
                *lock(&self.description) = desc;
                self.current = Some((r, cap));
            }
        }
        let monitor = match &self.current {
            Some((Resolved::Monitor(h), _)) => Some(*h),
            // Per una finestra minimizzata MonitorFromWindow usa la posizione prima della riduzione.
            Some((Resolved::Window(h), _)) => window_monitor(*h),
            None => None,
        };
        if let Some(px) = monitor_pixels(monitor.unwrap_or_else(primary_monitor)) {
            *lock(&self.native) = Some(px);
        }
        if monitor != self.target_monitor {
            self.target_monitor = monitor;
            let luid = monitor.and_then(adapter_luid_for_monitor);
            let cache = &mut self.usable_adapters;
            self.other_adapter = should_reopen(luid, self.slots.device.adapter_luid, |l| {
                // Una prova per adattatore e per sessione: creare un device costa decine di ms.
                if let Some(&(_, ok)) = cache.iter().find(|(c, _)| *c == l) {
                    return ok;
                }
                let ok = GpuDevice::create(Some(l)).is_ok();
                cache.push((l, ok));
                ok
            });
            if self.other_adapter {
                tracing::warn!("{ADAPTER_CHANGED} (device {:?})", self.slots.device);
            }
        }
        let state = match &self.current {
            None => SourceState::Waiting(format!("in attesa di {}", self.target.label())),
            Some((Resolved::Window(h), _)) if is_minimized(*h) => {
                self.health.reset(now);
                SourceState::Waiting("finestra minimizzata".into())
            }
            Some(_) if self.other_adapter => SourceState::Waiting(ADAPTER_CHANGED.into()),
            // DDA del monitor vede tutto, fullscreen esclusivo compreso: un frame nero è davvero nero
            // (schermata di caricamento), non un gioco che sfugge alla cattura.
            Some((Resolved::Monitor(_), Capture::Dda(_))) => SourceState::Running,
            Some((r, _)) => {
                let foreground = matches!(r, Resolved::Window(h) if is_foreground(*h));
                let arrived = self.stats.arrived.load(Ordering::Relaxed);
                match self.health.check(&self.slots, arrived, foreground, now) {
                    Some(warning) => SourceState::Waiting(warning.into()),
                    None => SourceState::Running,
                }
            }
        };
        let frames_bad = matches!(state, SourceState::Waiting(ref m) if m == crate::health::BLACK_MSG || m == crate::health::FROZEN_MSG);
        self.stats.set_state(state);
        self.exclusive_fullscreen(frames_bad, now);
        Ok(())
    }

    /// Passa la finestra a Desktop Duplication del suo monitor quando è a schermo intero esclusivo,
    /// e torna a WGC quando non lo è più ([`exclusive_switch`]).
    fn exclusive_fullscreen(&mut self, frames_bad: bool, now: Instant) {
        let Some((Resolved::Window(h), cap)) = &self.current else { return };
        let (h, on_dda) = (*h, matches!(cap, Capture::Dda(_)));
        let Some(to_dda) = exclusive_switch(on_dda, frames_bad, is_foreground(h) && covers_monitor(h)) else { return };
        let r = Resolved::Window(h);
        let opened = if to_dda {
            match window_monitor(h) {
                Some(m) => DdaCapture::start(m, self.slots.clone(), self.stats.clone(), self.cursor).map(Capture::Dda),
                None => return,
            }
        } else {
            self.open(r)
        };
        match opened {
            Ok(cap) => {
                let desc = format!("{} · {}", r.describe(), cap.backend());
                tracing::info!(source = %desc, "schermo intero esclusivo: {}", if to_dda { "uso Desktop Duplication" } else { "torno a WGC" });
                *lock(&self.description) = desc;
                self.current = Some((r, cap));
                self.health.reset(now);
            }
            Err(e) => tracing::warn!("cambio di cattura per lo schermo intero non riuscito: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::mpsc;

    use cliprec_core::QpcTime;
    use windows::Win32::UI::WindowsAndMessaging::{SW_MINIMIZE, SW_SHOWNOACTIVATE};

    use super::*;
    use crate::pacer::grid_pts;
    use crate::testutil::{TestWindow, exclusive_capture, shared_capture};

    type Frame = (QpcTime, u64, bool, u32, u32);

    fn collect(src: &mut CaptureSource, fps: u32) -> mpsc::Receiver<Frame> {
        let (tx, rx) = mpsc::channel();
        src.start(fps, Box::new(move |f| tx.send((f.pts, f.index, f.repeated, f.width, f.height)).unwrap()))
            .unwrap();
        rx
    }

    fn assert_on_grid(frames: &[Frame], fps: u32) {
        let (pts0, idx0, ..) = frames[0];
        let start = pts0.add_hns(-grid_pts(QpcTime(0), idx0, fps).0);
        for &(pts, index, ..) in frames {
            assert_eq!(pts, grid_pts(start, index, fps), "pts fuori griglia al frame {index}");
        }
    }

    /// WGC sul monitor principale a 60 e 144 fps per 2 s. Una finestrella animata garantisce che il
    /// contenuto cambi (da 24H2 WGC non consegna frame se lo schermo è fermo). Nessun pixel letto.
    #[test]
    fn monitor_wgc_60_and_144_fps() {
        let _capture = shared_capture();
        let _anim = TestWindow::open("cliprec-test-anim", 320, 240);
        let (mw, mh) = monitor_pixels(primary_monitor()).unwrap();
        println!("MinUpdateInterval disponibile: {}", wgc::has_min_update_interval());
        for fps in [60u32, 144] {
            let device = crate::device_for_monitor(0).unwrap();
            let mut src = CaptureSource::new(device, Target::Monitor(0), false).prefer_desktop_duplication(false);
            let rx = collect(&mut src, fps);
            std::thread::sleep(Duration::from_secs(2));
            let running = src.stats();
            src.stop();
            let frames: Vec<_> = rx.try_iter().collect();
            let s = src.stats();
            let new = frames.iter().filter(|f| !f.2).count();
            println!(
                "monitor {mw}×{mh} a {fps} fps: consegnati {}, nuovi {new}, ripetuti {}, persi {}, arrivati da WGC {}; \
                 pts − cattura {:?} ms, arrivo − cattura {:?} ms",
                s.frames_delivered,
                s.frames_repeated,
                s.frames_dropped,
                src.frames_arrived(),
                src.latency_ms(),
                src.stats.arrival.percentiles_ms()
            );
            let expected = 2 * fps as usize;
            assert!(frames.len() >= expected * 95 / 100 && frames.len() <= expected + 2, "{} frame", frames.len());
            assert_on_grid(&frames, fps);
            assert_eq!(running.state, SourceState::Running);
            assert_eq!((running.source_width, running.source_height), (mw, mh));
            assert!(src.frames_arrived() >= 1, "WGC non ha consegnato nulla");
            assert!(src.describe().starts_with("Monitor 1"), "{}", src.describe());
        }
    }

    /// Desktop Duplication forzata sul monitor principale a 144 fps per 2 s (solo conteggi).
    #[test]
    fn monitor_desktop_duplication_144_fps() {
        let _capture = shared_capture();
        let _anim = TestWindow::open("cliprec-test-anim-dda", 320, 240);
        let (mw, mh) = monitor_pixels(primary_monitor()).unwrap();
        let device = crate::device_for_monitor(0).unwrap();
        // Con il puntatore (m17): il disegno passa dalla pipeline del puntatore a ogni frame.
        let mut src = CaptureSource::new(device, Target::Monitor(0), true).prefer_desktop_duplication(true);
        let rx = collect(&mut src, 144);
        std::thread::sleep(Duration::from_secs(2));
        let running = src.stats();
        let desc = src.describe();
        src.stop();
        let frames: Vec<_> = rx.try_iter().collect();
        let new = frames.iter().filter(|f| !f.2).count();
        println!(
            "{desc} a 144 fps: consegnati {}, nuovi {new}, arrivati da DDA {}; pts − cattura {:?} ms, arrivo − cattura {:?} ms",
            frames.len(),
            src.frames_arrived(),
            src.latency_ms(),
            src.stats.arrival.percentiles_ms()
        );
        assert!(desc.contains("Desktop Duplication"), "{desc}");
        assert!(frames.len() >= 273 && frames.len() <= 290, "{} frame", frames.len());
        assert_on_grid(&frames, 144);
        assert_eq!(running.state, SourceState::Running);
        assert_eq!((running.source_width, running.source_height), (mw, mh));
        assert!(src.frames_arrived() >= 1, "DDA non ha consegnato nulla");
    }

    /// Finestra propria catturata, ridimensionata, minimizzata e chiusa durante la cattura.
    #[test]
    fn window_capture_survives_resize_minimize_and_close() {
        let _capture = shared_capture();
        let title = format!("cliprec-test-window-{}", std::process::id());
        let win = TestWindow::open(&title, 320, 240);
        let device = crate::device_for_monitor(0).unwrap();
        let mut src = CaptureSource::new(device, Target::Window(title.clone()), false);
        let rx = collect(&mut src, 60);
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(src.stats().state, SourceState::Running);
        assert!(src.describe().contains(&title), "{}", src.describe());
        win.resize(480, 360);
        std::thread::sleep(Duration::from_millis(600));
        win.show(SW_MINIMIZE);
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(src.stats().state, SourceState::Waiting("finestra minimizzata".into()));
        win.show(SW_SHOWNOACTIVATE);
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(src.stats().state, SourceState::Running);
        drop(win);
        std::thread::sleep(Duration::from_millis(600));
        let after_close = src.stats();
        src.stop();
        let frames: Vec<_> = rx.try_iter().collect();
        // Il primo frame ha già la dimensione della finestra, non quella del monitor.
        let (_, _, repeated, w0, h0) = frames[0];
        assert!(!repeated && (200..=330).contains(&w0) && h0 <= 250, "primo frame {w0}×{h0}");
        let sizes: BTreeSet<(u32, u32)> = frames.iter().filter(|f| !f.2).map(|f| (f.3, f.4)).collect();
        println!("dimensioni consegnate: {sizes:?}; arrivati {}; {after_close:?}", src.frames_arrived());
        assert!(matches!(after_close.state, SourceState::Waiting(_)), "{:?}", after_close.state);
        assert_on_grid(&frames, 60);
        assert!(frames.len() >= 170, "il pacer non si è fermato: {} frame", frames.len());
        assert!(sizes.iter().all(|&(w, _)| w >= 200), "frame della finestra minimizzata consegnato: {sizes:?}");
        let small = sizes.iter().any(|&(w, h)| (200..=330).contains(&w) && h <= 250);
        let big = sizes.iter().any(|&(w, h)| (400..=500).contains(&w) && h >= 300);
        assert!(small && big, "ridimensionamento non visto: {sizes:?}");
    }

    /// Latenza tra la produzione del contenuto (`SystemRelativeTime`) e il pts consegnato, con WGC
    /// sulla finestra GDI propria che cambia colore ogni ~16 ms.
    #[test]
    fn wgc_window_latency_is_measured() {
        let _capture = shared_capture();
        for fps in [30u32, 60, 144] {
            let title = format!("cliprec-test-latency-{fps}-{}", std::process::id());
            let _win = TestWindow::open(&title, 320, 240);
            let device = crate::device_for_monitor(0).unwrap();
            let mut src = CaptureSource::new(device, Target::Window(title), false);
            let _rx = collect(&mut src, fps);
            std::thread::sleep(Duration::from_secs(2));
            src.stop();
            let (median, p95) = src.latency_ms().unwrap();
            let (a50, a95) = src.stats.arrival.percentiles_ms().unwrap();
            println!(
                "WGC finestra a {fps} fps: pts − SystemRelativeTime mediana {median:.2} ms, p95 {p95:.2} ms; \
                 arrivo − SystemRelativeTime mediana {a50:.2} ms, p95 {a95:.2} ms; nuovi {}",
                src.stats().frames_delivered - src.stats().frames_repeated
            );
            // SystemRelativeTime è l'istante di visualizzazione: arriva prima, e il tick (al più un
            // periodo dopo l'arrivo) resta prima o poco dopo. Fuori da qui la misura è cambiata.
            assert!(a50 < 0.0, "SystemRelativeTime non è più nel futuro: {a50} ms");
            let period = 1000.0 / f64::from(fps);
            assert!((-25.0..period).contains(&median), "pts − SystemRelativeTime {median} ms");
        }
    }

    /// Il «gioco» si chiude e si riapre: la sorgente aspetta e lo ritrova da sola.
    #[test]
    fn window_target_waits_and_finds_the_window_again() {
        let _capture = shared_capture();
        let title = format!("cliprec-test-restart-{}", std::process::id());
        let device = crate::device_for_monitor(0).unwrap();
        let mut src = CaptureSource::new(device, Target::Window(title.clone()), false);
        let rx = collect(&mut src, 30);
        assert!(matches!(src.stats().state, SourceState::Waiting(_)));
        // Permanenze minime fisse (la soglia dei frame sotto le conta), poi attese sullo stato fino a 3 s:
        // sotto carico (altri test GPU in parallelo) la sorveglianza trova la finestra più tardi.
        let wait_for = |src: &CaptureSource, ok: &dyn Fn(&SourceState) -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while !ok(&src.stats().state) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            src.stats().state
        };
        for _ in 0..2 {
            let win = TestWindow::open(&title, 240, 180);
            std::thread::sleep(Duration::from_millis(700));
            let state = wait_for(&src, &|s| *s == SourceState::Running);
            assert_eq!(state, SourceState::Running);
            drop(win);
            std::thread::sleep(Duration::from_millis(500));
            let state = wait_for(&src, &|s| matches!(s, SourceState::Waiting(_)));
            assert!(matches!(state, SourceState::Waiting(_)), "{state:?}");
        }
        src.stop();
        let frames: Vec<_> = rx.try_iter().collect();
        assert_on_grid(&frames, 30);
        // Circa 72 frame nominali in 2,4 s a 30 fps; sotto carico (altri test GPU) se ne perdono alcuni.
        assert!(frames.len() >= 54, "{} frame", frames.len());
    }

    /// C1: finestra minimizzata all'avvio. La dimensione nativa è quella del monitor, il segnaposto ha
    /// quella dimensione e nessun frame della finestra minimizzata (~146×28) viene consegnato, nemmeno
    /// al ripristino (quando il frame pool è ancora della dimensione minimizzata).
    #[test]
    fn minimized_window_at_start_never_yields_tiny_frames() {
        let _capture = shared_capture();
        let title = format!("cliprec-test-minimized-{}", std::process::id());
        let win = TestWindow::open(&title, 320, 240);
        win.show(SW_MINIMIZE);
        let hwnd = win.hwnd();
        for _ in 0..100 {
            if is_minimized(hwnd) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(is_minimized(hwnd), "la finestra di prova non si è minimizzata");
        let monitor = monitor_pixels(window_monitor(hwnd).unwrap_or_else(primary_monitor)).unwrap();
        let device = crate::device_for_monitor(0).unwrap();
        let mut src = CaptureSource::new(device, Target::Window(title), false);
        let rx = collect(&mut src, 60);
        std::thread::sleep(Duration::from_millis(1000));
        let minimized_state = src.stats().state;
        win.show(SW_SHOWNOACTIVATE);
        std::thread::sleep(Duration::from_millis(1000));
        let native = src.native_size();
        src.stop();
        drop(win);
        let frames: Vec<_> = rx.try_iter().collect();
        let sizes: BTreeSet<(u32, u32)> = frames.iter().map(|f| (f.3, f.4)).collect();
        println!("monitor {monitor:?}, nativa {native:?}, dimensioni consegnate {sizes:?}, {minimized_state:?}");
        assert_eq!(minimized_state, SourceState::Waiting("finestra minimizzata".into()));
        assert!(sizes.iter().all(|&(w, h)| w >= 200 && h >= 150), "frame minuscoli consegnati: {sizes:?}");
        assert_eq!((frames[0].3, frames[0].4), monitor, "il segnaposto non ha la dimensione nativa");
        assert!(sizes.iter().any(|&(w, _)| (200..=330).contains(&w)), "la finestra ripristinata non è arrivata");
        assert_eq!(native, Some(monitor));
    }

    /// C1: `native_size` è la risoluzione del monitor del bersaglio, anche prima di `start` e anche
    /// se la finestra non c'è (allora il principale); la sintetica dà la sua dimensione.
    #[test]
    fn native_size_is_the_target_monitor() {
        let primary = monitor_pixels(primary_monitor()).unwrap();
        let device = crate::device_for_monitor(0).unwrap();
        let src = CaptureSource::new(device.clone(), Target::Monitor(0), false);
        assert_eq!(src.native_size(), Some(primary));
        let src = CaptureSource::new(device.clone(), Target::Window("cliprec-nessuna-finestra-così".into()), false);
        assert_eq!(src.native_size(), Some(primary));
        let auto = Target::Auto { games: Vec::new(), any_fullscreen: false };
        assert_eq!(CaptureSource::new(device.clone(), auto, false).native_size(), Some(primary));
        assert_eq!(crate::SyntheticSource::new(device, 640, 360).native_size(), Some((640, 360)));
    }

    /// Una finestra tutta nera porta allo stato `Waiting` con l'avviso sui frame neri.
    #[test]
    fn black_window_is_reported() {
        let _capture = shared_capture();
        let title = format!("cliprec-test-black-{}", std::process::id());
        let _win = TestWindow::open_with(&title, 320, 240, true);
        let device = crate::device_for_monitor(0).unwrap();
        let mut src = CaptureSource::new(device, Target::Window(title), false);
        let _rx = collect(&mut src, 30);
        std::thread::sleep(Duration::from_millis(5500));
        let state = src.stats().state;
        src.stop();
        assert_eq!(state, SourceState::Waiting(crate::health::BLACK_MSG.into()));
    }

    /// Device su un adattatore che non pilota il monitor (qui l'Intel integrata, con lo schermo
    /// sulla NVIDIA): la cattura prosegue tra adattatori e lo stato lo segnala.
    #[test]
    fn capture_on_other_adapter_is_reported() {
        let _capture = exclusive_capture();
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1};
        let monitor_luid = adapter_luid_for_monitor(primary_monitor()).unwrap();
        // SAFETY: solo test; chiamate COM su interfacce valide.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.unwrap();
        let other = (0..)
            // SAFETY: vedi sopra.
            .map_while(|i| unsafe { factory.EnumAdapters1(i) }.ok())
            // SAFETY: vedi sopra.
            .filter_map(|a| unsafe { a.GetDesc1() }.ok())
            .filter(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0)
            .map(|d| cliprec_core::gpu::luid_to_i64(d.AdapterLuid))
            .find(|&l| l != monitor_luid);
        let Some(other) = other else {
            println!("un solo adattatore hardware: niente da verificare");
            return;
        };
        let device = GpuDevice::create(Some(other)).unwrap();
        println!("device su {device:?}, monitor su {monitor_luid:#x}");
        let mut src = CaptureSource::new(device, Target::Monitor(0), false);
        let rx = collect(&mut src, 60);
        std::thread::sleep(Duration::from_millis(700));
        let state = src.stats().state;
        let desc = src.describe();
        src.stop();
        let n = rx.try_iter().count();
        println!("{desc}: {n} frame, arrivati {}, {state:?}", src.frames_arrived());
        assert_eq!(state, SourceState::Waiting(ADAPTER_CHANGED.into()));
        assert!(n >= 10, "{n} frame");
    }

    /// Un device già di ripiego (adattatore del monitor inutilizzabile) non deve mai produrre
    /// ADAPTER_CHANGED, altrimenti l'app riavvierebbe la sessione all'infinito.
    #[test]
    fn fallback_device_never_asks_to_reopen() {
        let (monitor_gpu, default_gpu) = (0xae43, 0xcea9);
        // Ripiego: il bersaglio è sull'adattatore che non regge un device.
        assert!(!should_reopen(Some(monitor_gpu), default_gpu, |_| false));
        // Adattatore diverso e usabile: riaprire conviene.
        assert!(should_reopen(Some(monitor_gpu), default_gpu, |_| true));
        // Stesso adattatore o adattatore ignoto: niente da fare, e nessuna prova di creazione.
        assert!(!should_reopen(Some(default_gpu), default_gpu, |_| unreachable!()));
        assert!(!should_reopen(None, default_gpu, |_| unreachable!()));
    }

    #[test]
    fn exclusive_fullscreen_switches_to_dda_and_back() {
        // WGC, frame neri o fermi, gioco a schermo intero in primo piano: Desktop Duplication.
        assert_eq!(exclusive_switch(false, true, true), Some(true));
        // Frame neri ma non a schermo intero (caricamento, finestra nera): resta WGC.
        assert_eq!(exclusive_switch(false, true, false), None);
        // Su DDA finché il gioco resta a schermo intero in primo piano, anche con frame buoni.
        assert_eq!(exclusive_switch(true, false, true), None);
        // Alt+Tab o modalità finestra: si torna a WGC (DDA vedrebbe il desktop).
        assert_eq!(exclusive_switch(true, false, false), Some(false));
        assert_eq!(exclusive_switch(false, false, true), None);
    }

    #[test]
    fn auto_mode_picks_listed_or_any_fullscreen_app() {
        let primary = primary_monitor();
        let game = HWND(0x1234 as *mut _);
        let fs = || Some((game, "Game.exe".to_owned()));
        let games = vec!["osu!.exe".to_owned(), "game".to_owned()];
        assert_eq!(auto_pick(fs(), &games, false, primary), Resolved::Window(game));
        assert_eq!(auto_pick(Some((game, "browser.exe".into())), &games, false, primary), Resolved::Monitor(primary));
        assert_eq!(auto_pick(Some((game, "browser.exe".into())), &games, true, primary), Resolved::Window(game));
        assert_eq!(auto_pick(None, &games, true, primary), Resolved::Monitor(primary));
    }

    #[test]
    fn process_target_finds_own_window_and_missing_monitor_fails() {
        let title = format!("cliprec-test-proc-{}", std::process::id());
        let win = TestWindow::open(&title, 200, 150);
        let exe = std::env::current_exe().unwrap();
        let exe = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert!(matches!(Target::Process(exe).resolve().unwrap(), Some(Resolved::Window(_))));
        drop(win);
        assert!(Target::Window("cliprec-nessuna-finestra-così".into()).resolve().unwrap().is_none());
        let device = crate::device_for_monitor(0).unwrap();
        let mut src = CaptureSource::new(device, Target::Monitor(99), false);
        assert!(matches!(src.start(60, Box::new(|_| ())), Err(SourceError::NotFound(_))));
    }
}
