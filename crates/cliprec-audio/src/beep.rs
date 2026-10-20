//! Sorgente sintetica: bip di 50 ms che inizia esattamente a ogni secondo intero di [`QpcTime`]
//! (come il lampo bianco della sorgente video sintetica), silenzio altrove.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cliprec_core::time::frames_to_hns;
use cliprec_core::{AudioChunk, AudioSource, CHANNELS, ChunkCallback, HNS_PER_SEC, QpcTime, SAMPLE_RATE, SourceError, SourceState};

/// Campioni per blocco consegnato (10 ms).
pub const BEEP_BLOCK_FRAMES: usize = 480;
/// Durata del bip in campioni (50 ms).
pub const BEEP_FRAMES: u64 = 2_400;
const AMPLITUDE: f32 = 0.5;

/// Tono sintetico a ritmo reale, con timestamp QPC esatti.
pub struct BeepSource {
    name: String,
    frequency: f32,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<SourceState>>,
    thread: Option<JoinHandle<()>>,
}

impl BeepSource {
    /// `frequency` in Hz (1000 di default) per distinguere più sorgenti.
    pub fn new(name: impl Into<String>, frequency: f32) -> Self {
        Self {
            name: name.into(),
            frequency,
            stop: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(SourceState::Stopped)),
            thread: None,
        }
    }
}

/// Riempie `out` (stereo) con il segnale a partire dal campione globale `frame`, contato dal
/// secondo intero di QPC `second_origin`: bip se `frame % 48000 < 2400`.
pub fn render_beep(frequency: f32, frame: u64, out: &mut [f32]) {
    let rate = u64::from(SAMPLE_RATE);
    for (i, f) in out.chunks_exact_mut(CHANNELS).enumerate() {
        let in_second = (frame + i as u64) % rate;
        let s = if in_second < BEEP_FRAMES {
            AMPLITUDE * (std::f32::consts::TAU * frequency * in_second as f32 / rate as f32).sin()
        } else {
            0.0
        };
        f.fill(s);
    }
}

fn set_state(state: &Mutex<SourceState>, s: SourceState) {
    *state.lock().unwrap_or_else(|e| e.into_inner()) = s;
}

/// Attesa prima di riavviare il thread dopo un panic.
const RESTART_WAIT: Duration = Duration::from_secs(1);

impl AudioSource for BeepSource {
    fn start(&mut self, mut on_chunk: ChunkCallback) -> Result<(), SourceError> {
        if self.thread.is_some() {
            return Ok(());
        }
        self.stop.store(false, Ordering::Release);
        let stop = self.stop.clone();
        let state = self.state.clone();
        let frequency = self.frequency;
        let thread = std::thread::Builder::new()
            .name(format!("beep {}", self.name))
            .spawn(move || {
                let _mmcss = crate::wasapi_source::Mmcss::register();
                let now = QpcTime::now();
                // Origine sul secondo intero: i secondi cadono su multipli esatti di 48000 campioni.
                let origin = QpcTime(now.0 - now.0.rem_euclid(HNS_PER_SEC));
                // Primo campione: il primo blocco intero dopo adesso.
                let elapsed = cliprec_core::time::hns_to_frames(now.hns_since(origin)).max(0) as u64;
                let mut frame = elapsed.div_ceil(BEEP_BLOCK_FRAMES as u64) * BEEP_BLOCK_FRAMES as u64;
                let mut buf = vec![0.0f32; BEEP_BLOCK_FRAMES * CHANNELS];
                while !stop.load(Ordering::Acquire) {
                    set_state(&state, SourceState::Running);
                    let run = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        while !stop.load(Ordering::Acquire) {
                            // Consegna ogni blocco quando il suo ultimo campione è nel passato, come un dispositivo.
                            let block_end = origin.add_hns(frames_to_hns((frame + BEEP_BLOCK_FRAMES as u64) as i64));
                            let now = QpcTime::now();
                            if now < block_end {
                                let wait = block_end.hns_since(now).clamp(0, 50_000) as u64;
                                std::thread::sleep(Duration::from_nanos(wait * 100));
                                continue;
                            }
                            render_beep(frequency, frame, &mut buf);
                            on_chunk(AudioChunk { qpc: origin.add_hns(frames_to_hns(frame as i64)), samples: &buf });
                            frame += BEEP_BLOCK_FRAMES as u64;
                        }
                    }));
                    if let Err(p) = run {
                        // Supervisione: stato d'errore, attesa, ripresa dal blocco successivo (i blocchi
                        // persi nell'attesa si recuperano subito, i tempi restano esatti).
                        let msg = crate::panic_message(p.as_ref());
                        tracing::error!("panic nel thread del bip: {msg}");
                        set_state(&state, SourceState::Error(format!("errore interno (panic): {msg}")));
                        frame += BEEP_BLOCK_FRAMES as u64;
                        let until = std::time::Instant::now() + RESTART_WAIT;
                        while std::time::Instant::now() < until && !stop.load(Ordering::Acquire) {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
                set_state(&state, SourceState::Stopped);
            })
            .map_err(|e| SourceError::Failed(format!("thread del bip: {e}")))?;
        self.thread = Some(thread);
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
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

impl Drop for BeepSource {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beeps_start_on_whole_seconds() {
        type Chunks = Arc<Mutex<Vec<(QpcTime, Vec<f32>)>>>;
        let chunks: Chunks = Arc::default();
        let sink = chunks.clone();
        let mut src = BeepSource::new("bip", 1000.0);
        src.start(Box::new(move |c| sink.lock().unwrap().push((c.qpc, c.samples.to_vec()))))
            .unwrap();
        std::thread::sleep(Duration::from_millis(2300));
        assert_eq!(src.state(), SourceState::Running);
        src.stop();
        assert_eq!(src.state(), SourceState::Stopped);
        let chunks = chunks.lock().unwrap();

        // Contiguità e ritmo reale.
        let total: usize = chunks.iter().map(|c| c.1.len() / 2).sum();
        assert!((total as i64 - 2300 * 48).abs() < 48 * 120, "campioni {total}");
        for w in chunks.windows(2) {
            let end = w[0].0.add_hns(frames_to_hns(BEEP_BLOCK_FRAMES as i64));
            assert!((w[1].0.0 - end.0).abs() <= 1);
        }

        // Bip esattamente nei primi 2400 campioni di ogni secondo intero di QPC, zero altrove.
        let mut loud_seconds = std::collections::BTreeSet::new();
        for (qpc, s) in chunks.iter() {
            for (i, f) in s.chunks_exact(2).enumerate() {
                let t = qpc.add_hns(frames_to_hns(i as i64));
                let off = cliprec_core::time::hns_to_frames(t.0.rem_euclid(HNS_PER_SEC));
                if off >= BEEP_FRAMES as i64 && off < 48_000 {
                    assert_eq!(f[0], 0.0, "suono fuori dal bip a {t:?}");
                } else if off > 0 && off % 24 != 0 && off < BEEP_FRAMES as i64 {
                    assert!(f[0].abs() > 1e-3, "silenzio dentro il bip a {t:?}");
                    loud_seconds.insert(t.0.div_euclid(HNS_PER_SEC));
                }
            }
        }
        assert!(loud_seconds.len() >= 2, "bip trovati: {}", loud_seconds.len());
    }

    /// M9: un panic nel thread (qui nella callback) porta la sorgente in `Error`, poi riparte da
    /// sola e i blocchi restano contigui.
    #[test]
    fn panic_in_thread_restarts_source() {
        type Chunks = Arc<Mutex<Vec<QpcTime>>>;
        let chunks: Chunks = Arc::default();
        let sink = chunks.clone();
        let mut calls = 0;
        let mut src = BeepSource::new("bip", 1000.0);
        src.start(Box::new(move |c| {
            calls += 1;
            if calls == 5 {
                panic!("panic di prova nella callback");
            }
            sink.lock().unwrap().push(c.qpc);
        }))
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let after_panic = src.state();
        std::thread::sleep(Duration::from_millis(1500));
        let later = src.state();
        src.stop();
        println!("dopo il panic: {after_panic:?}, poi {later:?}");
        assert!(matches!(after_panic, SourceState::Error(ref m) if m.contains("panic")), "{after_panic:?}");
        assert_eq!(later, SourceState::Running);
        let chunks = chunks.lock().unwrap();
        let last = *chunks.last().unwrap();
        assert!(QpcTime::now().hns_since(last) < 5_000_000, "il bip deve aver ripreso");
        // Contiguità: manca solo il blocco consegnato alla callback che è andata in panic.
        let holes = chunks.windows(2).filter(|w| w[1].hns_since(w[0]) > frames_to_hns(BEEP_BLOCK_FRAMES as i64) + 1).count();
        assert!(holes <= 1, "{holes} buchi");
    }

    #[test]
    fn render_is_silent_outside_beep() {
        let mut buf = vec![1.0f32; 48_000 * 2];
        render_beep(1000.0, 0, &mut buf);
        assert!(buf[2 * BEEP_FRAMES as usize..].iter().all(|s| *s == 0.0));
        assert!(buf[..2 * BEEP_FRAMES as usize].iter().any(|s| s.abs() > 0.4));
    }
}
