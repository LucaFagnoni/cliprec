//! Processo principale di ClipRec: shell (tray, hotkey, notifiche, IPC) e motore.
#![windows_subsystem = "windows"]

mod autostart;
mod engine;
mod hotkey;
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
    let Some(_instance) = sys::acquire_single_instance(&sys::mutex_name())? else {
        tracing::info!("ClipRec è già in esecuzione: esco");
        return Ok(());
    };

    let config = Config::load(&shell::config_path()).unwrap_or_else(|e| {
        tracing::error!("config.toml: {e}; uso i valori predefiniti");
        Config::default()
    });

    let (cmd_tx, cmd_rx) = channel::<AppCommand>();
    let (event_tx, event_rx) = channel::<EngineEvent>();
    let status = Arc::new(Mutex::new(EngineStatus::default()));

    let engine = {
        let (config, status) = (config.clone(), Arc::clone(&status));
        supervisor::spawn_supervised("motore", move || engine::run(config.clone(), &cmd_rx, event_tx.clone(), Arc::clone(&status)))?
    };

    let result = shell::run(shell::ShellIo { commands: cmd_tx, events: event_rx, status, config });

    // Il motore si ferma con `Quit`; gli diamo qualche secondo per chiudere i file aperti.
    let deadline = Instant::now() + Duration::from_secs(5);
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
