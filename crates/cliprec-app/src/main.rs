//! Processo principale di ClipRec: shell (tray, hotkey, notifiche, IPC) e motore.
#![windows_subsystem = "windows"]

mod autostart;
mod engine;
mod gamepad;
mod hotkey;
mod i18n;
mod icon;
mod ipc_server;
mod logging;
mod notify;
mod rawinput;
mod shell;
mod supervisor;
mod sys;

use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cliprec_core::{AppCommand, Config, EngineEvent, EngineStatus};

fn main() -> anyhow::Result<()> {
    let _log = logging::init()?;
    cliprec_encode::quiet_ffmpeg_logs();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--prova") {
        return prova(&args[1..]);
    }
    if args.first().is_some_and(|a| a == "--sessione-lunga") {
        // `--sessione-lunga <minuti> <cartella>`: sorgenti sintetiche, CSV di memoria e frame.
        let minutes = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60);
        let out = args.get(2).map_or_else(|| Config::default().clips.folder().join("sessione-lunga"), Into::into);
        return engine::run_soak(Config::default(), minutes, out);
    }
    // Dopo «Riavvia come amministratore» la vecchia istanza sta ancora chiudendo: la si aspetta.
    let instance = if args.iter().any(|a| a == sys::RESTART_ARG) {
        sys::acquire_single_instance_wait(&sys::mutex_name(), Duration::from_secs(10), Duration::from_millis(100))?
    } else {
        sys::acquire_single_instance(&sys::mutex_name())?
    };
    let Some(_instance) = instance else {
        tracing::info!("ClipRec è già in esecuzione: avviso la prima istanza ed esco");
        sys::ping_first_instance();
        return Ok(());
    };

    let config_path = shell::config_path();
    let config = if config_path.exists() {
        Config::load(&config_path).unwrap_or_else(|e| {
            tracing::error!("config.toml: {e}; uso i valori predefiniti");
            Config::default()
        })
    } else {
        first_run_config(&config_path)
    };

    let (cmd_tx, cmd_rx) = channel::<AppCommand>();
    for e in gamepad::set_from_config(&config) {
        tracing::warn!("controller: {e}");
    }
    if let Err(e) = gamepad::spawn(cmd_tx.clone()) {
        tracing::warn!("thread del controller non avviato: {e}");
    }
    let (event_tx, event_rx) = channel::<EngineEvent>();
    let status = Arc::new(Mutex::new(EngineStatus::default()));

    let engine = {
        let (config, status) = (config.clone(), Arc::clone(&status));
        supervisor::spawn_supervised("motore", move || engine::run(config.clone(), &cmd_rx, event_tx.clone(), Arc::clone(&status)))?
    };

    let result = shell::run(shell::ShellIo { commands: cmd_tx, events: event_rx, status, config });

    // Il motore si ferma con `Quit` e aspetta fino a 10 s i salvataggi in corso: qui si aspetta di più.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !engine.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    result
}

/// `cliprec-app --prova [secondi] [--sintetica] [--cartella DIR]`: registra, salva una clip video e
/// una audio, scrive l'esito in `prova.txt` nella cartella delle clip ed esce. Senza `--sintetica`
/// usa le sorgenti di config.toml.
fn prova(args: &[String]) -> anyhow::Result<()> {
    let seconds = args.first().and_then(|s| s.parse().ok()).unwrap_or(10);
    let synthetic = args.iter().any(|a| a == "--sintetica");
    let mut config = Config::load(&shell::config_path()).unwrap_or_default();
    if let Some(dir) = args.iter().position(|a| a == "--cartella").and_then(|i| args.get(i + 1)) {
        config.clips.folder = Some(dir.into());
    }
    let out = config.clips.folder();
    std::fs::create_dir_all(&out)?;
    let report = match engine::run_test_clip(config, seconds, synthetic, out.clone()) {
        Ok(clips) => clips.iter().map(|c| format!("{:?} {} ({:.1} s, {} B)
", c.kind, c.path.display(), c.duration_secs, c.bytes)).collect(),
        Err(e) => format!("ERRORE: {e:#}
"),
    };
    tracing::info!("prova: {report}");
    std::fs::write(out.join("prova.txt"), report)?;
    Ok(())
}

/// Primo avvio: predefiniti (video automatico, audio di sistema e microfono, hotkey assegnate),
/// più Discord se è aperto; il file viene scritto così la finestra impostazioni lo ritrova.
fn first_run_config(path: &std::path::Path) -> Config {
    let config = first_run_defaults(cliprec_audio::find_root_pid("Discord.exe").is_some());
    match config.save(path) {
        Ok(()) => tracing::info!("primo avvio: configurazione scritta in {}", path.display()),
        Err(e) => tracing::error!("primo avvio: {e}"),
    }
    // Primo avvio: si aprono le impostazioni (sorgenti proposte, hotkey, pulsante «Prova»).
    let settings = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("cliprec-settings.exe")));
    if !notify::headless()
        && let Some(exe) = settings.filter(|p| p.exists())
        && let Err(e) = std::process::Command::new(exe).spawn()
    {
        tracing::warn!("primo avvio: impostazioni non aperte: {e}");
    }
    config
}

fn first_run_defaults(discord_running: bool) -> Config {
    let mut config = Config::default();
    if discord_running {
        config.audio.sources.push(cliprec_core::config::AudioSourceConfig {
            name: "Discord".into(),
            kind: cliprec_core::config::AudioSourceKind::App { exe: "Discord.exe".into() },
            volume: 1.0,
        });
    }
    config
}

#[cfg(test)]
mod tests {
    #[test]
    fn first_run_adds_discord_only_when_open() {
        assert_eq!(super::first_run_defaults(false).audio.sources.len(), 2);
        let with = super::first_run_defaults(true);
        assert_eq!(with.audio.sources.last().map(|s| s.name.as_str()), Some("Discord"));
        assert_eq!(with.hotkeys.video, "Ctrl+Alt+F10");
    }
}
