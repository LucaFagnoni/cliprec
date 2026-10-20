//! Snapshot del buffer passati al muxer e risultato di un salvataggio.

use std::path::PathBuf;

use crate::time::QpcTime;
use crate::video::{EncodedPacket, VideoStreamParams};

/// Pacchetti video di una clip, in ordine di decodifica. Il primo è un keyframe.
#[derive(Clone, Debug)]
pub struct VideoSnapshot {
    pub params: VideoStreamParams,
    pub packets: Vec<EncodedPacket>,
}

impl VideoSnapshot {
    /// pts del primo frame: l'istante a cui va allineato l'audio.
    pub fn start(&self) -> Option<QpcTime> {
        self.packets.iter().map(|p| p.pts).min()
    }
}

/// Una traccia audio decompressa.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioTrackPcm {
    pub name: String,
    /// f32 interleaved stereo a 48 kHz.
    pub samples: Vec<f32>,
}

/// Tracce audio di una clip, tutte allineate allo stesso campione iniziale e lunghe uguali.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioSnapshot {
    /// Istante QPC del primo campione di ogni traccia.
    pub start: QpcTime,
    pub tracks: Vec<AudioTrackPcm>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ClipKind {
    Video,
    Audio,
}

/// Clip scritta su disco.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SavedClip {
    pub kind: ClipKind,
    /// File della clip, o cartella per le clip audio a un file per traccia.
    pub path: PathBuf,
    pub duration_secs: f64,
    pub bytes: u64,
}
