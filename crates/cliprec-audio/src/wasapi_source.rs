//! Sorgenti WASAPI (modulo FFI): loopback di un dispositivo d'uscita, dispositivo d'ingresso,
//! process loopback di un'applicazione. Un thread per sorgente, registrato in MMCSS «Pro Audio»,
//! a eventi con attesa massima di 10 ms; i vuoti diventano silenzio tramite [`Continuity`].

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cliprec_core::config::AudioSourceKind;
use cliprec_core::{AudioSource, CHANNELS, ChunkCallback, QpcTime, SAMPLE_RATE, SourceError, SourceState};
use wasapi::{
    AudioCaptureClient, AudioClient, DeviceEnumerator, DeviceEventCallbacks, DeviceEventRegistration, Direction,
    DisconnectReason, EventCallbacks, EventRegistration, Handle, Role, SampleType, StreamMode, WasapiError, WaveFormat,
};
use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED, ERROR_NOT_FOUND, HANDLE};
use windows::Win32::Media::Audio::{AUDCLNT_E_DEVICE_IN_USE, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_SERVICE_NOT_RUNNING};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW};
use windows::core::w;

use crate::dsp::{Continuity, mono_to_stereo};

/// Attesa massima di un evento WASAPI: il loopback non segnala nulla quando non suona niente.
const EVENT_TIMEOUT_MS: u32 = 10;
/// Ogni quanto si ritenta l'apertura di un dispositivo o la ricerca dell'app.
const RETRY: Duration = Duration::from_secs(3);
/// Durata del buffer condiviso richiesto a WASAPI.
const BUFFER_HNS: i64 = 1_000_000;
/// Pacchetto massimo gestito senza allocare (1 s).
const MAX_PACKET_FRAMES: usize = SAMPLE_RATE as usize;

/// Stato `Waiting` tra `start` e la fine del primo tentativo di apertura.
pub const OPENING: &str = "apertura in corso";
/// Fine del messaggio di attesa di una sorgente App la cui app non è aperta.
pub const NOT_RUNNING: &str = "non è in esecuzione";

const MIC_PRIVACY: &str = "Windows nega l'accesso al microfono: apri Impostazioni → Privacy e sicurezza → Microfono \
     e attiva «Consenti alle app desktop di accedere al microfono»";
const EXCLUSIVE: &str = "un'altra app usa il dispositivo in modalità esclusiva: il suo audio non è catturabile";

/// Registrazione del thread corrente in MMCSS («Pro Audio»), annullata quando viene distrutta.
pub(crate) struct Mmcss(HANDLE);

impl Mmcss {
    pub(crate) fn register() -> Option<Self> {
        let mut index = 0u32;
        // SAFETY: il nome è una stringa wide statica terminata da zero; `index` è una variabile locale valida.
        let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) };
        match handle {
            Ok(h) => Some(Self(h)),
            Err(e) => {
                tracing::warn!("registrazione MMCSS fallita: {e}");
                None
            }
        }
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        // SAFETY: l'handle viene da AvSetMmThreadCharacteristicsW su questo stesso thread.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

/// Elenco dei dispositivi attivi: (ID endpoint, nome leggibile). `input` = ingressi, altrimenti uscite.
pub fn list_devices(input: bool) -> Result<Vec<(String, String)>, SourceError> {
    let com = wasapi::initialize_mta();
    let result = (|| {
        let dir = if input { Direction::Capture } else { Direction::Render };
        let enumerator = DeviceEnumerator::new().map_err(fail)?;
        let collection = enumerator.get_device_collection(&dir).map_err(fail)?;
        let mut out = Vec::new();
        for i in 0..collection.get_nbr_devices().map_err(fail)? {
            let dev = collection.get_device_at_index(i).map_err(fail)?;
            out.push((dev.get_id().map_err(fail)?, dev.get_friendlyname().unwrap_or_default()));
        }
        Ok(out)
    })();
    if com.is_ok() {
        wasapi::deinitialize();
    }
    result
}

fn fail(e: WasapiError) -> SourceError {
    SourceError::Failed(e.to_string())
}

/// Sorgente WASAPI di uno dei tre tipi di [`AudioSourceKind`].
///
/// - `Output`: loopback del dispositivo (predefinito se `None`, e lo segue se cambia).
/// - `Input`: microfono o interfaccia; un ingresso mono viene copiato su L e R.
/// - `App`: process loopback dell'albero di processi dell'exe, agganciato al processo radice e
///   ritrovato ogni 3 s se l'app non c'è o si riavvia (stato `Waiting`).
///
/// In ogni caso la sorgente consegna campioni continui: finché manca il dispositivo o l'app, o
/// quando nulla suona, consegna silenzio.
/// Come è stata aperta la cattura, per diagnostica e stato.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    /// Loopback di un dispositivo d'uscita, o un dispositivo d'ingresso: endpoint WASAPI.
    Endpoint,
    /// Process loopback in modalità EXCLUDE sull'albero di ClipRec: tutto l'audio tranne il nostro.
    ExcludeSelf,
    /// Process loopback in modalità INCLUDE sull'albero di un'app (PID radice).
    App(u32),
    /// Process loopback in modalità EXCLUDE sull'albero di un'app (PID radice): tutto tranne l'app.
    ExcludeApp(u32),
}

const MODE_NONE: u64 = 0;
const MODE_ENDPOINT: u64 = 1;
const MODE_EXCLUDE_SELF: u64 = 2;
const MODE_APP: u64 = 3;
const MODE_EXCLUDE_APP: u64 = 4;

pub struct WasapiSource {
    name: String,
    kind: AudioSourceKind,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<SourceState>>,
    thread: Option<JoinHandle<()>>,
    /// Thread normale (non MMCSS) che cerca il PID dell'app ogni 3 s.
    watcher: Option<JoinHandle<()>>,
    /// Modalità dell'ultima apertura riuscita: tipo nei 32 bit alti, PID nei bassi.
    mode: Arc<AtomicU64>,
    /// Per i test: l'apertura fallisce con `AccessDenied` finché è vero.
    #[cfg(test)]
    fail_open: Arc<AtomicBool>,
}

impl WasapiSource {
    pub fn new(name: impl Into<String>, kind: AudioSourceKind) -> Self {
        Self {
            name: name.into(),
            kind,
            stop: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(SourceState::Stopped)),
            thread: None,
            watcher: None,
            mode: Arc::new(AtomicU64::new(MODE_NONE)),
            #[cfg(test)]
            fail_open: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Ricerca periodica del processo radice dell'app, fuori dal thread audio: Toolhelp alloca e
/// scorre tutti i processi. Ogni mezzo secondo, o subito dopo un evento di Windows se
/// [`crate::procwatch::listen`] è attivo. Pubblica il PID (0 = assente) e chiede la riapertura se cambia.
fn watch_pid(exe: String, pid: Arc<AtomicU32>, flags: Arc<DeviceFlags>, stop: Arc<AtomicBool>) {
    let mut seen = crate::procwatch::generation();
    while !stop.load(Ordering::Acquire) {
        let until = Instant::now() + crate::procwatch::POLL;
        while Instant::now() < until {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let now = crate::procwatch::generation();
            if now != seen {
                seen = now;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let found = find_root_pid(&exe).unwrap_or(0);
        if pid.swap(found, Ordering::AcqRel) != found {
            flags.signal(true);
        }
    }
}

impl WasapiSource {
    /// Modalità dell'ultima apertura riuscita (`None` finché non si è aperto nulla).
    pub fn capture_mode(&self) -> Option<CaptureMode> {
        let v = self.mode.load(Ordering::Acquire);
        match v >> 32 {
            MODE_ENDPOINT => Some(CaptureMode::Endpoint),
            MODE_EXCLUDE_SELF => Some(CaptureMode::ExcludeSelf),
            MODE_APP => Some(CaptureMode::App(v as u32)),
            MODE_EXCLUDE_APP => Some(CaptureMode::ExcludeApp(v as u32)),
            _ => None,
        }
    }
}

impl AudioSource for WasapiSource {
    /// Avvia il thread di cattura e torna subito, in stato `Waiting("apertura in corso")`: il primo
    /// tentativo di apertura procede sul thread della sorgente, che poi riporta `Running`, `Waiting`
    /// o `Error` in `state()`. Nessun errore di apertura ferma la sorgente:
    /// dispositivo o app assenti danno `Waiting`, privacy del microfono, modalità esclusiva o altri
    /// errori danno `Error(messaggio)`; in entrambi i casi si consegna silenzio e si ritenta ogni
    /// 3 s (o subito a una notifica), quindi la traccia riprende da sola quando la causa sparisce.
    /// Torna `Err` solo se il thread non si crea.
    fn start(&mut self, on_chunk: ChunkCallback) -> Result<(), SourceError> {
        if self.thread.is_some() {
            return Ok(());
        }
        self.stop.store(false, Ordering::Release);
        let flags = Arc::new(DeviceFlags::default());
        let pid = Arc::new(AtomicU32::new(0));
        if let AudioSourceKind::App { exe } | AudioSourceKind::Except { exe } = &self.kind {
            // Prima ricerca qui, sul thread chiamante; poi un thread normale ogni 3 s.
            pid.store(find_root_pid(exe).unwrap_or(0), Ordering::Release);
            let (exe, pid, flags, stop) = (exe.clone(), pid.clone(), flags.clone(), self.stop.clone());
            self.watcher = Some(
                std::thread::Builder::new()
                    .name(format!("pid {}", self.name))
                    .spawn(move || watch_pid(exe, pid, flags, stop))
                    .map_err(|e| SourceError::Failed(format!("thread di ricerca dell'app: {e}")))?,
            );
        }
        if let Ok(mut g) = self.state.lock() {
            *g = SourceState::Waiting(OPENING.into());
        }
        let ctx = Capture {
            kind: self.kind.clone(),
            stop: self.stop.clone(),
            state: self.state.clone(),
            on_chunk,
            continuity: Continuity::new(),
            flags,
            pid,
            mode: self.mode.clone(),
            #[cfg(test)]
            fail_open: self.fail_open.clone(),
        };
        let thread = match std::thread::Builder::new().name(format!("audio {}", self.name)).spawn(move || ctx.run()) {
            Ok(t) => t,
            Err(e) => {
                self.stop();
                return Err(SourceError::Failed(format!("thread di cattura: {e}")));
            }
        };
        self.thread = Some(thread);
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for t in [self.thread.take(), self.watcher.take()].into_iter().flatten() {
            let _ = t.join();
        }
    }

    fn state(&self) -> SourceState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for WasapiSource {
    fn drop(&mut self) {
        self.stop();
    }
}

enum OpenError {
    /// Il dispositivo o l'app non ci sono (ancora): si attende in silenzio.
    Wait(String),
    /// Errore che richiede l'utente (privacy, modalità esclusiva…): stato d'errore, si ritenta.
    Fatal(SourceError),
}

/// Segnali dei callback di IMMNotificationClient (thread di sistema) al thread di cattura.
#[derive(Default)]
struct DeviceFlags {
    /// Il dispositivo da catturare è cambiato: riaprire anche se la cattura funziona.
    reopen: AtomicBool,
    /// Qualcosa è cambiato: chi attende un dispositivo riprova subito.
    wake: AtomicBool,
}

impl DeviceFlags {
    fn signal(&self, reopen: bool) {
        if reopen {
            self.reopen.store(true, Ordering::Release);
        }
        self.wake.store(true, Ordering::Release);
    }
}

/// Motivo della disconnessione della sessione, scritto dal callback di WASAPI.
const DISC_NONE: u8 = 0;
const DISC_EXCLUSIVE: u8 = 1;
const DISC_OTHER: u8 = 2;

struct Session {
    client: AudioClient,
    capture: AudioCaptureClient,
    event: Handle,
    channels: usize,
    /// PID agganciato (solo process loopback).
    pid: Option<u32>,
    disconnect: Arc<AtomicU8>,
    _events: Option<EventRegistration>,
}

struct Capture {
    kind: AudioSourceKind,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<SourceState>>,
    on_chunk: ChunkCallback,
    continuity: Continuity,
    flags: Arc<DeviceFlags>,
    /// PID radice dell'app, aggiornato da `watch_pid` (0 = non in esecuzione).
    pid: Arc<AtomicU32>,
    mode: Arc<AtomicU64>,
    #[cfg(test)]
    fail_open: Arc<AtomicBool>,
}

impl Capture {
    fn set_state(&self, s: SourceState) {
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if *g != s {
            tracing::info!(state = ?s, "sorgente audio");
            *g = s;
        }
    }

    fn run(mut self) {
        let com = wasapi::initialize_mta();
        let mmcss = Mmcss::register();
        // Supervisione: un panic nella cattura porta lo stato a `Error`, si consegna silenzio e si
        // riparte dopo l'attesa normale dei tentativi (3 s o una notifica di dispositivo).
        while !self.stop.load(Ordering::Acquire) {
            let Err(p) = std::panic::catch_unwind(AssertUnwindSafe(|| self.capture_loop())) else { break };
            let msg = crate::panic_message(p.as_ref());
            tracing::error!("panic nel thread di cattura audio: {msg}");
            self.set_state(SourceState::Error(format!("errore interno della cattura (panic): {msg}")));
            let flags = self.flags.clone();
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| self.wait_silently(&flags)));
        }
        drop(mmcss);
        if com.is_ok() {
            wasapi::deinitialize();
        }
        self.set_state(SourceState::Stopped);
    }

    fn capture_loop(&mut self) {
        let flags = self.flags.clone();
        // Notifiche dei dispositivi (IMMNotificationClient): solo segnali atomici nei callback.
        let enumerator = DeviceEnumerator::new().ok();
        let _registration = enumerator.as_ref().and_then(|e| self.register_device_events(e, flags.clone()));
        let mut buffers = Buffers::new();
        self.continuity.anchor(QpcTime::now());

        while !self.stop.load(Ordering::Acquire) {
            let outcome = self.open(enumerator.as_ref());
            match &outcome {
                Ok(_) => self.set_state(SourceState::Running),
                Err(e) => self.set_failure(e),
            }
            match outcome {
                Ok(session) => {
                    let result = self.stream(&session, &mut buffers, &flags);
                    let _ = session.client.stop_stream();
                    if let Err(e) = result {
                        self.set_failure(&e);
                        self.wait_silently(&flags);
                    }
                }
                Err(_) => self.wait_silently(&flags),
            }
        }
    }

    fn set_failure(&self, e: &OpenError) {
        self.set_state(match e {
            OpenError::Wait(msg) => SourceState::Waiting(msg.clone()),
            OpenError::Fatal(e) => SourceState::Error(e.to_string()),
        });
    }

    /// Consegna silenzio fino al prossimo tentativo (3 s o una notifica di dispositivo).
    fn wait_silently(&mut self, flags: &DeviceFlags) {
        let until = Instant::now() + RETRY;
        flags.reopen.store(false, Ordering::Release);
        while Instant::now() < until && !self.stop.load(Ordering::Acquire) && !flags.wake.swap(false, Ordering::AcqRel) {
            std::thread::sleep(Duration::from_millis(u64::from(EVENT_TIMEOUT_MS)));
            self.continuity.idle(QpcTime::now(), &mut *self.on_chunk);
        }
    }

    fn register_device_events(&self, enumerator: &DeviceEnumerator, flags: Arc<DeviceFlags>) -> Option<DeviceEventRegistration> {
        let (direction, device) = match &self.kind {
            AudioSourceKind::Input { device } => (Direction::Capture, device.clone()),
            AudioSourceKind::Output { device } => (Direction::Render, device.clone()),
            AudioSourceKind::App { .. } | AudioSourceKind::Except { .. } => return None,
        };
        let mut cb = DeviceEventCallbacks::new();
        if device.is_none() {
            let f = flags.clone();
            cb.set_default_device_callback(move |dir, role, _| {
                if dir == direction && role == Role::Console {
                    f.signal(true);
                }
            });
        }
        // Un cambiamento del nostro dispositivo fa riaprire; quello di un altro sveglia solo l'attesa.
        let on_device = move |id: String| flags.signal(device.as_deref() == Some(id.as_str()));
        let a = on_device.clone();
        cb.set_device_state_callback(move |id, _| a(id));
        cb.set_device_added_callback(on_device.clone());
        cb.set_device_removed_callback(on_device);
        enumerator.register_notification_callback(cb).ok()
    }

    fn open(&self, enumerator: Option<&DeviceEnumerator>) -> Result<Session, OpenError> {
        #[cfg(test)]
        if self.fail_open.load(Ordering::Acquire) {
            return Err(OpenError::Fatal(SourceError::AccessDenied(MIC_PRIVACY.into())));
        }
        let classify = |e: WasapiError| classify(e, &self.kind);
        let (mut client, channels, direction, pid, opened) = match &self.kind {
            AudioSourceKind::Output { device: None } => {
                // m7: tutto ciò che suona tranne ClipRec (il «ding» di salvataggio non finisce nelle
                // clip). Process loopback in modalità EXCLUDE sul nostro albero di processi.
                // ATTENZIONE: in wasapi 0.26 `include_tree = false` vuol dire proprio EXCLUDE.
                let client = AudioClient::new_application_loopback_client(std::process::id(), false).map_err(classify)?;
                (client, CHANNELS, Direction::Capture, None, MODE_EXCLUDE_SELF << 32)
            }
            AudioSourceKind::App { exe } => {
                let pid = self.pid.load(Ordering::Acquire);
                if pid == 0 {
                    return Err(OpenError::Wait(format!("{exe} {NOT_RUNNING}")));
                }
                // ATTENZIONE: in wasapi 0.26 `include_tree = false` vuol dire modalità EXCLUDE.
                let client = AudioClient::new_application_loopback_client(pid, true).map_err(classify)?;
                (client, CHANNELS, Direction::Capture, Some(pid), (MODE_APP << 32) | u64::from(pid))
            }
            AudioSourceKind::Except { .. } => {
                // Tutto tranne l'app, senza passare da un dispositivo: la sua copia nella traccia
                // dell'app e il resto restano allineati. App assente: si esclude ClipRec, e alla
                // comparsa dell'app `watch_pid` chiede la riapertura (PID 0 → PID vero).
                let pid = self.pid.load(Ordering::Acquire);
                let target = if pid == 0 { std::process::id() } else { pid };
                let client = AudioClient::new_application_loopback_client(target, false).map_err(classify)?;
                (client, CHANNELS, Direction::Capture, Some(pid), (MODE_EXCLUDE_APP << 32) | u64::from(pid))
            }
            AudioSourceKind::Input { device } | AudioSourceKind::Output { device } => {
                let input = matches!(self.kind, AudioSourceKind::Input { .. });
                let dir = if input { Direction::Capture } else { Direction::Render };
                let enumerator = enumerator.ok_or_else(|| OpenError::Wait("servizio audio non disponibile".into()))?;
                let dev = match device {
                    Some(id) => enumerator.get_device(id),
                    None => enumerator.get_default_device(&dir),
                }
                .map_err(classify)?;
                if !matches!(dev.get_state(), Ok(wasapi::DeviceState::Active)) {
                    return Err(OpenError::Wait("dispositivo non attivo".into()));
                }
                // Un ingresso mono si apre mono e si copia su L e R a guadagno unitario.
                let channels = match dev.get_device_format() {
                    Ok(f) if input && f.get_nchannels() == 1 => 1,
                    _ => CHANNELS,
                };
                (dev.get_iaudioclient().map_err(classify)?, channels, Direction::Capture, None, MODE_ENDPOINT << 32)
            }
        };
        let format = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, channels, None);
        let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: BUFFER_HNS };
        client.initialize_client(&format, &direction, &mode).map_err(classify)?;
        let event = client.set_get_eventhandle().map_err(classify)?;
        let capture = client.get_audiocaptureclient().map_err(classify)?;
        // Disconnessione della sessione (es. ExclusiveModeOverride). Il process loopback non la espone.
        let disconnect = Arc::new(AtomicU8::new(DISC_NONE));
        let events = client.get_audiosessioncontrol().ok().and_then(|ctl| {
            let d = disconnect.clone();
            let mut cb = EventCallbacks::new();
            cb.set_disconnected_callback(move |reason| {
                let v = if matches!(reason, DisconnectReason::ExclusiveModeOverride) { DISC_EXCLUSIVE } else { DISC_OTHER };
                d.store(v, Ordering::Release);
            });
            ctl.register_session_notification(cb).ok()
        });
        client.start_stream().map_err(classify)?;
        self.mode.store(opened, Ordering::Release);
        Ok(Session { client, capture, event, channels, pid, disconnect, _events: events })
    }

    /// Legge i pacchetti finché la sessione è valida. `Ok` = fermata richiesta o riapertura.
    fn stream(&mut self, s: &Session, b: &mut Buffers, flags: &DeviceFlags) -> Result<(), OpenError> {
        let classify = |e: WasapiError| classify(e, &self.kind);
        let block = 4 * s.channels;
        while !self.stop.load(Ordering::Acquire) {
            // Riapertura chiesta da una notifica di dispositivo o da `watch_pid` (app riavviata,
            // chiusa o radice cambiata: con PID 0 la riapertura finisce in `Waiting`).
            if flags.reopen.swap(false, Ordering::AcqRel) || s.pid.is_some_and(|p| p != self.pid.load(Ordering::Acquire)) {
                return Ok(());
            }
            match s.disconnect.load(Ordering::Acquire) {
                DISC_EXCLUSIVE => return Err(OpenError::Fatal(SourceError::Unsupported(EXCLUSIVE.into()))),
                DISC_OTHER => return Err(OpenError::Wait("dispositivo disconnesso".into())),
                _ => {}
            }
            let _ = s.event.wait_for_event(EVENT_TIMEOUT_MS);
            loop {
                match s.capture.get_next_packet_size().map_err(classify)? {
                    Some(0) | None => break,
                    Some(_) => {}
                }
                let (frames, info) = match s.capture.read_from_device(&mut b.bytes[..MAX_PACKET_FRAMES * block]) {
                    Ok((0, _)) => break,
                    Ok(r) => r,
                    // Pacchetto più lungo di 1 s: perso, il vuoto diventa silenzio.
                    Err(WasapiError::DataLengthTooShort { .. }) => continue,
                    Err(e) => return Err(classify(e)),
                };
                let frames = frames as usize;
                let stereo = &mut b.stereo[..frames * CHANNELS];
                if info.flags.silent {
                    stereo.fill(0.0);
                } else if s.channels == 1 {
                    let mono = &mut b.floats[..frames];
                    to_f32(&b.bytes[..frames * 4], mono);
                    mono_to_stereo(mono, stereo);
                } else {
                    to_f32(&b.bytes[..frames * block], stereo);
                }
                if info.flags.data_discontinuity {
                    tracing::debug!("discontinuità nei dati WASAPI");
                }
                let qpc = (!info.flags.timestamp_error && info.timestamp != 0).then_some(QpcTime(info.timestamp as i64));
                self.continuity.packet(qpc, stereo, QpcTime::now(), &mut *self.on_chunk);
            }
            self.continuity.idle(QpcTime::now(), &mut *self.on_chunk);
        }
        Ok(())
    }
}

/// Buffer preallocati del thread di cattura.
struct Buffers {
    bytes: Vec<u8>,
    floats: Vec<f32>,
    stereo: Vec<f32>,
}

impl Buffers {
    fn new() -> Self {
        Self {
            bytes: vec![0; MAX_PACKET_FRAMES * 4 * CHANNELS],
            floats: vec![0.0; MAX_PACKET_FRAMES],
            stereo: vec![0.0; MAX_PACKET_FRAMES * CHANNELS],
        }
    }
}

fn to_f32(bytes: &[u8], out: &mut [f32]) {
    for (o, b) in out.iter_mut().zip(bytes.chunks_exact(4)) {
        *o = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    }
}

fn classify(e: WasapiError, kind: &AudioSourceKind) -> OpenError {
    let code = match &e {
        WasapiError::Windows(w) => Some(w.code()),
        WasapiError::DeviceNotFound(_) => return OpenError::Wait("dispositivo non trovato".into()),
        _ => None,
    };
    match code {
        Some(c) if c == E_ACCESSDENIED => OpenError::Fatal(SourceError::AccessDenied(match kind {
            AudioSourceKind::Input { .. } => MIC_PRIVACY.into(),
            _ => format!("Windows nega la cattura audio: {e}"),
        })),
        Some(c) if c == AUDCLNT_E_DEVICE_IN_USE => OpenError::Fatal(SourceError::Unsupported(EXCLUSIVE.into())),
        Some(c) if c == ERROR_NOT_FOUND.to_hresult() || c == AUDCLNT_E_DEVICE_INVALIDATED => OpenError::Wait("dispositivo scollegato".into()),
        Some(c) if c == AUDCLNT_E_SERVICE_NOT_RUNNING => OpenError::Wait("servizio audio di Windows fermo".into()),
        _ => OpenError::Fatal(SourceError::Failed(e.to_string())),
    }
}

/// PID del processo radice dell'albero dell'exe: tra i processi con quel nome, risale ai genitori
/// con lo stesso nome (Chrome e Discord suonano da processi figli).
pub fn find_root_pid(exe: &str) -> Option<u32> {
    let want = exe.trim().to_ascii_lowercase();
    let want = want.strip_suffix(".exe").unwrap_or(&want);
    let procs = processes();
    let same = |name: &str| name.strip_suffix(".exe").unwrap_or(name) == want;
    let first = procs.iter().find(|p| same(&p.2))?;
    let mut cur = first;
    // Al massimo 64 passi: un PID riusato potrebbe creare un ciclo.
    for _ in 0..64 {
        match procs.iter().find(|p| p.0 == cur.1 && p.0 != cur.0 && same(&p.2)) {
            Some(parent) => cur = parent,
            None => break,
        }
    }
    Some(cur.0)
}

/// (pid, pid del genitore, nome dell'exe in minuscolo) di tutti i processi.
fn processes() -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    // SAFETY: nessun puntatore in ingresso; l'handle restituito viene chiuso sotto.
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return out;
    };
    let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    // SAFETY: `snap` è valido e `entry` ha `dwSize` impostato come richiesto dall'API.
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) }.is_ok();
    while ok {
        let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase();
        out.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
        // SAFETY: come sopra.
        ok = unsafe { Process32NextW(snap, &mut entry) }.is_ok();
    }
    // SAFETY: handle valido aperto qui sopra, chiuso una volta sola.
    let _ = unsafe { CloseHandle(snap) };
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;
    use cliprec_core::time::frames_to_hns;

    type Chunks = Arc<Mutex<Vec<(QpcTime, usize, f32)>>>;

    /// Esito di start, blocchi (inizio, campioni, picco), stato finale, secondi trascorsi tra start e stop.
    type Recorded = (Result<(), SourceError>, Vec<(QpcTime, usize, f32)>, SourceState, f64);

    fn record(kind: AudioSourceKind, ms: u64) -> Recorded {
        let chunks: Chunks = Arc::default();
        let sink = chunks.clone();
        let mut src = WasapiSource::new("test", kind);
        let t0 = Instant::now();
        let q0 = QpcTime::now();
        let started = src.start(Box::new(move |c| {
            let peak = c.samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            sink.lock().unwrap().push((c.qpc, c.samples.len() / 2, peak));
        }));
        std::thread::sleep(Duration::from_millis(ms));
        let state = src.state();
        src.stop();
        let wall = t0.elapsed().as_secs_f64();
        let v = chunks.lock().unwrap().clone();
        if let (Some(f), Some(l)) = (v.first(), v.last()) {
            let end = l.0.add_hns(frames_to_hns(l.1 as i64));
            println!("primo blocco {:.1} ms dopo start, fine {:.1} ms prima di stop", f.0.hns_since(q0) as f64 / 1e4, QpcTime::now().hns_since(end) as f64 / 1e4);
        }
        (started, v, state, wall)
    }

    fn assert_continuous(chunks: &[(QpcTime, usize, f32)], secs: f64) {
        let total: usize = chunks.iter().map(|c| c.1).sum();
        let dur = total as f64 / 48_000.0;
        println!("{} blocchi, {dur:.3} s, picco {:.4}", chunks.len(), chunks.iter().fold(0.0f32, |m, c| m.max(c.2)));
        // Il silenzio resta indietro di almeno 50 ms rispetto ad adesso (MIN_HOLDBACK_HNS); i timestamp del
        // loopback d'uscita invece sono avanti di circa la latenza di riproduzione (~45 ms misurati).
        assert!(dur > secs - 0.2 && dur <= secs + 0.1, "durata {dur} s invece di ~{secs} s");
        for w in chunks.windows(2) {
            let end = w[0].0.add_hns(frames_to_hns(w[0].1 as i64));
            // Il timestamp di ogni pacchetto è quello vero: tolleranza di 15 ms come Continuity.
            assert!(w[1].0.hns_since(end).abs() <= 150_000, "buco o sovrapposizione: {w:?}");
        }
    }

    /// Loopback reale dell'uscita predefinita per 2 s: di notte cattura silenzio, che deve
    /// comunque arrivare continuo. Non riproduce nulla.
    #[test]
    fn output_loopback_two_seconds_continuous() {
        let (started, chunks, state, wall) = record(AudioSourceKind::Output { device: None }, 2000);
        started.unwrap();
        println!("stato {state:?}");
        assert!(matches!(state, SourceState::Running | SourceState::Waiting(_)));
        assert_continuous(&chunks, wall);
    }

    /// Process loopback su explorer.exe (di solito muto): aggancio e silenzio continuo.
    #[test]
    fn app_loopback_on_explorer() {
        let pid = find_root_pid("explorer.exe");
        println!("explorer.exe radice: {pid:?}");
        let (started, chunks, state, wall) = record(AudioSourceKind::App { exe: "explorer.exe".into() }, 1500);
        started.unwrap();
        println!("stato {state:?}");
        if pid.is_some() {
            assert_eq!(state, SourceState::Running);
        }
        assert_continuous(&chunks, wall);
    }

    #[test]
    fn app_missing_waits_in_silence() {
        let (started, chunks, state, wall) = record(AudioSourceKind::App { exe: "nessuna-app-cliprec.exe".into() }, 1000);
        started.unwrap();
        assert!(matches!(state, SourceState::Waiting(_)), "{state:?}");
        assert_continuous(&chunks, wall);
        assert!(chunks.iter().all(|c| c.2 == 0.0));
    }

    #[test]
    fn root_pid_walks_up_same_exe() {
        // Questo processo di test non ha genitori con lo stesso nome: è la radice di sé stesso.
        let me = std::env::current_exe().unwrap();
        let name = me.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(find_root_pid(&name), Some(std::process::id()));
        assert_eq!(find_root_pid("nessuna-app-cliprec.exe"), None);
    }

    /// Enumera gli ingressi e apre/chiude il predefinito senza avviare lo stream (niente
    /// registrazione del microfono). Un rifiuto di privacy deve diventare `AccessDenied`.
    #[test]
    fn input_enumerate_and_open_close() {
        let devices = list_devices(true).unwrap();
        println!("ingressi: {devices:?}");
        let outputs = list_devices(false).unwrap();
        println!("uscite: {outputs:?}");
        let com = wasapi::initialize_mta();
        let result = (|| -> Result<(), SourceError> {
            let e = DeviceEnumerator::new().map_err(fail)?;
            let Ok(dev) = e.get_default_device(&Direction::Capture) else { return Ok(()) };
            let mut client = dev.get_iaudioclient().map_err(fail)?;
            let ch = dev.get_device_format().map(|f| f.get_nchannels()).unwrap_or(2) as usize;
            let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, ch.min(2), None);
            let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: BUFFER_HNS };
            client.initialize_client(&format, &Direction::Capture, &mode).map_err(|e| match classify(e, &AudioSourceKind::Input { device: None }) {
                OpenError::Fatal(e) => e,
                OpenError::Wait(m) => SourceError::NotFound(m),
            })?;
            Ok(())
        })();
        if com.is_ok() {
            wasapi::deinitialize();
        }
        match result {
            Ok(()) => {}
            Err(SourceError::AccessDenied(m)) => assert!(m.contains("Privacy")),
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn privacy_denial_maps_to_access_denied() {
        let denied = WasapiError::Windows(windows::core::Error::from_hresult(E_ACCESSDENIED));
        match classify(denied, &AudioSourceKind::Input { device: None }) {
            OpenError::Fatal(SourceError::AccessDenied(m)) => assert!(m.contains("Privacy e sicurezza → Microfono")),
            _ => panic!("E_ACCESSDENIED deve diventare AccessDenied"),
        }
        let gone = WasapiError::Windows(windows::core::Error::from_hresult(AUDCLNT_E_DEVICE_INVALIDATED));
        assert!(matches!(classify(gone, &AudioSourceKind::Output { device: None }), OpenError::Wait(_)));
        let busy = WasapiError::Windows(windows::core::Error::from_hresult(AUDCLNT_E_DEVICE_IN_USE));
        assert!(matches!(classify(busy, &AudioSourceKind::Output { device: None }), OpenError::Fatal(SourceError::Unsupported(_))));
    }

    /// Errore all'avvio (privacy simulata, sul loopback d'uscita: niente microfono): la sorgente
    /// parte in `Error` con il percorso dell'impostazione, consegna silenzio, e torna `Running` da
    /// sola al tentativo successivo quando la causa sparisce.
    #[test]
    fn startup_error_retries_until_running() {
        let chunks: Chunks = Arc::default();
        let sink = chunks.clone();
        let mut src = WasapiSource::new("test", AudioSourceKind::Output { device: None });
        src.fail_open.store(true, Ordering::Release);
        let t0 = Instant::now();
        src.start(Box::new(move |c| {
            let peak = c.samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            sink.lock().unwrap().push((c.qpc, c.samples.len() / 2, peak));
        }))
        .unwrap();
        assert!(t0.elapsed() < Duration::from_millis(200), "start non deve aspettare l'apertura");
        while src.state() == SourceState::Waiting(OPENING.into()) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        match src.state() {
            SourceState::Error(m) => assert!(m.contains("Privacy e sicurezza → Microfono"), "{m}"),
            s => panic!("atteso Error, stato {s:?}"),
        }
        std::thread::sleep(Duration::from_millis(500));
        src.fail_open.store(false, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(5);
        while src.state() != SourceState::Running && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let recovered = t0.elapsed().as_secs_f64();
        assert_eq!(src.state(), SourceState::Running);
        std::thread::sleep(Duration::from_millis(500));
        src.stop();
        let wall = t0.elapsed().as_secs_f64();
        println!("Running dopo {recovered:.2} s");
        assert!(recovered >= 2.9, "il nuovo tentativo arriva dopo 3 s, non prima: {recovered}");
        assert_continuous(&chunks.lock().unwrap(), wall);
    }

    /// M9: un panic sul thread di cattura (qui nella callback) porta la sorgente in `Error`, poi
    /// riparte da sola al tentativo successivo.
    #[test]
    fn panic_in_capture_thread_restarts_source() {
        let chunks: Chunks = Arc::default();
        let sink = chunks.clone();
        let mut calls = 0;
        let mut src = WasapiSource::new("test", AudioSourceKind::Output { device: None });
        src.start(Box::new(move |c| {
            calls += 1;
            if calls == 3 {
                panic!("panic di prova nella cattura");
            }
            sink.lock().unwrap().push((c.qpc, c.samples.len() / 2, 0.0));
        }))
        .unwrap();
        let t0 = Instant::now();
        let mut seen_error = None;
        while t0.elapsed() < Duration::from_secs(6) {
            match src.state() {
                SourceState::Error(m) => seen_error = Some(m),
                SourceState::Running if seen_error.is_some() => break,
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let state = src.state();
        std::thread::sleep(Duration::from_millis(300));
        src.stop();
        println!("errore visto: {seen_error:?}, stato finale {state:?}");
        assert!(seen_error.is_some_and(|m| m.contains("panic")), "lo stato deve passare a Error");
        assert_eq!(state, SourceState::Running);
        let last = chunks.lock().unwrap().last().unwrap().0;
        assert!(QpcTime::now().hns_since(last) < 5_000_000, "la cattura deve aver ripreso");
    }

    /// m7: «Sistema» sul dispositivo predefinito si apre in process loopback EXCLUDE sul nostro
    /// albero di processi (il «ding» di salvataggio non entra nella clip); con un dispositivo
    /// esplicito resta il loopback dell'endpoint. Nessun suono viene riprodotto.
    /// Un'app aperta mentre il buffer registra si aggancia entro un secondo (prima: fino a 3 s).
    #[test]
    fn app_started_later_is_found_within_a_second() {
        let (pid, flags, stop) = (Arc::new(AtomicU32::new(0)), Arc::new(DeviceFlags::default()), Arc::new(AtomicBool::new(false)));
        let (p, f, s) = (pid.clone(), flags.clone(), stop.clone());
        let watcher = std::thread::spawn(move || watch_pid("PING.EXE".into(), p, f, s));
        std::thread::sleep(Duration::from_millis(100));
        let mut child = std::process::Command::new(r"C:\Windows\System32\PING.EXE")
            .args(["-n", "6", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn()
            .unwrap();
        let t0 = Instant::now();
        while pid.load(Ordering::Acquire) != child.id() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let found = t0.elapsed();
        let reopen = flags.reopen.load(Ordering::Acquire);
        let _ = child.kill();
        let _ = child.wait();
        stop.store(true, Ordering::Release);
        let _ = watcher.join();
        assert!(found < Duration::from_secs(1), "app trovata dopo {found:?}");
        assert!(reopen, "la sorgente deve riaprirsi sul nuovo processo");
    }

    #[test]
    fn default_output_excludes_own_process() {
        let mode_after_open = |kind: AudioSourceKind| {
            let mut src = WasapiSource::new("test", kind);
            src.start(Box::new(|_| {})).unwrap();
            let t0 = Instant::now();
            while src.capture_mode().is_none() && t0.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(10));
            }
            let mode = src.capture_mode();
            src.stop();
            mode
        };
        let default = mode_after_open(AudioSourceKind::Output { device: None });
        println!("uscita predefinita: {default:?}");
        assert_eq!(default, Some(CaptureMode::ExcludeSelf));
        // «Tutto tranne un'app»: esclude l'albero dell'app per PID, e finché manca resta aperta.
        let explorer = find_root_pid("explorer.exe").unwrap_or(0);
        let except = mode_after_open(AudioSourceKind::Except { exe: "explorer.exe".into() });
        assert_eq!(except, Some(CaptureMode::ExcludeApp(explorer)));
        let absent = mode_after_open(AudioSourceKind::Except { exe: "nessuna-app-cliprec.exe".into() });
        assert_eq!(absent, Some(CaptureMode::ExcludeApp(0)), "senza l'app la sorgente registra comunque");
        if let Some((id, name)) = list_devices(false).unwrap().into_iter().next() {
            let explicit = mode_after_open(AudioSourceKind::Output { device: Some(id) });
            println!("uscita esplicita {name}: {explicit:?}");
            assert_eq!(explicit, Some(CaptureMode::Endpoint));
        }
    }

    /// Cattura reale dal microfono: solo a mano (`cargo test -- --ignored`), mai di notte.
    #[test]
    #[ignore]
    fn input_capture_manual() {
        let (started, chunks, state, wall) = record(AudioSourceKind::Input { device: None }, 2000);
        started.unwrap();
        println!("stato {state:?}");
        assert_continuous(&chunks, wall);
    }
}
