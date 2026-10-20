//! Mixer su griglia fissa ancorata al QPC: ogni sorgente arriva su code lock-free (`rtrb`), la sua
//! frequenza reale è stimata con una regressione posizione/QPC e corretta con il resampler
//! adattivo `rubato::Slip` prima della somma; dopo la somma c'è il limiter.

use cliprec_core::{AudioChunk, CHANNELS, QpcTime, SAMPLE_RATE, SourceError};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Adjustable, FixedAsync, Resampler, Slip};

use crate::dsp::{Limiter, RateEstimator};

/// Campioni per blocco della griglia del mixer (10 ms).
pub const MIX_BLOCK_FRAMES: usize = 480;
/// Oltre questo scarto (20 ms) si salta o si attende invece di correggere con il resampler.
const HARD_FRAMES: f64 = 960.0;
/// Frazione dello scarto corretta a ogni blocco (costante di tempo ~1 s).
const GAIN: f64 = 0.01;
const QUEUE_FRAMES: usize = 2 * SAMPLE_RATE as usize;
const TIMING_SLOTS: usize = 4096;

#[derive(Clone, Copy, Debug)]
struct Timing {
    qpc: QpcTime,
    pos: u64,
}

/// Lato produttore di un ingresso del mixer, usato dal thread di cattura. Non alloca.
pub struct MixInput {
    samples: Producer<f32>,
    timing: Producer<Timing>,
    pos: u64,
    /// Campioni persi perché la coda era piena (il mixer non consumava).
    pub dropped_frames: u64,
}

impl MixInput {
    pub fn push(&mut self, chunk: AudioChunk<'_>) {
        let frames = chunk.samples.len() / CHANNELS;
        let _ = self.timing.push(Timing { qpc: chunk.qpc, pos: self.pos });
        let fit = (self.samples.slots() / CHANNELS).min(frames);
        let _ = self.samples.push_partial_slice(&chunk.samples[..fit * CHANNELS]);
        self.dropped_frames += (frames - fit) as u64;
        self.pos += frames as u64;
    }
}

struct Lane {
    samples: Consumer<f32>,
    timing: Consumer<Timing>,
    est: RateEstimator,
    slip: Slip<f32>,
    /// Posizione nella sorgente del prossimo campione da leggere.
    read: u64,
    volume: f32,
    input: Vec<f32>,
    output: Vec<f32>,
    /// Ultimo scarto misurato in campioni (positivo = sorgente in anticipo).
    last_error: f64,
}

/// Somma sincronizzata di più sorgenti. `render` non alloca.
pub struct Mixer {
    lanes: Vec<Lane>,
    mix: Vec<f32>,
    limiter: Limiter,
}

impl Mixer {
    /// Un ingresso per volume; restituisce il mixer e i produttori da dare ai thread di cattura.
    pub fn new(volumes: &[f32]) -> Result<(Self, Vec<MixInput>), SourceError> {
        let mut lanes = Vec::with_capacity(volumes.len());
        let mut inputs = Vec::with_capacity(volumes.len());
        for &volume in volumes {
            let (sp, sc) = RingBuffer::new(QUEUE_FRAMES * CHANNELS);
            let (tp, tc) = RingBuffer::new(TIMING_SLOTS);
            let slip = Slip::new(MIX_BLOCK_FRAMES, CHANNELS, FixedAsync::Output)
                .map_err(|e| SourceError::Failed(format!("resampler del mixer: {e}")))?;
            let max_in = slip.input_frames_max();
            lanes.push(Lane {
                samples: sc,
                timing: tc,
                est: RateEstimator::default(),
                slip,
                read: 0,
                volume,
                input: vec![0.0; max_in * CHANNELS],
                output: vec![0.0; MIX_BLOCK_FRAMES * CHANNELS],
                last_error: 0.0,
            });
            inputs.push(MixInput { samples: sp, timing: tp, pos: 0, dropped_frames: 0 });
        }
        Ok((Self { lanes, mix: vec![0.0; MIX_BLOCK_FRAMES * CHANNELS], limiter: Limiter::default() }, inputs))
    }

    /// Scarto attuale della corsia `i` in millisecondi (positivo = la sorgente è in anticipo).
    pub fn lane_error_ms(&self, i: usize) -> f64 {
        self.lanes.get(i).map_or(0.0, |l| l.last_error * 1000.0 / f64::from(SAMPLE_RATE))
    }

    /// Frequenza stimata della corsia `i` in campioni per secondo di QPC.
    pub fn lane_rate(&self, i: usize) -> f64 {
        self.lanes.get(i).map_or(0.0, |l| l.est.rate())
    }

    /// Produce il blocco di [`MIX_BLOCK_FRAMES`] campioni che inizia all'istante `start`.
    pub fn render(&mut self, start: QpcTime) -> &[f32] {
        self.mix.fill(0.0);
        for lane in &mut self.lanes {
            if lane.render(start) {
                for (m, s) in self.mix.iter_mut().zip(&lane.output) {
                    *m += s * lane.volume;
                }
            }
        }
        self.limiter.process(&mut self.mix);
        &self.mix
    }
}

impl Lane {
    /// Riempie `output` con il blocco allineato a `start`; `false` se la corsia tace.
    fn render(&mut self, start: QpcTime) -> bool {
        while let Ok(t) = self.timing.pop() {
            self.est.add(t.qpc, t.pos);
        }
        if !self.est.ready() {
            return false;
        }
        let mut error = self.read as f64 - self.est.predict(start);
        self.last_error = error;
        if error > HARD_FRAMES {
            // I dati disponibili appartengono al futuro: la corsia tace finché la griglia li raggiunge.
            return false;
        }
        if error < -HARD_FRAMES {
            // Indietro (avvio, sorgente bloccata): si scartano i campioni ormai passati.
            let skip = ((-error) as usize).min(self.samples.slots() / CHANNELS);
            if let Ok(chunk) = self.samples.read_chunk(skip * CHANNELS) {
                chunk.commit_all();
            }
            self.read += skip as u64;
            error += skip as f64;
            if error < -HARD_FRAMES {
                return false;
            }
        }
        let wanted = MIX_BLOCK_FRAMES as f64 * self.est.rate() / f64::from(SAMPLE_RATE) - GAIN * error;
        // Fuori dal campo del resampler il rapporto viene limitato al massimo sostenibile: va bene così.
        let _ = self.slip.set_resample_ratio(MIX_BLOCK_FRAMES as f64 / wanted.max(1.0), false);
        let needed = self.slip.input_frames_next();
        let take = needed.min(self.samples.slots() / CHANNELS);
        let input = &mut self.input[..needed * CHANNELS];
        let _ = self.samples.pop_partial_slice(&mut input[..take * CHANNELS]);
        input[take * CHANNELS..].fill(0.0);
        self.read += take as u64;
        let (Ok(inp), Ok(mut out)) = (
            InterleavedSlice::new(&self.input[..needed * CHANNELS], CHANNELS, needed),
            InterleavedSlice::new_mut(&mut self.output[..], CHANNELS, MIX_BLOCK_FRAMES),
        ) else {
            return false;
        };
        self.slip.process_into_buffer(&inp, &mut out, None).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cliprec_core::HNS_PER_SEC;
    use cliprec_core::time::frames_to_hns;

    struct SimSource {
        rate: f64,
        t0: f64,
        next_frame: u64,
        channel: usize,
        jitter_seed: u64,
        /// Istanti veri (s) degli impulsi già emessi.
        impulses: Vec<f64>,
        buf: Vec<f32>,
    }

    impl SimSource {
        fn time_of(&self, frame: u64) -> f64 {
            self.t0 + frame as f64 / self.rate
        }

        fn pump(&mut self, now: f64, input: &mut MixInput) {
            while self.time_of(self.next_frame + 480) <= now {
                self.buf.fill(0.0);
                let first = self.next_frame;
                // Impulso sul campione più vicino a ogni secondo intero di tempo vero.
                let k = self.time_of(first).ceil();
                let n = ((k - self.t0) * self.rate).round() as u64;
                if n >= first && n < first + 480 {
                    self.buf[(n - first) as usize * 2 + self.channel] = 1.0;
                    self.impulses.push(self.time_of(n));
                }
                self.jitter_seed = self.jitter_seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let jitter = ((self.jitter_seed >> 33) % 6001) as i64 - 3000; // ±0,3 ms
                let qpc = QpcTime((self.time_of(first) * 1e7) as i64 + jitter);
                input.push(AudioChunk { qpc, samples: &self.buf });
                self.next_frame += 480;
            }
        }
    }

    /// Due sorgenti a 48000 e 48010 Hz (≈200 ppm) mixate per un'ora simulata.
    #[test]
    fn drift_stays_within_20ms_after_one_hour() {
        let (mut mixer, mut inputs) = Mixer::new(&[1.0, 1.0]).unwrap();
        let mut sources = [
            SimSource { rate: 48_000.0, t0: 1.0003, next_frame: 0, channel: 0, jitter_seed: 1, impulses: vec![], buf: vec![0.0; 960] },
            SimSource { rate: 48_010.0, t0: 1.2, next_frame: 0, channel: 1, jitter_seed: 2, impulses: vec![], buf: vec![0.0; 960] },
        ];
        let grid0 = 2 * HNS_PER_SEC;
        let latency = 0.1;
        let mut k: i64 = 0;
        let mut max_err = [0.0f64; 2];
        let mut last_err = [0.0f64; 2];
        let mut found = [0usize; 2];
        let hour = 3600.0 + 3.0;
        let mut now = 1.0;
        while now < hour {
            now += 0.01;
            for (s, input) in sources.iter_mut().zip(inputs.iter_mut()) {
                s.pump(now, input);
            }
            loop {
                let start = grid0 + frames_to_hns(k * MIX_BLOCK_FRAMES as i64);
                let end = grid0 + frames_to_hns((k + 1) * MIX_BLOCK_FRAMES as i64);
                if end as f64 / 1e7 > now - latency {
                    break;
                }
                let block = mixer.render(QpcTime(start));
                for (i, f) in block.chunks_exact(2).enumerate() {
                    for ch in 0..2 {
                        // L'impulso può essere spalmato su due campioni da una dissolvenza di Slip.
                        if f[ch] > 0.45 {
                            let t = (start + frames_to_hns(i as i64)) as f64 / 1e7;
                            let truth = sources[ch].impulses.iter().rev().find(|&&x| (x - t).abs() < 0.5);
                            if let Some(&truth) = truth {
                                let err = (t - truth) * 1000.0;
                                if t > 4.0 {
                                    max_err[ch] = max_err[ch].max(err.abs());
                                }
                                last_err[ch] = err;
                                found[ch] += 1;
                            }
                        }
                    }
                }
                k += 1;
            }
        }
        println!(
            "deriva: max |errore| A {:.3} ms, B {:.3} ms; finale A {:.3} ms, B {:.3} ms; stima B {:.3} Hz; impulsi {:?}",
            max_err[0], max_err[1], last_err[0], last_err[1], mixer.lane_rate(1), found
        );
        assert!(found[0] >= 3590 && found[1] >= 3590, "impulsi trovati {found:?}");
        assert!(max_err[0] <= 20.0 && max_err[1] <= 20.0, "errore massimo {max_err:?} ms");
        assert!((mixer.lane_rate(1) - 48_010.0).abs() < 1.0);
    }

    #[test]
    fn silent_lane_without_data_and_limiter_after_sum() {
        let (mut mixer, mut inputs) = Mixer::new(&[1.0, 1.0, 1.0]).unwrap();
        // Due sorgenti a piena scala in fase: la somma supererebbe 1, il limiter la tiene sotto soglia.
        let loud = vec![0.9f32; 960];
        for j in 0..20 {
            let qpc = QpcTime(frames_to_hns(j * 480));
            inputs[0].push(AudioChunk { qpc, samples: &loud });
            inputs[1].push(AudioChunk { qpc, samples: &loud });
        }
        for k in 0..10 {
            let block = mixer.render(QpcTime(frames_to_hns(k * 480)));
            assert!(block.iter().all(|s| s.abs() <= 0.98 + 1e-6));
            assert!(block.iter().any(|s| s.abs() > 0.9));
        }
    }
}
