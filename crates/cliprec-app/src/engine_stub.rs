//! Motore finto: registra i comandi nel log, mantiene uno stato inventato e risponde ai
//! salvataggi con un `ClipSaved` finto. L'integrazione lo sostituisce con il motore vero,
//! mantenendo la stessa firma di `run`.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cliprec_core::{AppCommand, ClipKind, Config, EngineEvent, EngineStatus, SavedClip};

/// Gira finché non arriva `AppCommand::Quit` (o il canale si chiude). Ritorna normalmente in
/// quel caso; un panic lo fa riavviare dal supervisore, quindi `commands` è preso per riferimento.
pub fn run(config: Config, commands: &Receiver<AppCommand>, events: Sender<EngineEvent>, status: Arc<Mutex<EngineStatus>>) {
    let mut config = config;
    update(&status, |s| {
        s.buffer_running = true;
        s.video_enabled = config.video.enabled;
        s.encoder = "stub".into();
        s.source = "sorgente finta".into();
        s.fps = f64::from(config.video.fps);
    });
    tracing::info!("motore finto avviato");
    loop {
        match commands.recv_timeout(Duration::from_millis(500)) {
            Ok(cmd) => {
                tracing::info!(?cmd, "comando ricevuto dal motore finto");
                match cmd {
                    AppCommand::Quit => break,
                    AppCommand::StartBuffer => update(&status, |s| s.buffer_running = true),
                    AppCommand::StopBuffer => update(&status, |s| {
                        s.buffer_running = false;
                        s.video_seconds = 0.0;
                        s.audio_seconds = 0.0;
                        s.buffer_bytes = 0;
                    }),
                    AppCommand::ReloadConfig => {
                        if let Ok(c) = Config::load(&Config::default_path()) {
                            config = c;
                        }
                    }
                    AppCommand::SaveVideoClip => save(&config, ClipKind::Video, &events, &status),
                    AppCommand::SaveAudioClip => save(&config, ClipKind::Audio, &events, &status),
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let (vmax, amax) = (f64::from(config.clips.video_seconds), f64::from(config.clips.audio_seconds));
        update(&status, |s| {
            if s.buffer_running {
                s.video_seconds = if s.video_enabled { (s.video_seconds + 0.5).min(vmax) } else { 0.0 };
                s.audio_seconds = (s.audio_seconds + 0.5).min(amax);
                s.buffer_bytes = (s.video_seconds * 600_000.0 + s.audio_seconds * 120_000.0) as u64;
            }
        });
    }
    tracing::info!("motore finto terminato");
}

fn update(status: &Mutex<EngineStatus>, f: impl FnOnce(&mut EngineStatus)) {
    if let Ok(mut s) = status.lock() {
        f(&mut s);
    }
}

fn save(config: &Config, kind: ClipKind, events: &Sender<EngineEvent>, status: &Mutex<EngineStatus>) {
    let (secs, name) = match kind {
        ClipKind::Video => (config.clips.video_seconds, "video.mp4"),
        ClipKind::Audio => (config.clips.audio_seconds, "audio"),
    };
    let path = config.clips.folder().join(format!("stub_{name}"));
    update(status, |s| {
        s.recent_clips.insert(0, path.clone());
        s.recent_clips.truncate(5);
    });
    let clip = SavedClip { kind, path, duration_secs: f64::from(secs), bytes: u64::from(secs) * 800_000 };
    // Se la shell non ascolta più non c'è nessuno da avvisare.
    let _ = events.send(EngineEvent::ClipSaved(clip));
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;

    use super::*;

    #[test]
    fn saves_emit_clip_saved_and_quit_ends() {
        let (ctx, crx) = channel();
        let (etx, erx) = channel();
        let status = Arc::new(Mutex::new(EngineStatus::default()));
        ctx.send(AppCommand::SaveVideoClip).unwrap();
        ctx.send(AppCommand::SaveAudioClip).unwrap();
        ctx.send(AppCommand::Quit).unwrap();
        run(Config::default(), &crx, etx, status.clone());
        let kinds: Vec<_> = erx
            .try_iter()
            .map(|e| match e {
                EngineEvent::ClipSaved(c) => c.kind,
                other => panic!("evento inatteso {other:?}"),
            })
            .collect();
        assert_eq!(kinds, [ClipKind::Video, ClipKind::Audio]);
        assert_eq!(status.lock().unwrap().recent_clips.len(), 2);
    }
}
