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

/// Buchi o sovrapposizioni fino a 15 ms tra un pacchetto e il precedente sono jitter: contigui.
const GAP_TOLERANCE_FRAMES: i64 = 720;
/// Trattenimento minimo del riempimento rispetto ad adesso.
pub const MIN_HOLDBACK_HNS: i64 = 500_000; // 50 ms
/// Margine sopra la latenza osservata dei pacchetti.
const HOLDBACK_MARGIN_HNS: i64 = 300_000; // 30 ms
const MAX_HOLDBACK_HNS: i64 = 10_000_000; // 1 s
/// Prima del primo pacchetto si aspetta tanto prima di riempire (la latenza è ancora ignota).
const STARTUP_GRACE_HNS: i64 = 5_000_000; // 500 ms
/// Il picco di latenza osservato cala di 10 ms al secondo.
const LATENCY_DECAY: f64 = 0.01;
/// Oltre questo vuoto (es. dopo la sospensione del PC) non si riempie tutto: si salta avanti.
// ponytail: tetto fisso; il buffer tratta i salti oltre 100 ms come discontinuità.
const MAX_FILL_FRAMES: i64 = 5 * 60 * SAMPLE_RATE as i64;
const ZERO_BLOCK_FRAMES: usize = 480;

/// Rende continua una sorgente senza mai scartare audio vero.
///
/// - I vuoti tra i timestamp QPC dei pacchetti diventano silenzio; buchi e sovrapposizioni entro
///   15 ms sono jitter e restano contigui.
/// - Senza pacchetti (loopback quando nulla suona) riempie di silenzio fino a `adesso − trattenimento`,
///   dove il trattenimento segue la latenza osservata dei pacchetti (picco con lento oblio) più 30 ms.
/// - Se un pacchetto arriva comunque sopra silenzio già emesso, i suoi campioni si consegnano tutti,
///   accodati: la linea dei tempi resta avanti di quel tanto («debito»), che si recupera togliendolo
///   dal prossimo silenzio da inserire.
#[derive(Debug)]
pub struct Continuity {
    /// Istante (consegnato) del prossimo campione.
    next: Option<QpcTime>,
    /// Inizio dello stream prima del primo pacchetto.
    anchor: Option<QpcTime>,
    /// Campioni di cui i tempi consegnati sono avanti rispetto ai timestamp del dispositivo.
    debt: i64,
    latency_peak: i64,
    last_observed: Option<QpcTime>,
    zeros: Vec<f32>,
    /// Campioni di silenzio inseriti, per diagnostica.
    pub filled_frames: u64,
}

impl Default for Continuity {
    fn default() -> Self {
        Self::new()
    }
}

impl Continuity {
    pub fn new() -> Self {
        Self {
            next: None,
            anchor: None,
            debt: 0,
            latency_peak: 0,
            last_observed: None,
            zeros: vec![0.0; ZERO_BLOCK_FRAMES * CHANNELS],
            filled_frames: 0,
        }
    }

    /// Istante del prossimo campione consegnato, se la sorgente ha già consegnato qualcosa.
    pub fn next_qpc(&self) -> Option<QpcTime> {
        self.next
    }

    /// Trattenimento attuale del riempimento di silenzio, in unità da 100 ns.
    pub fn holdback_hns(&self) -> i64 {
        (self.latency_peak + HOLDBACK_MARGIN_HNS).clamp(MIN_HOLDBACK_HNS, MAX_HOLDBACK_HNS)
    }

    /// Fissa l'inizio dello stream senza dati (es. una sorgente che parte in attesa).
    pub fn anchor(&mut self, at: QpcTime) {
        if self.next.is_none() {
            self.anchor = Some(at);
        }
    }

    fn observe(&mut self, now: QpcTime, age_hns: i64) {
        if let Some(t) = self.last_observed {
            self.latency_peak -= (now.hns_since(t).max(0) as f64 * LATENCY_DECAY) as i64;
        }
        self.last_observed = Some(now);
        self.latency_peak = self.latency_peak.max(age_hns.clamp(0, MAX_HOLDBACK_HNS));
    }

    /// Un pacchetto con il QPC del primo campione (`None` = timestamp non valido: segue il
    /// precedente), ricevuto all'istante `now`.
    pub fn packet(&mut self, qpc: Option<QpcTime>, samples: &[f32], now: QpcTime, out: &mut dyn FnMut(AudioChunk<'_>)) {
        let frames = (samples.len() / CHANNELS) as i64;
        if frames == 0 {
            return;
        }
        let samples = &samples[..frames as usize * CHANNELS];
        if let Some(q) = qpc {
            self.observe(now, now.hns_since(q));
        }
        let qpc = match (qpc, self.next) {
            (Some(q), None) => q,
            (None, None) => now.add_hns(-frames_to_hns(frames)),
            (None, Some(n)) => n,
            (Some(q), Some(n)) => {
                let expected = n.add_hns(-frames_to_hns(self.debt));
                let gap = hns_to_frames(q.hns_since(expected));
                if gap > GAP_TOLERANCE_FRAMES {
                    // Vuoto vero: prima si recupera il debito, poi silenzio per il resto.
                    let pay = self.debt.min(gap);
                    self.debt -= pay;
                    if gap > pay {
                        self.fill(n, gap - pay, out);
                    }
                    q.add_hns(frames_to_hns(self.debt))
                } else if gap < -GAP_TOLERANCE_FRAMES {
                    // Sopra campioni già consegnati: si accoda tutto, niente si scarta.
                    // ponytail: debito illimitato; se un dispositivo facesse regredire sempre i timestamp, i tempi resterebbero avanti.
                    self.debt -= gap;
                    n
                } else {
                    q.add_hns(frames_to_hns(self.debt))
                }
            }
        };
        self.anchor = None;
        out(AudioChunk { qpc, samples });
        self.next = Some(qpc.add_hns(frames_to_hns(frames)));
    }

    /// Nessun pacchetto: riempie di silenzio fino a `now − holdback_hns()`.
    pub fn idle(&mut self, now: QpcTime, out: &mut dyn FnMut(AudioChunk<'_>)) {
        let next = match (self.next, self.anchor) {
            (Some(n), _) => n,
            (None, Some(a)) if now.hns_since(a) > STARTUP_GRACE_HNS => a,
            _ => return,
        };
        let until = now.add_hns(-self.holdback_hns());
        let frames = hns_to_frames(until.hns_since(next));
        // Blocchi interi da 10 ms, per non frammentare il flusso in pezzetti.
        let frames = frames - frames % ZERO_BLOCK_FRAMES as i64;
        if frames > 0 {
            self.anchor = None;
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
        let now = QpcTime(q.unwrap_or(0) + frames_to_hns(frames as i64));
        c.packet(q.map(QpcTime), &data, now, &mut |ch| out.push((ch.qpc.0, ch.samples.len() / 2, ch.samples[0] == 0.0)));
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

        // Jitter fino a 15 ms, in avanti o indietro: contiguo, nessun riempimento, nulla scartato.
        out.clear();
        let next = c.next_qpc().unwrap().0;
        collect(&mut c, Some(next + 100_000), 480, &mut out);
        let next = c.next_qpc().unwrap().0;
        collect(&mut c, Some(next - 100_000), 480, &mut out);
        assert_eq!(out.iter().map(|o| (o.1, o.2)).collect::<Vec<_>>(), [(480, false), (480, false)]);

        // Sovrapposizione di 50 ms: i campioni si accodano tutti; il debito si recupera al vuoto seguente.
        out.clear();
        let next = c.next_qpc().unwrap().0;
        collect(&mut c, Some(next - 500_000), 480, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].0, out[0].1), (next, 480));
        let real_next = next - 500_000 + frames_to_hns(480);
        collect(&mut c, Some(real_next + 1_000_000), 480, &mut out); // 100 ms di vuoto vero
        let silent: usize = out.iter().filter(|o| o.2).map(|o| o.1).sum();
        assert_eq!(silent, 4800 - 2400, "il silenzio inserito sconta i 50 ms di debito");
        assert_eq!(out.last().unwrap().0, real_next + 1_000_000, "dopo il recupero i tempi tornano veri");
    }

    #[test]
    fn continuity_idle_fills_up_to_holdback() {
        let mut c = Continuity::new();
        let mut out = Vec::new();
        collect(&mut c, Some(0), 480, &mut out);
        out.clear();
        c.idle(QpcTime(10_000_000), &mut |ch| out.push((ch.qpc.0, ch.samples.len() / 2, true)));
        let total: i64 = out.iter().map(|o| o.1 as i64).sum();
        let expected = hns_to_frames(10_000_000 - MIN_HOLDBACK_HNS - frames_to_hns(480));
        assert!(total <= expected && total > expected - 480, "{total} vs {expected}");
    }

    /// Sorgente simulata come in cattura: ancoraggio all'avvio, pacchetti da 10 ms con QPC vero `t`
    /// (± jitter) consegnati a `t + 10 ms + latenza`, `idle` ogni millisecondo simulato.
    /// Restituisce (campioni veri consegnati, campioni veri usciti).
    fn simulate_latency(latency_ms: i64, jitter_hns: i64, secs: i64) -> (u64, u64) {
        let mut c = Continuity::new();
        let start = 10_000_000i64;
        c.anchor(QpcTime(start));
        let data = vec![0.5f32; 960];
        let (mut real_in, mut real_out) = (0u64, 0u64);
        let mut count = |ch: AudioChunk<'_>| real_out += ch.samples.iter().step_by(2).filter(|s| **s != 0.0).count() as u64;
        let mut seed = 1u64;
        let (mut k, mut now) = (0i64, start);
        while now < start + secs * 10_000_000 {
            now += 10_000;
            loop {
                let t = start + frames_to_hns(k * 480);
                if t + 100_000 + latency_ms * 10_000 > now {
                    break;
                }
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let jitter = if jitter_hns > 0 { ((seed >> 33) % (2 * jitter_hns as u64 + 1)) as i64 - jitter_hns } else { 0 };
                c.packet(Some(QpcTime(t + jitter)), &data, QpcTime(now), &mut count);
                real_in += 480;
                k += 1;
            }
            c.idle(QpcTime(now), &mut count);
        }
        (real_in, real_out)
    }

    /// M3: dispositivi con latenza costante oltre il vecchio trattenimento fisso di 50 ms.
    #[test]
    fn continuity_keeps_all_real_audio_with_high_latency() {
        for latency in [60, 80, 150] {
            let (sent, kept) = simulate_latency(latency, 0, 20);
            assert_eq!(kept, sent, "latenza {latency} ms: tenuti {:.1}% dei campioni veri", kept as f64 * 100.0 / sent as f64);
        }
    }

    /// M3: jitter di ±2,5 ms sui timestamp per 120 s non deve far perdere nulla.
    #[test]
    fn continuity_keeps_all_real_audio_with_jitter() {
        let (sent, kept) = simulate_latency(20, 25_000, 120);
        assert_eq!(kept, sent, "persi {:.2} s di audio vero", (sent - kept) as f64 / 48_000.0);
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
