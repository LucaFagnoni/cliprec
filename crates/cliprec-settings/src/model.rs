//! Logica pura: conversione tra il modello della UI (`Form` + righe audio) e `Config`,
//! validazione e stima della RAM. Niente Slint a runtime qui, così si testa senza finestra.

use cliprec_core::VideoCodec;
use cliprec_core::config::{
    AudioClipFormat, AudioSourceConfig, AudioSourceKind, CodecChoice, Config, MAX_AUDIO_SOURCES, OutputResolution,
    TrackMode, VideoContainer, VideoSourceConfig, default_max_bitrate_kbps,
};

use crate::{Form, SourceRow};

pub const FPS_CHOICES: [u32; 4] = [30, 60, 120, 144];
/// FLAC stereo a 16 bit: circa 6 MB al minuto per traccia (SPEC, «Clip solo audio»).
pub const AUDIO_MB_PER_MIN_PER_TRACK: f64 = 6.0;
/// Il buffer video copre N secondi più un GOP; per la stima si contano 2 s di GOP.
const GOP_SECONDS: f64 = 2.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    TooManySources,
    VideoSecondsZero,
    AudioSecondsZero,
    /// 0 = video, 1 = audio.
    HotkeyEmpty(u8),
    SameHotkey,
    /// 0 = video, 1 = audio.
    BadPad(u8),
    SamePad,
    BadBitrate,
    BadMaxMb,
    BadMonitor,
    EmptyVideoTarget,
    EmptySourceName(usize),
    EmptyAppExe(usize),
    EmptyTemplate,
}

fn trimmed(s: &slint::SharedString) -> String {
    s.trim().to_string()
}

/// Config -> (form, righe audio).
pub fn to_form(c: &Config) -> (Form, Vec<SourceRow>) {
    let (source_kind, source_target) = match &c.video.source {
        VideoSourceConfig::Auto => (0, String::new()),
        VideoSourceConfig::Monitor { index } => (1, index.to_string()),
        VideoSourceConfig::Window { title } => (2, title.clone()),
        VideoSourceConfig::Process { exe } => (3, exe.clone()),
    };
    let form = Form {
        video_enabled: c.video.enabled,
        source_kind,
        source_target: source_target.into(),
        codec: match c.video.codec {
            CodecChoice::Auto => 0,
            CodecChoice::H264 => 1,
            CodecChoice::Hevc => 2,
            CodecChoice::Av1 => 3,
        },
        resolution: match c.video.resolution {
            OutputResolution::Native => 0,
            OutputResolution::P1440 => 1,
            OutputResolution::P1080 => 2,
            OutputResolution::P720 => 3,
        },
        fps: FPS_CHOICES.iter().position(|&f| f == c.video.fps).unwrap_or(1) as i32,
        quality: i32::from(c.video.quality),
        max_bitrate: c.video.max_bitrate_kbps.map(|v| v.to_string()).unwrap_or_default().into(),
        capture_cursor: c.video.capture_cursor,
        any_fullscreen: c.video.any_fullscreen_app,
        games: c.video.games.join(", ").into(),
        audio_mode: i32::from(c.audio.mode == TrackMode::Merged),
        add_mix: c.audio.add_mix_track,
        hotkey_video: c.hotkeys.video.clone().into(),
        hotkey_audio: c.hotkeys.audio.clone().into(),
        pad_video: c.hotkeys.video_pad.clone().into(),
        pad_audio: c.hotkeys.audio_pad.clone().into(),
        video_seconds: i32::try_from(c.clips.video_seconds).unwrap_or(i32::MAX),
        audio_seconds: i32::try_from(c.clips.audio_seconds).unwrap_or(i32::MAX),
        folder: c.clips.folder.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default().into(),
        template: c.clips.filename_template.clone().into(),
        audio_format: match c.clips.audio_format {
            AudioClipFormat::FlacPerTrack => 0,
            AudioClipFormat::WavPerTrack => 1,
            AudioClipFormat::Mka => 2,
            AudioClipFormat::PolyWav => 3,
            AudioClipFormat::Opus => 4,
            AudioClipFormat::Aac => 5,
        },
        container: i32::from(c.clips.container == VideoContainer::Mkv),
        max_mb: c.memory.max_mb.map(|v| v.to_string()).unwrap_or_default().into(),
        lock_pages: c.memory.lock_pages,
        gpu_priority: i32::from(c.general.gpu_priority == "normal"),
        language: match c.general.language.as_str() {
            "it" => 1,
            "en" => 2,
            _ => 0,
        },
        start_with_windows: c.general.start_with_windows,
        sound: c.general.sound_on_save,
        notifications: c.general.notifications,
    };
    let rows = c
        .audio
        .sources
        .iter()
        .map(|s| {
            let (kind, target) = match &s.kind {
                AudioSourceKind::App { exe } => (0, exe.clone()),
                AudioSourceKind::Input { device } => (1, device.clone().unwrap_or_default()),
                AudioSourceKind::Output { device } => (2, device.clone().unwrap_or_default()),
            };
            SourceRow { name: s.name.clone().into(), kind, target: target.into(), volume: s.volume }
        })
        .collect();
    (form, rows)
}

/// (form, righe) -> Config, partendo da `base` per i campi che la UI non espone.
pub fn build_config(form: &Form, rows: &[SourceRow], base: &Config) -> Result<Config, Vec<Problem>> {
    let (c, problems) = build_lenient(form, rows, base);
    if problems.is_empty() { Ok(c) } else { Err(problems) }
}

/// Come [`build_config`] ma restituisce sempre una configurazione (con i campi errati ai valori
/// di riserva) più i problemi: serve alla stima dal vivo mentre l'utente sta ancora scrivendo.
pub fn build_lenient(form: &Form, rows: &[SourceRow], base: &Config) -> (Config, Vec<Problem>) {
    let mut problems = Vec::new();
    let mut c = base.clone();

    let target = trimmed(&form.source_target);
    c.video.enabled = form.video_enabled;
    c.video.source = match form.source_kind {
        1 => match target.parse::<u32>() {
            Ok(index) => VideoSourceConfig::Monitor { index },
            Err(_) => {
                problems.push(Problem::BadMonitor);
                VideoSourceConfig::Auto
            }
        },
        2 | 3 if target.is_empty() => {
            problems.push(Problem::EmptyVideoTarget);
            VideoSourceConfig::Auto
        }
        2 => VideoSourceConfig::Window { title: target },
        3 => VideoSourceConfig::Process { exe: target },
        _ => VideoSourceConfig::Auto,
    };
    c.video.codec = match form.codec {
        1 => CodecChoice::H264,
        2 => CodecChoice::Hevc,
        3 => CodecChoice::Av1,
        _ => CodecChoice::Auto,
    };
    c.video.resolution = match form.resolution {
        1 => OutputResolution::P1440,
        2 => OutputResolution::P1080,
        3 => OutputResolution::P720,
        _ => OutputResolution::Native,
    };
    c.video.fps = usize::try_from(form.fps).ok().and_then(|i| FPS_CHOICES.get(i)).copied().unwrap_or(60);
    c.video.quality = u8::try_from(form.quality.clamp(0, 51)).unwrap_or(23);
    c.video.max_bitrate_kbps = match optional_u32(&form.max_bitrate) {
        Ok(v) => v,
        Err(()) => {
            problems.push(Problem::BadBitrate);
            None
        }
    };
    c.video.capture_cursor = form.capture_cursor;
    c.video.any_fullscreen_app = form.any_fullscreen;
    c.video.games = form.games.split([',', ';']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();

    c.audio.mode = if form.audio_mode == 1 { TrackMode::Merged } else { TrackMode::Separate };
    // L'opzione vale solo in modalità separata: in traccia unica la si azzera.
    c.audio.add_mix_track = form.add_mix && c.audio.mode == TrackMode::Separate;
    if rows.len() > MAX_AUDIO_SOURCES {
        problems.push(Problem::TooManySources);
    }
    c.audio.sources = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let name = trimmed(&r.name);
            let t = trimmed(&r.target);
            if name.is_empty() {
                problems.push(Problem::EmptySourceName(i));
            }
            let device = (!t.is_empty()).then(|| t.clone());
            let kind = match r.kind {
                1 => AudioSourceKind::Input { device },
                2 => AudioSourceKind::Output { device },
                _ => {
                    if t.is_empty() {
                        problems.push(Problem::EmptyAppExe(i));
                    }
                    AudioSourceKind::App { exe: t }
                }
            };
            AudioSourceConfig { volume: r.volume.clamp(0.0, 2.0), ..AudioSourceConfig::new(name, kind) }
        })
        .collect();

    c.hotkeys.video = trimmed(&form.hotkey_video);
    c.hotkeys.audio = trimmed(&form.hotkey_audio);
    if c.hotkeys.video.is_empty() {
        problems.push(Problem::HotkeyEmpty(0));
    }
    if c.hotkeys.audio.is_empty() {
        problems.push(Problem::HotkeyEmpty(1));
    }
    if !c.hotkeys.video.is_empty() && c.hotkeys.video.eq_ignore_ascii_case(&c.hotkeys.audio) {
        problems.push(Problem::SameHotkey);
    }

    c.hotkeys.video_pad = trimmed(&form.pad_video);
    c.hotkeys.audio_pad = trimmed(&form.pad_audio);
    for (n, pad) in [&c.hotkeys.video_pad, &c.hotkeys.audio_pad].into_iter().enumerate() {
        if !pad_is_valid(pad) {
            problems.push(Problem::BadPad(n as u8));
        }
    }
    if !c.hotkeys.video_pad.is_empty() && pad_normalized(&c.hotkeys.video_pad) == pad_normalized(&c.hotkeys.audio_pad) {
        problems.push(Problem::SamePad);
    }

    let secs = |v: i32| u32::try_from(v).unwrap_or(0);
    c.clips.video_seconds = secs(form.video_seconds);
    c.clips.audio_seconds = secs(form.audio_seconds);
    if c.clips.video_seconds == 0 {
        problems.push(Problem::VideoSecondsZero);
    }
    if c.clips.audio_seconds == 0 {
        problems.push(Problem::AudioSecondsZero);
    }
    let folder = trimmed(&form.folder);
    c.clips.folder = (!folder.is_empty()).then(|| folder.into());
    c.clips.filename_template = trimmed(&form.template);
    if c.clips.filename_template.is_empty() {
        problems.push(Problem::EmptyTemplate);
    }
    c.clips.audio_format = match form.audio_format {
        1 => AudioClipFormat::WavPerTrack,
        2 => AudioClipFormat::Mka,
        3 => AudioClipFormat::PolyWav,
        4 => AudioClipFormat::Opus,
        5 => AudioClipFormat::Aac,
        _ => AudioClipFormat::FlacPerTrack,
    };
    c.clips.container = if form.container == 1 { VideoContainer::Mkv } else { VideoContainer::Mp4 };

    c.memory.max_mb = match optional_u32(&form.max_mb) {
        Ok(v) => v,
        Err(()) => {
            problems.push(Problem::BadMaxMb);
            None
        }
    };
    c.memory.lock_pages = form.lock_pages;
    c.general.gpu_priority = if form.gpu_priority == 1 { "normal" } else { "high" }.into();
    c.general.language = match form.language {
        1 => "it",
        2 => "en",
        _ => "auto",
    }
    .into();
    c.general.start_with_windows = form.start_with_windows;
    c.general.sound_on_save = form.sound;
    c.general.notifications = form.notifications;

    (c, problems)
}

const PAD_BUTTONS: [&str; 14] =
    ["a", "b", "x", "y", "lb", "rb", "ls", "rs", "back", "start", "dpadup", "dpaddown", "dpadleft", "dpadright"];

/// Vuoto o `Tasto+Tasto` con soli nomi ammessi (maiuscole indifferenti).
pub fn pad_is_valid(s: &str) -> bool {
    s.is_empty() || s.split('+').all(|t| PAD_BUTTONS.contains(&t.trim().to_ascii_lowercase().as_str()))
}

/// Forma canonica per confrontare due combinazioni (maiuscole e ordine indifferenti).
fn pad_normalized(s: &str) -> Vec<String> {
    let mut v: Vec<String> = s.split('+').map(|t| t.trim().to_ascii_lowercase()).collect();
    v.sort();
    v
}

/// Vuoto = `None`; un numero intero positivo = `Some`; altro = errore.
fn optional_u32(s: &slint::SharedString) -> Result<Option<u32>, ()> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<u32>() {
        Ok(v) if v > 0 => Ok(Some(v)),
        _ => Err(()),
    }
}

/// Stima della RAM dei buffer in MB (1 MB = 10^6 byte): bitrate × (N + GOP) + audio.
/// Il tetto di bitrate vuoto usa la tabella del core; con risoluzione nativa si assume 1440p.
pub fn estimate_ram_mb(c: &Config) -> f64 {
    let n = f64::from(c.clips.video_seconds);
    let m = f64::from(c.clips.audio_seconds);
    let video = if c.video.enabled {
        let codec = match c.video.codec {
            CodecChoice::H264 => VideoCodec::H264,
            CodecChoice::Av1 => VideoCodec::Av1,
            CodecChoice::Hevc | CodecChoice::Auto => VideoCodec::Hevc,
        };
        let height = c.video.resolution.height().unwrap_or(1440);
        let kbps = c.video.max_bitrate_kbps.unwrap_or_else(|| default_max_bitrate_kbps(codec, height, c.video.fps));
        f64::from(kbps) * 1000.0 / 8.0 * (n + GOP_SECONDS) / 1e6
    } else {
        0.0
    };
    // Il buffer audio copre il maggiore tra N e M (solo M senza video).
    let audio_secs = if c.video.enabled { n.max(m) } else { m };
    let tracks = c.audio.sources.len() as f64;
    let audio = tracks * AUDIO_MB_PER_MIN_PER_TRACK * audio_secs / 60.0;
    video + audio
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_of(c: &Config) -> (Form, Vec<SourceRow>) {
        to_form(c)
    }

    #[test]
    fn default_config_round_trips() {
        let c = Config::default();
        let (f, r) = rows_of(&c);
        assert_eq!(build_config(&f, &r, &c).unwrap(), c);
    }

    #[test]
    fn rich_config_round_trips() {
        let mut c = Config::default();
        c.video.source = VideoSourceConfig::Process { exe: "game.exe".into() };
        c.video.codec = CodecChoice::Av1;
        c.video.fps = 144;
        c.video.resolution = OutputResolution::P1080;
        c.video.max_bitrate_kbps = Some(20_000);
        c.video.games = vec!["a.exe".into(), "b.exe".into()];
        c.audio.add_mix_track = true;
        c.audio.sources = vec![
            AudioSourceConfig { volume: 0.5, ..AudioSourceConfig::new("Discord", AudioSourceKind::App { exe: "Discord.exe".into() }) },
            AudioSourceConfig::new("Mic", AudioSourceKind::Input { device: Some("{id}".into()) }),
        ];
        c.clips.folder = Some("D:\\Clips".into());
        c.clips.container = VideoContainer::Mkv;
        c.clips.audio_format = AudioClipFormat::Mka;
        c.memory.max_mb = Some(800);
        c.general.language = "en".into();
        c.general.gpu_priority = "normal".into();
        let (f, r) = rows_of(&c);
        assert_eq!(build_config(&f, &r, &c).unwrap(), c);
    }

    #[test]
    fn mix_track_only_in_separate_mode() {
        let mut c = Config::default();
        c.audio.add_mix_track = true;
        let (mut f, r) = rows_of(&c);
        f.audio_mode = 1;
        assert!(!build_config(&f, &r, &c).unwrap().audio.add_mix_track);
    }

    #[test]
    fn validation_catches_problems() {
        let c = Config::default();
        let (mut f, mut r) = rows_of(&c);
        f.video_seconds = 0;
        f.audio_seconds = -3;
        f.hotkey_audio = f.hotkey_video.clone();
        f.max_bitrate = "abc".into();
        f.max_mb = "0".into();
        for _ in 0..7 {
            r.push(SourceRow { name: "x".into(), kind: 1, target: "".into(), volume: 1.0 });
        }
        let p = build_config(&f, &r, &c).unwrap_err();
        for expected in [
            Problem::VideoSecondsZero,
            Problem::AudioSecondsZero,
            Problem::SameHotkey,
            Problem::BadBitrate,
            Problem::BadMaxMb,
            Problem::TooManySources,
        ] {
            assert!(p.contains(&expected), "manca {expected:?} in {p:?}");
        }
    }

    #[test]
    fn video_target_and_source_rows_are_checked() {
        let c = Config::default();
        let (mut f, mut r) = rows_of(&c);
        f.source_kind = 3;
        f.source_target = "  ".into();
        r.push(SourceRow { name: " ".into(), kind: 0, target: "".into(), volume: 1.0 });
        let p = build_config(&f, &r, &c).unwrap_err();
        assert!(p.contains(&Problem::EmptyVideoTarget));
        assert!(p.contains(&Problem::EmptySourceName(2)));
        assert!(p.contains(&Problem::EmptyAppExe(2)));
        f.source_kind = 1;
        f.source_target = "x".into();
        assert!(build_config(&f, &r, &c).unwrap_err().contains(&Problem::BadMonitor));
    }

    #[test]
    fn pad_round_trip_and_validation() {
        let mut c = Config::default();
        c.hotkeys.video_pad = "Back+A".into();
        c.hotkeys.audio_pad = "LB+RB+Y".into();
        let (f, r) = rows_of(&c);
        assert_eq!(build_config(&f, &r, &c).unwrap(), c);
        let (mut f, r) = rows_of(&Config::default());
        assert!(build_config(&f, &r, &c).is_ok(), "campi vuoti ammessi");
        f.pad_video = "dpadup+Start".into();
        assert!(build_config(&f, &r, &c).is_ok(), "maiuscole indifferenti");
        f.pad_video = "Back+Z".into();
        assert!(build_config(&f, &r, &c).unwrap_err().contains(&Problem::BadPad(0)));
        f.pad_video = "Back+A".into();
        f.pad_audio = "a+BACK".into();
        assert!(build_config(&f, &r, &c).unwrap_err().contains(&Problem::SamePad));
        f.pad_audio = "Back+".into();
        assert!(build_config(&f, &r, &c).unwrap_err().contains(&Problem::BadPad(1)));
    }

    #[test]
    fn ram_estimate() {
        // Default: HEVC 1440p60 = 16 666 kbit/s, 60 s + 2 s GOP, 2 tracce audio da 120 s.
        let c = Config::default();
        let video = 16_666.0 * 1000.0 / 8.0 * 62.0 / 1e6;
        let audio = 2.0 * 6.0 * 120.0 / 60.0;
        assert!((estimate_ram_mb(&c) - (video + audio)).abs() < 0.01);

        let mut a = Config::default();
        a.video.enabled = false;
        a.audio.sources.truncate(1);
        assert!((estimate_ram_mb(&a) - 12.0).abs() < 1e-9);

        let mut b = Config::default();
        b.video.max_bitrate_kbps = Some(8_000);
        b.clips.video_seconds = 10;
        b.clips.audio_seconds = 5;
        // 8 Mbit/s x 12 s = 12 MB, audio 2 tracce x 6 MB/min x 10 s = 2 MB
        assert!((estimate_ram_mb(&b) - 14.0).abs() < 1e-9);
    }
}
