//! Elaborazione senza allocazioni: mono → stereo, limiter, continuità (silenzio nei vuoti) e
//! stima della frequenza reale di una sorgente rispetto al QPC.

use cliprec_core::time::{frames_to_hns, hns_to_frames};
use cliprec_core::{AudioChunk, CHANNELS, QpcTime, SAMPLE_RATE};

/// Copia un segnale mono su L e R a guadagno unitario. `dst` deve essere lungo `2 * src.len()`.
pub fn mono_to_stereo(src: &[f32], dst: &mut [f32]) {
    for (s, d) in src.iter().zip(dst.chunks_exact_mut(2)) {
        d[0] = *s;
        d[1] = *s;
    }
}

/// Limiter di picco stereo collegato: attacco istantaneo (nessun campione supera mai la soglia),
/// rilascio esponenziale. Sotto soglia, a regime, il segnale passa invariato.
#[derive(Debug, Clone)]
pub struct Limiter {
    threshold: f32,
    release: f32,
    gain: f32,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new(0.98, 0.050)
    }
}

impl Limiter {
    /// `threshold` lineare (0.98 ≈ -0,18 dBFS), `release_secs` costante di tempo del rilascio.
    pub fn new(threshold: f32, release_secs: f32) -> Self {
        let release = 1.0 - (-1.0 / (release_secs * SAMPLE_RATE as f32)).exp();
        Self { threshold, release, gain: 1.0 }
    }

    /// Elabora sul posto campioni stereo interleaved.
    pub fn process(&mut self, samples: &mut [f32]) {
        for frame in samples.chunks_exact_mut(CHANNELS) {
            let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let target = if peak > self.threshold { self.threshold / peak } else { 1.0 };
            if target < self.gain {
                self.gain = target;
            } else {
                self.gain += (target - self.gain) * self.release;
                // Evita di restare per sempre a 0,99999…: sotto soglia il segnale deve essere intatto.
                if self.gain > 0.9999 {
                    self.gain = 1.0;
                }
            }
            if self.gain < 1.0 {
                for s in frame.iter_mut() {
                    *s *= self.gain;
                }
            }
        }
    }
}

/// Tolleranza sul timestamp di un pacchetto prima di considerarlo un vuoto o una sovrapposizione.
const GAP_TOLERANCE_FRAMES: i64 = 96; // 2 ms
/// Il riempimento a vuoto resta indietro di tanto rispetto ad adesso, per non sovrapporsi ai
/// pacchetti veri che arrivano con la latenza del dispositivo.
pub const IDLE_HOLDBACK_HNS: i64 = 500_000; // 50 ms
/// Oltre questo vuoto (es. dopo la sospensione del PC) non si riempie tutto: si salta avanti.
// ponytail: tetto fisso; se il buffer deve vedere ogni ora di sospensione, servirà un segnale di salto nel contratto.
const MAX_FILL_FRAMES: i64 = 5 * 60 * SAMPLE_RATE as i64;
const ZERO_BLOCK_FRAMES: usize = 480;

/// Rende continua una sorgente: riempie di zeri i vuoti calcolati dai timestamp QPC e i periodi
/// senza pacchetti, scarta le sovrapposizioni. Ogni pacchetto è riancorato al proprio timestamp,
/// quindi la deriva del dispositivo non si accumula qui.
#[derive(Debug)]
pub struct Continuity {
    /// Istante atteso del prossimo campione.
    next: Option<QpcTime>,
    zeros: Vec<f32>,
    /// Campioni di silenzio inseriti, per diagnostica.
    pub filled_frames: u64,
    /// Campioni scartati perché sovrapposti a dati già consegnati.
    pub dropped_frames: u64,
}

impl Default for Continuity {
    fn default() -> Self {
        Self::new()
    }
}

impl Continuity {
    pub fn new() -> Self {
        Self { next: None, zeros: vec![0.0; ZERO_BLOCK_FRAMES * CHANNELS], filled_frames: 0, dropped_frames: 0 }
    }

    /// Istante atteso del prossimo campione, se la sorgente ha già consegnato qualcosa.
    pub fn next_qpc(&self) -> Option<QpcTime> {
        self.next
    }

    /// Fissa l'inizio dello stream senza dati (es. una sorgente che parte in attesa).
    pub fn anchor(&mut self, at: QpcTime) {
        if self.next.is_none() {
            self.next = Some(at);
        }
    }

    /// Un pacchetto con il QPC del primo campione (`None` = timestamp non valido: segue il precedente).
    pub fn packet(&mut self, qpc: Option<QpcTime>, samples: &[f32], out: &mut dyn FnMut(AudioChunk<'_>)) {
        let frames = (samples.len() / CHANNELS) as i64;
        if frames == 0 {
            return;
        }
        let (qpc, mut samples) = match (qpc, self.next) {
            (Some(q), None) => (q, samples),
            (None, None) => (QpcTime::now().add_hns(-frames_to_hns(frames)), samples),
            (None, Some(n)) => (n, samples),
            (Some(q), Some(n)) => {
                let gap = hns_to_frames(q.hns_since(n));
                if gap > GAP_TOLERANCE_FRAMES {
                    self.fill(n, gap, out);
                    (q, samples)
                } else if gap < -GAP_TOLERANCE_FRAMES {
                    let overlap = (-gap).min(frames);
                    self.dropped_frames += overlap as u64;
                    if overlap == frames {
                        return;
                    }
                    (q.add_hns(frames_to_hns(overlap)), &samples[overlap as usize * CHANNELS..])
                } else {
                    (q, samples)
                }
            }
        };
        let frames = (samples.len() / CHANNELS) as i64;
        samples = &samples[..frames as usize * CHANNELS];
        out(AudioChunk { qpc, samples });
        self.next = Some(qpc.add_hns(frames_to_hns(frames)));
    }

    /// Nessun pacchetto: riempie di silenzio fino a `now - IDLE_HOLDBACK_HNS`.
    pub fn idle(&mut self, now: QpcTime, out: &mut dyn FnMut(AudioChunk<'_>)) {
        let Some(next) = self.next else { return };
        let until = now.add_hns(-IDLE_HOLDBACK_HNS);
        let frames = hns_to_frames(until.hns_since(next));
        // Blocchi interi da 10 ms, per non frammentare il flusso in pezzetti.
        let frames = frames - frames % ZERO_BLOCK_FRAMES as i64;
        if frames > 0 {
            self.fill(next, frames, out);
        }
    }

    fn fill(&mut self, from: QpcTime, frames: i64, out: &mut dyn FnMut(AudioChunk<'_>)) {
        let (from, frames) = if frames > MAX_FILL_FRAMES {
            (from.add_hns(frames_to_hns(frames - MAX_FILL_FRAMES)), MAX_FILL_FRAMES)
        } else {
            (from, frames)
        };
        let mut done = 0i64;
        while done < frames {
            let n = (frames - done).min(ZERO_BLOCK_FRAMES as i64);
            out(AudioChunk { qpc: from.add_hns(frames_to_hns(done)), samples: &self.zeros[..n as usize * CHANNELS] });
            done += n;
        }
        self.filled_frames += frames as u64;
        self.next = Some(from.add_hns(frames_to_hns(frames)));
    }
}

/// Regressione lineare con oblio esponenziale tra QPC e posizione (campioni) di una sorgente:
/// dà la frequenza reale rispetto al QPC e la posizione attesa a un istante qualsiasi.
#[derive(Debug, Clone, Default)]
pub struct RateEstimator {
    n: u64,
    origin: i64,
    mean_x: f64,
    mean_y: f64,
    var_x: f64,
    cov_xy: f64,
}

/// Punti oltre i quali la media diventa esponenziale (~30 s di pacchetti da 10 ms).
const RATE_WINDOW: f64 = 3000.0;
const MIN_SPAN_SECS: f64 = 0.5;

impl RateEstimator {
    pub fn add(&mut self, qpc: QpcTime, pos: u64) {
        if self.n == 0 {
            self.origin = qpc.0;
        }
        self.n += 1;
        let a = (1.0 / self.n as f64).max(1.0 / RATE_WINDOW);
        let x = (qpc.0 - self.origin) as f64 / 1e7;
        let dx = x - self.mean_x;
        let dy = pos as f64 - self.mean_y;
        self.mean_x += a * dx;
        self.mean_y += a * dy;
        self.var_x = (1.0 - a) * (self.var_x + a * dx * dx);
        self.cov_xy = (1.0 - a) * (self.cov_xy + a * dx * dy);
    }

    pub fn ready(&self) -> bool {
        self.n > 0
    }

    /// Campioni al secondo di QPC; nominale finché i dati non coprono abbastanza tempo.
    pub fn rate(&self) -> f64 {
        let nominal = f64::from(SAMPLE_RATE);
        if self.var_x < MIN_SPAN_SECS * MIN_SPAN_SECS / 12.0 {
            return nominal;
        }
        (self.cov_xy / self.var_x).clamp(nominal * 0.99, nominal * 1.01)
    }

    /// Posizione attesa (in campioni, frazionaria) all'istante `qpc`.
    pub fn predict(&self, qpc: QpcTime) -> f64 {
        let x = (qpc.0 - self.origin) as f64 / 1e7;
        self.mean_y + self.rate() * (x - self.mean_x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_to_stereo_unity_gain() {
        let src = [0.1, -0.5, 1.0];
        let mut dst = [0.0; 6];
        mono_to_stereo(&src, &mut dst);
        assert_eq!(dst, [0.1, 0.1, -0.5, -0.5, 1.0, 1.0]);
    }

    fn sine(amp: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let s = amp * (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0).sin();
                [s, -s]
            })
            .collect()
    }

    #[test]
    fn limiter_never_exceeds_threshold() {
        let mut lim = Limiter::default();
        for amp in [1.0f32, 2.0, 8.0, 100.0] {
            let mut s = sine(amp, 48_000);
            lim.process(&mut s);
            let peak = s.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(peak <= 0.98 + 1e-6, "ampiezza {amp}: picco {peak}");
        }
    }

    #[test]
    fn limiter_transparent_below_threshold() {
        let mut lim = Limiter::default();
        let clean = sine(0.9, 48_000);
        let mut s = clean.clone();
        lim.process(&mut s);
        assert_eq!(s, clean, "sotto soglia il limiter non deve toccare il segnale");

        // Dopo un picco il guadagno torna a 1 e il segnale torna intatto.
        let mut loud = sine(4.0, 4_800);
        lim.process(&mut loud);
        let mut quiet = sine(0.5, 48_000);
        lim.process(&mut quiet);
        let tail = &quiet[48_000..];
        assert_eq!(tail, &sine(0.5, 48_000)[48_000..]);
    }

    fn collect(c: &mut Continuity, q: Option<i64>, frames: usize, out: &mut Vec<(i64, usize, bool)>) {
        let data = vec![0.5f32; frames * 2];
        c.packet(q.map(QpcTime), &data, &mut |ch| out.push((ch.qpc.0, ch.samples.len() / 2, ch.samples[0] == 0.0)));
    }

    #[test]
    fn continuity_fills_qpc_gaps_with_silence() {
        let mut c = Continuity::new();
        let mut out = Vec::new();
        collect(&mut c, Some(10_000_000), 480, &mut out);
        // Il pacchetto seguente arriva 100 ms dopo la fine del primo: 4800 campioni di silenzio.
        collect(&mut c, Some(10_000_000 + 100_000 + 1_000_000), 480, &mut out);
        let silent: usize = out.iter().filter(|o| o.2).map(|o| o.1).sum();
        assert_eq!(silent, 4800);
        // Contiguità: ogni blocco inizia dove finisce il precedente (a meno dell'arrotondamento).
        for w in out.windows(2) {
            let end = w[0].0 + frames_to_hns(w[0].1 as i64);
            assert!((w[1].0 - end).abs() <= 1, "{w:?}");
        }
        let total: usize = out.iter().map(|o| o.1).sum();
        assert_eq!(total, 480 + 4800 + 480);

        // Piccolo jitter: nessun riempimento. Sovrapposizione: scartata.
        out.clear();
        let next = c.next_qpc().unwrap().0;
        collect(&mut c, Some(next + 300), 480, &mut out);
        assert_eq!(out.len(), 1);
        let next = c.next_qpc().unwrap().0;
        collect(&mut c, Some(next - 50_000), 480, &mut out); // 5 ms = 240 campioni già consegnati
        assert_eq!(out[1].1, 240);
        assert_eq!(c.dropped_frames, 240);
    }

    #[test]
    fn continuity_idle_fills_up_to_holdback() {
        let mut c = Continuity::new();
        let mut out = Vec::new();
        collect(&mut c, Some(0), 480, &mut out);
        out.clear();
        c.idle(QpcTime(10_000_000), &mut |ch| out.push((ch.qpc.0, ch.samples.len() / 2, true)));
        let total: i64 = out.iter().map(|o| o.1 as i64).sum();
        let expected = hns_to_frames(10_000_000 - IDLE_HOLDBACK_HNS - frames_to_hns(480));
        assert!(total <= expected && total > expected - 480, "{total} vs {expected}");
    }

    #[test]
    fn rate_estimator_finds_drift() {
        let mut e = RateEstimator::default();
        let true_rate = 48_010.0;
        for k in 0..6000u64 {
            let pos = k * 480;
            let jitter = ((k * 7919) % 11) as i64 * 500 - 2500; // ±0,25 ms
            let qpc = (pos as f64 / true_rate * 1e7) as i64 + 123_456_789 + jitter;
            e.add(QpcTime(qpc), pos);
        }
        assert!((e.rate() - true_rate).abs() < 0.5, "{}", e.rate());
    }
}
