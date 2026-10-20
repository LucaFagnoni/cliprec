//! Motore di ClipRec: collega sorgenti, encoder, buffer e salvataggio ai comandi della shell.
//!
//! Un thread esegue [`run`]; una [`Session`] tiene sorgenti ed encoder accesi e scrive nel
//! [`ReplayBuffer`], che sopravvive ai riavvii della sessione (device perso, configurazione
//! ricaricata) e a un panic del motore. La hotkey prende uno snapshot del buffer (lock
//! brevissimo) e un thread a bassa priorità scrive il file.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cliprec_buffer::{BufferConfig, ReplayBuffer};

use crate::i18n::{K, tr};
use cliprec_core::config::{Config, default_max_bitrate_kbps};
use cliprec_core::{
    AppCommand, AudioTrackSink, ClipKind, EncodeError, EngineEvent, EngineStatus, GpuDevice, GpuFrame, QpcTime,
    SourceState, VideoSource,
};

/// Clip mostrate nel menu della tray.
const RECENT_MAX: usize = 5;

/// Sorgenti iniettabili (test e sorgenti sintetiche); `None` = dalla configurazione.
#[derive(Default)]
pub struct SourceOverrides {
    pub video: Option<(GpuDevice, Box<dyn VideoSource>)>,
    pub audio: Option<Vec<Box<dyn cliprec_core::AudioSource>>>,
}

/// Stato che dura più di una sessione: ultime clip, salvataggi in corso, encoder in uso (per avvisare
/// quando un cambio di encoder svuota il buffer anche dopo un riavvio della sessione).
#[derive(Clone, Default)]
pub struct Shared {
    recent: Arc<Mutex<VecDeque<PathBuf>>>,
    saving: Arc<AtomicU32>,
    encoder_name: Arc<Mutex<String>>,
    /// Scritture su disco in corso e massimo osservato (per i test della coda dei salvataggi).
    writing: Arc<AtomicU32>,
    writing_peak: Arc<AtomicU32>,
    /// Codec che si aprono davvero su questo PC (`probe`), per lo stato e le impostazioni.
    available_codecs: Arc<Mutex<Vec<cliprec_core::VideoCodec>>>,
    /// Nomi degli encoder che si aprono (es. `hevc_amf`), nello stesso rilevamento.
    available_encoders: Arc<Mutex<Vec<String>>>,
    /// Esito dell'ultima clip di prova.
    last_test: Arc<Mutex<Option<String>>>,
    /// Rilevamento dei codec in corso (uno solo per processo).
    probing: Arc<AtomicBool>,
    /// Coda dei salvataggi: un solo thread a bassa priorità li scrive uno alla volta, così il picco di
    /// memoria non cresce con le pressioni ravvicinate e nessuna clip va persa (M5).
    queue: Arc<Mutex<Option<Sender<Job>>>>,
}

/// Lavoro per il thread dei salvataggi.
type Job = Box<dyn FnOnce() + Send>;

/// Attende la fine di un salvataggio accodato.
pub struct SaveHandle(Receiver<()>);

impl SaveHandle {
    /// Ritorna quando il salvataggio è finito (riuscito o no).
    pub fn join(self) -> Result<(), std::sync::mpsc::RecvError> {
        self.0.recv()
    }
}

impl Shared {
    /// Accoda un lavoro sul thread dei salvataggi, creandolo alla prima clip.
    fn enqueue(&self, job: Job) -> Option<SaveHandle> {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let job: Job = Box::new(move || {
            job();
            let _ = done_tx.send(());
        });
        let mut queue = self.queue.lock().ok()?;
        if queue.is_none() {
            let (tx, rx) = std::sync::mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("clip-save".into())
                .spawn(move || {
                    lower_thread_priority();
                    // Un panic perde solo quel lavoro: il thread resta vivo per i salvataggi successivi.
                    for job in rx {
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    }
                })
                .ok()?;
            *queue = Some(tx);
        }
        queue.as_ref()?.send(job).ok()?;
        Some(SaveHandle(done_rx))
    }

    /// Aspetta fino a `timeout` che i salvataggi in corso finiscano (uscita dall'app).
    fn wait_savers(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.saving.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Buffer dimensionato dalla configurazione, con un tetto pari a un quarto della RAM del PC.
pub fn new_buffer(config: &Config, events: &Sender<EngineEvent>) -> Arc<ReplayBuffer> {
    let audio_seconds = f64::from(config.clips.audio_seconds.max(config.clips.video_seconds));
    let video_seconds = f64::from(config.clips.video_seconds);
    // Senza tetto esplicito: stima generosa col bitrate massimo predefinito più alto (H.264 4K).
    let wanted = match config.memory.max_mb {
        Some(mb) => mb as usize * 1024 * 1024,
        None => {
            let kbps = config
                .video
                .max_bitrate_kbps
                .unwrap_or_else(|| default_max_bitrate_kbps(cliprec_core::VideoCodec::H264, 2160, config.video.fps));
            let kbps = if config.video.enabled { kbps } else { 0 };
            BufferConfig::estimate_bytes(u64::from(kbps) * 1000, video_seconds, audio_seconds, config.audio.plan().tracks.len().max(1))
        }
    };
    let max_bytes = buffer_cap(wanted, total_ram());
    // La stima automatica è volutamente larga (bitrate massimo 4K): ridurla in silenzio è normale.
    // Un limite scelto dall'utente che non sta nel tetto invece si segnala.
    if config.memory.max_mb.is_some() && max_bytes < wanted {
        let _ = events.send(EngineEvent::Warning(tr(
            K::EngRamLow,
            &[&(wanted >> 20).to_string(), &(max_bytes >> 20).to_string()],
        )));
    }
    Arc::new(ReplayBuffer::new(BufferConfig { video_seconds, audio_seconds, max_bytes, lock_pages: config.memory.lock_pages }))
}

/// Tetto del buffer: `wanted`, ma al massimo un quarto della RAM fisica (`total`, 0 = ignota).
/// Mai dalla RAM libera del momento: all'avvio di un gioco può scendere a poche decine di MB e il
/// buffer resterebbe minuscolo per tutta la sessione, mentre l'arena cresce solo quando serve.
fn buffer_cap(wanted: usize, total: usize) -> usize {
    if total == 0 { wanted } else { wanted.min(total / 4) }
}

fn total_ram() -> usize {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    // SAFETY: struttura valida con dwLength impostato come richiesto dall'API.
    match unsafe { GlobalMemoryStatusEx(&mut m) } {
        Ok(()) => usize::try_from(m.ullTotalPhys).unwrap_or(usize::MAX),
        Err(_) => 0,
    }
}

/// Una sessione di registrazione attiva: sorgenti ed encoder che riempiono il buffer.
pub struct Session {
    config: Config,
    buffer: Arc<ReplayBuffer>,
    shared: Shared,
    video: Option<Box<dyn VideoSource>>,
    audio: Option<cliprec_audio::AudioEngine>,
    encoder_name: Arc<Mutex<String>>,
    /// Il device GPU è perso: il loop ricrea la sessione (il buffer resta).
    restart: Arc<AtomicBool>,
    /// `false` dopo lo stop: un thread di apertura dell'encoder ancora in corso non deve più toccare
    /// il buffer (lo prenderà la sessione nuova con i suoi parametri).
    alive: Arc<Mutex<bool>>,
    /// Prossimo tentativo di riavviare il video assente o in errore, e attesa attuale.
    video_retry: (Instant, Duration),
    events: Sender<EngineEvent>,
    /// Ultimo stato visto per sorgente, per avvisare solo quando cambia.
    states: Vec<(String, SourceState)>,
    /// fps effettivi: ultimo campione (istante, frame consegnati) e valore calcolato (m23).
    fps_sample: (Instant, u64),
    fps_effective: f64,
}

impl Session {
    /// Avvia audio e video. Se una delle due parti non parte, l'altra prosegue e un avviso lo dice;
    /// errore solo se non parte nulla.
    pub fn start(
        config: &Config,
        buffer: Arc<ReplayBuffer>,
        shared: Shared,
        overrides: SourceOverrides,
        events: Sender<EngineEvent>,
    ) -> anyhow::Result<Self> {
        let encoder_name = shared.encoder_name.clone();
        let restart = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(Mutex::new(true));
        let warn = |what: &str, e: &dyn std::fmt::Display| {
            let _ = events.send(EngineEvent::Warning(tr(K::EngNotStarted, &[what, &e.to_string()])));
        };

        listen_process_events(config, &events);
        if overrides.audio.is_none() {
            block_exclusive(config, &events);
        }
        let sink = buffer.clone() as Arc<dyn AudioTrackSink>;
        let audio = match overrides.audio {
            // Sorgenti di prova al posto di quelle catturate dal piano della configurazione.
            Some(sources) => {
                cliprec_audio::AudioEngine::with_plan(sources, &[], &[], &[], &config.audio.plan().tracks, sink).map(Some)
            }
            None if !config.audio.plan().captures.is_empty() => cliprec_audio::AudioEngine::start(&config.audio, sink).map(Some),
            None => Ok(None),
        }
        .unwrap_or_else(|e| {
            warn("audio", &e);
            None
        });

        let mut session = Self {
            config: config.clone(),
            buffer,
            shared,
            video: None,
            audio,
            encoder_name,
            restart,
            alive,
            video_retry: (Instant::now(), Duration::from_secs(5)),
            events: events.clone(),
            states: Vec::new(),
            fps_sample: (Instant::now(), 0),
            fps_effective: 0.0,
        };
        if config.video.enabled {
            let opened = match overrides.video {
                Some(v) => Ok(v),
                None => cliprec_capture::open_source(&config.video),
            };
            if let Err(e) = session.start_video(opened) {
                warn("video", &tr(K::EngVideoOnlyAudio, &[&e.to_string()]));
            }
        }
        if session.video.is_none() && session.audio.is_none() && !config.video.enabled {
            anyhow::bail!("nessuna sorgente audio o video attiva");
        }
        Ok(session)
    }

    /// Avvia cattura ed encoder video sul device della sorgente.
    fn start_video(&mut self, opened: Result<(GpuDevice, Box<dyn VideoSource>), cliprec_core::SourceError>) -> anyhow::Result<()> {
        let (device, mut source) = opened?;
        // Un flag nuovo per ogni avvio: un thread di apertura dell'avvio precedente non tocca più il buffer.
        self.alive = Arc::new(Mutex::new(true));
        let high = self.config.general.gpu_priority != "normal";
        if let Err(e) = cliprec_encode::set_gpu_priority(&device, high) {
            tracing::warn!("priorità GPU: {e}");
        }
        // Mentre un gioco carica, CPU satura: con la classe normale pacer e compressore FLAC restano
        // indietro e i tick si perdono. «Sopra il normale» come l'opzione di OBS, mai «alta».
        use windows::Win32::System::Threading::{ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, NORMAL_PRIORITY_CLASS, SetPriorityClass};
        // SAFETY: pseudo-handle del processo corrente, sempre valida.
        if let Err(e) = unsafe { SetPriorityClass(GetCurrentProcess(), if high { ABOVE_NORMAL_PRIORITY_CLASS } else { NORMAL_PRIORITY_CLASS }) } {
            tracing::warn!("priorità del processo: {e}");
        }
        self.probe_codecs(&device);
        let on_frame = video_callback(
            &self.config,
            device,
            source.native_size(),
            self.buffer.clone(),
            self.encoder_name.clone(),
            self.events.clone(),
            self.restart.clone(),
            self.alive.clone(),
        );
        source.start(self.config.video.fps, on_frame)?;
        self.video = Some(source);
        Ok(())
    }

    /// Rileva una volta per processo, su un thread a parte, i codec che si aprono davvero (m14).
    fn probe_codecs(&self, device: &GpuDevice) {
        let known = self.shared.available_codecs.lock().map(|c| !c.is_empty()).unwrap_or(true);
        if known || self.shared.probing.swap(true, Ordering::SeqCst) {
            return;
        }
        let (device, shared) = (device.clone(), self.shared.clone());
        let spawned = std::thread::Builder::new().name("encoder-probe".into()).spawn(move || {
            lower_thread_priority();
            let mut codecs: Vec<cliprec_core::VideoCodec> = Vec::new();
            let mut encoders: Vec<String> = Vec::new();
            // Un panic nel driver non lascia `probing` acceso per sempre.
            let probed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cliprec_encode::probe(&device)));
            for (codec, name) in probed.unwrap_or_default() {
                if !codecs.contains(&codec) {
                    codecs.push(codec);
                }
                encoders.push(name);
            }
            if let Ok(mut e) = shared.available_encoders.lock() {
                *e = encoders;
            }
            if let Ok(mut c) = shared.available_codecs.lock() {
                *c = codecs;
            }
            shared.probing.store(false, Ordering::SeqCst);
        });
        if spawned.is_err() {
            self.shared.probing.store(false, Ordering::SeqCst);
        }
    }

    /// Ricrea solo cattura ed encoder (configurazione video cambiata); buffer e audio restano.
    fn restart_video(&mut self) {
        if let Ok(mut a) = self.alive.lock() {
            *a = false;
        }
        if let Some(mut v) = self.video.take() {
            v.stop();
        }
        if self.config.video.enabled
            && let Err(e) = self.start_video(cliprec_capture::open_source(&self.config.video))
        {
            let _ = self.events.send(EngineEvent::Warning(tr(K::EngNotStarted, &["video", &e.to_string()])));
        }
    }

    /// Ricrea solo le sorgenti audio (configurazione audio cambiata); buffer e video restano.
    fn restart_audio(&mut self) {
        if let Some(mut a) = self.audio.take() {
            a.stop();
        }
        if self.config.audio.plan().captures.is_empty() {
            return;
        }
        listen_process_events(&self.config, &self.events);
        block_exclusive(&self.config, &self.events);
        let sink = self.buffer.clone() as Arc<dyn AudioTrackSink>;
        match cliprec_audio::AudioEngine::start(&self.config.audio, sink) {
            Ok(a) => self.audio = Some(a),
            Err(e) => {
                let _ = self.events.send(EngineEvent::Warning(tr(K::EngNotStarted, &["audio", &e.to_string()])));
            }
        }
    }

    /// Video assente o in errore con il video abilitato: riprova con un'attesa crescente (5 s → 60 s).
    fn retry_video(&mut self) {
        if !self.config.video.enabled || Instant::now() < self.video_retry.0 {
            return;
        }
        let dead = match &self.video {
            None => true,
            Some(v) => matches!(v.stats().state, SourceState::Error(_)),
        };
        if !dead {
            self.video_retry.1 = Duration::from_secs(5);
            return;
        }
        if let Ok(mut a) = self.alive.lock() {
            *a = false;
        }
        if let Some(mut v) = self.video.take() {
            v.stop();
        }
        let wait = self.video_retry.1;
        self.video_retry = (Instant::now() + wait, (wait * 2).min(Duration::from_secs(60)));
        match self.start_video(cliprec_capture::open_source(&self.config.video)) {
            Ok(()) => {
                let _ = self.events.send(EngineEvent::Warning(tr(K::EngVideoRestarted, &[])));
            }
            Err(e) => tracing::warn!("video non ripartito: {e}; nuovo tentativo tra {} s", wait.as_secs()),
        }
    }

    pub fn status(&self) -> EngineStatus {
        let stats = self.buffer.stats();
        let capture = self.video.as_ref().map(|v| v.stats()).unwrap_or_default();
        let problem = status_problem(&self.states);
        EngineStatus {
            buffer_running: true,
            video_enabled: self.video.is_some(),
            video_seconds: stats.video_seconds,
            audio_seconds: stats.audio_seconds,
            buffer_bytes: stats.bytes as u64,
            ram_locked: stats.locked,
            encoder: self.encoder_name.lock().map(|n| n.clone()).unwrap_or_default(),
            source: self.video.as_ref().map(|v| v.describe()).unwrap_or_default(),
            fps: if self.video.is_some() { self.fps_effective } else { 0.0 },
            frames_dropped: capture.frames_dropped,
            saving: self.shared.saving.load(Ordering::SeqCst),
            recent_clips: self.shared.recent.lock().map(|r| r.iter().cloned().collect()).unwrap_or_default(),
            last_error: if stats.memory_limited {
                Some(tr(K::EngRamLimited, &[&format!("{:.0}", stats.video_seconds)]))
            } else {
                problem
            },
            available_codecs: self.shared.available_codecs.lock().map(|c| c.clone()).unwrap_or_default(),
            available_encoders: self.shared.available_encoders.lock().map(|e| e.clone()).unwrap_or_default(),
            last_test: self.shared.last_test.lock().map(|t| t.clone()).unwrap_or_default(),
        }
    }

    /// Ritardo attuale del mixer (adattivo: segue la sorgente più lenta), per chiudere lo snapshot audio.
    fn mix_latency(&self) -> i64 {
        self.audio.as_ref().map_or(cliprec_audio::MIX_LATENCY_HNS, cliprec_audio::AudioEngine::mix_latency_hns)
    }

    /// Aggiorna le misure dal vivo: fps effettivi = frame consegnati dal pacer al secondo, su
    /// finestre di almeno un secondo (m23). Chiamata dal loop ogni 500 ms.
    fn measure(&mut self) {
        let Some(v) = &self.video else { return };
        let delivered = v.stats().frames_delivered;
        let (t0, d0) = self.fps_sample;
        let dt = t0.elapsed().as_secs_f64();
        if dt >= 1.0 {
            if d0 > 0 || self.fps_effective > 0.0 {
                self.fps_effective = delivered.saturating_sub(d0) as f64 / dt;
            }
            self.fps_sample = (Instant::now(), delivered);
        }
    }

    /// Clip di prova da 5 s («Prova» delle impostazioni): dopo il salvataggio scrive nello stato un
    /// esito leggibile, con il video presente e non nero e ogni traccia audio non silenziosa (m24).
    pub fn save_test(&self) -> Option<SaveHandle> {
        let kind = if self.video.is_some() { ClipKind::Video } else { ClipKind::Audio };
        let end = QpcTime::now();
        let audio_end = audio_snapshot_end(end, &self.config, self.mix_latency());
        let audio = self
            .buffer
            .snapshot_audio_refs(audio_end.add_hns(-5 * cliprec_core::HNS_PER_SEC), audio_end)
            .map(|a| a.select_tracks(self.config.audio.plan().clip(kind)));
        let video_ok = self.video.as_ref().map(|v| {
            let st = v.stats();
            let encoder = self.encoder_name.lock().map(|n| n.clone()).unwrap_or_default();
            let ok = st.frames_delivered > 0 && st.state == SourceState::Running && !encoder.is_empty();
            (ok, encoder)
        });
        let saved = self.save(kind, 5);
        let shared = self.shared.clone();
        // Accodato dopo il salvataggio: la coda è unica, quindi gira quando la clip è scritta.
        let verdict = self.shared.enqueue(Box::new(move || {
            let mut parts = Vec::new();
            match video_ok {
                Some((true, encoder)) => parts.push(tr(K::EngTestVideoOk, &[&encoder])),
                Some((false, _)) => parts.push(tr(K::EngTestVideoMissing, &[])),
                None => {}
            }
            if let Some(refs) = audio {
                for t in refs.render().tracks {
                    let peak = t.samples.iter().fold(0f32, |m, s| m.max(s.abs()));
                    let key = if peak > 1e-3 { K::EngTestTrackOk } else { K::EngTestTrackSilent };
                    parts.push(tr(key, &[&t.name]));
                }
            }
            if let Ok(mut l) = shared.last_test.lock() {
                *l = Some(parts.join(" · "));
            }
        }));
        drop(saved);
        verdict
    }

    /// Avvisa quando una sorgente entra in attesa o in errore (la shell deduplica per tipo).
    fn poll_states(&mut self) {
        let mut now: Vec<(String, SourceState)> = self.audio.as_ref().map(|a| a.states()).unwrap_or_default();
        if let Some(v) = &self.video {
            let state = v.stats().state;
            // Il bersaglio è passato su un monitor di un'altra GPU: device, sorgente ed encoder vanno
            // ricreati sull'adattatore giusto (catturare tra GPU diverse scende a pochi fps).
            if state == SourceState::Waiting(cliprec_capture::ADAPTER_CHANGED.to_string()) {
                self.restart.store(true, Ordering::SeqCst);
            }
            now.push((v.describe(), state));
        }
        for (name, st) in &now {
            let changed = !self.states.iter().any(|(n, s)| n == name && s == st);
            if changed
                && let SourceState::Waiting(m) | SourceState::Error(m) = st
                && warns_about(m)
            {
                let _ = self.events.send(EngineEvent::Warning(format!("{name}: {m}")));
            }
        }
        self.states = now;
    }

    /// Prende lo snapshot degli ultimi `seconds` secondi (lock brevissimo, prima di qualunque accesso al
    /// disco) e accoda la scrittura sul thread dei salvataggi. Senza video nel buffer (encoder non
    /// pronto) la hotkey video salva la clip audio, con un avviso.
    pub fn save(&self, kind: ClipKind, seconds: u32) -> Option<SaveHandle> {
        let pressed = Instant::now();
        let end = QpcTime::now();
        let audio_end = audio_snapshot_end(end, &self.config, self.mix_latency());
        let events = self.events.clone();
        let fail = move |events: &Sender<EngineEvent>, kind: ClipKind, reason: String| {
            let _ = events.send(EngineEvent::ClipFailed { kind, reason });
        };
        let stats = self.buffer.stats();
        // Il video si ferma dove finisce l'audio già scritto: con un mixer in ritardo (dispositivo
        // Bluetooth, ritardo negativo) la clip finirebbe altrimenti con l'audio muto (m3).
        let video_end = if self.audio.is_some() { audio_end } else { end };
        let video = match kind {
            ClipKind::Video => match self.buffer.snapshot_video(f64::from(seconds), video_end) {
                Some(v) => Some(v),
                None => {
                    let _ = events.send(EngineEvent::Warning(tr(K::EngVideoFallbackAudio, &[])));
                    None
                }
            },
            ClipKind::Audio => None,
        };
        let kind = if video.is_some() { ClipKind::Video } else { ClipKind::Audio };
        let audio_start = match &video {
            Some(v) => v.start().unwrap_or(end),
            None => audio_end.add_hns(-i64::from(seconds) * cliprec_core::HNS_PER_SEC),
        };
        // Sotto lock solo i riferimenti ai blocchi FLAC; la decompressione è nel thread di salvataggio.
        let audio = self
            .buffer
            .snapshot_audio_refs(audio_start, audio_end)
            .map(|a| a.select_tracks(self.config.audio.plan().clip(kind)))
            .filter(|a| a.track_count() > 0);
        if video.is_none() && audio.is_none() {
            fail(&events, kind, tr(if kind == ClipKind::Video { K::EngVideoEmpty } else { K::EngAudioEmpty }, &[]));
            return None;
        }
        if let Some(v) = &video {
            let last = v.packets.iter().map(|p| p.pts.add_hns(p.duration)).max().unwrap_or(end);
            let stale = end.hns_since(last);
            if stale > 2 * cliprec_core::HNS_PER_SEC {
                let secs = format!("{:.0}", stale as f64 / cliprec_core::HNS_PER_SEC as f64);
                let _ = events.send(EngineEvent::Warning(tr(K::EngClipStale, &[&secs])));
            }
        }
        if kind == ClipKind::Video && stats.memory_limited {
            let _ = events.send(EngineEvent::Warning(tr(
                K::EngClipShortRam,
                &[&seconds.to_string(), &format!("{:.0}", stats.video_seconds)],
            )));
        }
        if let Some((name, SourceState::Waiting(m) | SourceState::Error(m))) =
            self.video.as_ref().map(|v| (v.describe(), v.stats().state))
        {
            let _ = events.send(EngineEvent::Warning(tr(K::EngClipVideoWaiting, &[&name, &m])));
        }
        let snapshot_ms = pressed.elapsed().as_secs_f64() * 1000.0;

        let folder = self.config.clips.folder();
        let game = self.video.as_ref().map(|v| game_name(&v.describe()));
        let name = clip_name(&self.config.clips.filename_template, kind, game);
        let opts = cliprec_mux::VideoClipOptions {
            container: self.config.clips.container,
            audio_bit_rate: self.config.clips.audio_bitrate_kbps.clamp(32, 512) * 1000,
        };
        let format = self.config.clips.audio_format;
        let memory_limited = stats.memory_limited;
        let shared = self.shared.clone();
        shared.saving.fetch_add(1, Ordering::SeqCst);
        let job: Job = Box::new(move || {
            /// Decrementa il contatore anche se il salvataggio va in panic.
            struct Done(Arc<AtomicU32>);
            impl Drop for Done {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _done = Done(shared.saving.clone());
            let now_writing = shared.writing.fetch_add(1, Ordering::SeqCst) + 1;
            shared.writing_peak.fetch_max(now_writing, Ordering::SeqCst);
            let write = move || -> Result<cliprec_core::SavedClip, String> {
                // Il disco si tocca solo qui, dopo lo snapshot (m1).
                std::fs::create_dir_all(&folder)
                    .map_err(|e| tr(K::EngFolder, &[&folder.display().to_string(), &e.to_string()]))?;
                let path = folder.join(name);
                // Il muxer aggiunge l'estensione e « (2)» se il nome esiste già.
                match video {
                    // L'audio si decomprime a blocchi mentre si scrive: il picco di memoria non cresce
                    // con durata e numero di tracce (M5).
                    Some(video) => {
                        cliprec_mux::write_video_clip(&path, &video, audio.as_ref(), &opts).map_err(|e| e.to_string())
                    }
                    None => {
                        let audio = audio.ok_or_else(|| tr(K::EngAudioEmpty, &[]))?;
                        cliprec_mux::write_audio_clip(&path, &audio, format).map_err(|e| e.to_string())
                    }
                }
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(write))
                .unwrap_or_else(|_| Err(tr(K::EngSaveInternal, &[])));
            shared.writing.fetch_sub(1, Ordering::SeqCst);
            match result {
                Ok(clip) => {
                    tracing::info!(
                        snapshot_ms,
                        file_ms = pressed.elapsed().as_secs_f64() * 1000.0,
                        secondi = clip.duration_secs,
                        byte = clip.bytes,
                        "clip {:?} salvata",
                        clip.kind
                    );
                    // Clip più corta di N per un motivo diverso dalla RAM (buffer appena avviato o
                    // svuotato da un cambio di encoder o di risoluzione): lo si dice (m2).
                    if !memory_limited && clip.duration_secs + 0.5 < f64::from(seconds) {
                        let _ = events.send(EngineEvent::Warning(tr(
                            K::EngClipShort,
                            &[&format!("{:.0}", clip.duration_secs), &seconds.to_string()],
                        )));
                    }
                    if let Ok(mut r) = shared.recent.lock() {
                        r.push_front(clip.path.clone());
                        r.truncate(RECENT_MAX);
                    }
                    let _ = events.send(EngineEvent::ClipSaved(clip));
                }
                Err(e) => fail(&events, kind, e),
            }
        });
        let handle = self.shared.enqueue(job);
        if handle.is_none() {
            self.shared.saving.fetch_sub(1, Ordering::SeqCst);
            fail(&self.events, kind, tr(K::EngSaveThread, &["coda non disponibile"]));
        }
        handle
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Ok(mut a) = self.alive.lock() {
            *a = false;
        }
        if let Some(v) = self.video.as_mut() {
            v.stop();
        }
        if let Some(a) = self.audio.as_mut() {
            a.stop();
        }
    }
}

/// Esito dell'apertura di un encoder sul thread di apertura.
type Opened = Result<cliprec_encode::Encoder, String>;

/// Callback del pacer. L'encoder si apre su un thread a parte (serve la dimensione della sorgente,
/// e aprirlo e prenotare il buffer dentro il pacer bloccherebbe i tick): finché non è pronto i frame
/// non si codificano. Se un encoder fallisce si passa al candidato successivo; se falliscono tutti si
/// riprova con un backoff da 1 a 30 s. Un device perso chiede al loop di ricreare la sessione.
// I parametri sono le parti di `Session` che la callback del pacer deve possedere.
#[allow(clippy::too_many_arguments)]
fn video_callback(
    config: &Config,
    device: GpuDevice,
    native: Option<(u32, u32)>,
    buffer: Arc<ReplayBuffer>,
    encoder_name: Arc<Mutex<String>>,
    events: Sender<EngineEvent>,
    restart: Arc<AtomicBool>,
    alive: Arc<Mutex<bool>>,
) -> Box<dyn FnMut(&GpuFrame) + Send> {
    let video_cfg = config.video.clone();
    let fps = config.video.fps.max(1);
    let (opened_tx, opened_rx) = std::sync::mpsc::channel::<Opened>();
    let mut encoder: Option<cliprec_encode::Encoder> = None;
    let mut opening = false;
    let mut failed: Vec<String> = Vec::new();
    let mut retry_at = Instant::now();
    let mut backoff = Duration::from_secs(1);
    let mut last_error = String::new();
    let mut next_index: Option<u64> = None;
    let mut encoding_since: Option<Instant> = None;
    Box::new(move |frame: &GpuFrame| {
        // L'attesa torna a 1 s solo dopo 10 s di codifica riuscita, non a ogni apertura.
        if encoding_since.is_some_and(|t| t.elapsed() > Duration::from_secs(10)) {
            backoff = Duration::from_secs(1);
            failed.clear();
        }
        if encoder.is_none() {
            match opened_rx.try_recv() {
                Ok(Ok(e)) => {
                    opening = false;
                    next_index = None;
                    encoding_since = Some(Instant::now());
                    encoder = Some(e);
                }
                Ok(Err(msg)) => {
                    opening = false;
                    failed.clear();
                    retry_at = Instant::now() + backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    if msg != last_error {
                        let _ = events.send(EngineEvent::Error(tr(K::EngEncoderError, &[&msg])));
                        last_error = msg;
                    }
                }
                Err(_) => {}
            }
        }
        let Some(enc) = encoder.as_mut() else {
            if !opening && Instant::now() >= retry_at {
                opening = true;
                // L'uscita si decide dalla dimensione nativa del bersaglio (monitor), scalata secondo la
                // risoluzione scelta; mai dal primo frame, che può essere un segnaposto o una finestra
                // minimizzata (C1). Senza dimensione nativa si ripiega sul frame.
                let (w, h) = native.unwrap_or((frame.width, frame.height));
                let settings = cliprec_encode::EncoderSettings::from_config(&video_cfg, w, h);
                let (device, buffer, encoder_name, tx, skip, alive, events) = (
                    device.clone(),
                    buffer.clone(),
                    encoder_name.clone(),
                    opened_tx.clone(),
                    failed.clone(),
                    alive.clone(),
                    events.clone(),
                );
                let spawned = std::thread::Builder::new().name("encoder-open".into()).spawn(move || {
                    // Un panic nell'apertura diventa un errore: senza risposta `opening` resterebbe vero
                    // e il video non ripartirebbe più (M9).
                    let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        #[cfg(test)]
                        if tests::PANIC_IN_OPEN.swap(false, Ordering::SeqCst) {
                            panic!("panic iniettato nell'apertura dell'encoder");
                        }
                        #[cfg(test)]
                        let skip: Vec<String> = if tests::FORCE_SOFTWARE.load(Ordering::SeqCst) {
                            cliprec_encode::candidates(settings.codec)
                                .into_iter()
                                .map(|(_, n)| n)
                                .filter(|n| n != cliprec_encode::SOFTWARE_ENCODER)
                                .collect()
                        } else {
                            skip
                        };
                        open_encoder(&device, &settings, &skip, &buffer, &encoder_name, &alive, &events)
                    }))
                    .unwrap_or_else(|_| Err("errore interno nell'apertura dell'encoder".into()));
                    let _ = tx.send(opened);
                });
                if spawned.is_err() {
                    opening = false;
                }
            }
            return;
        };
        let mut push = |p: cliprec_core::PacketRef<'_>| buffer.push_video(p);
        // Il pacer salta l'indice sui tick persi: ripetendo l'ultimo frame codificato per quei tick il
        // file resta a frequenza costante e l'audio resta allineato (come i frame duplicati di OBS).
        // Un frame `repeated` costa solo il passaggio all'encoder, che rimanda la superficie già
        // convertita: niente copie né conversioni sulla GPU. ponytail: al massimo 250 ms per tick;
        // oltre, il buco resta nel file come salto di timestamp.
        let missing = next_index.map_or(0, |n| frame.index.saturating_sub(n)).min(u64::from(fps / 4));
        let mut result = Ok(());
        for k in (1..=missing).rev() {
            let filler = GpuFrame {
                texture: frame.texture.clone(),
                width: frame.width,
                height: frame.height,
                pts: frame.pts.add_hns(-(k as i64) * cliprec_core::HNS_PER_SEC / i64::from(fps)),
                index: frame.index - k,
                repeated: true,
            };
            result = result.and_then(|()| enc.encode(&filler, &mut push));
        }
        let result = result.and_then(|()| enc.encode(frame, &mut push));
        match result {
            Ok(()) => next_index = Some(frame.index + 1),
            Err(e) => {
                let name = enc.params().encoder_name.clone();
                if matches!(e, EncodeError::DeviceLost) {
                    // Il loop ricrea device, sorgente ed encoder: qui non si apre altro sul device morto.
                    restart.store(true, Ordering::SeqCst);
                    retry_at = Instant::now() + Duration::from_secs(3600);
                } else {
                    let _ = events.send(EngineEvent::Warning(tr(K::EngEncoderFailed, &[&name, &e.to_string()])));
                    failed.push(name);
                }
                encoder = None;
                next_index = None;
                encoding_since = None;
            }
        }
    })
}

/// Prova i candidati in ordine saltando quelli già falliti; prenota il buffer per i nuovi parametri.
fn open_encoder(
    device: &GpuDevice,
    settings: &cliprec_encode::EncoderSettings,
    skip: &[String],
    buffer: &ReplayBuffer,
    encoder_name: &Mutex<String>,
    alive: &Mutex<bool>,
    events: &Sender<EngineEvent>,
) -> Opened {
    let e = cliprec_encode::open_after(device, settings, skip).map_err(|e| e.to_string())?;
    // Sotto il lock di `alive`: se la sessione si è fermata nel frattempo, i parametri del buffer li
    // imposterà la sessione nuova, e questo encoder viene scartato.
    let alive = alive.lock().map_err(|_| "sessione chiusa".to_string())?;
    if !*alive {
        return Err("sessione chiusa durante l'apertura dell'encoder".into());
    }
    let mut name = encoder_name.lock().map_err(|_| "stato dell'encoder non disponibile".to_string())?;
    if !name.is_empty() && *name != e.params().encoder_name {
        let _ = events.send(EngineEvent::Warning(tr(K::EngEncoderChanged, &[&name, &e.params().encoder_name])));
    }
    // 4:4:4 chiesto ma l'encoder aperto non lo sa fare: si registra in 4:2:0 e lo si dice.
    if *name != e.params().encoder_name
        && settings.advanced.color_format == cliprec_core::config::ColorFormat::Yuv444
        && !settings.chroma444_for(&e.params().encoder_name)
    {
        let _ = events.send(EngineEvent::Warning(tr(K::EngNo444, &[&e.params().encoder_name])));
    }
    buffer.set_video_params(e.params().clone());
    name.clone_from(&e.params().encoder_name);
    drop(name);
    drop(alive);
    tracing::info!(encoder = %e.params().encoder_name, "encoder video pronto");
    Ok(e)
}

fn lower_thread_priority() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
    // SAFETY: pseudo-handle del thread corrente, sempre valido.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
}

/// `{game}` del nome file: l'eseguibile catturato senza `.exe` («osu!» da «osu! (osu!.exe) · WGC»),
/// altrimenti «Desktop».
fn game_name(describe: &str) -> String {
    // ponytail: ricavato dalla descrizione della sorgente; un campo `exe` in CaptureStats sarebbe più robusto.
    describe
        .rsplit_once(".exe)")
        .and_then(|(head, _)| head.rsplit_once('(').map(|(_, exe)| exe.to_string()))
        .filter(|exe| !exe.is_empty())
        .unwrap_or_else(|| "Desktop".into())
}

/// Nome della clip dal modello: `{game}`, `{date}` (AAAA-MM-GG), `{time}` (hh-mm-ss), `{kind}`.
fn clip_name(template: &str, kind: ClipKind, game: Option<String>) -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    // SAFETY: GetLocalTime non ha precondizioni.
    let t = unsafe { GetLocalTime() };
    let game = game.unwrap_or_else(|| "Audio".into());
    let name = template
        .replace("{game}", &game)
        .replace("{date}", &format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay))
        .replace("{time}", &format!("{:02}-{:02}-{:02}", t.wHour, t.wMinute, t.wSecond))
        .replace("{kind}", if kind == ClipKind::Video { "video" } else { "audio" });
    sanitize(&name)
}

/// Toglie i caratteri non ammessi nei nomi di file di Windows e limita la lunghezza.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .take(120)
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.');
    if trimmed.is_empty() { "clip".into() } else { trimmed.into() }
}

/// Configurazione e buffer correnti, conservati fuori da [`run`] così un riavvio dopo un panic del
/// motore riparte dalla configurazione ricaricata e con il buffer intatto.
#[derive(Clone)]
struct Persist {
    config: Config,
    /// `None` con il buffer fermo: la sua RAM è restituita al sistema (m11).
    buffer: Option<Arc<ReplayBuffer>>,
    shared: Shared,
}

impl Persist {
    /// Riferimento debole al buffer conservato (test di «Ferma buffer»).
    #[cfg(test)]
    fn buffer_weak(&self) -> Option<std::sync::Weak<ReplayBuffer>> {
        self.buffer.as_ref().map(Arc::downgrade)
    }
}

fn persist() -> &'static Mutex<Option<Persist>> {
    static P: OnceLock<Mutex<Option<Persist>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// Stati d'attesa che meritano un avviso. No per l'apertura in corso e per un'app registrata che
/// l'utente ha chiuso: è normale, lo stato resta visibile nella tray.
fn warns_about(message: &str) -> bool {
    message != cliprec_audio::OPENING && !message.ends_with(cliprec_audio::NOT_RUNNING)
}

/// Eventi di Windows per le app aperte e chiuse, se chiesti in config: senza permessi di
/// amministratore si avvisa e resta la ricerca ogni mezzo secondo.
fn listen_process_events(config: &Config, events: &Sender<EngineEvent>) {
    if config.audio.process_events
        && let Err(e) = cliprec_audio::procwatch::listen()
    {
        let _ = events.send(EngineEvent::Warning(e));
    }
}

/// Toglie la modalità esclusiva dai dispositivi registrati (`audio.block_exclusive`) e avvisa per
/// quelli cambiati. Mai nei test né senza interfaccia: è un'impostazione di Windows.
fn block_exclusive(config: &Config, events: &Sender<EngineEvent>) {
    if cfg!(test) || crate::notify::headless() || !config.audio.block_exclusive {
        return;
    }
    for name in cliprec_audio::forbid_exclusive_mode(&config.audio.plan().captures) {
        let _ = events.send(EngineEvent::Warning(tr(K::EngExclusiveOff, &[&name])));
    }
}

/// Problema da mostrare nello stato (icona d'errore): solo le sorgenti in errore. Una sorgente in
/// attesa (app chiusa, dispositivo assente) è uno stato normale (m10).
fn status_problem(states: &[(String, SourceState)]) -> Option<String> {
    states.iter().find_map(|(name, st)| match st {
        SourceState::Error(m) => Some(format!("{name}: {m}")),
        _ => None,
    })
}

/// Fine dello snapshot audio di una clip presa adesso: le sorgenti consegnano a blocchi di 10 ms e il
/// mixer (traccia unica o «Mix») scrive con `MIX_LATENCY` di ritardo, quindi ci si ferma prima di
/// quanto non è ancora scritto, con due blocchi di margine (m3).
fn audio_snapshot_end(now: QpcTime, config: &Config, mix_latency_hns: i64) -> QpcTime {
    let margin = 2 * cliprec_core::time::frames_to_hns(cliprec_audio::MIX_BLOCK_FRAMES as i64);
    let plan = config.audio.plan();
    let mixed = plan.has_mix();
    // Una sorgente con ritardo negativo scrive fino a `adesso − |ritardo|` anche nella sua traccia.
    let early = plan.captures.iter().map(|s| -s.delay_ms.clamp(-cliprec_audio::MAX_DELAY_MS, 0)).max().unwrap_or(0);
    let early = i64::from(early) * 10_000;
    let latency = if mixed { mix_latency_hns.max(early) } else { early };
    now.add_hns(-(latency + margin))
}

/// Quali parti ricreare alla ricarica della configurazione.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RestartScope {
    buffer: bool,
    video: bool,
    audio: bool,
}

fn restart_scope(old: &Config, new: &Config) -> RestartScope {
    let buffer = old.clips.video_seconds != new.clips.video_seconds
        || old.clips.audio_seconds != new.clips.audio_seconds
        || old.memory != new.memory
        || old.video.enabled != new.video.enabled;
    let video = old.video != new.video || old.general.gpu_priority != new.general.gpu_priority;
    let audio = old.audio != new.audio;
    // Il buffer nuovo richiede comunque di ripartire da capo.
    RestartScope { buffer, video: video && !buffer, audio: audio && !buffer }
}

/// Loop del motore: un thread, comandi dalla shell, stato aggiornato ogni 500 ms.
/// `commands` è preso per riferimento così il supervisore lo può riavviare dopo un panic.
/// Con `start_buffer = false` (es. `config.toml` illeggibile) il buffer resta fermo finché la shell non
/// manda `StartBuffer` o una configurazione valida con `ReloadConfig`.
pub fn run(
    config: Config,
    start_buffer: bool,
    config_path: &Path,
    commands: &Receiver<AppCommand>,
    events: Sender<EngineEvent>,
    status: Arc<Mutex<EngineStatus>>,
) {
    let restored = persist().lock().ok().and_then(|p| p.clone());
    let (mut config, mut buffer, shared) = match restored {
        Some(p) => (p.config, p.buffer, p.shared),
        None => (config, None, Shared::default()),
    };
    // Il buffer esiste solo con la registrazione attiva: «Ferma buffer» ne restituisce la RAM (m11).
    let start = |config: &Config, buffer: &mut Option<Arc<ReplayBuffer>>| {
        let b = buffer.get_or_insert_with(|| new_buffer(config, &events)).clone();
        match Session::start(config, b, shared.clone(), SourceOverrides::default(), events.clone()) {
            Ok(s) => Some(s),
            Err(e) => {
                let _ = events.send(EngineEvent::Error(tr(K::EngStartFailed, &[&format!("{e:#}")])));
                None
            }
        }
    };
    // Avanzi di un'uscita brusca durante un salvataggio, su un thread a parte: una cartella su un disco
    // di rete lento non deve ritardare l'avvio del buffer (m20).
    let folder = config.clips.folder();
    let _ = std::thread::Builder::new().name("clip-cleanup".into()).spawn(move || {
        let removed = cliprec_mux::cleanup_stale(&folder);
        if removed > 0 {
            tracing::info!(removed, "file incompleti rimossi dalla cartella delle clip");
        }
    });
    let mut session = if start_buffer { start(&config, &mut buffer) } else { None };
    // Avvio con config illeggibile: si riparte solo da un file valido, mai dalla config in memoria (M2).
    let mut blocked = !start_buffer;
    loop {
        if let Ok(mut p) = persist().lock() {
            *p = Some(Persist { config: config.clone(), buffer: buffer.clone(), shared: shared.clone() });
        }
        match commands.recv_timeout(Duration::from_millis(500)) {
            Ok(cmd @ (AppCommand::SaveVideoClip | AppCommand::SaveAudioClip | AppCommand::SaveTestClip)) => {
                if let Some(s) = &session {
                    let clips = &s.config.clips;
                    let accepted = match cmd {
                        AppCommand::SaveTestClip => s.save_test(),
                        AppCommand::SaveAudioClip => s.save(ClipKind::Audio, clips.audio_seconds),
                        // In modalità solo audio la hotkey video salva una clip audio di N secondi.
                        _ if s.video.is_none() => s.save(ClipKind::Audio, clips.video_seconds),
                        _ => s.save(ClipKind::Video, clips.video_seconds),
                    };
                    // Snapshot preso e scrittura accodata: il suono di conferma parte adesso, dopo lo
                    // snapshot, quindi non finisce nella clip appena richiesta.
                    if accepted.is_some() {
                        let _ = events.send(EngineEvent::ClipRequested);
                    }
                } else {
                    let _ = events.send(EngineEvent::Warning(tr(K::EngBufferStopped, &[])));
                }
            }
            Ok(AppCommand::StartBuffer) => {
                if blocked {
                    match Config::load(config_path) {
                        Ok(new) => {
                            config = new;
                            blocked = false;
                        }
                        Err(e) => {
                            let _ = events.send(EngineEvent::Error(format!("config.toml: {e}")));
                        }
                    }
                }
                if session.is_none() && !blocked {
                    session = start(&config, &mut buffer);
                }
            }
            Ok(AppCommand::StopBuffer) => {
                drop(session.take());
                buffer = None;
            }
            Ok(AppCommand::ReloadConfig) => match Config::load(config_path) {
                Ok(new) => {
                    let scope = restart_scope(&config, &new);
                    config = new;
                    let was_running = session.is_some();
                    if std::mem::take(&mut blocked) {
                        // Config corretta dopo un avvio con config illeggibile: il buffer parte adesso.
                        session = start(&config, &mut buffer);
                    } else if scope.buffer {
                        drop(session.take());
                        buffer = None;
                        if was_running {
                            buffer = Some(new_buffer(&config, &events));
                            session = start(&config, &mut buffer);
                        }
                    } else if let Some(s) = session.as_mut() {
                        // Solo le parti cambiate ripartono: una modifica all'audio non tocca cattura,
                        // encoder e buffer video, e viceversa (m2).
                        s.config = config.clone();
                        if scope.video {
                            s.restart_video();
                        }
                        if scope.audio {
                            s.restart_audio();
                        }
                    }
                }
                Err(e) => {
                    let _ = events.send(EngineEvent::Error(format!("config.toml: {e}")));
                }
            },
            Ok(AppCommand::Quit) | Err(RecvTimeoutError::Disconnected) => {
                drop(session.take());
                shared.wait_savers(Duration::from_secs(10));
                if let Ok(mut p) = persist().lock() {
                    *p = None;
                }
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if session.as_ref().is_some_and(|s| s.restart.load(Ordering::SeqCst)) {
            let _ = events.send(EngineEvent::Warning(tr(K::EngDeviceLost, &[])));
            drop(session.take());
            session = start(&config, &mut buffer);
        }
        if let Some(s) = session.as_mut() {
            s.poll_states();
            s.retry_video();
            s.measure();
        }
        if let Ok(mut st) = status.lock() {
            *st = session.as_ref().map(Session::status).unwrap_or_else(|| EngineStatus {
                recent_clips: shared.recent.lock().map(|r| r.iter().cloned().collect()).unwrap_or_default(),
                available_codecs: shared.available_codecs.lock().map(|c| c.clone()).unwrap_or_default(),
                available_encoders: shared.available_encoders.lock().map(|e| e.clone()).unwrap_or_default(),
                last_test: shared.last_test.lock().map(|t| t.clone()).unwrap_or_default(),
                ..Default::default()
            });
        }
    }
}

/// Prova della catena senza tray né hotkey: registra `seconds` secondi, salva una clip video e
/// una clip audio nella cartella `out` e restituisce i file scritti.
/// Con `synthetic` usa la sorgente video con lampo a ogni secondo e due bip (1 kHz e 1,5 kHz):
/// niente schermo dell'utente, niente microfono, niente altoparlanti.
pub fn run_test_clip(mut config: Config, seconds: u32, synthetic: bool, out: PathBuf) -> anyhow::Result<Vec<cliprec_core::SavedClip>> {
    config.clips.folder = Some(out);
    config.clips.video_seconds = seconds.saturating_sub(1).max(1);
    config.clips.audio_seconds = seconds.saturating_sub(1).max(1);
    let mut overrides = SourceOverrides::default();
    if synthetic {
        let device = GpuDevice::create(None)?;
        let video = cliprec_capture::SyntheticSource::new(device.clone(), 1920, 1080);
        overrides.video = Some((device, Box::new(video)));
        overrides.audio = Some(vec![
            Box::new(cliprec_audio::BeepSource::new("Bip 1 kHz", 1000.0)) as Box<dyn cliprec_core::AudioSource>,
            Box::new(cliprec_audio::BeepSource::new("Bip 1,5 kHz", 1500.0)),
        ]);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let buffer = new_buffer(&config, &tx);
    let session = Session::start(&config, buffer, Shared::default(), overrides, tx)?;
    std::thread::sleep(Duration::from_secs(u64::from(seconds)));
    let handles: Vec<_> = [ClipKind::Video, ClipKind::Audio]
        .into_iter()
        .filter_map(|k| session.save(k, config.clips.video_seconds))
        .collect();
    drop(session);
    for h in handles {
        let _ = h.join();
    }
    let mut saved = Vec::new();
    for event in rx.try_iter() {
        match event {
            EngineEvent::ClipSaved(clip) => saved.push(clip),
            EngineEvent::ClipFailed { kind, reason } => anyhow::bail!("clip {kind:?} non salvata: {reason}"),
            other => tracing::warn!(?other, "evento durante la prova"),
        }
    }
    Ok(saved)
}

/// Sessione lunga con sorgenti sintetiche (criterio «8 h senza crescita di memoria oltre i buffer
/// né frame persi in modo sistematico»): ogni minuto una riga CSV in `out/sessione-lunga.csv` con
/// memoria privata del processo, RAM del buffer, secondi coperti, frame persi e ripetuti; ogni
/// 10 minuti salva una clip video.
pub fn run_soak(mut config: Config, minutes: u32, out: PathBuf) -> anyhow::Result<()> {
    use std::io::Write;
    config.clips.folder = Some(out.clone());
    std::fs::create_dir_all(&out)?;
    let device = GpuDevice::create(None)?;
    let video = cliprec_capture::SyntheticSource::new(device.clone(), 1920, 1080);
    let overrides = SourceOverrides {
        video: Some((device, Box::new(video))),
        audio: Some(vec![
            Box::new(cliprec_audio::BeepSource::new("Bip 1 kHz", 1000.0)) as Box<dyn cliprec_core::AudioSource>,
            Box::new(cliprec_audio::BeepSource::new("Bip 1,5 kHz", 1500.0)),
        ]),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let buffer = new_buffer(&config, &tx);
    let session = Session::start(&config, buffer, Shared::default(), overrides, tx)?;
    let mut csv = std::fs::File::create(out.join("sessione-lunga.csv"))?;
    writeln!(csv, "minuto,memoria_privata_mb,buffer_mb,video_s,audio_s,frame_consegnati,frame_ripetuti,frame_persi,clip")?;
    for minute in 1..=minutes {
        std::thread::sleep(Duration::from_secs(60));
        let clip = if minute % 10 == 0 {
            session.save(ClipKind::Video, config.clips.video_seconds).map(|h| h.join().is_ok()).unwrap_or(false)
        } else {
            false
        };
        let st = session.status();
        let cap = session.video.as_ref().map(|v| v.stats()).unwrap_or_default();
        writeln!(
            csv,
            "{minute},{:.1},{:.1},{:.1},{:.1},{},{},{},{}",
            private_bytes() as f64 / 1e6,
            st.buffer_bytes as f64 / 1e6,
            st.video_seconds,
            st.audio_seconds,
            cap.frames_delivered,
            cap.frames_repeated,
            cap.frames_dropped,
            if clip { "salvata" } else { "" },
        )?;
        csv.flush()?;
        for event in rx.try_iter() {
            match event {
                EngineEvent::ClipSaved(c) => tracing::info!("sessione lunga: clip {}", c.path.display()),
                other => tracing::warn!(?other, "sessione lunga"),
            }
        }
    }
    Ok(())
}

/// Memoria privata del processo (commit), in byte.
fn private_bytes() -> usize {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
    // SAFETY: la struttura è valida e ne passiamo la dimensione esatta; lo pseudo-handle del
    // processo corrente è sempre valido.
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            std::ptr::from_mut(&mut c).cast::<PROCESS_MEMORY_COUNTERS>(),
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };
    if ok.is_ok() { c.PrivateUsage } else { 0 }
}

#[cfg(test)]
mod tests {
    use std::thread::JoinHandle;

    use super::*;
    use cliprec_core::config::TrackMode;

    /// I test end-to-end aprono sessioni NVENC e lanciano ffmpeg/ffprobe: in fila, così sotto carico
    /// non falliscono per risorse del sistema (avvio di processi, sessioni dell'encoder).
    fn heavy() -> std::sync::MutexGuard<'static, ()> {
        static HEAVY: Mutex<()> = Mutex::new(());
        HEAVY.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Il tetto dipende dalla RAM totale, non da quella libera all'avvio di un gioco.
    #[test]
    fn buffer_cap_follows_total_ram() {
        const GB: usize = 1 << 30;
        assert_eq!(buffer_cap(GB, 16 * GB), GB);
        assert_eq!(buffer_cap(8 * GB, 16 * GB), 4 * GB);
        assert_eq!(buffer_cap(GB, 0), GB, "RAM ignota: nessun tetto");
        assert!(total_ram() >= GB, "{}", total_ram());
    }

    #[test]
    fn clip_names_are_valid_file_names() {
        let n = clip_name("{game}_{date}_{time}", ClipKind::Video, Some("a:b".into()));
        assert!(n.starts_with("a_b_"));
        assert_eq!(sanitize("  ...  "), "clip");
        assert_eq!(sanitize(&"x".repeat(300)).len(), 120);
        assert_eq!(game_name("ClipRec testwindow (xtask.exe) · WGC"), "xtask");
        assert_eq!(game_name("osu! (osu!.exe) · WGC"), "osu!");
        assert_eq!(game_name("Monitor 1 (2560×1440) · WGC"), "Desktop");
    }

    /// Sorgente video che non parte (es. monitor scollegato).
    struct FailingSource;

    impl VideoSource for FailingSource {
        fn start(&mut self, _fps: u32, _on_frame: cliprec_core::FrameCallback) -> Result<(), cliprec_core::SourceError> {
            Err(cliprec_core::SourceError::NotFound("monitor 3".into()))
        }
        fn stop(&mut self) {}
        fn stats(&self) -> cliprec_core::CaptureStats {
            cliprec_core::CaptureStats::default()
        }
        fn describe(&self) -> String {
            "sorgente che fallisce".into()
        }
    }

    fn beeps() -> Vec<Box<dyn cliprec_core::AudioSource>> {
        vec![
            Box::new(cliprec_audio::BeepSource::new("Bip 1 kHz", 1000.0)),
            Box::new(cliprec_audio::BeepSource::new("Bip 1,5 kHz", 1500.0)),
        ]
    }

    fn fault_dir(name: &str) -> PathBuf {
        let d = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/guasti").join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Guasto: il video non parte. L'audio prosegue, un avviso lo dice e la clip audio si salva.
    #[test]
    fn video_failure_falls_back_to_audio_only() {
        let _serial = heavy();
        let mut config = Config::default();
        config.clips.folder = Some(fault_dir("solo-audio"));
        let (tx, rx) = std::sync::mpsc::channel();
        let overrides = SourceOverrides {
            video: Some((GpuDevice::create(None).unwrap(), Box::new(FailingSource))),
            audio: Some(beeps()),
        };
        let session = Session::start(&config, new_buffer(&config, &tx), Shared::default(), overrides, tx).unwrap();
        assert!(session.video.is_none());
        std::thread::sleep(Duration::from_secs(3));
        session.save(ClipKind::Audio, 2).unwrap().join().unwrap();
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::Warning(w) if w.contains("video non avviato"))), "{events:?}");
        assert!(events.iter().any(|e| matches!(e, EngineEvent::ClipSaved(c) if c.kind == ClipKind::Audio)), "{events:?}");
    }

    /// Guasto: la cartella delle clip non si può creare. `ClipFailed`, nessun crash, nessun file.
    #[test]
    fn unwritable_folder_reports_clip_failed() {
        let _serial = heavy();
        let dir = fault_dir("cartella");
        let blocker = dir.join("file.txt");
        std::fs::write(&blocker, b"non una cartella").unwrap();
        let mut config = Config::default();
        config.clips.folder = Some(blocker.join("clip"));
        config.video.enabled = false;
        let (tx, rx) = std::sync::mpsc::channel();
        let overrides = SourceOverrides { video: None, audio: Some(beeps()) };
        let session = Session::start(&config, new_buffer(&config, &tx), Shared::default(), overrides, tx).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        if let Some(h) = session.save(ClipKind::Audio, 1) {
            h.join().unwrap();
        }
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::ClipFailed { .. })), "{events:?}");
    }

    /// Due pressioni nello stesso secondo: due clip distinte e valide, mai un errore.
    #[test]
    fn two_saves_in_the_same_second_give_two_valid_clips() {
        let _serial = heavy();
        use cliprec_tests::probe::{Expect, check};
        let mut config = Config::default();
        config.clips.folder = Some(fault_dir("doppia"));
        config.clips.video_seconds = 2;
        let (tx, rx) = std::sync::mpsc::channel();
        let device = GpuDevice::create(None).unwrap();
        let video = cliprec_capture::SyntheticSource::new(device.clone(), 1280, 720);
        let overrides = SourceOverrides { video: Some((device, Box::new(video))), audio: Some(beeps()) };
        let session = Session::start(&config, new_buffer(&config, &tx), Shared::default(), overrides, tx).unwrap();
        std::thread::sleep(Duration::from_secs(4));
        let a = session.save(ClipKind::Video, 2).unwrap();
        let b = session.save(ClipKind::Video, 2).unwrap();
        a.join().unwrap();
        b.join().unwrap();
        let paths: Vec<PathBuf> = rx
            .try_iter()
            .filter_map(|e| match e {
                EngineEvent::ClipSaved(c) => Some(c.path),
                _ => None,
            })
            .collect();
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert_ne!(paths[0], paths[1]);
        for p in &paths {
            let v = check(p, &Expect { min_duration: Some(2.0), audio_tracks: Some(2), fps: Some(60.0), ..Expect::default() }).unwrap();
            assert!(v.is_empty(), "{}: {v:?}", p.display());
        }
    }

    /// Sorgente sintetica che blocca la callback per 72 ms quattro frame prima di ogni lampo: il pacer
    /// perde i tre tick successivi e consegna proprio il frame del lampo.
    struct StallBeforeFlash(cliprec_capture::SyntheticSource);

    impl VideoSource for StallBeforeFlash {
        fn start(&mut self, fps: u32, mut on_frame: cliprec_core::FrameCallback) -> Result<(), cliprec_core::SourceError> {
            let period = cliprec_core::HNS_PER_SEC / i64::from(fps);
            self.0.start(
                fps,
                Box::new(move |f: &GpuFrame| {
                    on_frame(f);
                    let next_second = (f.pts.0 / cliprec_core::HNS_PER_SEC + 1) * cliprec_core::HNS_PER_SEC;
                    if f.pts.0 + 3 * period < next_second && next_second <= f.pts.0 + 4 * period {
                        std::thread::sleep(Duration::from_millis(72));
                    }
                }),
            )
        }
        fn stop(&mut self) {
            self.0.stop();
        }
        fn stats(&self) -> cliprec_core::CaptureStats {
            self.0.stats()
        }
        fn describe(&self) -> String {
            self.0.describe()
        }
    }

    /// Tick persi subito prima di un lampo: i riempitivi ripetono il frame precedente, non quello
    /// nuovo, quindi il lampo non compare in anticipo e la sincronia resta entro ±20 ms.
    #[test]
    fn lost_ticks_before_flash_keep_av_sync() {
        let _serial = heavy();
        let mut config = Config::default();
        config.clips.folder = Some(fault_dir("tick-persi"));
        config.clips.video_seconds = 5;
        let (tx, rx) = std::sync::mpsc::channel();
        let device = GpuDevice::create(None).unwrap();
        let video = StallBeforeFlash(cliprec_capture::SyntheticSource::new(device.clone(), 1280, 720));
        let overrides = SourceOverrides { video: Some((device, Box::new(video))), audio: Some(beeps()) };
        let session = Session::start(&config, new_buffer(&config, &tx), Shared::default(), overrides, tx).unwrap();
        std::thread::sleep(Duration::from_secs(7));
        let dropped = session.video.as_ref().map(|v| v.stats().frames_dropped).unwrap_or(0);
        session.save(ClipKind::Video, 5).unwrap().join().unwrap();
        let clip = rx
            .try_iter()
            .find_map(|e| match e {
                EngineEvent::ClipSaved(c) => Some(c.path),
                _ => None,
            })
            .expect("clip salvata");
        assert!(dropped >= 9, "il test deve davvero perdere tick: {dropped}");
        let sync = cliprec_tests::sync::measure_sync(&clip).unwrap();
        println!("tick persi {dropped}; {sync:#?}");
        let v = sync.violations(20.0);
        assert!(v.is_empty(), "{v:?}");
    }

    /// Modalità audio nella catena completa: traccia unica (una sola traccia «Mix») e tracce separate
    /// con il «Mix» in testa (tre tracce, la prima predefinita), con i bip ancora sincronizzati.
    #[test]
    fn merged_and_mix_track_modes_end_to_end() {
        let _serial = heavy();
        use cliprec_tests::probe::{Expect, check};
        for (mode, add_mix, names) in [
            (TrackMode::Merged, false, vec!["Mix"]),
            (TrackMode::Separate, true, vec!["Mix", "Bip 1 kHz", "Bip 1,5 kHz"]),
        ] {
            let mut config = Config::default();
            config.clips.folder = Some(fault_dir(&format!("modo-{mode:?}-{add_mix}")));
            config.clips.video_seconds = 3;
            config.audio.mode = mode;
            config.audio.add_mix_track = add_mix;
            let (tx, rx) = std::sync::mpsc::channel();
            let device = GpuDevice::create(None).unwrap();
            let video = cliprec_capture::SyntheticSource::new(device.clone(), 1280, 720);
            let overrides = SourceOverrides { video: Some((device, Box::new(video))), audio: Some(beeps()) };
            let session = Session::start(&config, new_buffer(&config, &tx), Shared::default(), overrides, tx).unwrap();
            std::thread::sleep(Duration::from_secs(5));
            session.save(ClipKind::Video, 3).unwrap().join().unwrap();
            let clip = rx
                .try_iter()
                .find_map(|e| match e {
                    EngineEvent::ClipSaved(c) => Some(c.path),
                    _ => None,
                })
                .expect("clip salvata");
            let names: Vec<String> = names.into_iter().map(String::from).collect();
            let expect = Expect {
                min_duration: Some(3.0),
                audio_tracks: Some(names.len()),
                track_names: Some(names),
                fps: Some(60.0),
                ..Expect::default()
            };
            let v = check(&clip, &expect).unwrap();
            assert!(v.is_empty(), "{mode:?} mix={add_mix}: {v:?}");
            let sync = cliprec_tests::sync::measure_sync(&clip).unwrap();
            let v = sync.violations(20.0);
            assert!(v.is_empty(), "{mode:?} mix={add_mix}: {v:?}");
        }
    }

    /// Aggancio per M9: il prossimo thread di apertura dell'encoder va in panic.
    pub(super) static PANIC_IN_OPEN: AtomicBool = AtomicBool::new(false);
    /// Aggancio per M6: gli encoder hardware vengono saltati, si apre quello software.
    pub(super) static FORCE_SOFTWARE: AtomicBool = AtomicBool::new(false);

    /// Sorgente che parte ma non consegna mai frame: l'encoder non è mai pronto.
    struct NeverSource;

    impl VideoSource for NeverSource {
        fn start(&mut self, _fps: u32, _on_frame: cliprec_core::FrameCallback) -> Result<(), cliprec_core::SourceError> {
            Ok(())
        }
        fn stop(&mut self) {}
        fn stats(&self) -> cliprec_core::CaptureStats {
            cliprec_core::CaptureStats { state: SourceState::Running, ..Default::default() }
        }
        fn describe(&self) -> String {
            "sorgente senza frame".into()
        }
    }

    /// Texture BGRA 146×28, come una finestra minimizzata.
    fn tiny_texture(device: &GpuDevice) -> windows::Win32::Graphics::Direct3D11::ID3D11Texture2D {
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
        };
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 146,
            Height: 28,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: descrittore completo e valido; puntatore d'uscita locale.
        unsafe { device.device.CreateTexture2D(&desc, None, Some(&mut texture)) }.unwrap();
        texture.unwrap()
    }

    /// Sintetica 1280×720 che per i primi `small_for` frame consegna una finestra minimizzata
    /// (146×28) e dichiara la dimensione nativa del monitor (1280×720).
    struct SmallFirst {
        inner: cliprec_capture::SyntheticSource,
        device: GpuDevice,
        small_for: u64,
    }

    impl VideoSource for SmallFirst {
        fn start(&mut self, fps: u32, mut on_frame: cliprec_core::FrameCallback) -> Result<(), cliprec_core::SourceError> {
            let tiny = tiny_texture(&self.device);
            let n = self.small_for;
            self.inner.start(
                fps,
                Box::new(move |f: &GpuFrame| {
                    if f.index < n {
                        let small = GpuFrame {
                            texture: tiny.clone(),
                            width: 146,
                            height: 28,
                            pts: f.pts,
                            index: f.index,
                            repeated: f.repeated,
                        };
                        on_frame(&small);
                    } else {
                        on_frame(f);
                    }
                }),
            )
        }
        fn stop(&mut self) {
            self.inner.stop();
        }
        fn stats(&self) -> cliprec_core::CaptureStats {
            self.inner.stats()
        }
        fn describe(&self) -> String {
            "finestra minimizzata".into()
        }
        fn native_size(&self) -> Option<(u32, u32)> {
            Some((1280, 720))
        }
    }

    /// Sintetica che dopo ogni frame blocca il pacer per 40 ms: a 60 fps se ne consegnano circa 25.
    struct StallEvery(cliprec_capture::SyntheticSource);

    impl VideoSource for StallEvery {
        fn start(&mut self, fps: u32, mut on_frame: cliprec_core::FrameCallback) -> Result<(), cliprec_core::SourceError> {
            self.0.start(
                fps,
                Box::new(move |f: &GpuFrame| {
                    on_frame(f);
                    std::thread::sleep(Duration::from_millis(40));
                }),
            )
        }
        fn stop(&mut self) {
            self.0.stop();
        }
        fn stats(&self) -> cliprec_core::CaptureStats {
            self.0.stats()
        }
        fn describe(&self) -> String {
            self.0.describe()
        }
    }

    /// Tono costante 0,25 a blocchi di 10 ms in tempo reale, con l'istante QPC esatto.
    struct ConstantTone {
        name: String,
        /// Ritardo di consegna dei blocchi (dispositivo ad alta latenza, es. Bluetooth).
        lag_hns: i64,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl ConstantTone {
        fn new(name: &str) -> Self {
            Self::late(name, 0)
        }
        fn late(name: &str, lag_ms: i64) -> Self {
            Self { name: name.into(), lag_hns: lag_ms * 10_000, stop: Arc::default(), thread: None }
        }
    }

    impl cliprec_core::AudioSource for ConstantTone {
        fn start(&mut self, mut on_chunk: cliprec_core::ChunkCallback) -> Result<(), cliprec_core::SourceError> {
            let (stop, lag) = (self.stop.clone(), self.lag_hns);
            self.thread = Some(std::thread::spawn(move || {
                let block = vec![0.25f32; 480 * 2];
                let start = QpcTime::now();
                let mut n = 0i64;
                while !stop.load(Ordering::SeqCst) {
                    let qpc = start.add_hns(n * 100_000);
                    while QpcTime::now().hns_since(qpc) < 100_000 + lag {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    on_chunk(cliprec_core::AudioChunk { qpc, samples: &block });
                    n += 1;
                }
            }));
            Ok(())
        }
        fn stop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
        fn state(&self) -> SourceState {
            SourceState::Running
        }
        fn name(&self) -> &str {
            &self.name
        }
    }

    fn synthetic_session(name: &str, config: &mut Config, shared: Shared) -> (Session, Arc<ReplayBuffer>, Receiver<EngineEvent>) {
        config.clips.folder = Some(fault_dir(name));
        let (tx, rx) = std::sync::mpsc::channel();
        let device = GpuDevice::create(None).unwrap();
        let video = cliprec_capture::SyntheticSource::new(device.clone(), 1280, 720);
        let buffer = new_buffer(config, &tx);
        let overrides = SourceOverrides { video: Some((device, Box::new(video))), audio: Some(beeps()) };
        let session = Session::start(config, buffer.clone(), shared, overrides, tx).unwrap();
        (session, buffer, rx)
    }

    /// C1: l'uscita si decide dalla dimensione nativa del bersaglio, non dal primo frame. Con una
    /// finestra minimizzata (146×28) all'avvio l'encoder deve aprirsi subito a 1280×720.
    #[test]
    fn c1_encoder_uses_native_size_not_first_frame() {
        let _serial = heavy();
        let mut config = Config::default();
        config.clips.video_seconds = 3;
        config.clips.folder = Some(fault_dir("c1"));
        let (tx, _rx) = std::sync::mpsc::channel();
        let device = GpuDevice::create(None).unwrap();
        let video = SmallFirst {
            inner: cliprec_capture::SyntheticSource::new(device.clone(), 1280, 720),
            device: device.clone(),
            small_for: 600,
        };
        let buffer = new_buffer(&config, &tx);
        let overrides = SourceOverrides { video: Some((device, Box::new(video))), audio: Some(beeps()) };
        let session = Session::start(&config, buffer.clone(), Shared::default(), overrides, tx).unwrap();
        std::thread::sleep(Duration::from_millis(2000));
        let size = buffer.snapshot_video(1.0, QpcTime::now()).map(|s| (s.params.width, s.params.height));
        drop(session);
        assert_eq!(size, Some((1280, 720)), "encoder alla dimensione nativa entro 2 s");
    }

    /// Uscita scalata (richiesta di Luca): con un bersaglio nativo a 1440p e l'uscita scelta a 1080p,
    /// l'encoder lavora a 1920×1080 dal primo frame.
    #[test]
    fn output_can_be_scaled_below_native() {
        let _serial = heavy();
        let mut config = Config::default();
        config.clips.video_seconds = 2;
        config.video.resolution = cliprec_core::config::OutputResolution::P1080;
        config.clips.folder = Some(fault_dir("scala"));
        let (tx, _rx) = std::sync::mpsc::channel();
        let device = GpuDevice::create(None).unwrap();
        let video = cliprec_capture::SyntheticSource::new(device.clone(), 2560, 1440);
        let buffer = new_buffer(&config, &tx);
        let overrides = SourceOverrides { video: Some((device, Box::new(video))), audio: Some(beeps()) };
        let session = Session::start(&config, buffer.clone(), Shared::default(), overrides, tx).unwrap();
        std::thread::sleep(Duration::from_millis(2000));
        let size = buffer.snapshot_video(1.0, QpcTime::now()).map(|s| (s.params.width, s.params.height));
        drop(session);
        assert_eq!(size, Some((1920, 1080)));
    }

    /// M6: senza video nel buffer (encoder mai pronto) la hotkey video salva comunque l'audio.
    #[test]
    fn m6_video_hotkey_saves_audio_when_no_video_is_ready() {
        let _serial = heavy();
        let mut config = Config::default();
        config.clips.folder = Some(fault_dir("m6"));
        let (tx, rx) = std::sync::mpsc::channel();
        let device = GpuDevice::create(None).unwrap();
        let buffer = new_buffer(&config, &tx);
        let overrides = SourceOverrides { video: Some((device, Box::new(NeverSource))), audio: Some(beeps()) };
        let session = Session::start(&config, buffer, Shared::default(), overrides, tx).unwrap();
        std::thread::sleep(Duration::from_millis(2500));
        let handle = session.save(ClipKind::Video, 2);
        assert!(handle.is_some(), "la hotkey video non deve fallire se c'è audio");
        handle.unwrap().join().unwrap();
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::ClipSaved(c) if c.kind == ClipKind::Audio)), "{events:?}");
    }

    /// M6, catena completa con l'encoder di riserva: senza encoder hardware la sessione apre SVT-AV1
    /// e la clip MP4 è valida per ffprobe (60 fps costanti, due tracce, durata ≥ N). Test
    /// d'integrazione: il ripiego in `open_after` ha il suo test di regressione in `cliprec-encode`.
    #[test]
    fn m6_software_encoder_clip_is_valid_mp4() {
        use cliprec_tests::probe::{Expect, check};
        let _serial = heavy();
        let mut config = Config::default();
        config.clips.video_seconds = 2;
        FORCE_SOFTWARE.store(true, Ordering::SeqCst);
        let (session, _buffer, rx) = synthetic_session("m6-svtav1", &mut config, Shared::default());
        std::thread::sleep(Duration::from_secs(4));
        let encoder = session.status().encoder;
        session.save(ClipKind::Video, 2).unwrap().join().unwrap();
        drop(session);
        FORCE_SOFTWARE.store(false, Ordering::SeqCst);
        assert_eq!(encoder, "libsvtav1");
        let clip = rx
            .try_iter()
            .find_map(|e| match e {
                EngineEvent::ClipSaved(c) => Some(c.path),
                _ => None,
            })
            .expect("clip salvata");
        let expect = Expect { min_duration: Some(2.0), audio_tracks: Some(2), fps: Some(60.0), ..Expect::default() };
        let violations = check(&clip, &expect).unwrap();
        assert!(violations.is_empty(), "{violations:?}");
    }

    /// M5: le clip si scrivono una alla volta (picco di memoria limitato), ma nessuna va persa.
    #[test]
    fn m5_saves_are_written_one_at_a_time() {
        let _serial = heavy();
        let mut config = Config::default();
        config.clips.video_seconds = 3;
        let shared = Shared::default();
        let (session, _buffer, rx) = synthetic_session("m5", &mut config, shared.clone());
        std::thread::sleep(Duration::from_secs(4));
        let handles: Vec<_> = (0..3).filter_map(|_| session.save(ClipKind::Video, 3)).collect();
        for h in handles {
            h.join().unwrap();
        }
        let saved = rx.try_iter().filter(|e| matches!(e, EngineEvent::ClipSaved(_))).count();
        assert_eq!(saved, 3);
        assert_eq!(shared.writing_peak.load(Ordering::SeqCst), 1, "scritture contemporanee");
    }

    /// M9: un panic nel thread di apertura dell'encoder non blocca il video per sempre.
    #[test]
    fn m9_panic_while_opening_the_encoder_is_retried() {
        let _serial = heavy();
        let mut config = Config::default();
        config.clips.video_seconds = 3;
        PANIC_IN_OPEN.store(true, Ordering::SeqCst);
        let (session, buffer, _rx) = synthetic_session("m9", &mut config, Shared::default());
        std::thread::sleep(Duration::from_millis(3500));
        let opened = buffer.snapshot_video(1.0, QpcTime::now()).is_some();
        drop(session);
        PANIC_IN_OPEN.store(false, Ordering::SeqCst);
        assert!(opened, "dopo il panic l'apertura dell'encoder va ritentata");
    }

    /// m1: lo snapshot si prende prima di toccare il disco; una cartella non creabile dà `ClipFailed`
    /// dal thread di salvataggio, non blocca la pressione.
    #[test]
    fn m1_snapshot_is_taken_before_touching_the_disk() {
        let _serial = heavy();
        let dir = fault_dir("m1");
        let blocker = dir.join("file.txt");
        std::fs::write(&blocker, b"non una cartella").unwrap();
        let mut config = Config::default();
        config.clips.folder = Some(blocker.join("clip"));
        config.video.enabled = false;
        let (tx, rx) = std::sync::mpsc::channel();
        let overrides = SourceOverrides { video: None, audio: Some(beeps()) };
        let session = Session::start(&config, new_buffer(&config, &tx), Shared::default(), overrides, tx).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let handle = session.save(ClipKind::Audio, 1);
        assert!(handle.is_some(), "lo snapshot deve precedere l'accesso al disco");
        handle.unwrap().join().unwrap();
        assert!(rx.try_iter().any(|e| matches!(e, EngineEvent::ClipFailed { .. })));
    }

    /// m2: una modifica solo audio non riavvia cattura ed encoder (né svuota il buffer video).
    #[test]
    fn m2_audio_only_change_does_not_restart_video() {
        let a = Config::default();
        let mut b = a.clone();
        b.audio.sources[0].volume = 0.5;
        assert_eq!(restart_scope(&a, &b), RestartScope { buffer: false, video: false, audio: true });
        let mut c = a.clone();
        c.video.fps = 30;
        assert_eq!(restart_scope(&a, &c), RestartScope { buffer: false, video: true, audio: false });
        let mut d = a.clone();
        d.hotkeys.video = "Mouse5".into();
        assert_eq!(restart_scope(&a, &d), RestartScope::default());
    }

    /// m2: una clip più corta di N (buffer appena svuotato o appena avviato) lo dice.
    #[test]
    fn m2_short_clip_is_reported() {
        let _serial = heavy();
        let mut config = Config::default();
        config.clips.video_seconds = 10;
        let (session, _buffer, rx) = synthetic_session("m2", &mut config, Shared::default());
        std::thread::sleep(Duration::from_secs(3));
        session.save(ClipKind::Video, 10).unwrap().join().unwrap();
        let warned = rx.try_iter().any(|e| matches!(e, EngineEvent::Warning(w) if w.contains("invece di")));
        assert!(warned, "manca l'avviso della clip più corta");
    }

    /// m3: in traccia unica la coda della clip non è silenziosa (latenza del mixer di 150 ms).
    #[test]
    fn m3_mix_clip_has_no_silent_tail() {
        let _serial = heavy();
        let mut config = Config::default();
        config.video.enabled = false;
        config.audio.mode = TrackMode::Merged;
        config.clips.folder = Some(fault_dir("m3"));
        let (tx, _rx) = std::sync::mpsc::channel();
        let buffer = new_buffer(&config, &tx);
        let sources: Vec<Box<dyn cliprec_core::AudioSource>> =
            vec![Box::new(ConstantTone::new("A")), Box::new(ConstantTone::new("B"))];
        let overrides = SourceOverrides { video: None, audio: Some(sources) };
        let session = Session::start(&config, buffer.clone(), Shared::default(), overrides, tx).unwrap();
        std::thread::sleep(Duration::from_millis(2500));
        let end = audio_snapshot_end(QpcTime::now(), &config, session.mix_latency());
        let snap = buffer.snapshot_audio(end.add_hns(-cliprec_core::HNS_PER_SEC), end).unwrap();
        drop(session);
        let t = &snap.tracks[0].samples;
        let tail = &t[t.len() - 4800 * 2..];
        let silent = tail.iter().filter(|s| s.abs() < 0.01).count();
        assert_eq!(silent, 0, "{} campioni muti negli ultimi 100 ms", silent / 2);
    }

    /// m3 (verifica): anche la clip video in traccia unica non finisce con l'audio muto. Con una
    /// sorgente ad alta latenza (500 ms, come un microfono Bluetooth) il mixer scrive più indietro
    /// dell'encoder video: il video si deve fermare dove finisce l'audio scritto, invece di essere
    /// completato con zeri.
    #[test]
    fn m3_video_clip_has_no_silent_tail() {
        let _serial = heavy();
        let mut config = Config::default();
        config.audio.mode = TrackMode::Merged;
        config.clips.video_seconds = 2;
        config.clips.folder = Some(fault_dir("m3-video"));
        let (tx, rx) = std::sync::mpsc::channel();
        let device = GpuDevice::create(None).unwrap();
        let video = cliprec_capture::SyntheticSource::new(device.clone(), 1280, 720);
        let buffer = new_buffer(&config, &tx);
        let sources: Vec<Box<dyn cliprec_core::AudioSource>> = vec![Box::new(ConstantTone::late("A", 500))];
        let overrides = SourceOverrides { video: Some((device, Box::new(video))), audio: Some(sources) };
        let session = Session::start(&config, buffer, Shared::default(), overrides, tx).unwrap();
        std::thread::sleep(Duration::from_secs(5));
        assert!(session.mix_latency() > 5_000_000, "il mixer non si è adattato: {} ms", session.mix_latency() / 10_000);
        session.save(ClipKind::Video, 2).unwrap().join().unwrap();
        drop(session);
        let clip = rx
            .try_iter()
            .find_map(|e| match e {
                EngineEvent::ClipSaved(c) => Some(c.path),
                _ => None,
            })
            .expect("clip salvata");
        let pcm = cliprec_tests::ffmpeg::output(cliprec_tests::ffmpeg::ffmpeg().arg("-i").arg(&clip).args([
            "-map", "0:a:0", "-f", "f32le", "-ac", "1", "-ar", "48000", "-",
        ]))
        .unwrap();
        let samples: Vec<f32> = pcm.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let loud = samples.iter().filter(|s| s.abs() > 0.1).count();
        assert!(loud > samples.len() / 2, "il tono non c'è: {loud} campioni su {}", samples.len());
        let silent_ms = samples.iter().rev().take_while(|s| s.abs() < 0.05).count() as f64 / 48.0;
        assert!(silent_ms < 40.0, "la clip video finisce con {silent_ms:.0} ms di audio muto");
    }

    /// Ritardo negativo in tracce separate (verifica, R5): i blocchi di quella sorgente finiscono a
    /// `adesso − |ritardo|`, quindi la fine dello snapshot deve arretrare altrettanto.
    #[test]
    fn negative_delay_moves_the_snapshot_end_back() {
        let mut config = Config::default();
        config.audio.mode = TrackMode::Separate;
        config.audio.add_mix_track = false;
        config.audio.sources[0].delay_ms = -500;
        let now = QpcTime::now();
        let back = now.hns_since(audio_snapshot_end(now, &config, 0));
        assert!(back >= 500 * 10_000, "la fine arretra di {} ms, non di 500", back / 10_000);
        config.audio.sources[0].delay_ms = -5_000;
        let back = now.hns_since(audio_snapshot_end(now, &config, 0));
        assert!(back < 1_100 * 10_000, "il ritardo è limitato a 1 s");
    }

    /// Coda dei salvataggi (verifica, R4): un panic in un lavoro non ferma quelli successivi.
    #[test]
    fn save_queue_survives_a_panicking_job() {
        let shared = Shared::default();
        let first = shared.enqueue(Box::new(|| panic!("panic di prova nella coda dei salvataggi"))).unwrap();
        assert!(first.join().is_err());
        let second = shared.enqueue(Box::new(|| {}));
        assert!(second.is_some_and(|h| h.join().is_ok()), "la coda è morta dopo il panic");
    }

    /// M2 (verifica, R1): dopo un avvio con config illeggibile, «Avvia buffer» non parte con la config
    /// in memoria finché il file non è valido, e salvare una config valida avvia il buffer.
    #[test]
    fn m2_fixed_config_starts_the_stopped_buffer() {
        let _serial = heavy();
        let dir = fault_dir("m2-ricarica");
        let path = dir.join("config.toml");
        // Nessuna cattura vera: video spento, una sola app che non è in esecuzione.
        let mut config = Config::default();
        config.video.enabled = false;
        config.audio.sources = vec![cliprec_core::config::AudioSourceConfig::new(
            "Assente",
            cliprec_core::config::AudioSourceKind::App { exe: "cliprec-app-assente.exe".into() },
        )];
        config.clips.folder = Some(dir.clone());
        std::fs::write(&path, "[clips
video_seconds = = 30").unwrap();
        if let Ok(mut p) = persist().lock() {
            *p = None;
        }
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (ev_tx, _ev_rx) = std::sync::mpsc::channel();
        let status = Arc::new(Mutex::new(EngineStatus::default()));
        let running = |s: &Arc<Mutex<EngineStatus>>| s.lock().unwrap().buffer_running;
        let (cfg, st, p) = (config.clone(), status.clone(), path.clone());
        let engine = std::thread::spawn(move || run(cfg, false, &p, &cmd_rx, ev_tx, st));
        std::thread::sleep(Duration::from_millis(800));
        let at_start = running(&status);
        cmd_tx.send(AppCommand::StartBuffer).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        let after_start_with_bad_file = running(&status);
        config.save(&path).unwrap();
        cmd_tx.send(AppCommand::ReloadConfig).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        let after_fix = running(&status);
        cmd_tx.send(AppCommand::Quit).unwrap();
        engine.join().unwrap();
        assert!(!at_start, "il buffer non deve partire con una config illeggibile");
        assert!(!after_start_with_bad_file, "«Avvia buffer» non deve usare la config in memoria se il file è illeggibile");
        assert!(after_fix, "la config corretta e salvata deve avviare il buffer");
    }

    /// m10: una sorgente in attesa (app chiusa) non è un errore; un errore vero sì.
    #[test]
    fn m10_waiting_sources_are_not_errors() {
        let waiting = vec![("Discord".to_string(), SourceState::Waiting("Discord.exe non è in esecuzione".into()))];
        assert_eq!(status_problem(&waiting), None);
        let error = vec![("Microfono".to_string(), SourceState::Error("accesso negato".into()))];
        assert!(status_problem(&error).is_some());
    }

    /// Un'app registrata chiusa dall'utente non fa partire un avviso a ogni chiusura; gli altri
    /// stati d'attesa sì.
    #[test]
    fn closing_a_recorded_app_is_not_a_warning() {
        assert!(!warns_about(&format!("Discord.exe {}", cliprec_audio::NOT_RUNNING)));
        assert!(!warns_about(cliprec_audio::OPENING));
        assert!(warns_about("dispositivo scollegato"));
    }

    /// Il suono di conferma parte alla richiesta: `ClipRequested` arriva prima che la clip sia scritta.
    #[test]
    fn clip_request_is_confirmed_before_the_clip_is_written() {
        let _serial = heavy();
        let dir = fault_dir("richiesta");
        let mut config = Config::default();
        config.video.enabled = false;
        config.audio.sources = vec![cliprec_core::config::AudioSourceConfig::new(
            "Assente",
            cliprec_core::config::AudioSourceKind::App { exe: "cliprec-app-assente.exe".into() },
        )];
        config.clips.folder = Some(dir.clone());
        config.clips.audio_seconds = 1;
        if let Ok(mut p) = persist().lock() {
            *p = None;
        }
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let status = Arc::new(Mutex::new(EngineStatus::default()));
        let path = dir.join("config.toml");
        let engine = std::thread::spawn(move || run(config, true, &path, &cmd_rx, ev_tx, status));
        std::thread::sleep(Duration::from_millis(1500));
        cmd_tx.send(AppCommand::SaveAudioClip).unwrap();
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !seen.iter().any(|e| matches!(e, EngineEvent::ClipSaved(_))) {
            if let Ok(e) = ev_rx.recv_timeout(Duration::from_millis(100)) {
                seen.push(e);
            }
        }
        cmd_tx.send(AppCommand::Quit).unwrap();
        engine.join().unwrap();
        let requested = seen.iter().position(|e| matches!(e, EngineEvent::ClipRequested));
        let saved = seen.iter().position(|e| matches!(e, EngineEvent::ClipSaved(_)));
        assert!(requested.is_some() && saved.is_some(), "{seen:?}");
        assert!(requested < saved, "la conferma deve arrivare prima della clip scritta: {seen:?}");
    }

    /// m11: «Ferma buffer» libera la RAM del buffer.
    #[test]
    fn m11_stop_buffer_frees_the_buffer() {
        let _serial = heavy();
        let mut config = Config::default();
        config.video.enabled = false;
        config.audio.sources.clear();
        config.clips.folder = Some(fault_dir("m11"));
        if let Ok(mut p) = persist().lock() {
            *p = None;
        }
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (ev_tx, _ev_rx) = std::sync::mpsc::channel();
        let status = Arc::new(Mutex::new(EngineStatus::default()));
        let engine = std::thread::spawn(move || run(config, true, &crate::shell::config_path(), &cmd_rx, ev_tx, status));
        std::thread::sleep(Duration::from_millis(800));
        let weak = persist().lock().unwrap().as_ref().and_then(Persist::buffer_weak).unwrap();
        cmd_tx.send(AppCommand::StopBuffer).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let still_alive = weak.upgrade().is_some();
        cmd_tx.send(AppCommand::Quit).unwrap();
        engine.join().unwrap();
        assert!(!still_alive, "il buffer resta in RAM dopo «Ferma buffer»");
    }

    /// m14: lo stato riporta i codec che si aprono davvero (per le impostazioni).
    #[test]
    fn m14_status_lists_available_codecs() {
        let _serial = heavy();
        let mut config = Config::default();
        let (session, _buffer, _rx) = synthetic_session("m14", &mut config, Shared::default());
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut codecs = Vec::new();
        while Instant::now() < deadline && codecs.is_empty() {
            std::thread::sleep(Duration::from_millis(500));
            codecs = session.status().available_codecs;
        }
        assert!(codecs.contains(&cliprec_core::VideoCodec::H264), "{codecs:?}");
    }

    /// m23: «fps» nello stato sono quelli effettivi, non quelli configurati.
    #[test]
    fn m23_status_fps_is_effective() {
        let _serial = heavy();
        let mut config = Config::default();
        config.clips.folder = Some(fault_dir("m23"));
        let (tx, _rx) = std::sync::mpsc::channel();
        let device = GpuDevice::create(None).unwrap();
        let video = StallEvery(cliprec_capture::SyntheticSource::new(device.clone(), 1280, 720));
        let overrides = SourceOverrides { video: Some((device, Box::new(video))), audio: Some(beeps()) };
        let mut session = Session::start(&config, new_buffer(&config, &tx), Shared::default(), overrides, tx).unwrap();
        std::thread::sleep(Duration::from_secs(1));
        session.measure();
        std::thread::sleep(Duration::from_secs(2));
        session.measure();
        let fps = session.status().fps;
        drop(session);
        assert!(fps > 10.0 && fps < 40.0, "fps effettivi {fps}, configurati 60");
    }

    /// m24: la clip di prova lascia un esito leggibile nello stato (audio e video verificati).
    #[test]
    fn m24_test_clip_reports_a_verdict() {
        let _serial = heavy();
        let mut config = Config::default();
        let shared = Shared::default();
        let (session, _buffer, _rx) = synthetic_session("m24", &mut config, shared.clone());
        std::thread::sleep(Duration::from_millis(6500));
        session.save_test().unwrap().join().unwrap();
        let verdict = shared.last_test.lock().unwrap().clone();
        let v = verdict.expect("nessun esito della prova");
        assert!(v.contains("Bip 1 kHz") && v.contains("video"), "{v}");
    }

    #[test]
    fn reload_restarts_only_for_capture_settings() {
        let a = Config::default();
        let mut b = a.clone();
        b.hotkeys.video = "Mouse5".into();
        b.clips.filename_template = "{date}".into();
        assert_eq!(restart_scope(&a, &b), RestartScope::default());
        b.video.fps = 144;
        assert!(restart_scope(&a, &b).video);
    }

    /// Tracce diverse per tipo di clip dalla stessa sessione: clip video con una traccia per sorgente,
    /// clip solo audio con la sola traccia «Mix».
    #[test]
    fn video_and_audio_clips_have_their_own_tracks() {
        let _serial = heavy();
        use cliprec_core::config::TrackLayout;
        use cliprec_tests::probe::{Expect, check, check_audio_dir};
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/tracce-per-clip");
        let _ = std::fs::remove_dir_all(&out);
        let mut config = Config::default();
        config.audio.mode = TrackMode::Separate;
        config.audio.add_mix_track = false;
        config.audio.audio_clip = Some(TrackLayout { mode: TrackMode::Merged, add_mix_track: false });
        let clips = run_test_clip(config, 4, true, out).unwrap();
        let video = clips.iter().find(|c| c.kind == ClipKind::Video).expect("clip video");
        let audio = clips.iter().find(|c| c.kind == ClipKind::Audio).expect("clip audio");
        let names = vec!["Bip 1 kHz".to_string(), "Bip 1,5 kHz".to_string()];
        let expect = Expect { audio_tracks: Some(2), track_names: Some(names), ..Expect::default() };
        let v = check(&video.path, &expect).unwrap();
        assert!(v.is_empty(), "clip video: {v:?}");
        let expect = Expect { audio_tracks: Some(1), ..Expect::default() };
        let v = if audio.path.is_dir() { check_audio_dir(&audio.path, &expect) } else { check(&audio.path, &expect) }.unwrap();
        assert!(v.is_empty(), "clip audio: {v:?} ({})", audio.path.display());
    }

    /// Elenchi di sorgenti diversi: la clip video ha solo la sua sorgente, la clip audio solo l'altra
    /// (le due sorgenti di prova prendono il posto delle due catturate dal piano).
    #[test]
    fn video_and_audio_clips_have_their_own_sources() {
        let _serial = heavy();
        use cliprec_core::config::{AudioSourceConfig, AudioSourceKind};
        use cliprec_tests::probe::{Expect, check, check_audio_dir};
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/sorgenti-per-clip");
        let _ = std::fs::remove_dir_all(&out);
        let mut config = Config::default();
        config.audio.sources = vec![AudioSourceConfig::new("Sistema", AudioSourceKind::Output { device: None })];
        config.audio.audio_clip_sources = Some(vec![AudioSourceConfig::new("Microfono", AudioSourceKind::Input { device: None })]);
        let clips = run_test_clip(config, 4, true, out).unwrap();
        let video = clips.iter().find(|c| c.kind == ClipKind::Video).expect("clip video");
        let audio = clips.iter().find(|c| c.kind == ClipKind::Audio).expect("clip audio");
        let expect = Expect { audio_tracks: Some(1), track_names: Some(vec!["Bip 1 kHz".into()]), ..Expect::default() };
        let v = check(&video.path, &expect).unwrap();
        assert!(v.is_empty(), "clip video: {v:?}");
        let expect = Expect { audio_tracks: Some(1), ..Expect::default() };
        let v = if audio.path.is_dir() { check_audio_dir(&audio.path, &expect) } else { check(&audio.path, &expect) }.unwrap();
        assert!(v.is_empty(), "clip audio: {v:?} ({})", audio.path.display());
        // Energia a 1 kHz e a 1,5 kHz (Goertzel) della prima traccia audio di ciascuna clip.
        let energy = |path: &std::path::Path, f: f64| {
            let ffmpeg = std::env::var("FFMPEG_TOOLS_DIR").unwrap_or_else(|_| "D:/Repositories/cliprec/third_party/ffmpeg".into());
            let pcm = std::process::Command::new(std::path::Path::new(&ffmpeg).join("bin").join("ffmpeg.exe"))
                .args(["-v", "error", "-i"])
                .arg(path)
                .args(["-map", "0:a:0", "-ac", "1", "-ar", "48000", "-f", "f32le", "-"])
                .output()
                .unwrap()
                .stdout;
            let coeff = 2.0 * (2.0 * std::f64::consts::PI * f / 48_000.0).cos();
            let (mut s1, mut s2) = (0.0f64, 0.0f64);
            for b in pcm.chunks_exact(4) {
                let s0 = f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]])) + coeff * s1 - s2;
                (s2, s1) = (s1, s0);
            }
            s1 * s1 + s2 * s2 - coeff * s1 * s2
        };
        let video_ratio = energy(&video.path, 1000.0) / energy(&video.path, 1500.0);
        let audio_ratio = energy(&audio.path, 1500.0) / energy(&audio.path, 1000.0);
        println!("energia 1 kHz / 1,5 kHz nella clip video {video_ratio:.0}, 1,5 / 1 kHz nella clip audio {audio_ratio:.0}");
        assert!(video_ratio > 10.0 && audio_ratio > 10.0, "ogni clip deve avere solo il bip della sua sorgente");
    }

    /// Catena verticale con sorgenti sintetiche (lampo e bip a ogni secondo QPC): 8 s di
    /// registrazione, una clip video MP4 e una clip audio, validate con ffprobe e con la misura
    /// della sincronia. Richiede una GPU con encoder hardware.
    #[test]
    fn vertical_chain_with_synthetic_sources() {
        let _serial = heavy();
        use cliprec_tests::probe::{Expect, check, check_audio_dir};
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/e2e");
        let _ = std::fs::remove_dir_all(&out);
        let clips = run_test_clip(Config::default(), 8, true, out).unwrap();
        let video = clips.iter().find(|c| c.kind == ClipKind::Video).expect("clip video");
        let audio = clips.iter().find(|c| c.kind == ClipKind::Audio).expect("clip audio");
        println!("{clips:#?}");

        let names = vec!["Bip 1 kHz".to_string(), "Bip 1,5 kHz".to_string()];
        let expect = Expect {
            min_duration: Some(7.0),
            audio_tracks: Some(2),
            track_names: Some(names.clone()),
            fps: Some(60.0),
            ..Expect::default()
        };
        let violations = check(&video.path, &expect).unwrap();
        assert!(violations.is_empty(), "clip video: {violations:?}");
        let sync = cliprec_tests::sync::measure_sync(&video.path).unwrap();
        println!("sincronia video: {sync:#?}");
        let violations = sync.violations(20.0);
        assert!(violations.is_empty(), "sincronia video: {violations:?}");

        let expect = Expect { min_duration: Some(7.0), audio_tracks: Some(2), ..Expect::default() };
        let violations =
            if audio.path.is_dir() { check_audio_dir(&audio.path, &expect) } else { check(&audio.path, &expect) }.unwrap();
        assert!(violations.is_empty(), "clip audio: {violations:?}");
        let sync = cliprec_tests::sync::measure_sync(&audio.path).unwrap();
        println!("sincronia audio: {sync:#?}");
        assert!(sync.inter_track.iter().all(|s| s.max_abs_ms <= 20.0), "tracce audio: {sync:?}");
    }
}
