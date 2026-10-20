//! Testi della shell in italiano e inglese: una tabella `(chiave, it, en)`, segnaposto `{0}`, `{1}`…
//! Gli avvisi che arrivano dal motore restano come sono.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    It,
    En,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum K {
    StatusStopped,
    StatusVideo,
    StatusAudio,
    MenuStop,
    MenuStart,
    MenuRecent,
    MenuNoClips,
    MenuFolder,
    MenuSettings,
    MenuQuit,
    ToastSaved,
    ToastAudioSaved,
    ToastFailed,
    ToastError,
    ToastOpen,
    ToastFolder,
    HotkeyVideo,
    HotkeyAudio,
    HotkeyConflict,
    HotkeyInvalid,
    ElevatedGame,
    SettingsMissing,
    AlreadyRunning,
}

#[cfg(test)]
pub const ALL: &[K] = &[
    K::StatusStopped,
    K::StatusVideo,
    K::StatusAudio,
    K::MenuStop,
    K::MenuStart,
    K::MenuRecent,
    K::MenuNoClips,
    K::MenuFolder,
    K::MenuSettings,
    K::MenuQuit,
    K::ToastSaved,
    K::ToastAudioSaved,
    K::ToastFailed,
    K::ToastError,
    K::ToastOpen,
    K::ToastFolder,
    K::HotkeyVideo,
    K::HotkeyAudio,
    K::HotkeyConflict,
    K::HotkeyInvalid,
    K::ElevatedGame,
    K::SettingsMissing,
    K::AlreadyRunning,
];

const TABLE: &[(K, &str, &str)] = &[
    (K::StatusStopped, "Buffer fermo", "Buffer stopped"),
    (K::StatusVideo, "Buffer: {0} s video · {1} s audio · {2} MB", "Buffer: {0} s video · {1} s audio · {2} MB"),
    (K::StatusAudio, "Buffer: {0} s audio · {1} MB", "Buffer: {0} s audio · {1} MB"),
    (K::MenuStop, "Ferma buffer", "Stop buffer"),
    (K::MenuStart, "Avvia buffer", "Start buffer"),
    (K::MenuRecent, "Ultime clip", "Recent clips"),
    (K::MenuNoClips, "Nessuna clip", "No clips yet"),
    (K::MenuFolder, "Apri cartella clip", "Open clips folder"),
    (K::MenuSettings, "Impostazioni…", "Settings…"),
    (K::MenuQuit, "Esci", "Quit"),
    (K::ToastSaved, "Clip salvata", "Clip saved"),
    (K::ToastAudioSaved, "Clip audio salvata", "Audio clip saved"),
    (K::ToastFailed, "Clip non salvata", "Clip not saved"),
    (K::ToastError, "ClipRec: errore", "ClipRec: error"),
    (K::ToastOpen, "Apri", "Open"),
    (K::ToastFolder, "Cartella", "Folder"),
    (K::HotkeyVideo, "Clip video", "Video clip"),
    (K::HotkeyAudio, "Clip audio", "Audio clip"),
    (K::HotkeyConflict, "La hotkey {0} «{1}» coincide con {2}", "The {0} hotkey \"{1}\" is also used by {2}"),
    (K::HotkeyInvalid, "Hotkey {0} «{1}» non valida: {2}", "Invalid {0} hotkey \"{1}\": {2}"),
    (
        K::ElevatedGame,
        "{0} è in esecuzione come amministratore: le hotkey potrebbero non arrivare. Riavvia ClipRec come amministratore.",
        "{0} is running as administrator: hotkeys may not reach ClipRec. Restart ClipRec as administrator.",
    ),
    (K::SettingsMissing, "cliprec-settings.exe non trovato accanto a ClipRec", "cliprec-settings.exe not found next to ClipRec"),
    (K::AlreadyRunning, "ClipRec è già in esecuzione", "ClipRec is already running"),
];

/// Testo grezzo, con i segnaposto.
pub fn raw(lang: Lang, key: K) -> &'static str {
    TABLE.iter().find(|(k, ..)| *k == key).map_or("", |&(_, it, en)| if lang == Lang::It { it } else { en })
}

/// Testo con `{0}`, `{1}`… sostituiti da `args`.
pub fn fmt(lang: Lang, key: K, args: &[&str]) -> String {
    let mut s = raw(lang, key).to_owned();
    for (i, a) in args.iter().enumerate() {
        s = s.replace(&format!("{{{i}}}"), a);
    }
    s
}

/// «it», «en» o «auto»; con «auto» decide la lingua primaria della UI di Windows (0x10 = italiano).
pub fn resolve(setting: &str, ui_language_id: u16) -> Lang {
    match setting.trim().to_ascii_lowercase().as_str() {
        "it" => Lang::It,
        "en" => Lang::En,
        _ if ui_language_id & 0x3FF == 0x10 => Lang::It,
        _ => Lang::En,
    }
}

fn system_ui_language() -> u16 {
    // SAFETY: nessuna precondizione.
    unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Imposta la lingua corrente da `config.general.language`.
pub fn set_from_config(setting: &str) {
    let l = resolve(setting, system_ui_language());
    CURRENT.store(u8::from(l == Lang::En), Ordering::Relaxed);
}

pub fn current() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == 1 { Lang::En } else { Lang::It }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_choice() {
        assert_eq!(resolve("it", 0x0409), Lang::It);
        assert_eq!(resolve("en", 0x0410), Lang::En);
        assert_eq!(resolve("auto", 0x0410), Lang::It, "italiano (Italia)");
        assert_eq!(resolve("auto", 0x0810), Lang::It, "italiano (Svizzera): stessa lingua primaria");
        assert_eq!(resolve("auto", 0x0409), Lang::En);
        assert_eq!(resolve("auto", 0x0407), Lang::En, "tedesco");
        assert_eq!(resolve("klingon", 0x0409), Lang::En, "valore sconosciuto = auto");
    }

    #[test]
    fn every_key_has_both_languages_exactly_once() {
        for &k in ALL {
            assert_eq!(TABLE.iter().filter(|(t, ..)| *t == k).count(), 1, "{k:?}");
            assert!(!raw(Lang::It, k).is_empty() && !raw(Lang::En, k).is_empty(), "{k:?}");
        }
        assert_eq!(ALL.len(), TABLE.len());
    }

    #[test]
    fn placeholders_match_between_languages() {
        for &k in ALL {
            for i in 0..4 {
                let p = format!("{{{i}}}");
                assert_eq!(raw(Lang::It, k).contains(&p), raw(Lang::En, k).contains(&p), "{k:?} {p}");
            }
        }
        assert_eq!(fmt(Lang::En, K::StatusAudio, &["120", "412"]), "Buffer: 120 s audio · 412 MB");
    }
}
