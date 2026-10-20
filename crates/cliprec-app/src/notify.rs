//! Notifiche toast e suono. Gli errori non arrivano mai a raffica: un'avvertenza per tipo ogni
//! `MIN_INTERVAL`. In modalità `CLIPREC_HEADLESS` (notte, test) non si mostra e non si suona nulla.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::i18n::{K, current, fmt};
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
        // Le voci scadute non servono più: la tabella resta piccola anche in 8 ore.
        let min = self.min_interval;
        self.last.retain(|_, t| now.duration_since(*t) < min);
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
    // Parole intere: «ram» non deve riconoscere «frame» o «programma».
    let words: Vec<&str> = t.split(|c: char| !c.is_alphanumeric()).collect();
    for k in [
        "fullscreen",
        "schermo intero",
        "microfono",
        "microphone",
        "privacy",
        "ram",
        "memoria",
        "memory",
        "disco",
        "disk",
        "encoder",
        "amministratore",
        "administrator",
    ] {
        let hit = if k.contains(' ') { t.contains(k) } else { words.contains(&k) };
        if hit {
            return k.to_owned();
        }
    }
    t
}

pub fn clip_summary(c: &SavedClip) -> String {
    format!("{} s · {} MB", c.duration_secs.round() as u64, (c.bytes as f64 / 1_000_000.0).round() as u64)
}

/// Azione collegata a un avviso: apre una pagina delle impostazioni di Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastLink {
    MicrophonePrivacy,
}

impl ToastLink {
    pub fn uri(self) -> &'static str {
        match self {
            Self::MicrophonePrivacy => "ms-settings:privacy-microphone",
        }
    }
}

/// Mappa il testo di un avviso del motore all'azione utile (m26). Per ora solo la privacy del microfono.
pub fn link_for(text: &str) -> Option<ToastLink> {
    let t = text.to_lowercase();
    ((t.contains("microfono") || t.contains("microphone")) && t.contains("privacy")).then_some(ToastLink::MicrophonePrivacy)
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
            // Il suono conferma la richiesta appena premuta, non la fine del salvataggio.
            EngineEvent::ClipRequested => {
                if self.sound {
                    play_request_sound();
                }
            }
            EngineEvent::ClipSaved(c) => {
                if self.enabled {
                    show_clip_saved(c);
                }
            }
            EngineEvent::ClipFailed { kind, reason } => {
                let k = format!("clip-{kind:?}:{}", error_kind(reason));
                self.problem(&k, &fmt(current(), K::ToastFailed, &[]), reason);
            }
            EngineEvent::Warning(t) => self.problem(&format!("w:{}", error_kind(t)), "ClipRec", t),
            EngineEvent::Error(t) => self.problem(&format!("e:{}", error_kind(t)), &fmt(current(), K::ToastError, &[]), t),
        }
    }

    fn problem(&mut self, kind: &str, title: &str, text: &str) {
        tracing::warn!(kind, "{title}: {text}");
        if self.enabled && self.dedup.allow(kind, Instant::now()) {
            match link_for(text) {
                Some(link) => {
                    let label = fmt(current(), K::ToastOpenSettings, &[]);
                    show(title, text, &[(&label, "uri", &PathBuf::from(link.uri()))]);
                }
                None => show(title, text, &[]),
            }
        }
    }
}

fn show_clip_saved(c: &SavedClip) {
    let l = current();
    let title = fmt(l, if c.kind == ClipKind::Audio { K::ToastAudioSaved } else { K::ToastSaved }, &[]);
    let (open, folder) = (fmt(l, K::ToastOpen, &[]), fmt(l, K::ToastFolder, &[]));
    show(&title, &clip_summary(c), &[(&open, "open", &c.path), (&folder, "folder", &c.path)]);
}

/// Mostra un toast; i pulsanti aprono il file («Apri») o la cartella che lo contiene («Cartella»).
fn show(title: &str, text: &str, buttons: &[(&str, &str, &PathBuf)]) {
    if headless() {
        tracing::info!(title, text, "toast soppresso (headless)");
        return;
    }
    let mut toast = Toast::new(APP_ID).title(title).text1(text).sound(None);
    let mut paths: HashMap<String, PathBuf> = HashMap::new();
    for (label, action, path) in buttons {
        toast = toast.add_button(label, action);
        paths.insert((*action).to_owned(), (*path).clone());
    }
    if !paths.is_empty() {
        toast = toast.on_activated(move |action| {
            if let Some(path) = action.as_deref().and_then(|a| paths.get(a)) {
                match action.as_deref() {
                    Some("folder") => open_folder_of(path),
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
fn play_request_sound() {
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
    fn kinds_match_whole_words_and_table_stays_small() {
        assert_eq!(error_kind("RAM insufficiente: il buffer copre 30 s"), "ram");
        assert_ne!(error_kind("la callback dei frame è andata in panic"), "ram");
        assert_ne!(error_kind("programma chiuso"), "ram");
        assert_eq!(error_kind("Video a schermo intero esclusivo"), "schermo intero");
        let mut d = Dedup::new(Duration::from_secs(60));
        let t0 = Instant::now();
        for i in 0..1000 {
            d.allow(&format!("avviso {i}"), t0 + Duration::from_secs(i));
        }
        assert!(d.last.len() <= 61);
    }

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

    #[test]
    fn m26_microphone_privacy_errors_get_the_settings_button() {
        let engine_text = "Windows nega l'accesso al microfono: apri Impostazioni → Privacy e sicurezza → Microfono \
             e attiva «Consenti alle app desktop di accedere al microfono»";
        assert_eq!(link_for(engine_text), Some(ToastLink::MicrophonePrivacy));
        assert_eq!(link_for("Microphone access denied by Windows privacy settings"), Some(ToastLink::MicrophonePrivacy));
        assert_eq!(link_for("RAM insufficiente"), None);
        assert_eq!(link_for("un'altra app usa il dispositivo in modalità esclusiva"), None);
        assert_eq!(ToastLink::MicrophonePrivacy.uri(), "ms-settings:privacy-microphone");
    }
}
