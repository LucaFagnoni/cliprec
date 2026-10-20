//! Il motore: avvia le sorgenti e le instrada verso le tracce del sink.
//!
//! - Tracce separate: ogni sorgente scrive direttamente la sua traccia dal proprio thread, con
//!   l'istante QPC reale del primo campione di ogni blocco.
//! - Traccia unica, o traccia «Mix» in testa: le sorgenti passano dal [`Mixer`] su un thread che
//!   produce blocchi da 10 ms su una griglia ancorata al QPC, con un ritardo fisso.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use cliprec_core::config::{AudioConfig, MAX_AUDIO_SOURCES, TrackMode};
use cliprec_core::time::{frames_to_hns, hns_to_frames};
use cliprec_core::{AudioChunk, AudioSource, AudioTrackSink, CHANNELS, HNS_PER_SEC, QpcTime, SAMPLE_RATE, SourceError, SourceState};

use crate::mixer::{MIX_BLOCK_FRAMES, MixInput, Mixer};
use crate::wasapi_source::WasapiSource;

/// Ritardo del mixer rispetto ad adesso: deve superare la latenza delle sorgenti più il
/// trattenimento del riempimento di silenzio (50 ms).
pub const MIX_LATENCY_HNS: i64 = 1_500_000; // 150 ms
/// Nome della traccia con la somma di tutte le sorgenti.
pub const MIX_TRACK_NAME: &str = "Mix";

/// Instradamento di una sorgente, eseguito sul suo thread di cattura.
struct Route {
    track: Option<usize>,
    sink: Arc<dyn AudioTrackSink>,
    volume: f32,
    scratch: Vec<f32>,
    mix: Option<MixInput>,
}

impl Route {
    fn push(&mut self, c: AudioChunk<'_>) {
        if let Some(track) = self.track {
            if self.volume == 1.0 {
                self.sink.push(track, c.qpc, c.samples);
            } else {
                if self.scratch.len() < c.samples.len() {
                    // Cresce solo per pacchetti più lunghi di 1 s: a regime non alloca.
                    self.scratch.resize(c.samples.len(), 0.0);
                }
                let out = &mut self.scratch[..c.samples.len()];
                for (d, s) in out.iter_mut().zip(c.samples) {
                    *d = s * self.volume;
                }
                self.sink.push(track, c.qpc, out);
            }
        }
        if let Some(mix) = &mut self.mix {
            mix.push(c);
        }
    }
}

/// Motore audio in esecuzione. Si ferma con [`AudioEngine::stop`] o quando viene distrutto.
pub struct AudioEngine {
    sources: Vec<Box<dyn AudioSource>>,
    errors: Vec<Option<String>>,
    stop: Arc<AtomicBool>,
    mixer_thread: Option<JoinHandle<()>>,
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
        Self::with_sources(sources, &volumes, cfg.mode, cfg.add_mix_track, sink)
    }

    /// Avvia sorgenti già costruite. `volumes[i]` è il guadagno lineare della sorgente `i` (1.0 se
    /// manca). Tracce: in modalità separata le sorgenti nell'ordine dato, precedute da «Mix» se
    /// `add_mix_track`; in traccia unica la sola «Mix».
    ///
    /// Una sorgente che non parte (es. microfono negato dalla privacy di Windows) non ferma le
    /// altre: la sua traccia prosegue in silenzio e l'errore resta in [`AudioEngine::states`].
    pub fn with_sources(
        mut sources: Vec<Box<dyn AudioSource>>,
        volumes: &[f32],
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
                scratch: vec![0.0; SAMPLE_RATE as usize * CHANNELS],
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
        let mixer_thread = if mixer.is_some() || !silent.is_empty() {
            let stop = stop.clone();
            Some(
                std::thread::Builder::new()
                    .name("mixer audio".into())
                    .spawn(move || mixer_loop(mixer, silent, sink, stop))
                    .map_err(|e| SourceError::Failed(format!("thread del mixer: {e}")))?,
            )
        } else {
            None
        };
        Ok(Self { sources, errors, stop, mixer_thread })
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

fn mixer_loop(mut mixer: Option<Mixer>, silent: Vec<usize>, sink: Arc<dyn AudioTrackSink>, stop: Arc<AtomicBool>) {
    let mmcss = crate::wasapi_source::Mmcss::register();
    let zeros = vec![0.0f32; MIX_BLOCK_FRAMES * CHANNELS];
    // Griglia ancorata al secondo intero di QPC: i blocchi cadono su multipli esatti di 480 campioni.
    let now = QpcTime::now();
    let origin = QpcTime(now.0 - now.0.rem_euclid(HNS_PER_SEC));
    let block = MIX_BLOCK_FRAMES as i64;
    let mut k = hns_to_frames(now.add_hns(-MIX_LATENCY_HNS).hns_since(origin)).div_euclid(block);
    while !stop.load(Ordering::Acquire) {
        let ready_until = QpcTime::now().add_hns(-MIX_LATENCY_HNS);
        loop {
            let start = origin.add_hns(frames_to_hns(k * block));
            if origin.add_hns(frames_to_hns((k + 1) * block)) > ready_until {
                break;
            }
            if let Some(m) = &mut mixer {
                sink.push(0, start, m.render(start));
            }
            for &t in &silent {
                sink.push(t, start, &zeros);
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
        let mut engine = AudioEngine::with_sources(beeps(), &[1.0, 0.5], TrackMode::Separate, true, sink.clone()).unwrap();
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
        let mut engine = AudioEngine::with_sources(beeps(), &[], TrackMode::Merged, false, sink.clone()).unwrap();
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
                AudioSourceConfig { name: "Sistema".into(), kind: AudioSourceKind::Output { device: None }, volume: 1.0 },
                AudioSourceConfig { name: "Explorer".into(), kind: AudioSourceKind::App { exe: "explorer.exe".into() }, volume: 0.8 },
            ],
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
        let src = |name: &str, kind| AudioSourceConfig { name: name.into(), kind, volume: 1.0 };
        let cfg = AudioConfig {
            mode: TrackMode::Merged,
            add_mix_track: false,
            sources: vec![
                src("Sistema", AudioSourceKind::Output { device: None }),
                src("Explorer", AudioSourceKind::App { exe: "explorer.exe".into() }),
                src("Assente", AudioSourceKind::App { exe: "nessuna-app-cliprec.exe".into() }),
            ],
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
        let mut engine = AudioEngine::with_sources(sources, &[], TrackMode::Separate, false, sink.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(700));
        assert!(matches!(engine.states()[1].1, SourceState::Error(_)));
        engine.stop();
        sink.check_contiguous(0.4);
        assert!(sink.data.lock().unwrap()[1].iter().all(|s| *s == 0.0));
    }
}
