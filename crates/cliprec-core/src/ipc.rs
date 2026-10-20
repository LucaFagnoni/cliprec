//! Protocollo tra `cliprec-settings` e il core su named pipe locale.
//!
//! Il core è il server (`CreateNamedPipeW`, solo client locali dello stesso utente); le
//! impostazioni sono il client (`std::fs::OpenOptions` sul nome della pipe). Ogni messaggio è
//! una riga JSON terminata da `\n`; a ogni [`IpcRequest`] corrisponde una [`IpcResponse`].

use crate::status::EngineStatus;

/// Nome della pipe, per utente e sessione.
pub fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    format!(r"\\.\pipe\cliprec-{user}")
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum IpcRequest {
    Status,
    /// La configurazione è stata scritta: il core la rilegge.
    ReloadConfig,
    /// Pulsante «Prova»: salva una clip di 5 s.
    TestClip,
    /// Chiude ClipRec (usato dai test e dagli script).
    Quit,
    /// Salva una clip video come la hotkey (automazione: Stream Deck, macro, script).
    SaveVideoClip,
    /// Salva una clip audio come la hotkey.
    SaveAudioClip,
    /// Stato degli aggiornamenti ([`IpcResponse::Update`]).
    UpdateStatus,
    /// Cerca subito una versione nuova (risposta `Ok`; l'esito arriva in `UpdateStatus`).
    CheckUpdate,
    /// Scarica e installa la versione trovata, poi riavvia ClipRec.
    InstallUpdate,
}

/// Fase dell'aggiornamento automatico.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    /// Nessuna versione più recente.
    UpToDate,
    Downloading,
    Installing,
    /// File sostituiti: ClipRec si sta riavviando.
    Restarting,
    Failed(String),
}

/// Stato degli aggiornamenti tenuto dal core.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UpdateInfo {
    /// Versione in esecuzione.
    pub current: String,
    /// Versione più recente disponibile, se più nuova di `current`.
    pub available: Option<String>,
    /// Note della release (testo Markdown di GitHub).
    pub notes: String,
    pub phase: UpdatePhase,
    /// Avanzamento del download da 0 a 1 (solo in `Downloading`, se la dimensione è nota).
    pub progress: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum IpcResponse {
    Status(EngineStatus),
    Update(UpdateInfo),
    Ok,
    Error(String),
}

/// Serializza un messaggio come riga JSON.
pub fn encode_line<T: serde::Serialize>(msg: &T) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(msg)?;
    s.push('\n');
    Ok(s)
}

pub fn decode_line<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip() {
        let r = IpcResponse::Status(EngineStatus { video_seconds: 12.5, ..Default::default() });
        let line = encode_line(&r).unwrap();
        assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
        assert_eq!(decode_line::<IpcResponse>(&line).unwrap(), r);
        assert!(pipe_name().starts_with(r"\\.\pipe\cliprec-"));
    }
}
