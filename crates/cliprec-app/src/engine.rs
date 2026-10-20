//! Motore di ClipRec: collega sorgenti, encoder, buffer e salvataggio ai comandi della shell.
//!
//! Un thread esegue [`run`]; una [`Session`] tiene sorgenti, encoder e buffer accesi. La hotkey
//! prende uno snapshot del buffer (lock brevissimo) e un thread a bassa priorità scrive il file.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cliprec_buffer::{BufferConfig, ReplayBuffer};
use cliprec_core::config::{Config, TrackMode, default_max_bitrate_kbps};
use cliprec_core::{
    AppCommand, AudioTrackSink, ClipKind, EngineEvent, EngineStatus, GpuDevice, GpuFrame, QpcTime,
    VideoEncoder, VideoSource,
};

/// Sorgenti iniettabili (test e sorgenti sintetiche); `None` = dalla configurazione.
#[derive(Default)]
pub struct SourceOverrides {
    pub video: Option<(GpuDevice, Box<dyn VideoSource>)>,
    pub audio: Option<Vec<Box<dyn cliprec_core::AudioSource>>>,
}

/// Una sessione di registrazione attiva: buffer sempre pieno, pronto per le hotkey.
pub struct Session {
    config: Config,
    buffer: Arc<ReplayBuffer>,
    video: Option<Box<dyn VideoSource>>,
    audio: Option<cliprec_audio::AudioEngine>,
    encoder_name: Arc<Mutex<String>>,
    events: Sender<EngineEvent>,
}

impl Session {
    pub fn start(config: &Config, overrides: SourceOverrides, events: Sender<EngineEvent>) -> anyhow::Result<Self> {
        let audio_seconds = f64::from(config.clips.audio_seconds.max(config.clips.video_seconds));
        let video_seconds = f64::from(config.clips.video_seconds);
        // Senza tetto esplicito: stima generosa col bitrate massimo predefinito più alto (H.264 4K).
        let max_bytes = match config.memory.max_mb {
            Some(mb) => mb as usize * 1024 * 1024,
            None => {
                let kbps = config.video.max_bitrate_kbps.unwrap_or_else(|| {
                    default_max_bitrate_kbps(cliprec_core::VideoCodec::H264, 2160, config.video.fps)
                });
                BufferConfig::estimate_bytes(u64::from(kbps) * 1000, video_seconds, audio_seconds, config.audio.sources.len() + 1)
            }
        };
        let buffer = Arc::new(ReplayBuffer::new(BufferConfig {
            video_seconds,
            audio_seconds,
            max_bytes,
            lock_pages: config.memory.lock_pages,
        }));
        let encoder_name = Arc::new(Mutex::new(String::new()));

        let audio = match overrides.audio {
            Some(sources) => Some(cliprec_audio::AudioEngine::with_sources(
                sources,
                &[],
                config.audio.mode,
                config.audio.add_mix_track && config.audio.mode == TrackMode::Separate,
                buffer.clone() as Arc<dyn AudioTrackSink>,
            )?),
            None if !config.audio.sources.is_empty() => {
                Some(cliprec_audio::AudioEngine::start(&config.audio, buffer.clone() as Arc<dyn AudioTrackSink>)?)
            }
            None => None,
        };

        let video = if config.video.enabled {
            let (device, mut source) = match overrides.video {
                Some(v) => v,
                None => cliprec_capture::open_source(&config.video)?,
            };
            if let Err(e) = cliprec_encode::set_gpu_priority(&device, config.general.gpu_priority != "normal") {
                tracing::warn!("priorità GPU: {e}");
            }
            let on_frame = video_callback(config, device, buffer.clone(), encoder_name.clone(), events.clone());
            source.start(config.video.fps, on_frame)?;
            Some(source)
        } else {
            None
        };

        Ok(Self { config: config.clone(), buffer, video, audio, encoder_name, events })
    }

    pub fn status(&self) -> EngineStatus {
        let stats = self.buffer.stats();
        let capture = self.video.as_ref().map(|v| v.stats()).unwrap_or_default();
        EngineStatus {
            buffer_running: true,
            video_enabled: self.video.is_some(),
            video_seconds: stats.video_seconds,
            audio_seconds: stats.audio_seconds,
            buffer_bytes: stats.bytes as u64,
            ram_locked: stats.locked,
            encoder: self.encoder_name.lock().map(|n| n.clone()).unwrap_or_default(),
            source: self.video.as_ref().map(|v| v.describe()).unwrap_or_default(),
            fps: f64::from(self.config.video.fps),
            frames_dropped: capture.frames_dropped,
            last_error: stats.memory_limited.then(|| {
                format!("RAM insufficiente: il buffer copre solo {:.0} s di video", stats.video_seconds)
            }),
            ..Default::default()
        }
    }

    /// Prende lo snapshot degli ultimi `seconds` secondi (lock brevissimo) e scrive il file su un
    /// thread a bassa priorità.
    pub fn save(&self, kind: ClipKind, seconds: u32) -> Option<std::thread::JoinHandle<()>> {
        let end = QpcTime::now();
        let folder = self.config.clips.folder();
        if let Err(e) = std::fs::create_dir_all(&folder) {
            let _ = self.events.send(EngineEvent::ClipFailed { kind, reason: format!("cartella {}: {e}", folder.display()) });
            return None;
        }
        let name = clip_name(&self.config.clips.filename_template, kind, self.video.as_ref().map(|v| v.describe()));
        let events = self.events.clone();
        let fail = move |events: &Sender<EngineEvent>, reason: String| {
            let _ = events.send(EngineEvent::ClipFailed { kind, reason });
        };
        match kind {
            ClipKind::Video => {
                let Some(video) = self.buffer.snapshot_video(f64::from(seconds), end) else {
                    fail(&events, "il buffer video è ancora vuoto".into());
                    return None;
                };
                let start = video.start().unwrap_or(end);
                let audio = self.buffer.snapshot_audio(start, end);
                // Il muxer aggiunge l'estensione e « (2)» se il nome esiste già.
                let path = folder.join(name);
                let opts = cliprec_mux::VideoClipOptions { container: self.config.clips.container, ..Default::default() };
                spawn_saver(move || {
                    match cliprec_mux::write_video_clip(&path, &video, audio.as_ref(), &opts) {
                        Ok(clip) => {
                            let _ = events.send(EngineEvent::ClipSaved(clip));
                        }
                        Err(e) => fail(&events, e.to_string()),
                    }
                })
            }
            ClipKind::Audio => {
                let start = end.add_hns(-i64::from(seconds) * cliprec_core::HNS_PER_SEC);
                let Some(audio) = self.buffer.snapshot_audio(start, end) else {
                    fail(&events, "il buffer audio è ancora vuoto".into());
                    return None;
                };
                let format = self.config.clips.audio_format;
                let path = folder.join(name);
                spawn_saver(move || match cliprec_mux::write_audio_clip(&path, &audio, format) {
                    Ok(clip) => {
                        let _ = events.send(EngineEvent::ClipSaved(clip));
                    }
                    Err(e) => fail(&events, e.to_string()),
                })
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(v) = self.video.as_mut() {
            v.stop();
        }
        if let Some(a) = self.audio.as_mut() {
            a.stop();
        }
    }
}

/// Callback del pacer: apre l'encoder al primo frame (serve la dimensione della sorgente),
/// codifica e passa i pacchetti al buffer. Se l'encoder fallisce lo riapre al frame dopo.
fn video_callback(
    config: &Config,
    device: GpuDevice,
    buffer: Arc<ReplayBuffer>,
    encoder_name: Arc<Mutex<String>>,
    events: Sender<EngineEvent>,
) -> Box<dyn FnMut(&GpuFrame) + Send> {
    let video_cfg = config.video.clone();
    let mut encoder: Option<Box<dyn VideoEncoder>> = None;
    let mut last_error = String::new();
    let mut next_index: Option<u64> = None;
    let fps = config.video.fps.max(1);
    Box::new(move |frame: &GpuFrame| {
        if encoder.is_none() {
            let settings = cliprec_encode::EncoderSettings::from_config(&video_cfg, frame.width, frame.height);
            match cliprec_encode::open_best(&device, &settings) {
                Ok(e) => {
                    buffer.set_video_params(e.params().clone());
                    if let Ok(mut n) = encoder_name.lock() {
                        n.clone_from(&e.params().encoder_name);
                    }
                    encoder = Some(e);
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg != last_error {
                        let _ = events.send(EngineEvent::Error(format!("encoder video: {msg}")));
                        last_error = msg;
                    }
                    return;
                }
            }
        }
        let Some(enc) = encoder.as_mut() else { return };
        let mut push = |p: cliprec_core::PacketRef<'_>| buffer.push_video(p);
        // Il pacer salta l'indice sui tick persi: ripetendo la texture corrente per quei tick il file
        // resta a frequenza costante e l'audio resta allineato. Oltre un quarto di secondo il buco resta.
        let missing = next_index.map_or(0, |n| frame.index.saturating_sub(n)).min(u64::from(fps / 4));
        let mut result = Ok(());
        for k in (1..=missing).rev() {
            let filler = GpuFrame {
                texture: frame.texture.clone(),
                width: frame.width,
                height: frame.height,
                pts: frame.pts.add_hns(-(k as i64) * cliprec_core::HNS_PER_SEC / i64::from(fps)),
                index: frame.index - k,
                repeated: true,
            };
            result = result.and_then(|()| enc.encode(&filler, &mut push));
        }
        if let Err(e) = result.and_then(|()| enc.encode(frame, &mut push)) {
            let _ = events.send(EngineEvent::Warning(format!("encoder video riaperto: {e}")));
            encoder = None;
            next_index = None;
            return;
        }
        next_index = Some(frame.index + 1);
    })
}

fn spawn_saver(f: impl FnOnce() + Send + 'static) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("clip-save".into())
        .spawn(move || {
            crate::engine::lower_thread_priority();
            f();
        })
        .ok()
}

fn lower_thread_priority() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
    // SAFETY: pseudo-handle del thread corrente, sempre valido.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
}

/// Nome della clip dal modello: `{game}`, `{date}` (AAAA-MM-GG), `{time}` (hh-mm-ss), `{kind}`.
fn clip_name(template: &str, kind: ClipKind, source: Option<String>) -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    // SAFETY: GetLocalTime non ha precondizioni.
    let t = unsafe { GetLocalTime() };
    let game = source.unwrap_or_else(|| "ClipRec".into());
    let name = template
        .replace("{game}", &game)
        .replace("{date}", &format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay))
        .replace("{time}", &format!("{:02}-{:02}-{:02}", t.wHour, t.wMinute, t.wSecond))
        .replace("{kind}", if kind == ClipKind::Video { "video" } else { "audio" });
    sanitize(&name)
}

/// Toglie i caratteri non ammessi nei nomi di file di Windows.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.');
    if trimmed.is_empty() { "clip".into() } else { trimmed.into() }
}

/// Loop del motore: un thread, comandi dalla shell, stato aggiornato ogni 500 ms.
/// `commands` è preso per riferimento così il supervisore lo può riavviare dopo un panic.
pub fn run(mut config: Config, commands: &Receiver<AppCommand>, events: Sender<EngineEvent>, status: Arc<Mutex<EngineStatus>>) {
    let config_path = crate::shell::config_path();
    let start = |config: &Config| match Session::start(config, SourceOverrides::default(), events.clone()) {
        Ok(s) => Some(s),
        Err(e) => {
            let _ = events.send(EngineEvent::Error(format!("avvio del buffer: {e:#}")));
            None
        }
    };
    let mut session = start(&config);
    loop {
        match commands.recv_timeout(Duration::from_millis(500)) {
            Ok(cmd @ (AppCommand::SaveVideoClip | AppCommand::SaveAudioClip | AppCommand::SaveTestClip)) => {
                if let Some(s) = &session {
                    let clips = &s.config.clips;
                    let (kind, seconds) = match cmd {
                        AppCommand::SaveAudioClip => (ClipKind::Audio, clips.audio_seconds),
                        // In modalità solo audio la hotkey video salva una clip audio di N secondi.
                        _ if s.video.is_none() => (ClipKind::Audio, clips.video_seconds),
                        AppCommand::SaveTestClip => (ClipKind::Video, 5),
                        _ => (ClipKind::Video, clips.video_seconds),
                    };
                    let seconds = if cmd == AppCommand::SaveTestClip { 5 } else { seconds };
                    s.save(kind, seconds);
                } else {
                    let _ = events.send(EngineEvent::Warning("il buffer è fermo: avvialo dal menu della tray".into()));
                }
            }
            Ok(AppCommand::StartBuffer) => {
                if session.is_none() {
                    session = start(&config);
                }
            }
            Ok(AppCommand::StopBuffer) => session = None,
            Ok(AppCommand::ReloadConfig) => match Config::load(&config_path) {
                Ok(c) => {
                    config = c;
                    // Chiude la sessione vecchia (cattura, encoder, dispositivi) prima di aprire la nuova.
                    drop(session.take());
                    session = start(&config);
                }
                Err(e) => {
                    let _ = events.send(EngineEvent::Error(format!("config.toml: {e}")));
                }
            },
            Ok(AppCommand::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if let Ok(mut st) = status.lock() {
            let recent = std::mem::take(&mut st.recent_clips);
            *st = session.as_ref().map(Session::status).unwrap_or_default();
            st.recent_clips = recent;
        }
    }
}

/// Prova della catena senza tray né hotkey: registra `seconds` secondi, salva una clip video e
/// una clip audio nella cartella `out` e restituisce i file scritti.
/// Con `synthetic` usa la sorgente video con lampo a ogni secondo e due bip (1 kHz e 1,5 kHz):
/// niente schermo dell'utente, niente microfono, niente altoparlanti.
pub fn run_test_clip(mut config: Config, seconds: u32, synthetic: bool, out: PathBuf) -> anyhow::Result<Vec<cliprec_core::SavedClip>> {
    config.clips.folder = Some(out);
    config.clips.video_seconds = seconds.saturating_sub(1).max(1);
    config.clips.audio_seconds = seconds.saturating_sub(1).max(1);
    let mut overrides = SourceOverrides::default();
    if synthetic {
        let device = GpuDevice::create(None)?;
        let video = cliprec_capture::SyntheticSource::new(device.clone(), 1920, 1080);
        overrides.video = Some((device, Box::new(video)));
        overrides.audio = Some(vec![
            Box::new(cliprec_audio::BeepSource::new("Bip 1 kHz", 1000.0)) as Box<dyn cliprec_core::AudioSource>,
            Box::new(cliprec_audio::BeepSource::new("Bip 1,5 kHz", 1500.0)),
        ]);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let session = Session::start(&config, overrides, tx)?;
    std::thread::sleep(Duration::from_secs(u64::from(seconds)));
    let handles: Vec<_> = [ClipKind::Video, ClipKind::Audio]
        .into_iter()
        .filter_map(|k| session.save(k, config.clips.video_seconds))
        .collect();
    drop(session);
    for h in handles {
        let _ = h.join();
    }
    let mut saved = Vec::new();
    for event in rx.try_iter() {
        match event {
            EngineEvent::ClipSaved(clip) => saved.push(clip),
            EngineEvent::ClipFailed { kind, reason } => anyhow::bail!("clip {kind:?} non salvata: {reason}"),
            other => tracing::warn!(?other, "evento durante la prova"),
        }
    }
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_names_are_valid_file_names() {
        let n = clip_name("{game}_{date}_{time}", ClipKind::Video, Some("Monitor 1: 2560×1440".into()));
        assert!(!n.contains(':') && n.starts_with("Monitor 1_ 2560×1440_"));
        assert_eq!(sanitize("  ...  "), "clip");
    }

    /// Catena verticale con sorgenti sintetiche (lampo e bip a ogni secondo QPC): 8 s di
    /// registrazione, una clip video MP4 e una clip audio, validate con ffprobe e con la misura
    /// della sincronia. Richiede una GPU con encoder hardware.
    #[test]
    fn vertical_chain_with_synthetic_sources() {
        use cliprec_tests::probe::{Expect, check, check_audio_dir};
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/e2e");
        let _ = std::fs::remove_dir_all(&out);
        let clips = run_test_clip(Config::default(), 8, true, out).unwrap();
        let video = clips.iter().find(|c| c.kind == ClipKind::Video).expect("clip video");
        let audio = clips.iter().find(|c| c.kind == ClipKind::Audio).expect("clip audio");
        println!("{clips:#?}");

        let names = vec!["Bip 1 kHz".to_string(), "Bip 1,5 kHz".to_string()];
        let expect = Expect {
            min_duration: Some(7.0),
            audio_tracks: Some(2),
            track_names: Some(names.clone()),
            fps: Some(60.0),
            ..Expect::default()
        };
        let violations = check(&video.path, &expect).unwrap();
        assert!(violations.is_empty(), "clip video: {violations:?}");
        let sync = cliprec_tests::sync::measure_sync(&video.path).unwrap();
        println!("sincronia video: {sync:#?}");
        let violations = sync.violations(20.0);
        assert!(violations.is_empty(), "sincronia video: {violations:?}");

        let expect = Expect { min_duration: Some(7.0), audio_tracks: Some(2), ..Expect::default() };
        let violations =
            if audio.path.is_dir() { check_audio_dir(&audio.path, &expect) } else { check(&audio.path, &expect) }.unwrap();
        assert!(violations.is_empty(), "clip audio: {violations:?}");
        let sync = cliprec_tests::sync::measure_sync(&audio.path).unwrap();
        println!("sincronia audio: {sync:#?}");
        assert!(sync.inter_track.iter().all(|s| s.max_abs_ms <= 20.0), "tracce audio: {sync:?}");
    }
}
