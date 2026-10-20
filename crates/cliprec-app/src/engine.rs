//! Motore di ClipRec: collega sorgenti, encoder, buffer e salvataggio ai comandi della shell.
//!
//! Un thread esegue [`run`]; una [`Session`] tiene sorgenti ed encoder accesi e scrive nel
//! [`ReplayBuffer`], che sopravvive ai riavvii della sessione (device perso, configurazione
//! ricaricata) e a un panic del motore. La hotkey prende uno snapshot del buffer (lock
//! brevissimo) e un thread a bassa priorità scrive il file.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cliprec_buffer::{BufferConfig, ReplayBuffer};
use cliprec_core::config::{Config, TrackMode, default_max_bitrate_kbps};
use cliprec_core::{
    AppCommand, AudioTrackSink, ClipKind, EncodeError, EngineEvent, EngineStatus, GpuDevice, GpuFrame, QpcTime,
    SourceState, VideoEncoder, VideoSource,
};

/// Clip mostrate nel menu della tray.
const RECENT_MAX: usize = 5;

/// Sorgenti iniettabili (test e sorgenti sintetiche); `None` = dalla configurazione.
#[derive(Default)]
pub struct SourceOverrides {
    pub video: Option<(GpuDevice, Box<dyn VideoSource>)>,
    pub audio: Option<Vec<Box<dyn cliprec_core::AudioSource>>>,
}

/// Stato che dura più di una sessione: ultime clip, salvataggi in corso.
#[derive(Clone, Default)]
pub struct Shared {
    recent: Arc<Mutex<VecDeque<PathBuf>>>,
    saving: Arc<AtomicU32>,
}

impl Shared {
    /// Aspetta fino a `timeout` che i salvataggi in corso finiscano (uscita dall'app).
    fn wait_savers(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.saving.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Buffer dimensionato dalla configurazione; con poca RAM libera il tetto scende e un avviso lo dice.
/// `reclaim`: byte del buffer che questo sostituisce, ancora vivo durante la misura della RAM libera.
pub fn new_buffer(config: &Config, events: &Sender<EngineEvent>, reclaim: usize) -> Arc<ReplayBuffer> {
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
            BufferConfig::estimate_bytes(u64::from(kbps) * 1000, video_seconds, audio_seconds, config.audio.sources.len() + 1)
        }
    };
    // ponytail: avviso e tetto ridotto a metà della RAM libera invece della proposta interattiva della
    // specifica; la finestra impostazioni mostra già la stima per scegliere durata e qualità.
    let available = available_ram().saturating_add(reclaim);
    let max_bytes = if available > 0 && wanted > available / 2 {
        let _ = events.send(EngineEvent::Warning(format!(
            "RAM libera insufficiente: il buffer chiederebbe {} MB, ne uso {}. Riduci durata o qualità nelle impostazioni.",
            wanted >> 20,
            (available / 2) >> 20
        )));
        available / 2
    } else {
        wanted
    };
    Arc::new(ReplayBuffer::new(BufferConfig { video_seconds, audio_seconds, max_bytes, lock_pages: config.memory.lock_pages }))
}

fn available_ram() -> usize {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    // SAFETY: struttura valida con dwLength impostato come richiesto dall'API.
    match unsafe { GlobalMemoryStatusEx(&mut m) } {
        Ok(()) => usize::try_from(m.ullAvailPhys).unwrap_or(usize::MAX),
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
        let encoder_name = Arc::new(Mutex::new(String::new()));
        let restart = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(Mutex::new(true));
        let warn = |what: &str, e: &dyn std::fmt::Display| {
            let _ = events.send(EngineEvent::Warning(format!("{what} non avviato: {e}")));
        };

        let sink = buffer.clone() as Arc<dyn AudioTrackSink>;
        let audio = match overrides.audio {
            Some(sources) => cliprec_audio::AudioEngine::with_sources(
                sources,
                &[],
                config.audio.mode,
                config.audio.add_mix_track && config.audio.mode == TrackMode::Separate,
                sink,
            )
            .map(Some),
            None if !config.audio.sources.is_empty() => cliprec_audio::AudioEngine::start(&config.audio, sink).map(Some),
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
        };
        if config.video.enabled {
            let opened = match overrides.video {
                Some(v) => Ok(v),
                None => cliprec_capture::open_source(&config.video),
            };
            if let Err(e) = session.start_video(opened) {
                warn("video", &format!("{e}; continuo solo con l'audio e riprovo tra poco"));
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
        if let Err(e) = cliprec_encode::set_gpu_priority(&device, self.config.general.gpu_priority != "normal") {
            tracing::warn!("priorità GPU: {e}");
        }
        let on_frame = video_callback(
            &self.config,
            device,
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
        if let Some(mut v) = self.video.take() {
            v.stop();
        }
        let wait = self.video_retry.1;
        self.video_retry = (Instant::now() + wait, (wait * 2).min(Duration::from_secs(60)));
        match self.start_video(cliprec_capture::open_source(&self.config.video)) {
            Ok(()) => {
                let _ = self.events.send(EngineEvent::Warning("video ripartito".into()));
            }
            Err(e) => tracing::warn!("video non ripartito: {e}; nuovo tentativo tra {} s", wait.as_secs()),
        }
    }

    pub fn status(&self) -> EngineStatus {
        let stats = self.buffer.stats();
        let capture = self.video.as_ref().map(|v| v.stats()).unwrap_or_default();
        let problem = self.states.iter().find_map(|(name, st)| match st {
            SourceState::Waiting(m) | SourceState::Error(m) if m != cliprec_audio::OPENING => Some(format!("{name}: {m}")),
            _ => None,
        });
        EngineStatus {
            buffer_running: true,
            video_enabled: self.video.is_some(),
            video_seconds: stats.video_seconds,
            audio_seconds: stats.audio_seconds,
            buffer_bytes: stats.bytes as u64,
            ram_locked: stats.locked,
            encoder: self.encoder_name.lock().map(|n| n.clone()).unwrap_or_default(),
            source: self.video.as_ref().map(|v| v.describe()).unwrap_or_default(),
            fps: f64::from(self.config.video.fps),
            frames_dropped: capture.frames_dropped,
            saving: self.shared.saving.load(Ordering::SeqCst),
            recent_clips: self.shared.recent.lock().map(|r| r.iter().cloned().collect()).unwrap_or_default(),
            last_error: if stats.memory_limited {
                Some(format!("RAM insufficiente: il buffer copre solo {:.0} s di video", stats.video_seconds))
            } else {
                problem
            },
        }
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
                && m != cliprec_audio::OPENING
            {
                let _ = self.events.send(EngineEvent::Warning(format!("{name}: {m}")));
            }
        }
        self.states = now;
    }

    /// Prende lo snapshot degli ultimi `seconds` secondi (lock brevissimo) e scrive il file su un
    /// thread a bassa priorità.
    pub fn save(&self, kind: ClipKind, seconds: u32) -> Option<JoinHandle<()>> {
        let pressed = Instant::now();
        let end = QpcTime::now();
        let folder = self.config.clips.folder();
        let events = self.events.clone();
        let fail = move |events: &Sender<EngineEvent>, reason: String| {
            let _ = events.send(EngineEvent::ClipFailed { kind, reason });
        };
        if let Err(e) = std::fs::create_dir_all(&folder) {
            fail(&events, format!("cartella {}: {e}", folder.display()));
            return None;
        }
        let game = self.video.as_ref().map(|v| game_name(&v.describe()));
        let name = clip_name(&self.config.clips.filename_template, kind, game);
        // Il muxer aggiunge l'estensione e « (2)» se il nome esiste già.
        let path = folder.join(name);
        let stats = self.buffer.stats();
        if stats.memory_limited {
            let _ = events.send(EngineEvent::Warning(format!(
                "clip più corta di {seconds} s: la RAM del buffer copre solo {:.0} s",
                stats.video_seconds
            )));
        }
        if let Some((name, SourceState::Waiting(m) | SourceState::Error(m))) =
            self.video.as_ref().map(|v| (v.describe(), v.stats().state))
        {
            let _ = events.send(EngineEvent::Warning(format!("clip salvata, ma il video era in attesa ({name}: {m})")));
        }
        let job: Box<dyn FnOnce() -> Result<cliprec_core::SavedClip, String> + Send> = match kind {
            ClipKind::Video => {
                let Some(video) = self.buffer.snapshot_video(f64::from(seconds), end) else {
                    fail(&events, "il buffer video è ancora vuoto".into());
                    return None;
                };
                let start = video.start().unwrap_or(end);
                let last = video.packets.iter().map(|p| p.pts.add_hns(p.duration)).max().unwrap_or(end);
                let stale = end.hns_since(last);
                if stale > 2 * cliprec_core::HNS_PER_SEC {
                    let _ = events.send(EngineEvent::Warning(format!(
                        "la clip video finisce {:.0} s prima della pressione: cattura o encoder fermi",
                        stale as f64 / cliprec_core::HNS_PER_SEC as f64
                    )));
                }
                // Sotto lock solo i riferimenti ai blocchi FLAC; la decompressione è nel thread di salvataggio.
                let audio = self.buffer.snapshot_audio_refs(start, end);
                let opts = cliprec_mux::VideoClipOptions { container: self.config.clips.container, ..Default::default() };
                Box::new(move || {
                    let audio = audio.map(cliprec_buffer::AudioRefs::render);
                    cliprec_mux::write_video_clip(&path, &video, audio.as_ref(), &opts).map_err(|e| e.to_string())
                })
            }
            ClipKind::Audio => {
                let start = end.add_hns(-i64::from(seconds) * cliprec_core::HNS_PER_SEC);
                let Some(audio) = self.buffer.snapshot_audio_refs(start, end) else {
                    fail(&events, "il buffer audio è ancora vuoto".into());
                    return None;
                };
                let format = self.config.clips.audio_format;
                Box::new(move || cliprec_mux::write_audio_clip(&path, &audio.render(), format).map_err(|e| e.to_string()))
            }
        };
        let snapshot_ms = pressed.elapsed().as_secs_f64() * 1000.0;
        let shared = self.shared.clone();
        shared.saving.fetch_add(1, Ordering::SeqCst);
        let spawned = std::thread::Builder::new().name("clip-save".into()).spawn(move || {
            lower_thread_priority();
            /// Decrementa il contatore anche se il salvataggio va in panic.
            struct Done(Arc<AtomicU32>);
            impl Drop for Done {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _done = Done(shared.saving.clone());
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
                .unwrap_or_else(|_| Err("errore interno durante il salvataggio".into()));
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
                    if let Ok(mut r) = shared.recent.lock() {
                        r.push_front(clip.path.clone());
                        r.truncate(RECENT_MAX);
                    }
                    let _ = events.send(EngineEvent::ClipSaved(clip));
                }
                Err(e) => fail(&events, e),
            }
        });
        match spawned {
            Ok(h) => Some(h),
            Err(e) => {
                self.shared.saving.fetch_sub(1, Ordering::SeqCst);
                let _ = self.events.send(EngineEvent::ClipFailed { kind, reason: format!("thread di salvataggio: {e}") });
                None
            }
        }
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
type Opened = Result<Box<dyn VideoEncoder>, String>;

/// Callback del pacer. L'encoder si apre su un thread a parte (serve la dimensione della sorgente,
/// e aprirlo e prenotare il buffer dentro il pacer bloccherebbe i tick): finché non è pronto i frame
/// non si codificano. Se un encoder fallisce si passa al candidato successivo; se falliscono tutti si
/// riprova con un backoff da 1 a 30 s. Un device perso chiede al loop di ricreare la sessione.
fn video_callback(
    config: &Config,
    device: GpuDevice,
    buffer: Arc<ReplayBuffer>,
    encoder_name: Arc<Mutex<String>>,
    events: Sender<EngineEvent>,
    restart: Arc<AtomicBool>,
    alive: Arc<Mutex<bool>>,
) -> Box<dyn FnMut(&GpuFrame) + Send> {
    let video_cfg = config.video.clone();
    let fps = config.video.fps.max(1);
    let (opened_tx, opened_rx) = std::sync::mpsc::channel::<Opened>();
    let mut encoder: Option<Box<dyn VideoEncoder>> = None;
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
                        let _ = events.send(EngineEvent::Error(format!("encoder video: {msg}")));
                        last_error = msg;
                    }
                }
                Err(_) => {}
            }
        }
        let Some(enc) = encoder.as_mut() else {
            if !opening && Instant::now() >= retry_at {
                opening = true;
                let settings = cliprec_encode::EncoderSettings::from_config(&video_cfg, frame.width, frame.height);
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
                    let _ = tx.send(open_encoder(&device, &settings, &skip, &buffer, &encoder_name, &alive, &events));
                });
                if spawned.is_err() {
                    opening = false;
                }
            }
            return;
        };
        let mut push = |p: cliprec_core::PacketRef<'_>| buffer.push_video(p);
        // Il pacer salta l'indice sui tick persi: ripetendo la texture corrente per quei tick il file
        // resta a frequenza costante e l'audio resta allineato. Oltre un quarto di secondo il buco resta.
        // ponytail: al massimo 100 ms per tick, per non alimentare altri ritardi sotto carico GPU.
        let missing = next_index.map_or(0, |n| frame.index.saturating_sub(n)).min(u64::from(fps / 10));
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
        match result.and_then(|()| enc.encode(frame, &mut push)) {
            Ok(()) => next_index = Some(frame.index + 1),
            Err(e) => {
                if matches!(e, EncodeError::DeviceLost) {
                    restart.store(true, Ordering::SeqCst);
                }
                let name = enc.params().encoder_name.clone();
                let _ = events.send(EngineEvent::Warning(format!("encoder {name} fallito ({e}): passo al successivo")));
                failed.push(name);
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
        let _ = events.send(EngineEvent::Warning(format!(
            "encoder cambiato da {} a {}: il buffer video riparte da zero",
            name,
            e.params().encoder_name
        )));
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
    buffer: Arc<ReplayBuffer>,
    shared: Shared,
}

fn persist() -> &'static Mutex<Option<Persist>> {
    static P: OnceLock<Mutex<Option<Persist>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// Le sezioni che cambiano sorgenti, encoder o buffer; il resto si applica senza riavviare.
fn needs_restart(old: &Config, new: &Config) -> bool {
    old.video != new.video
        || old.audio != new.audio
        || old.memory != new.memory
        || old.clips.video_seconds != new.clips.video_seconds
        || old.clips.audio_seconds != new.clips.audio_seconds
        || old.general.gpu_priority != new.general.gpu_priority
}

/// Loop del motore: un thread, comandi dalla shell, stato aggiornato ogni 500 ms.
/// `commands` è preso per riferimento così il supervisore lo può riavviare dopo un panic.
pub fn run(config: Config, commands: &Receiver<AppCommand>, events: Sender<EngineEvent>, status: Arc<Mutex<EngineStatus>>) {
    let config_path = crate::shell::config_path();
    let restored = persist().lock().ok().and_then(|p| p.clone());
    let (mut config, mut buffer, shared) = match restored {
        Some(p) => (p.config, p.buffer, p.shared),
        None => {
            let b = new_buffer(&config, &events, 0);
            (config, b, Shared::default())
        }
    };
    let start = |config: &Config, buffer: &Arc<ReplayBuffer>| {
        match Session::start(config, buffer.clone(), shared.clone(), SourceOverrides::default(), events.clone()) {
            Ok(s) => Some(s),
            Err(e) => {
                let _ = events.send(EngineEvent::Error(format!("avvio del buffer: {e:#}")));
                None
            }
        }
    };
    let mut session = start(&config, &buffer);
    loop {
        if let Ok(mut p) = persist().lock() {
            *p = Some(Persist { config: config.clone(), buffer: buffer.clone(), shared: shared.clone() });
        }
        match commands.recv_timeout(Duration::from_millis(500)) {
            Ok(cmd @ (AppCommand::SaveVideoClip | AppCommand::SaveAudioClip | AppCommand::SaveTestClip)) => {
                if let Some(s) = &session {
                    let clips = &s.config.clips;
                    let (kind, seconds) = match cmd {
                        AppCommand::SaveAudioClip => (ClipKind::Audio, clips.audio_seconds),
                        AppCommand::SaveTestClip if s.video.is_none() => (ClipKind::Audio, 5),
                        AppCommand::SaveTestClip => (ClipKind::Video, 5),
                        // In modalità solo audio la hotkey video salva una clip audio di N secondi.
                        _ if s.video.is_none() => (ClipKind::Audio, clips.video_seconds),
                        _ => (ClipKind::Video, clips.video_seconds),
                    };
                    s.save(kind, seconds);
                } else {
                    let _ = events.send(EngineEvent::Warning("il buffer è fermo: avvialo dal menu della tray".into()));
                }
            }
            Ok(AppCommand::StartBuffer) => {
                if session.is_none() {
                    session = start(&config, &buffer);
                }
            }
            Ok(AppCommand::StopBuffer) => session = None,
            Ok(AppCommand::ReloadConfig) => match Config::load(&config_path) {
                Ok(new) => {
                    let restart = needs_restart(&config, &new);
                    let resize = config.clips.video_seconds != new.clips.video_seconds
                        || config.clips.audio_seconds != new.clips.audio_seconds
                        || config.memory != new.memory
                        || config.video.enabled != new.video.enabled;
                    config = new;
                    let was_running = session.is_some();
                    if resize {
                        drop(session.take());
                        buffer = new_buffer(&config, &events, buffer.stats().bytes);
                        if was_running {
                            session = start(&config, &buffer);
                        }
                    } else if restart && was_running {
                        // Chiude la sessione vecchia (cattura, encoder, dispositivi) prima di aprire la nuova.
                        drop(session.take());
                        session = start(&config, &buffer);
                    } else if let Some(s) = session.as_mut() {
                        s.config = config.clone();
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
            let _ = events.send(EngineEvent::Warning("device GPU perso: riavvio cattura ed encoder".into()));
            drop(session.take());
            session = start(&config, &buffer);
        }
        if let Some(s) = session.as_mut() {
            s.poll_states();
            s.retry_video();
        }
        if let Ok(mut st) = status.lock() {
            *st = session.as_ref().map(Session::status).unwrap_or_else(|| EngineStatus {
                recent_clips: shared.recent.lock().map(|r| r.iter().cloned().collect()).unwrap_or_default(),
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
    let buffer = new_buffer(&config, &tx, 0);
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
    let buffer = new_buffer(&config, &tx, 0);
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
    use super::*;

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
        let mut config = Config::default();
        config.clips.folder = Some(fault_dir("solo-audio"));
        let (tx, rx) = std::sync::mpsc::channel();
        let overrides = SourceOverrides {
            video: Some((GpuDevice::create(None).unwrap(), Box::new(FailingSource))),
            audio: Some(beeps()),
        };
        let session = Session::start(&config, new_buffer(&config, &tx, 0), Shared::default(), overrides, tx).unwrap();
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
        let dir = fault_dir("cartella");
        let blocker = dir.join("file.txt");
        std::fs::write(&blocker, b"non una cartella").unwrap();
        let mut config = Config::default();
        config.clips.folder = Some(blocker.join("clip"));
        config.video.enabled = false;
        let (tx, rx) = std::sync::mpsc::channel();
        let overrides = SourceOverrides { video: None, audio: Some(beeps()) };
        let session = Session::start(&config, new_buffer(&config, &tx, 0), Shared::default(), overrides, tx).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        assert!(session.save(ClipKind::Audio, 1).is_none());
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::ClipFailed { .. })), "{events:?}");
    }

    /// Due pressioni nello stesso secondo: due clip distinte e valide, mai un errore.
    #[test]
    fn two_saves_in_the_same_second_give_two_valid_clips() {
        use cliprec_tests::probe::{Expect, check};
        let mut config = Config::default();
        config.clips.folder = Some(fault_dir("doppia"));
        config.clips.video_seconds = 2;
        let (tx, rx) = std::sync::mpsc::channel();
        let device = GpuDevice::create(None).unwrap();
        let video = cliprec_capture::SyntheticSource::new(device.clone(), 1280, 720);
        let overrides = SourceOverrides { video: Some((device, Box::new(video))), audio: Some(beeps()) };
        let session = Session::start(&config, new_buffer(&config, &tx, 0), Shared::default(), overrides, tx).unwrap();
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

    /// Modalità audio nella catena completa: traccia unica (una sola traccia «Mix») e tracce separate
    /// con il «Mix» in testa (tre tracce, la prima predefinita), con i bip ancora sincronizzati.
    #[test]
    fn merged_and_mix_track_modes_end_to_end() {
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
            let session = Session::start(&config, new_buffer(&config, &tx, 0), Shared::default(), overrides, tx).unwrap();
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

    #[test]
    fn reload_restarts_only_for_capture_settings() {
        let a = Config::default();
        let mut b = a.clone();
        b.hotkeys.video = "Mouse5".into();
        b.clips.filename_template = "{date}".into();
        assert!(!needs_restart(&a, &b));
        b.video.fps = 144;
        assert!(needs_restart(&a, &b));
    }

    /// Catena verticale con sorgenti sintetiche (lampo e bip a ogni secondo QPC): 8 s di
    /// registrazione, una clip video MP4 e una clip audio, validate con ffprobe e con la misura
    /// della sincronia. Richiede una GPU con encoder hardware.
    #[test]
    fn vertical_chain_with_synthetic_sources() {
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
