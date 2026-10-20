//! Contratti condivisi di ClipRec.
//!
//! Solo l'agente principale modifica questo crate. Gli altri crate implementano i trait
//! qui definiti ([`VideoSource`], [`AudioSource`], [`AudioTrackSink`]) e si scambiano solo i tipi
//! qui definiti; l'encoder è il tipo concreto `cliprec_encode::Encoder`.
//!
//! Flusso dei dati:
//!
//! ```text
//! VideoSource --GpuFrame (CFR)--> Encoder (cliprec-encode) --PacketRef--> ReplayBuffer (cliprec-buffer)
//! AudioSource --AudioChunk-->  audio engine  --AudioTrackSink--> ReplayBuffer
//! hotkey --> ReplayBuffer::snapshot --> VideoSnapshot/AudioSnapshot --> cliprec-mux --> file
//! ```
//!
//! Tutti i timestamp sono [`QpcTime`]: unità da 100 ns sull'orologio QPC del sistema.

pub mod audio;
pub mod clip;
pub mod config;
pub mod gpu;
pub mod ipc;
pub mod status;
pub mod time;
pub mod video;

pub use audio::{AudioChunk, AudioSource, AudioTrackSink, CHANNELS, ChunkCallback, SAMPLE_RATE};
pub use clip::{AudioSnapshot, AudioTrackPcm, ClipKind, SavedClip, VideoSnapshot};
pub use config::Config;
pub use gpu::{GpuDevice, GpuError};
pub use status::{AppCommand, EngineEvent, EngineStatus, SourceError, SourceState};
pub use time::{HNS_PER_SEC, QpcTime};
pub use video::{
    CaptureStats, EncodeError, EncodedPacket, FrameCallback, GpuFrame, PacketData, PacketRef,
    VideoCodec, VideoSource, VideoStreamParams,
};
