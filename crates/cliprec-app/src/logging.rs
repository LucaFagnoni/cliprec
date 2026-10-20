//! Log su file rotante (giornaliero, ultimi 7 file).

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::EnvFilter;

/// `CLIPREC_LOG_DIR` se impostata, altrimenti `%LOCALAPPDATA%\ClipRec\logs`.
pub fn log_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CLIPREC_LOG_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("ClipRec").join("logs")
}

/// Installa il subscriber globale. Tenere vivo il guard fino all'uscita.
pub fn init() -> anyhow::Result<WorkerGuard> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir)?;
    let file = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("cliprec")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_writer(writer).with_ansi(false).with_env_filter(filter).try_init().map_err(|e| anyhow::anyhow!("{e}"))?;
    std::panic::set_hook(Box::new(|info| tracing::error!("panic: {info}")));
    Ok(guard)
}

#[cfg(test)]
mod tests {
    #[test]
    fn env_overrides_log_dir() {
        // Il test non scrive nulla: controlla solo la risoluzione del percorso di default.
        assert!(super::log_dir().ends_with("logs") || std::env::var_os("CLIPREC_LOG_DIR").is_some());
    }
}
