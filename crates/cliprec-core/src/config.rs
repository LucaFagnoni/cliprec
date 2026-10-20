//! Schema di `config.toml`. Ogni campo ha un default: un file vuoto o parziale è valido.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::video::VideoCodec;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub video: VideoConfig,
    pub audio: AudioConfig,
    pub clips: ClipsConfig,
    pub hotkeys: HotkeyConfig,
    pub memory: MemoryConfig,
    pub general: GeneralConfig,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoConfig {
    /// `false` = modalità solo audio: niente GPU né encoder video.
    pub enabled: bool,
    pub source: VideoSourceConfig,
    pub codec: CodecChoice,
    /// 30, 60, 120 o 144.
    pub fps: u32,
    pub resolution: OutputResolution,
    /// Qualità costante (CQ/QP), più bassa = migliore. Default 23.
    pub quality: u8,
    /// Tetto di bitrate in kbit/s; `None` = tabella di [`default_max_bitrate_kbps`].
    pub max_bitrate_kbps: Option<u32>,
    pub capture_cursor: bool,
    /// Se `true`, in modalità automatica cattura qualsiasi app a schermo intero, non solo i giochi in lista.
    pub any_fullscreen_app: bool,
    /// Eseguibili riconosciuti come giochi dalla modalità automatica.
    pub games: Vec<String>,
    /// Opzioni avanzate dell'encoder, come le «Avanzate» di OBS.
    pub advanced: EncoderAdvanced,
}

/// Campionamento del colore. 4:2:0 tiene il colore una volta ogni 2×2 pixel (compatibile con ogni
/// lettore); 4:4:4 per ogni pixel, senza aloni colorati sul testo (solo NVENC H.264/HEVC).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorFormat {
    #[default]
    Yuv420,
    Yuv444,
}

/// Controllo del bitrate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateControl {
    /// Qualità costante (`quality`) con il tetto `max_bitrate_kbps`.
    #[default]
    Quality,
    /// Bitrate variabile attorno a `advanced.bitrate_kbps`, fino al tetto.
    Vbr,
    /// Bitrate costante `advanced.bitrate_kbps`.
    Cbr,
}

/// Compromesso tra velocità dell'encoder e qualità a parità di bitrate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncoderPreset {
    Fastest,
    Fast,
    Balanced,
    #[default]
    Quality,
    Best,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EncoderAdvanced {
    pub color_format: ColorFormat,
    pub rate_control: RateControl,
    /// Obiettivo per VBR e CBR in kbit/s; `None` = 2/3 del tetto.
    pub bitrate_kbps: Option<u32>,
    pub preset: EncoderPreset,
    /// Secondi tra due fotogrammi chiave (1-10): più lungo dà file più piccoli, ma la clip può
    /// iniziare fino a questo intervallo prima di N secondi.
    pub keyframe_seconds: u32,
    /// B-frame; `None` = automatico (nessuno con NVENC per la memoria, 2 dove servono).
    pub b_frames: Option<u8>,
    /// Analisi in anticipo dei fotogrammi per distribuire meglio i bit (più memoria e latenza).
    pub lookahead: bool,
    /// Quantizzazione adattiva spaziale e temporale: più dettaglio dove l'occhio lo nota.
    pub adaptive_quantization: bool,
}

impl Default for EncoderAdvanced {
    fn default() -> Self {
        Self {
            color_format: ColorFormat::Yuv420,
            rate_control: RateControl::Quality,
            bitrate_kbps: None,
            preset: EncoderPreset::Quality,
            keyframe_seconds: 1,
            b_frames: None,
            lookahead: false,
            adaptive_quantization: false,
        }
    }
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            source: VideoSourceConfig::Auto,
            codec: CodecChoice::Auto,
            fps: 60,
            resolution: OutputResolution::Native,
            quality: 23,
            max_bitrate_kbps: None,
            capture_cursor: true,
            any_fullscreen_app: true,
            advanced: EncoderAdvanced::default(),
            games: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum VideoSourceConfig {
    /// Gioco o app a schermo intero in primo piano, altrimenti il monitor principale.
    #[default]
    Auto,
    /// Monitor per indice (0 = principale).
    Monitor { index: u32 },
    /// Finestra per titolo (corrispondenza parziale).
    Window { title: String },
    /// Gioco per nome dell'eseguibile, ritrovato se si riavvia.
    Process { exe: String },
}

/// `Auto` = HEVC se disponibile, altrimenti H.264.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodecChoice {
    #[default]
    Auto,
    H264,
    Hevc,
    Av1,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputResolution {
    #[default]
    Native,
    #[serde(rename = "1440p")]
    P1440,
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "720p")]
    P720,
}

impl OutputResolution {
    /// Altezza d'uscita, o `None` per la nativa.
    pub fn height(self) -> Option<u32> {
        match self {
            Self::Native => None,
            Self::P1440 => Some(1440),
            Self::P1080 => Some(1080),
            Self::P720 => Some(720),
        }
    }
}

/// Tetto di bitrate predefinito: H.264 16/25/50 Mbit/s a 1080p60/1440p60/4K60, HEVC e AV1 un
/// terzo in meno, scalato linearmente con gli fps rispetto a 60.
pub fn default_max_bitrate_kbps(codec: VideoCodec, height: u32, fps: u32) -> u32 {
    let h264_at_60: u32 = match height {
        0..=720 => 8_000,
        721..=1080 => 16_000,
        1081..=1440 => 25_000,
        _ => 50_000,
    };
    let base = match codec {
        VideoCodec::H264 => h264_at_60,
        VideoCodec::Hevc | VideoCodec::Av1 => h264_at_60 * 2 / 3,
    };
    (u64::from(base) * u64::from(fps.max(1)) / 60) as u32
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Tracce delle clip video.
    pub mode: TrackMode,
    /// Solo in modalità separata: aggiunge in testa una traccia «Mix» con la somma di tutto.
    pub add_mix_track: bool,
    /// Tracce delle clip solo audio; assente (configurazioni precedenti) = come le clip video.
    pub audio_clip: Option<TrackLayout>,
    /// Fino a 8 sorgenti, nell'ordine delle tracce.
    pub sources: Vec<AudioSourceConfig>,
    /// Eventi di Windows per agganciare subito le app aperte o chiuse (serve ClipRec da
    /// amministratore). Spento: ricerca ogni mezzo secondo.
    pub process_events: bool,
    /// Toglie il permesso di modalità esclusiva dai dispositivi registrati: un'app in esclusiva
    /// scavalca il mixer di Windows e nessun loopback la sente.
    pub block_exclusive: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            mode: TrackMode::Separate,
            add_mix_track: false,
            audio_clip: None,
            process_events: false,
            block_exclusive: true,
            sources: vec![
                AudioSourceConfig::new("Sistema", AudioSourceKind::Output { device: None }),
                AudioSourceConfig::new("Microfono", AudioSourceKind::Input { device: None }),
            ],
        }
    }
}

pub const MAX_AUDIO_SOURCES: usize = 8;

/// Tracce audio di un tipo di clip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackLayout {
    pub mode: TrackMode,
    /// Solo in modalità separata: traccia «Mix» in testa.
    #[serde(default)]
    pub add_mix_track: bool,
}

impl TrackLayout {
    /// Contiene la traccia «Mix» (traccia unica, o «Mix» in testa).
    pub fn mix(self) -> bool {
        self.mode == TrackMode::Merged || self.add_mix_track
    }

    /// Contiene una traccia per sorgente.
    pub fn sources(self) -> bool {
        self.mode == TrackMode::Separate
    }
}

impl AudioConfig {
    pub fn layout(&self, kind: crate::ClipKind) -> TrackLayout {
        let video = TrackLayout { mode: self.mode, add_mix_track: self.add_mix_track };
        match kind {
            crate::ClipKind::Video => video,
            crate::ClipKind::Audio => self.audio_clip.unwrap_or(video),
        }
    }

    /// Tracce registrate nel buffer: quelle che servono alle clip video e a quelle solo audio.
    /// Ordine: «Mix» (se serve) e poi una traccia per sorgente (se serve).
    pub fn recorded(&self) -> TrackLayout {
        let (v, a) = (self.layout(crate::ClipKind::Video), self.layout(crate::ClipKind::Audio));
        if v.sources() || a.sources() {
            TrackLayout { mode: TrackMode::Separate, add_mix_track: v.mix() || a.mix() }
        } else {
            TrackLayout { mode: TrackMode::Merged, add_mix_track: false }
        }
    }

    /// La traccia `track` del buffer (vedi [`Self::recorded`]) va nelle clip di tipo `kind`.
    pub fn keeps_track(&self, kind: crate::ClipKind, track: usize) -> bool {
        let layout = self.layout(kind);
        if self.recorded().mix() && track == 0 { layout.mix() } else { layout.sources() }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackMode {
    /// Una traccia per sorgente.
    #[default]
    Separate,
    /// Tutte le sorgenti sommate in tempo reale in una traccia, con limiter.
    Merged,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioSourceConfig {
    /// Nome della traccia nel file, es. «Discord».
    pub name: String,
    #[serde(flatten)]
    pub kind: AudioSourceKind,
    /// Guadagno lineare, 1.0 = invariato.
    #[serde(default = "one")]
    pub volume: f32,
    /// Ritardo applicato alla sorgente in millisecondi (positivo = l'audio esce più tardi), per
    /// compensare la latenza di un dispositivo, come il «Sync Offset» di OBS. Da -1000 a 1000.
    #[serde(default)]
    pub delay_ms: i32,
    /// Unisce i canali (media di sinistra e destra) e mette lo stesso segnale su entrambi: per i
    /// microfoni mono collegati a un ingresso stereo, che altrimenti si sentono da un lato solo.
    #[serde(default)]
    pub mono: bool,
}

impl AudioSourceConfig {
    /// Sorgente con volume 1 e nessun ritardo.
    pub fn new(name: impl Into<String>, kind: AudioSourceKind) -> Self {
        Self { name: name.into(), kind, volume: 1.0, delay_ms: 0, mono: false }
    }
}

fn one() -> f32 {
    1.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AudioSourceKind {
    /// Audio di un'applicazione e dei suoi processi figli (process loopback), per nome dell'exe.
    App { exe: String },
    /// Dispositivo d'ingresso per ID endpoint; `None` = predefinito di Windows (lo segue se cambia).
    Input { device: Option<String> },
    /// Loopback di un dispositivo d'uscita; `None` = predefinito di Windows.
    Output { device: Option<String> },
    /// Tutto ciò che suona sul PC tranne un'applicazione e i suoi processi figli (process loopback
    /// in esclusione), per nome dell'exe. Finché l'app non è in esecuzione esclude solo ClipRec.
    Except { exe: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipsConfig {
    /// N: durata della clip video.
    pub video_seconds: u32,
    /// M: durata della clip audio.
    pub audio_seconds: u32,
    /// Cartella delle clip; `None` = `%USERPROFILE%\Videos\ClipRec`.
    pub folder: Option<PathBuf>,
    /// Segnaposto: `{game}`, `{date}` (AAAA-MM-GG), `{time}` (hh-mm-ss), `{kind}` (video/audio).
    pub filename_template: String,
    pub container: VideoContainer,
    pub audio_format: AudioClipFormat,
    /// Bitrate AAC per traccia delle clip video, in kbit/s.
    pub audio_bitrate_kbps: u32,
}

impl Default for ClipsConfig {
    fn default() -> Self {
        Self {
            video_seconds: 60,
            audio_seconds: 120,
            folder: None,
            filename_template: "{game}_{date}_{time}".into(),
            container: VideoContainer::Mp4,
            audio_format: AudioClipFormat::FlacPerTrack,
            audio_bitrate_kbps: 192,
        }
    }
}

impl ClipsConfig {
    pub fn folder(&self) -> PathBuf {
        self.folder.clone().unwrap_or_else(|| {
            let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default();
            home.join("Videos").join("ClipRec")
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoContainer {
    #[default]
    Mp4,
    Mkv,
}

/// Formato delle clip solo audio.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioClipFormat {
    /// Un FLAC per traccia in una cartella per clip (in traccia unica: un solo file).
    #[default]
    FlacPerTrack,
    /// Un WAV per traccia in una cartella per clip (in traccia unica: un solo file).
    WavPerTrack,
    /// Un unico MKA multitraccia.
    Mka,
    /// Un unico WAV polifonico (2 canali per traccia, nomi in iXML): REAPER lo divide, Audition legge i nomi.
    PolyWav,
    /// Opus per la condivisione: un file per traccia (in traccia unica: un solo file).
    Opus,
    /// AAC-LC in M4A per la condivisione: un file per traccia (in traccia unica: un solo file).
    Aac,
    /// MP3 (LAME) per la massima compatibilità: un file per traccia (in traccia unica: un solo file).
    Mp3,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    /// Es. "Ctrl+Alt+F10", "Mouse5", "Ctrl+Mouse4".
    pub video: String,
    pub audio: String,
    /// Facoltativo: tasto o combinazione del controller XInput, es. "Back+A", "LB+RB+Y"; vuoto = nessuno.
    /// Tasti: A, B, X, Y, LB, RB, LS, RS, Back, Start, DPadUp, DPadDown, DPadLeft, DPadRight.
    pub video_pad: String,
    pub audio_pad: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self { video: "Ctrl+Alt+F10".into(), audio: "Ctrl+Alt+F9".into(), video_pad: String::new(), audio_pad: String::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Tetto della RAM dei buffer in MB; `None` = stima da bitrate × durata + audio.
    pub max_mb: Option<u32>,
    /// Blocca le pagine dei buffer in RAM (`VirtualLock`).
    pub lock_pages: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { max_mb: None, lock_pages: true }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// "it", "en" o "auto".
    pub language: String,
    pub start_with_windows: bool,
    /// Con `start_with_windows`: all'accesso parte come amministratore (attività pianificata con
    /// privilegi massimi invece del valore `Run`). Serve per i giochi avviati come amministratore.
    pub start_as_admin: bool,
    pub sound_on_save: bool,
    pub notifications: bool,
    /// All'avvio e ogni giorno cerca una versione nuova e chiede se installarla.
    pub check_updates: bool,
    /// Classe di priorità GPU: "high" (default) o "normal". Mai "realtime".
    pub gpu_priority: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            language: "auto".into(),
            start_with_windows: false,
            start_as_admin: false,
            sound_on_save: true,
            notifications: true,
            check_updates: true,
            gpu_priority: "high".into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("lettura di {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("config.toml non valido: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("serializzazione della configurazione: {0}")]
    Serialize(#[from] toml::ser::Error),
}

impl Config {
    /// `config.toml` accanto all'exe se esiste (modalità portable), altrimenti in `%APPDATA%\ClipRec`.
    pub fn default_path() -> PathBuf {
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
            let portable = dir.join("config.toml");
            if portable.exists() {
                return portable;
            }
        }
        let appdata = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
        appdata.join("ClipRec").join("config.toml")
    }

    /// Legge il file; se non esiste restituisce i default.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Io { path: path.to_path_buf(), source }),
        }
    }

    /// Scrive in modo atomico (file temporaneo accanto e rinomina).
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(self)?;
        let io = |source| ConfigError::Io { path: path.to_path_buf(), source };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn video_and_audio_clips_keep_their_own_tracks() {
        use crate::ClipKind::{Audio, Video};
        let layout = |mode, add_mix_track| TrackLayout { mode, add_mix_track };
        let mut a = AudioConfig { mode: TrackMode::Separate, add_mix_track: false, ..AudioConfig::default() };
        // Configurazione vecchia: la clip audio segue la video, nessun «Mix» registrato.
        assert_eq!(a.recorded(), layout(TrackMode::Separate, false));
        assert!(a.keeps_track(Audio, 0) && a.keeps_track(Video, 1));
        // Video per sorgente, audio in traccia unica: si registrano Mix + sorgenti.
        a.audio_clip = Some(layout(TrackMode::Merged, false));
        assert_eq!(a.recorded(), layout(TrackMode::Separate, true));
        assert!(!a.keeps_track(Video, 0) && a.keeps_track(Video, 1) && a.keeps_track(Video, 2));
        assert!(a.keeps_track(Audio, 0) && !a.keeps_track(Audio, 1));
        // Entrambe in traccia unica: solo il Mix.
        a.mode = TrackMode::Merged;
        assert_eq!(a.recorded(), layout(TrackMode::Merged, false));
        assert!(a.keeps_track(Video, 0) && a.keeps_track(Audio, 0));
        // Video unica, audio per sorgente con «Mix» in testa: tutto in entrambe tranne le sorgenti nel video.
        a.audio_clip = Some(layout(TrackMode::Separate, true));
        assert!(a.keeps_track(Video, 0) && !a.keeps_track(Video, 1));
        assert!(a.keeps_track(Audio, 0) && a.keeps_track(Audio, 1));
    }

    use super::*;

    #[test]
    fn default_round_trips_and_partial_files_are_valid() {
        let c = Config::default();
        let text = toml::to_string_pretty(&c).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), c);

        let partial: Config = toml::from_str(
            "[clips]\nvideo_seconds = 30\n[[audio.sources]]\nname = \"Discord\"\nkind = \"app\"\nexe = \"Discord.exe\"\n",
        )
        .unwrap();
        assert_eq!(partial.clips.video_seconds, 30);
        assert_eq!(partial.clips.audio_seconds, 120);
        assert_eq!(partial.audio.sources.len(), 1);
        assert_eq!(partial.audio.sources[0].kind, AudioSourceKind::App { exe: "Discord.exe".into() });
        let except: AudioSourceConfig = toml::from_str("name = \"Sistema\"\nkind = \"except\"\nexe = \"Discord.exe\"").unwrap();
        assert_eq!(except.kind, AudioSourceKind::Except { exe: "Discord.exe".into() });
        assert_eq!(partial.audio.sources[0].volume, 1.0);
    }

    #[test]
    fn bitrate_table() {
        assert_eq!(default_max_bitrate_kbps(VideoCodec::H264, 1080, 60), 16_000);
        assert_eq!(default_max_bitrate_kbps(VideoCodec::H264, 2160, 60), 50_000);
        assert_eq!(default_max_bitrate_kbps(VideoCodec::Hevc, 1440, 60), 16_666);
        assert_eq!(default_max_bitrate_kbps(VideoCodec::H264, 1080, 120), 32_000);
    }
}
