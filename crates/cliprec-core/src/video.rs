//! Frame GPU, pacchetti compressi e trait di sorgente ed encoder video.

use std::sync::Arc;

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

use crate::status::{SourceError, SourceState};
use crate::time::QpcTime;

/// Un frame video sulla GPU, già a frequenza costante.
///
/// La texture è `DXGI_FORMAT_B8G8R8A8_UNORM`, vive sul [`crate::GpuDevice`] della sessione ed è
/// della dimensione della sorgente (che può cambiare tra un frame e l'altro). Resta valida solo
/// durante la chiamata a [`FrameCallback`]: chi la riceve deve finire di leggerla (o accodarne
/// la copia sul contesto immediato) prima di tornare.
pub struct GpuFrame {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    /// Istante di presentazione sulla griglia CFR: `start + index * HNS_PER_SEC / fps`.
    pub pts: QpcTime,
    /// Indice progressivo del frame in uscita dal pacer, da 0.
    pub index: u64,
    /// `true` se il pacer ha ripetuto l'ultimo frame perché non ne era arrivato uno nuovo.
    pub repeated: bool,
}

/// Chiamata dal thread del pacer a ogni tick, a frequenza costante.
pub type FrameCallback = Box<dyn FnMut(&GpuFrame) + Send>;

/// Contatori di una sorgente video.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CaptureStats {
    /// Frame consegnati alla callback (nuovi + ripetuti).
    pub frames_delivered: u64,
    pub frames_repeated: u64,
    /// Tick del pacer saltati perché la callback era ancora occupata.
    pub frames_dropped: u64,
    pub source_width: u32,
    pub source_height: u32,
    pub state: SourceState,
}

/// Una sorgente video (monitor, finestra, gioco, sintetica) con pacer a frequenza costante.
pub trait VideoSource: Send {
    /// Avvia la cattura e il pacer a `fps` frame al secondo. `on_frame` viene chiamata dal thread
    /// del pacer; se è ancora occupata al tick successivo, il tick è contato come perso.
    fn start(&mut self, fps: u32, on_frame: FrameCallback) -> Result<(), SourceError>;
    /// Ferma cattura e pacer; dopo il ritorno `on_frame` non viene più chiamata.
    fn stop(&mut self);
    fn stats(&self) -> CaptureStats;
    /// Descrizione leggibile, es. «Monitor 1 (2560×1440)» o «osu!.exe».
    fn describe(&self) -> String;
    /// Dimensione nativa del bersaglio in pixel fisici: la risoluzione del monitor su cui sta (o
    /// starà) la sorgente. Definisce l'uscita «nativa» dell'encoder, che quindi non dipende dal primo
    /// frame (un segnaposto o una finestra minimizzata). `None` se ancora ignota.
    fn native_size(&self) -> Option<(u32, u32)> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

/// Parametri dello stream compresso, necessari al muxer.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoStreamParams {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Header globali come prodotti dall'encoder con `AV_CODEC_FLAG_GLOBAL_HEADER`. Per NVENC e QSV
    /// sono in formato Annex B (`00 00 00 01`), come i pacchetti: il muxer MP4 di FFmpeg li converte.
    pub extradata: Vec<u8>,
    /// Tetto di bitrate (maxrate) in bit/s.
    pub bit_rate: u64,
    /// Nome dell'encoder, es. `hevc_nvenc`.
    pub encoder_name: String,
}

/// Pacchetto compresso preso in prestito dall'encoder: chi lo riceve copia `data` se lo tiene.
/// Con i B-frame il `dts` dei primi pacchetti può precedere il `pts` del primo frame.
#[derive(Clone, Copy, Debug)]
pub struct PacketRef<'a> {
    pub data: &'a [u8],
    pub pts: QpcTime,
    pub dts: QpcTime,
    /// Durata in unità da 100 ns.
    pub duration: i64,
    /// Keyframe IDR (GOP chiuso): punto di partenza valido per una clip.
    pub keyframe: bool,
}

/// Byte di un pacchetto con conteggio dei riferimenti; il buffer può tenerli in un'arena.
#[derive(Clone)]
pub struct PacketData(pub Arc<dyn AsRef<[u8]> + Send + Sync>);

impl std::ops::Deref for PacketData {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        (*self.0).as_ref()
    }
}

impl From<Vec<u8>> for PacketData {
    fn from(v: Vec<u8>) -> Self {
        Self(Arc::new(v))
    }
}

impl std::fmt::Debug for PacketData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PacketData({} B)", self.len())
    }
}

/// Pacchetto compresso conservato nel buffer.
#[derive(Clone, Debug)]
pub struct EncodedPacket {
    pub data: PacketData,
    pub pts: QpcTime,
    pub dts: QpcTime,
    pub duration: i64,
    pub keyframe: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("encoder non disponibile: {0}")]
    Unsupported(String),
    #[error("sessione dell'encoder rifiutata: {0}")]
    SessionRejected(String),
    #[error("device GPU perso")]
    DeviceLost,
    #[error("errore dell'encoder: {0}")]
    Failed(String),
}

/// Encoder video hardware. La conversione BGRA→NV12 e la scala nella risoluzione d'uscita
/// (con bande nere) sono responsabilità dell'encoder.
pub trait VideoEncoder: Send {
    fn params(&self) -> &VideoStreamParams;
    /// Codifica un frame; i pacchetti pronti vanno a `sink` in ordine di decodifica.
    /// Prima di tornare deve aver finito di leggere `frame.texture` (o accodato la copia).
    fn encode(&mut self, frame: &GpuFrame, sink: &mut dyn FnMut(PacketRef<'_>)) -> Result<(), EncodeError>;
    /// Svuota l'encoder alla chiusura.
    fn flush(&mut self, sink: &mut dyn FnMut(PacketRef<'_>)) -> Result<(), EncodeError>;
}
