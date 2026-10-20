//! Orologio comune: QPC in unità da 100 ns.

use std::sync::OnceLock;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::audio::SAMPLE_RATE;

/// Unità da 100 ns in un secondo.
pub const HNS_PER_SEC: i64 = 10_000_000;

/// Istante sull'orologio QPC del sistema, in unità da 100 ns.
///
/// Stessa unità e origine di `pu64QPCPosition` di WASAPI `GetBuffer` e di
/// `SystemRelativeTime` dei frame di Windows.Graphics.Capture: video e audio si
/// allineano senza conversioni.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QpcTime(pub i64);

impl QpcTime {
    /// Istante attuale.
    pub fn now() -> Self {
        let mut ticks = 0i64;
        // SAFETY: il puntatore è una variabile locale valida; QPC non fallisce da Windows XP in poi.
        let _ = unsafe { QueryPerformanceCounter(&mut ticks) };
        Self::from_ticks(ticks, qpc_frequency())
    }

    /// Converte tick QPC grezzi (`QueryPerformanceCounter`) con la frequenza data.
    pub fn from_ticks(ticks: i64, frequency: i64) -> Self {
        Self((i128::from(ticks) * i128::from(HNS_PER_SEC) / i128::from(frequency.max(1))) as i64)
    }

    pub fn from_secs_f64(secs: f64) -> Self {
        Self((secs * HNS_PER_SEC as f64).round() as i64)
    }

    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / HNS_PER_SEC as f64
    }

    /// Questo istante spostato di `hns` unità da 100 ns (anche negative).
    pub fn add_hns(self, hns: i64) -> Self {
        Self(self.0.saturating_add(hns))
    }

    /// Distanza in unità da 100 ns da `earlier` (negativa se `earlier` è dopo).
    pub fn hns_since(self, earlier: QpcTime) -> i64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// Frequenza QPC in tick al secondo, letta una volta sola.
pub fn qpc_frequency() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: il puntatore è una variabile locale valida.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    })
}

/// Durata di `frames` campioni a [`SAMPLE_RATE`] in unità da 100 ns (arrotondata per difetto).
pub fn frames_to_hns(frames: i64) -> i64 {
    (i128::from(frames) * i128::from(HNS_PER_SEC) / i128::from(SAMPLE_RATE)) as i64
}

/// Numero di campioni a [`SAMPLE_RATE`] contenuti in `hns` (arrotondato al più vicino).
pub fn hns_to_frames(hns: i64) -> i64 {
    let num = i128::from(hns) * i128::from(SAMPLE_RATE);
    let den = i128::from(HNS_PER_SEC);
    ((num + num.signum() * den / 2) / den) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_round_trip() {
        assert_eq!(frames_to_hns(48_000), HNS_PER_SEC);
        assert_eq!(hns_to_frames(HNS_PER_SEC), 48_000);
        assert_eq!(hns_to_frames(-HNS_PER_SEC), -48_000);
        assert_eq!(QpcTime::from_ticks(3, 3).0, HNS_PER_SEC);
        assert!(QpcTime::now() > QpcTime(0));
        let t = QpcTime::from_secs_f64(1.5);
        assert_eq!(t.add_hns(-5_000_000).hns_since(QpcTime(0)), HNS_PER_SEC);
    }
}
