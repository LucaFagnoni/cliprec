//! Formato audio interno e trait delle sorgenti audio.

use crate::status::{SourceError, SourceState};
use crate::time::QpcTime;

/// Frequenza di campionamento interna, uguale per tutte le sorgenti e tracce.
pub const SAMPLE_RATE: u32 = 48_000;
/// Canali interni: stereo interleaved (L, R). I microfoni mono vanno copiati su entrambi.
pub const CHANNELS: usize = 2;

/// Blocco di campioni consegnato da una sorgente.
#[derive(Clone, Copy, Debug)]
pub struct AudioChunk<'a> {
    /// Istante QPC del primo campione (da `GetBuffer`, o calcolato per i vuoti riempiti).
    pub qpc: QpcTime,
    /// f32 interleaved stereo a 48 kHz; `samples.len()` è pari.
    pub samples: &'a [f32],
}

pub type ChunkCallback = Box<dyn FnMut(AudioChunk<'_>) + Send>;

/// Una sorgente audio: applicazione (process loopback), dispositivo d'ingresso, loopback di
/// un dispositivo d'uscita, o tono sintetico per i test.
pub trait AudioSource: Send {
    /// Avvia la cattura su un thread proprio (registrato in MMCSS). La sorgente consegna
    /// campioni continui: i vuoti segnalati dai timestamp QPC sono già riempiti di silenzio.
    fn start(&mut self, on_chunk: ChunkCallback) -> Result<(), SourceError>;
    fn stop(&mut self);
    fn state(&self) -> SourceState;
    /// Nome scelto dall'utente, scritto come nome della traccia.
    fn name(&self) -> &str;
}

/// Destinazione delle tracce prodotte dal motore audio (implementata dal ring buffer).
///
/// Per ogni traccia i blocchi sono contigui: il primo campione di un blocco segue l'ultimo del
/// precedente. `start` è l'istante QPC del primo campione del blocco, e serve al buffer per
/// tagliare tutte le tracce sullo stesso istante. Può scostarsi di qualche ms dalla fine calcolata
/// del blocco precedente (QPC reale del pacchetto): il buffer tratta i campioni come contigui.
/// Uno scarto oltre 100 ms (es. dopo una sospensione) è una discontinuità: il buffer apre un
/// nuovo segmento e allo snapshot riempie di silenzio il tratto mancante.
pub trait AudioTrackSink: Send + Sync {
    /// Dichiara le tracce (nell'ordine del file) prima del primo `push`.
    fn set_tracks(&self, names: &[String]);
    fn push(&self, track: usize, start: QpcTime, samples: &[f32]);
}
