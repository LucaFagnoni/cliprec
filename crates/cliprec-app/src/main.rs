//! Processo principale di ClipRec: shell (tray, hotkey, notifiche, IPC) e motore.
#![windows_subsystem = "windows"]

mod autostart;
mod engine_stub;
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

    // L'integrazione sostituisce `engine_stub::run` con il motore vero, stessa forma.
    let engine = {
        let (config, status) = (config.clone(), Arc::clone(&status));
        supervisor::spawn_supervised("motore", move || engine_stub::run(config.clone(), &cmd_rx, event_tx.clone(), Arc::clone(&status)))?
    };

    let result = shell::run(shell::ShellIo { commands: cmd_tx, events: event_rx, status, config });

    // Il motore si ferma con `Quit`; gli diamo qualche secondo per chiudere i file aperti.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !engine.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    result
}
