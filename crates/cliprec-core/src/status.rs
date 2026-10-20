//! Comandi della shell verso il motore, eventi del motore e stato dal vivo.

use std::path::PathBuf;

use crate::clip::{ClipKind, SavedClip};

/// Comandi dalla shell (hotkey, menu tray, IPC) al motore.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AppCommand {
    SaveVideoClip,
    SaveAudioClip,
    /// Pulsante «Prova» delle impostazioni: clip video di 5 s (audio in modalità solo audio).
    SaveTestClip,
    StartBuffer,
    StopBuffer,
    /// Rilegge config.toml e riavvia ciò che è cambiato.
    ReloadConfig,
    Quit,
}

/// Eventi dal motore alla shell, per notifiche e icona.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum EngineEvent {
    ClipSaved(SavedClip),
    ClipFailed { kind: ClipKind, reason: String },
    /// Avviso non bloccante (es. pagine non bloccate in RAM, frame neri).
    Warning(String),
    /// Errore che ferma una parte del motore.
    Error(String),
}

/// Stato dal vivo mostrato in tray, nel pannello di stato e inviato via IPC.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EngineStatus {
    pub buffer_running: bool,
    pub video_enabled: bool,
    /// Secondi che il buffer copre davvero adesso.
    pub video_seconds: f64,
    pub audio_seconds: f64,
    pub buffer_bytes: u64,
    /// `true` se le pagine del buffer sono bloccate in RAM (`VirtualLock`).
    pub ram_locked: bool,
    pub encoder: String,
    pub source: String,
    pub fps: f64,
    pub frames_dropped: u64,
    pub saving: u32,
    pub recent_clips: Vec<PathBuf>,
    pub last_error: Option<String>,
}

/// Stato di una sorgente audio o video.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SourceState {
    #[default]
    Stopped,
    Running,
    /// In attesa che l'app o il dispositivo ricompaia; la traccia prosegue in silenzio.
    Waiting(String),
    Error(String),
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("sorgente non trovata: {0}")]
    NotFound(String),
    #[error("accesso negato: {0}")]
    AccessDenied(String),
    #[error("non supportato: {0}")]
    Unsupported(String),
    #[error("errore della sorgente: {0}")]
    Failed(String),
}

impl From<windows::core::Error> for SourceError {
    fn from(e: windows::core::Error) -> Self {
        Self::Failed(e.to_string())
    }
}
