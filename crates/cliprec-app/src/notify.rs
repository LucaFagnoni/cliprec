//! Notifiche toast e suono. Gli errori non arrivano mai a raffica: un'avvertenza per tipo ogni
//! `MIN_INTERVAL`. In modalità `CLIPREC_HEADLESS` (notte, test) non si mostra e non si suona nulla.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cliprec_core::{ClipKind, EngineEvent, SavedClip};
use tauri_winrt_notification::Toast;

/// AppUserModelID registrato dal collegamento nel menu Start.
pub const APP_ID: &str = "ClipRec.App";
pub const MIN_INTERVAL: Duration = Duration::from_secs(60);

/// `CLIPREC_HEADLESS=1`: nessun toast, suono, registro o menu Start.
pub fn headless() -> bool {
    std::env::var_os("CLIPREC_HEADLESS").is_some_and(|v| v != "0")
}

/// Limita la frequenza per tipo di avviso.
#[derive(Debug)]
pub struct Dedup {
    min_interval: Duration,
    last: HashMap<String, Instant>,
}

impl Dedup {
    pub fn new(min_interval: Duration) -> Self {
        Self { min_interval, last: HashMap::new() }
    }

    /// `true` se l'avviso di questo tipo può essere mostrato adesso.
    pub fn allow(&mut self, kind: &str, now: Instant) -> bool {
        match self.last.get(kind) {
            Some(t) if now.duration_since(*t) < self.min_interval => false,
            _ => {
                self.last.insert(kind.to_owned(), now);
                true
            }
        }
    }
}

/// Tipo di un evento d'errore, per la deduplica: la prima parola chiave nota, altrimenti il testo.
pub fn error_kind(text: &str) -> String {
    let t = text.to_lowercase();
    for k in ["fullscreen", "schermo intero", "microfono", "privacy", "ram", "memoria", "disco", "encoder", "amministratore"] {
        if t.contains(k) {
            return k.to_owned();
        }
    }
    t
}

pub fn clip_summary(c: &SavedClip) -> String {
    format!("{} s · {} MB", c.duration_secs.round() as u64, (c.bytes as f64 / 1_000_000.0).round() as u64)
}

pub struct Notifier {
    pub enabled: bool,
    pub sound: bool,
    dedup: Dedup,
}

impl Notifier {
    pub fn new(enabled: bool, sound: bool) -> Self {
        Self { enabled, sound, dedup: Dedup::new(MIN_INTERVAL) }
    }

    pub fn handle(&mut self, ev: &EngineEvent) {
        match ev {
            EngineEvent::ClipSaved(c) => {
                if self.sound {
                    play_save_sound();
                }
                if self.enabled {
                    show_clip_saved(c);
                }
            }
            EngineEvent::ClipFailed { kind, reason } => {
                let k = format!("clip-{kind:?}:{}", error_kind(reason));
                self.problem(&k, "Clip non salvata", reason);
            }
            EngineEvent::Warning(t) => self.problem(&format!("w:{}", error_kind(t)), "ClipRec", t),
            EngineEvent::Error(t) => self.problem(&format!("e:{}", error_kind(t)), "ClipRec: errore", t),
        }
    }

    fn problem(&mut self, kind: &str, title: &str, text: &str) {
        tracing::warn!(kind, "{title}: {text}");
        if self.enabled && self.dedup.allow(kind, Instant::now()) {
            show(title, text, &[]);
        }
    }
}

fn show_clip_saved(c: &SavedClip) {
    let title = if c.kind == ClipKind::Audio { "Clip audio salvata" } else { "Clip salvata" };
    show(title, &clip_summary(c), &[("Apri", &c.path), ("Cartella", &c.path)]);
}

/// Mostra un toast; i pulsanti aprono il file («Apri») o la cartella che lo contiene («Cartella»).
fn show(title: &str, text: &str, buttons: &[(&str, &PathBuf)]) {
    if headless() {
        tracing::info!(title, text, "toast soppresso (headless)");
        return;
    }
    let mut toast = Toast::new(APP_ID).title(title).text1(text).sound(None);
    let mut paths: HashMap<String, PathBuf> = HashMap::new();
    for (label, path) in buttons {
        toast = toast.add_button(label, label);
        paths.insert((*label).to_owned(), (*path).clone());
    }
    if !paths.is_empty() {
        toast = toast.on_activated(move |action| {
            if let Some(path) = action.as_deref().and_then(|a| paths.get(a)) {
                match action.as_deref() {
                    Some("Cartella") => open_folder_of(path),
                    _ => crate::sys::open_path(path),
                }
            }
            Ok(())
        });
    }
    if let Err(e) = toast.show() {
        tracing::warn!("toast non mostrato: {e}");
    }
}

fn open_folder_of(path: &Path) {
    let dir = if path.is_dir() { Some(path) } else { path.parent() };
    if let Some(d) = dir {
        crate::sys::open_path(d);
    }
}

/// Suono breve di sistema, asincrono.
fn play_save_sound() {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT};
    if headless() {
        return;
    }
    // SAFETY: alias statico terminato da NUL; SND_ASYNC non trattiene il puntatore oltre la chiamata per gli alias.
    let _ = unsafe { PlaySoundW(windows::core::w!("SystemAsterisk"), None, SND_ALIAS | SND_ASYNC | SND_NODEFAULT) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_limits_per_kind() {
        let mut d = Dedup::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(d.allow("ram", t0));
        assert!(!d.allow("ram", t0 + Duration::from_secs(10)));
        assert!(d.allow("disco", t0 + Duration::from_secs(10)), "altro tipo");
        assert!(d.allow("ram", t0 + Duration::from_secs(61)));
    }

    #[test]
    fn error_kinds_group_similar_texts() {
        assert_eq!(error_kind("RAM insufficiente: 12 MB"), error_kind("ram esaurita"));
        assert_ne!(error_kind("Disco pieno"), error_kind("Microfono bloccato"));
    }

    #[test]
    fn summary_text() {
        let c = SavedClip { kind: ClipKind::Video, path: "x.mp4".into(), duration_secs: 60.0, bytes: 48_000_000 };
        assert_eq!(clip_summary(&c), "60 s · 48 MB");
    }
}
