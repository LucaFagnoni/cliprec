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
mod startup;
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
        // `--sessione-lunga <minuti> <cartella>`: sorgenti sintetiche, CSV di memoria e frame; codec,
        // fps, risoluzione, durate e video sì/no da config.toml (`CLIPREC_CONFIG`).
        let minutes = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60);
        let config = Config::load(&shell::config_path()).unwrap_or_default();
        let out = args.get(2).map_or_else(|| config.clips.folder().join("sessione-lunga"), Into::into);
        return engine::run_soak(config, minutes, out);
    }
    // Dopo «Riavvia come amministratore» la vecchia istanza sta ancora chiudendo: la si aspetta.
    let instance = if args.iter().any(|a| a == sys::RESTART_ARG) {
        sys::acquire_single_instance_wait(&sys::mutex_name(), sys::RESTART_WAIT, Duration::from_millis(100))?
    } else {
        sys::acquire_single_instance(&sys::mutex_name())?
    };
    let Some(_instance) = instance else {
        tracing::info!("ClipRec è già in esecuzione: avviso la prima istanza ed esco");
        sys::ping_first_instance();
        return Ok(());
    };

    i18n::set_from_config("auto");
    let startup = startup::startup_config(&shell::config_path(), cliprec_audio::find_root_pid("Discord.exe").is_some(), !notify::headless() && autostart::run_value_present());
    let config = startup.config.clone();
    open_settings_if_needed(startup.open_settings);

    let (cmd_tx, cmd_rx) = channel::<AppCommand>();
    for e in gamepad::set_from_config(&config) {
        tracing::warn!("controller: {e}");
    }
    if let Err(e) = gamepad::spawn(cmd_tx.clone()) {
        tracing::warn!("thread del controller non avviato: {e}");
    }
    let (event_tx, event_rx) = channel::<EngineEvent>();
    if let Some(e) = &startup.error {
        // Il toast parte appena la shell è su: l'evento resta in coda nel canale.
        let _ = event_tx.send(EngineEvent::Error(e.clone()));
    }
    let status = Arc::new(Mutex::new(EngineStatus::default()));

    let engine = {
        let (config, status, start_buffer) = (config.clone(), Arc::clone(&status), startup.start_buffer);
        supervisor::spawn_supervised("motore", move || engine::run(config.clone(), start_buffer, &shell::config_path(), &cmd_rx, event_tx.clone(), Arc::clone(&status)))?
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

/// Primo avvio o config illeggibile: si apre la finestra impostazioni (non in modalità headless).
fn open_settings_if_needed(open: bool) {
    let settings = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("cliprec-settings.exe")));
    if open
        && !notify::headless()
        && let Some(exe) = settings.filter(|p| p.exists())
        && let Err(e) = std::process::Command::new(exe).spawn()
    {
        tracing::warn!("impostazioni non aperte: {e}");
    }
}
