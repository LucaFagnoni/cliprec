//! Scelta della configurazione all'avvio: config assente (primo avvio), valida o illeggibile.

use std::path::Path;

use cliprec_core::Config;

pub struct Startup {
    pub config: Config,
    /// `false` se il buffer non deve partire da solo (config illeggibile).
    pub start_buffer: bool,
    /// Errore da notificare all'utente (config illeggibile).
    pub error: Option<String>,
    /// Aprire la finestra impostazioni (primo avvio o config illeggibile).
    pub open_settings: bool,
}

pub fn startup_config(path: &Path, discord_running: bool, run_value_present: bool) -> Startup {
    if path.exists() {
        match Config::load(path) {
            Ok(config) => Startup { config, start_buffer: true, error: None, open_settings: false },
            Err(e) => {
                // Niente ripartenza silenziosa con i predefiniti (catturerebbero schermo e microfono):
                // buffer fermo, errore all'utente, impostazioni aperte. Il file non si tocca.
                tracing::error!("{}: {e}; buffer fermo", path.display());
                let msg = crate::i18n::fmt(crate::i18n::current(), crate::i18n::K::ConfigInvalid, &[&path.display().to_string(), &e.to_string()]);
                Startup { config: Config::default(), start_buffer: false, error: Some(msg), open_settings: true }
            }
        }
    } else {
        let config = first_run_defaults(discord_running, run_value_present);
        match config.save(path) {
            Ok(()) => tracing::info!("primo avvio: configurazione scritta in {}", path.display()),
            Err(e) => tracing::error!("primo avvio: {e}"),
        }
        Startup { config, start_buffer: true, error: None, open_settings: true }
    }
}

/// Primo avvio: predefiniti (video automatico, audio di sistema e microfono, hotkey assegnate),
/// più Discord se è aperto.
pub fn first_run_defaults(discord_running: bool, run_value_present: bool) -> Config {
    let mut config = Config::default();
    // L'installer può aver scritto il valore Run: la config lo adotta invece di cancellarlo.
    config.general.start_with_windows = run_value_present;
    if discord_running {
        config.audio.sources.push(cliprec_core::config::AudioSourceConfig::new(
            "Discord",
            cliprec_core::config::AudioSourceKind::App { exe: "Discord.exe".into() },
        ));
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/test-tmp")).join(format!("startup-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn first_run_adds_discord_only_when_open() {
        assert_eq!(first_run_defaults(false, false).audio.sources.len(), 2);
        let with = first_run_defaults(true, false);
        assert_eq!(with.audio.sources.last().map(|s| s.name.as_str()), Some("Discord"));
        assert_eq!(with.hotkeys.video, "Ctrl+Alt+F10");
    }

    #[test]
    fn m2_unreadable_config_keeps_buffer_stopped_and_reports() {
        let path = temp("m2");
        std::fs::write(&path, "[clips\nvideo_seconds = = 30").unwrap();
        let s = startup_config(&path, false, false);
        assert!(!s.start_buffer, "il buffer non deve partire con i predefiniti");
        let err = s.error.expect("serve un errore da notificare");
        assert!(err.contains("config.toml"), "{err}");
        assert!(s.open_settings);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[clips\nvideo_seconds = = 30", "il file non va sovrascritto");
    }

    #[test]
    fn m12_first_run_adopts_the_installer_autostart_value() {
        assert!(first_run_defaults(false, true).general.start_with_windows, "il valore Run dell'installer va adottato");
        assert!(!first_run_defaults(false, false).general.start_with_windows);
        let path = temp("m12");
        let s = startup_config(&path, false, true);
        assert!(s.config.general.start_with_windows);
        assert!(Config::load(&path).unwrap().general.start_with_windows, "la config scritta lo conserva");
    }

    #[test]
    fn valid_and_first_run_configs_start_the_buffer() {
        let path = temp("ok");
        std::fs::write(&path, "[clips]\nvideo_seconds = 30\n").unwrap();
        let s = startup_config(&path, false, false);
        assert!(s.start_buffer && s.error.is_none() && !s.open_settings);
        assert_eq!(s.config.clips.video_seconds, 30);
        let new = temp("new");
        let s = startup_config(&new, false, false);
        assert!(s.start_buffer && s.open_settings && new.exists());
    }
}
