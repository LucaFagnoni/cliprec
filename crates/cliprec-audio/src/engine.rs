//! Il motore: avvia le sorgenti e le instrada verso le tracce del sink.
//!
//! - Tracce separate: ogni sorgente scrive direttamente la sua traccia dal proprio thread, con
//!   l'istante QPC reale del primo campione di ogni blocco.
//! - Traccia unica, o traccia «Mix» in testa: le sorgenti passano dal [`Mixer`] su un thread che
//!   produce blocchi da 10 ms su una griglia ancorata al QPC, con un ritardo fisso.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cliprec_core::config::{AudioConfig, MAX_AUDIO_SOURCES, TrackMode};
use cliprec_core::time::{frames_to_hns, hns_to_frames};
use cliprec_core::{AudioChunk, AudioSource, AudioTrackSink, CHANNELS, HNS_PER_SEC, QpcTime, SAMPLE_RATE, SourceError, SourceState};

use crate::mixer::{MIX_BLOCK_FRAMES, MixInput, Mixer};
use crate::wasapi_source::WasapiSource;

/// Ritardo minimo del mixer rispetto ad adesso. Cresce da solo fino a [`MAX_MIX_LATENCY_HNS`] se
/// i dati di una sorgente arrivano più tardi (latenza del dispositivo, trattenimento del silenzio,
/// ritardo negativo): il valore attuale è [`AudioEngine::mix_latency_hns`].
pub const MIX_LATENCY_HNS: i64 = 1_500_000; // 150 ms
/// Tetto del ritardo del mixer (le code degli ingressi tengono 2 s).
pub const MAX_MIX_LATENCY_HNS: i64 = 18_000_000;
/// Margine sopra l'età massima dei dati in arrivo.
const MIX_LATENCY_MARGIN_HNS: i64 = 300_000;
/// Ritardo massimo per sorgente, in millisecondi, in entrambe le direzioni.
pub const MAX_DELAY_MS: i32 = 1_000;
/// Nome della traccia con la somma di tutte le sorgenti.
pub const MIX_TRACK_NAME: &str = "Mix";

/// Instradamento di una sorgente, eseguito sul suo thread di cattura.
struct Route {
    track: Option<usize>,
    sink: Arc<dyn AudioTrackSink>,
    volume: f32,
    /// Ritardo della sorgente in unità da 100 ns, sommato al QPC di ogni blocco.
    delay_hns: i64,
    scratch: Vec<f32>,
    /// Canali uniti: media di sinistra e destra su entrambi.
    mono: bool,
    mono_buf: Vec<f32>,
    mix: Option<MixInput>,
}

impl Route {
    fn push(&mut self, c: AudioChunk<'_>) {
        let qpc = c.qpc.add_hns(self.delay_hns);
        let n = c.samples.len();
        if self.mono {
            if self.mono_buf.len() < n {
                // Cresce solo per pacchetti più lunghi di 1 s: a regime non alloca.
                self.mono_buf.resize(n, 0.0);
            }
            for (o, f) in self.mono_buf[..n].chunks_exact_mut(CHANNELS).zip(c.samples.chunks_exact(CHANNELS)) {
                let m = (f[0] + f[1]) * 0.5;
                o.fill(m);
            }
        }
        let samples: &[f32] = if self.mono { &self.mono_buf[..n] } else { c.samples };
        if let Some(track) = self.track {
            if self.volume == 1.0 {
                self.sink.push(track, qpc, samples);
            } else {
                if self.scratch.len() < n {
                    // Cresce solo per pacchetti più lunghi di 1 s: a regime non alloca.
                    self.scratch.resize(n, 0.0);
                }
                let out = &mut self.scratch[..n];
                for (d, s) in out.iter_mut().zip(samples) {
                    *d = s * self.volume;
                }
                self.sink.push(track, qpc, out);
            }
        }
        if let Some(mix) = &mut self.mix {
            mix.push(AudioChunk { qpc, samples });
        }
    }
}

/// Motore audio in esecuzione. Si ferma con [`AudioEngine::stop`] o quando viene distrutto.
pub struct AudioEngine {
    sources: Vec<Box<dyn AudioSource>>,
    errors: Vec<Option<String>>,
    stop: Arc<AtomicBool>,
    mixer_thread: Option<JoinHandle<()>>,
    mix_latency: Arc<AtomicI64>,
    mixer_error: Arc<Mutex<Option<String>>>,
}

impl AudioEngine {
    /// Costruisce le sorgenti WASAPI dalla configurazione (al massimo 8) e le avvia.
    pub fn start(cfg: &AudioConfig, sink: Arc<dyn AudioTrackSink>) -> Result<Self, SourceError> {
        let specs = &cfg.sources[..cfg.sources.len().min(MAX_AUDIO_SOURCES)];
        let sources = specs
            .iter()
            .map(|s| Box::new(WasapiSource::new(s.name.clone(), s.kind.clone())) as Box<dyn AudioSource>)
            .collect();
        let volumes: Vec<f32> = specs.iter().map(|s| s.volume).collect();
        let delays: Vec<i32> = specs.iter().map(|s| s.delay_ms).collect();
        let monos: Vec<bool> = specs.iter().map(|s| s.mono).collect();
        Self::with_sources(sources, &volumes, &delays, &monos, cfg.mode, cfg.add_mix_track, sink)
    }

    /// Avvia sorgenti già costruite. `volumes[i]` è il guadagno lineare della sorgente `i` (1.0 se
    /// manca); `delays_ms[i]` il suo ritardo in millisecondi (0 se manca, limitato a
    /// ±[`MAX_DELAY_MS`]; positivo = l'audio esce più tardi), applicato in tutte le tracce e nel
    /// mix come lo «Sync Offset» di OBS. Tracce: in modalità separata le sorgenti nell'ordine dato, precedute da «Mix» se
    /// `add_mix_track`; in traccia unica la sola «Mix».
    ///
    /// Una sorgente che non parte (es. microfono negato dalla privacy di Windows) non ferma le
    /// altre: la sua traccia prosegue in silenzio e l'errore resta in [`AudioEngine::states`].
    pub fn with_sources(
        mut sources: Vec<Box<dyn AudioSource>>,
        volumes: &[f32],
        delays_ms: &[i32],
        monos: &[bool],
        mode: TrackMode,
        add_mix_track: bool,
        sink: Arc<dyn AudioTrackSink>,
    ) -> Result<Self, SourceError> {
        let n = sources.len();
        let separate = mode == TrackMode::Separate;
        let mixed = !separate || add_mix_track;
        let volume = |i: usize| volumes.get(i).copied().unwrap_or(1.0);

        let mut names = Vec::with_capacity(n + 1);
        if mixed {
            names.push(MIX_TRACK_NAME.to_string());
        }
        if separate {
            names.extend(sources.iter().map(|s| s.name().to_string()));
        }
        sink.set_tracks(&names);
        let first_track = usize::from(mixed);

        let (mixer, inputs) = if mixed {
            let vols: Vec<f32> = (0..n).map(volume).collect();
            let (m, inputs) = Mixer::new(&vols)?;
            (Some(m), inputs.into_iter().map(Some).collect::<Vec<_>>())
        } else {
            (None, (0..n).map(|_| None).collect())
        };

        let mut errors = vec![None; n];
        for (i, (src, mix)) in sources.iter_mut().zip(inputs).enumerate() {
            let mut route = Route {
                track: separate.then_some(first_track + i),
                sink: sink.clone(),
                volume: volume(i),
                delay_hns: i64::from(delays_ms.get(i).copied().unwrap_or(0).clamp(-MAX_DELAY_MS, MAX_DELAY_MS)) * 10_000,
                scratch: vec![0.0; SAMPLE_RATE as usize * CHANNELS],
                mono: monos.get(i).copied().unwrap_or(false),
                mono_buf: vec![0.0; SAMPLE_RATE as usize * CHANNELS],
                mix,
            };
            if let Err(e) = src.start(Box::new(move |c| route.push(c))) {
                tracing::warn!(source = src.name(), "sorgente audio non avviata: {e}");
                errors[i] = Some(e.to_string());
            }
        }

        // Tracce delle sorgenti fallite: silenzio sulla griglia del mixer.
        let silent: Vec<usize> = if separate {
            errors.iter().enumerate().filter(|(_, e)| e.is_some()).map(|(i, _)| first_track + i).collect()
        } else {
            Vec::new()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let mix_latency = Arc::new(AtomicI64::new(MIX_LATENCY_HNS));
        let mixer_error = Arc::new(Mutex::new(None));
        let mixer_thread = if mixer.is_some() || !silent.is_empty() {
            let (stop, latency, error) = (stop.clone(), mix_latency.clone(), mixer_error.clone());
            Some(
                std::thread::Builder::new()
                    .name("mixer audio".into())
                    .spawn(move || mixer_loop(mixer, silent, sink, stop, latency, error))
                    .map_err(|e| SourceError::Failed(format!("thread del mixer: {e}")))?,
            )
        } else {
            None
        };
        Ok(Self { sources, errors, stop, mixer_thread, mix_latency, mixer_error })
    }

    /// Ultimo panic del thread del mixer (il mixer prosegue col blocco successivo), se c'è stato.
    pub fn mixer_error(&self) -> Option<String> {
        self.mixer_error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Ritardo attuale del mixer (traccia unica e «Mix») rispetto ad adesso, in unità da 100 ns:
    /// i blocchi più recenti di questa traccia coprono fino a `adesso − mix_latency_hns()`.
    pub fn mix_latency_hns(&self) -> i64 {
        self.mix_latency.load(Ordering::Relaxed)
    }

    /// Nome e stato di ogni sorgente, nell'ordine di configurazione.
    pub fn states(&self) -> Vec<(String, SourceState)> {
        self.sources
            .iter()
            .zip(&self.errors)
            .map(|(s, e)| {
                let state = match e {
                    Some(e) => SourceState::Error(e.clone()),
                    None => s.state(),
                };
                (s.name().to_string(), state)
            })
            .collect()
    }

    pub fn stop(&mut self) {
        for s in &mut self.sources {
            s.stop();
        }
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.mixer_thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

fn mixer_loop(
    mut mixer: Option<Mixer>,
    silent: Vec<usize>,
    sink: Arc<dyn AudioTrackSink>,
    stop: Arc<AtomicBool>,
    latency: Arc<AtomicI64>,
    error: Arc<Mutex<Option<String>>>,
) {
    let mmcss = crate::wasapi_source::Mmcss::register();
    let zeros = vec![0.0f32; MIX_BLOCK_FRAMES * CHANNELS];
    // Griglia ancorata al secondo intero di QPC: i blocchi cadono su multipli esatti di 480 campioni.
    let now = QpcTime::now();
    let origin = QpcTime(now.0 - now.0.rem_euclid(HNS_PER_SEC));
    let block = MIX_BLOCK_FRAMES as i64;
    let mut k = hns_to_frames(now.add_hns(-MIX_LATENCY_HNS).hns_since(origin)).div_euclid(block);
    while !stop.load(Ordering::Acquire) {
        let needed = mixer.as_ref().map_or(0, |m| m.input_latency_hns() + MIX_LATENCY_MARGIN_HNS);
        let current = needed.clamp(MIX_LATENCY_HNS, MAX_MIX_LATENCY_HNS);
        latency.store(current, Ordering::Relaxed);
        let ready_until = QpcTime::now().add_hns(-current);
        loop {
            let start = origin.add_hns(frames_to_hns(k * block));
            if origin.add_hns(frames_to_hns((k + 1) * block)) > ready_until {
                break;
            }
            // Supervisione: un panic perde al più questo blocco; il mixer prosegue col successivo.
            let block = std::panic::catch_unwind(AssertUnwindSafe(|| {
                if let Some(m) = &mut mixer {
                    sink.push(0, start, m.render(start));
                }
                for &t in &silent {
                    sink.push(t, start, &zeros);
                }
            }));
            if let Err(p) = block {
                let msg = crate::panic_message(p.as_ref());
                tracing::error!("panic nel mixer audio: {msg}");
                *error.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("errore interno del mixer (panic): {msg}"));
            }
            k += 1;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(mmcss);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BeepSource;
    use std::sync::Mutex;

    /// Mono: un microfono su un solo canale finisce su entrambi, a metà livello (poi il volume).
    #[test]
    fn mono_source_puts_the_same_signal_on_both_channels() {
        let sink = Arc::new(TestSink::default());
        sink.set_tracks(&["Mic".into()]);
        let mut route = Route {
            track: Some(0),
            sink: sink.clone(),
            volume: 2.0,
            delay_hns: 0,
            scratch: Vec::new(),
            mono: true,
            mono_buf: Vec::new(),
            mix: None,
        };
        // Solo il canale destro ha segnale, come un microfono sull'ingresso 2.
        route.push(AudioChunk { qpc: QpcTime(0), samples: &[0.0, 0.5, 0.0, 0.5] });
        assert_eq!(sink.data.lock().unwrap()[0], vec![0.5; 4]);
    }

    #[derive(Default)]
    struct TestSink {
        names: Mutex<Vec<String>>,
        /// Per traccia: (inizio, campioni) di ogni blocco e i campioni stessi.
        blocks: Mutex<Vec<Vec<(QpcTime, usize)>>>,
        data: Mutex<Vec<Vec<f32>>>,
    }

    impl AudioTrackSink for TestSink {
        fn set_tracks(&self, names: &[String]) {
            *self.names.lock().unwrap() = names.to_vec();
            *self.blocks.lock().unwrap() = vec![Vec::new(); names.len()];
            *self.data.lock().unwrap() = vec![Vec::new(); names.len()];
        }
        fn push(&self, track: usize, start: QpcTime, samples: &[f32]) {
            assert_eq!(samples.len() % 2, 0);
            self.blocks.lock().unwrap()[track].push((start, samples.len() / 2));
            self.data.lock().unwrap()[track].extend_from_slice(samples);
        }
    }

    impl TestSink {
        fn check_contiguous(&self, min_secs: f64) {
            for (t, blocks) in self.blocks.lock().unwrap().iter().enumerate() {
                let total: usize = blocks.iter().map(|b| b.1).sum();
                assert!(total as f64 >= min_secs * 48_000.0, "traccia {t}: {total} campioni");
                for w in blocks.windows(2) {
                    let end = w[0].0.add_hns(frames_to_hns(w[0].1 as i64));
                    assert!((w[1].0.hns_since(end)).abs() <= 2, "traccia {t} non contigua: {w:?}");
                }
            }
        }
    }

    fn beeps() -> Vec<Box<dyn AudioSource>> {
        vec![Box::new(BeepSource::new("A", 1000.0)), Box::new(BeepSource::new("B", 1500.0))]
    }

    #[test]
    fn separate_mode_with_mix_track() {
        let sink = Arc::new(TestSink::default());
        let mut engine = AudioEngine::with_sources(beeps(), &[1.0, 0.5], &[], &[], TrackMode::Separate, true, sink.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        assert!(engine.states().iter().all(|(_, s)| *s == SourceState::Running));
        engine.stop();
        assert_eq!(*sink.names.lock().unwrap(), ["Mix", "A", "B"]);
        sink.check_contiguous(0.9);
        // Volume 0,5 applicato alla traccia B.
        let data = sink.data.lock().unwrap();
        let peak = |v: &[f32]| v.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        if peak(&data[1]) > 0.4 {
            assert!(peak(&data[2]) < 0.26);
        }
    }

    #[test]
    fn merged_mode_single_track_beeps_on_whole_seconds() {
        let sink = Arc::new(TestSink::default());
        let mut engine = AudioEngine::with_sources(beeps(), &[], &[], &[], TrackMode::Merged, false, sink.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(2600));
        engine.stop();
        assert_eq!(*sink.names.lock().unwrap(), ["Mix"]);
        sink.check_contiguous(2.0);
        let blocks = sink.blocks.lock().unwrap();
        let data = sink.data.lock().unwrap();
        let (start, _) = blocks[0][0];
        let mut loud = 0;
        for (i, f) in data[0].chunks_exact(2).enumerate() {
            let t = start.add_hns(frames_to_hns(i as i64));
            let off = hns_to_frames(t.0.rem_euclid(HNS_PER_SEC));
            if (2_400 + 48..48_000 - 48).contains(&off) {
                assert!(f[0].abs() < 1e-3, "suono fuori dal bip a {off} campioni dal secondo");
            } else if (48..2_400 - 48).contains(&off) && f[0].abs() > 0.1 {
                loud += 1;
            }
        }
        assert!(loud > 1000, "bip nel mix: {loud} campioni");
    }

    /// Catena reale senza microfono: loopback d'uscita + process loopback di explorer.exe.
    #[test]
    fn start_from_config_with_real_sources() {
        use cliprec_core::config::{AudioSourceConfig, AudioSourceKind};
        let cfg = AudioConfig {
            mode: TrackMode::Separate,
            add_mix_track: true,
            sources: vec![
                AudioSourceConfig::new("Sistema", AudioSourceKind::Output { device: None }),
                AudioSourceConfig { volume: 0.8, ..AudioSourceConfig::new("Explorer", AudioSourceKind::App { exe: "explorer.exe".into() }) },
            ],
            ..AudioConfig::default()
        };
        let sink = Arc::new(TestSink::default());
        let mut engine = AudioEngine::start(&cfg, sink.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        println!("{:?}", engine.states());
        engine.stop();
        assert_eq!(*sink.names.lock().unwrap(), ["Mix", "Sistema", "Explorer"]);
        let blocks = sink.blocks.lock().unwrap();
        for (t, b) in blocks.iter().enumerate() {
            let total: usize = b.iter().map(|x| x.1).sum();
            println!("traccia {t}: {} blocchi, {:.3} s", b.len(), total as f64 / 48_000.0);
            assert!(total as f64 > 1.0 * 48_000.0, "traccia {t}: {total}");
            for w in b.windows(2) {
                let end = w[0].0.add_hns(frames_to_hns(w[0].1 as i64));
                assert!(w[1].0.hns_since(end).abs() <= 20_000, "traccia {t}: {w:?}");
            }
        }
    }

    /// Riavvio della sessione: `start` non aspetta le aperture WASAPI, e gli stati poi diventano
    /// significativi da soli.
    #[test]
    fn start_with_three_sources_returns_quickly() {
        use crate::wasapi_source::OPENING;
        use cliprec_core::config::{AudioSourceConfig, AudioSourceKind};
        let src = |name: &str, kind| AudioSourceConfig::new(name, kind);
        let cfg = AudioConfig {
            mode: TrackMode::Merged,
            add_mix_track: false,
            sources: vec![
                src("Sistema", AudioSourceKind::Output { device: None }),
                src("Explorer", AudioSourceKind::App { exe: "explorer.exe".into() }),
                src("Assente", AudioSourceKind::App { exe: "nessuna-app-cliprec.exe".into() }),
            ],
            ..AudioConfig::default()
        };
        let sink = Arc::new(TestSink::default());
        let t0 = std::time::Instant::now();
        let mut engine = AudioEngine::start(&cfg, sink).unwrap();
        let took = t0.elapsed();
        println!("AudioEngine::start con 3 sorgenti: {took:?}");
        assert!(took < Duration::from_secs(1), "{took:?}");
        let opening = SourceState::Waiting(OPENING.into());
        while engine.states().iter().any(|(_, s)| *s == opening) && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let states = engine.states();
        println!("{states:?} dopo {:?}", t0.elapsed());
        engine.stop();
        assert_eq!(states[0].1, SourceState::Running);
        assert!(matches!(states[2].1, SourceState::Waiting(ref m) if m.contains("non è in esecuzione")));
    }

    /// Sorgente che consegna blocchi di 0,5 costante con 300 ms di latenza (timestamp veri nel passato).
    struct LateSource {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl AudioSource for LateSource {
        fn start(&mut self, mut on_chunk: cliprec_core::ChunkCallback) -> Result<(), SourceError> {
            let stop = self.stop.clone();
            self.thread = Some(std::thread::spawn(move || {
                let buf = vec![0.5f32; 960];
                let t0 = QpcTime::now().add_hns(-3_000_000);
                let mut k = 0i64;
                while !stop.load(Ordering::Acquire) {
                    let qpc = t0.add_hns(frames_to_hns(k * 480));
                    if QpcTime::now().hns_since(qpc) >= 3_000_000 + frames_to_hns(480) {
                        on_chunk(AudioChunk { qpc, samples: &buf });
                        k += 1;
                    } else {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            }));
            Ok(())
        }
        fn stop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(t) = self.thread.take() {
                t.join().unwrap();
            }
        }
        fn state(&self) -> SourceState {
            SourceState::Running
        }
        fn name(&self) -> &str {
            "Lenta"
        }
    }

    /// M3 nel mixer: una sorgente con latenza oltre MIX_LATENCY (es. microfono a 300 ms) deve
    /// comparire nel Mix, non essere scartata come «passata».
    #[test]
    fn mix_keeps_late_source() {
        let sink = Arc::new(TestSink::default());
        let late: Box<dyn AudioSource> = Box::new(LateSource { stop: Arc::default(), thread: None });
        let mut engine = AudioEngine::with_sources(vec![late], &[], &[], &[], TrackMode::Merged, false, sink.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(2000));
        engine.stop();
        let data = sink.data.lock().unwrap();
        // Il primo mezzo secondo può essere silenzio di avvio; poi il segnale deve esserci.
        let tail = &data[0][48_000..];
        let audible = tail.iter().filter(|s| (**s - 0.5).abs() < 1e-3).count();
        let ratio = audible as f64 / tail.len().max(1) as f64;
        println!("Mix con sorgente a 300 ms: {:.1}% dei campioni con il segnale", ratio * 100.0);
        assert!(ratio > 0.95, "{:.1}%", ratio * 100.0);
    }

    /// M9: un panic nel thread del mixer (qui nel sink, al primo blocco del Mix) non ferma il Mix.
    #[test]
    fn mixer_survives_panic() {
        struct PanicOnce(TestSink, AtomicBool);
        impl AudioTrackSink for PanicOnce {
            fn set_tracks(&self, names: &[String]) {
                self.0.set_tracks(names);
            }
            fn push(&self, track: usize, start: QpcTime, samples: &[f32]) {
                if track == 0 && !self.1.swap(true, Ordering::AcqRel) {
                    panic!("panic di prova nel mixer");
                }
                self.0.push(track, start, samples);
            }
        }
        let sink = Arc::new(PanicOnce(TestSink::default(), AtomicBool::new(false)));
        let mut engine = AudioEngine::with_sources(beeps(), &[], &[], &[], TrackMode::Merged, false, sink.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(1000));
        assert!(engine.mixer_error().is_some_and(|m| m.contains("panic di prova")));
        engine.stop();
        let total: usize = sink.0.blocks.lock().unwrap()[0].iter().map(|b| b.1).sum();
        println!("Mix dopo il panic: {:.3} s", total as f64 / 48_000.0);
        assert!(total > 48_000 / 2, "il Mix si è fermato: {total} campioni");
    }

    /// Ritardo per sorgente: A a +100 ms, B a −100 ms, nelle tracce separate e nel Mix.
    #[test]
    fn per_source_delay_shifts_beeps() {
        let sink = Arc::new(TestSink::default());
        let mut engine = AudioEngine::with_sources(beeps(), &[], &[100, -100], &[], TrackMode::Separate, true, sink.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(2600));
        engine.stop();
        let blocks = sink.blocks.lock().unwrap();
        let data = sink.data.lock().unwrap();
        // Campioni forti per traccia, divisi per finestra di 50 ms dall'inizio del secondo intero.
        let loud_windows = |track: usize| {
            let mut windows = [0usize; 20];
            let mut pos = 0usize;
            for &(start, frames) in &blocks[track] {
                for i in 0..frames {
                    let t = start.add_hns(frames_to_hns(i as i64));
                    if data[track][(pos + i) * 2].abs() > 0.1 {
                        windows[(hns_to_frames(t.0.rem_euclid(HNS_PER_SEC)) / 2_400).clamp(0, 19) as usize] += 1;
                    }
                }
                pos += frames;
            }
            windows
        };
        let (mix, a, b) = (loud_windows(0), loud_windows(1), loud_windows(2));
        println!("finestre da 50 ms con suono: Mix {mix:?}\nA {a:?}\nB {b:?}");
        let only = |w: &[usize; 20], idx: usize| w.iter().enumerate().all(|(i, n)| (i == idx) == (*n > 0));
        assert!(only(&a, 2), "A (+100 ms) deve suonare solo in 100-150 ms");
        assert!(only(&b, 18), "B (−100 ms) deve suonare solo in 900-950 ms");
        assert!(mix[2] > 0 && mix[18] > 0 && mix[0] == 0, "il Mix deve seguire i ritardi");
    }

    struct Denied;
    impl AudioSource for Denied {
        fn start(&mut self, _: cliprec_core::ChunkCallback) -> Result<(), SourceError> {
            Err(SourceError::AccessDenied("microfono".into()))
        }
        fn stop(&mut self) {}
        fn state(&self) -> SourceState {
            SourceState::Stopped
        }
        fn name(&self) -> &str {
            "Mic"
        }
    }

    #[test]
    fn failed_source_track_continues_in_silence() {
        let sink = Arc::new(TestSink::default());
        let sources: Vec<Box<dyn AudioSource>> = vec![Box::new(BeepSource::new("A", 1000.0)), Box::new(Denied)];
        let mut engine = AudioEngine::with_sources(sources, &[], &[], &[], TrackMode::Separate, false, sink.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(700));
        assert!(matches!(engine.states()[1].1, SourceState::Error(_)));
        engine.stop();
        sink.check_contiguous(0.4);
        assert!(sink.data.lock().unwrap()[1].iter().all(|s| *s == 0.0));
    }
}
