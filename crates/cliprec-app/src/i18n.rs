//! Testi della shell in italiano e inglese: una tabella `(chiave, it, en)`, segnaposto `{0}`, `{1}`…
//! Anche gli avvisi del motore (`Eng*`); i messaggi interni dei crate restano in italiano.

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
    MenuRestartAdmin,
    RestartFailed,
    ConfigInvalid,
    ToastSaved,
    ToastAudioSaved,
    ToastFailed,
    ToastError,
    ToastOpen,
    ToastFolder,
    ToastOpenSettings,
    HotkeyVideo,
    HotkeyAudio,
    HotkeyConflict,
    HotkeyInvalid,
    ElevatedGame,
    SettingsMissing,
    AlreadyRunning,
    EngRamLow,
    EngNotStarted,
    EngVideoOnlyAudio,
    EngVideoRestarted,
    EngRamLimited,
    EngFolder,
    EngClipShortRam,
    EngClipVideoWaiting,
    EngVideoEmpty,
    EngAudioEmpty,
    EngClipStale,
    EngSaveInternal,
    EngSaveThread,
    EngEncoderError,
    EngEncoderFailed,
    EngEncoderChanged,
    EngNo444,
    EngStartFailed,
    EngBufferStopped,
    EngDeviceLost,
    EngVideoFallbackAudio,
    EngClipShort,
    EngTestVideoOk,
    EngTestVideoMissing,
    EngTestTrackOk,
    EngTestTrackSilent,
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
    K::MenuRestartAdmin,
    K::RestartFailed,
    K::ConfigInvalid,
    K::ToastSaved,
    K::ToastAudioSaved,
    K::ToastFailed,
    K::ToastError,
    K::ToastOpen,
    K::ToastFolder,
    K::ToastOpenSettings,
    K::HotkeyVideo,
    K::HotkeyAudio,
    K::HotkeyConflict,
    K::HotkeyInvalid,
    K::ElevatedGame,
    K::SettingsMissing,
    K::AlreadyRunning,
    K::EngRamLow,
    K::EngNotStarted,
    K::EngVideoOnlyAudio,
    K::EngVideoRestarted,
    K::EngRamLimited,
    K::EngFolder,
    K::EngClipShortRam,
    K::EngClipVideoWaiting,
    K::EngVideoEmpty,
    K::EngAudioEmpty,
    K::EngClipStale,
    K::EngSaveInternal,
    K::EngSaveThread,
    K::EngEncoderError,
    K::EngEncoderFailed,
    K::EngEncoderChanged,
    K::EngNo444,
    K::EngStartFailed,
    K::EngBufferStopped,
    K::EngDeviceLost,
    K::EngVideoFallbackAudio,
    K::EngClipShort,
    K::EngTestVideoOk,
    K::EngTestVideoMissing,
    K::EngTestTrackOk,
    K::EngTestTrackSilent,
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
    (K::MenuRestartAdmin, "Riavvia come amministratore", "Restart as administrator"),
    (K::RestartFailed, "Riavvio come amministratore non riuscito: {0}", "Could not restart as administrator: {0}"),
    (
        K::ConfigInvalid,
        "Impossibile leggere {0}: {1}. Il buffer resta fermo: correggi il file o riapri le impostazioni.",
        "Cannot read {0}: {1}. The buffer stays stopped: fix the file or reopen the settings.",
    ),
    (K::ToastSaved, "Clip salvata", "Clip saved"),
    (K::ToastAudioSaved, "Clip audio salvata", "Audio clip saved"),
    (K::ToastFailed, "Clip non salvata", "Clip not saved"),
    (K::ToastError, "ClipRec: errore", "ClipRec: error"),
    (K::ToastOpen, "Apri", "Open"),
    (K::ToastFolder, "Cartella", "Folder"),
    (K::ToastOpenSettings, "Apri impostazioni", "Open settings"),
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
    (K::EngRamLow, "RAM libera insufficiente: il buffer chiederebbe {0} MB, ne uso {1}. Riduci durata o qualità nelle impostazioni.", "Not enough free RAM: the buffer would need {0} MB, using {1}. Lower duration or quality in the settings."),
    (K::EngNotStarted, "{0} non avviato: {1}", "{0} not started: {1}"),
    (K::EngVideoOnlyAudio, "{0}; continuo solo con l'audio e riprovo tra poco", "{0}; continuing with audio only, retrying shortly"),
    (K::EngVideoRestarted, "video ripartito", "video restarted"),
    (K::EngRamLimited, "RAM insufficiente: il buffer copre solo {0} s di video", "Not enough RAM: the buffer covers only {0} s of video"),
    (K::EngFolder, "cartella {0}: {1}", "folder {0}: {1}"),
    (K::EngClipShortRam, "clip più corta di {0} s: la RAM del buffer copre solo {1} s", "clip shorter than {0} s: the buffer RAM covers only {1} s"),
    (K::EngClipVideoWaiting, "clip salvata, ma il video era in attesa ({0}: {1})", "clip saved, but video was waiting ({0}: {1})"),
    (K::EngVideoEmpty, "il buffer video è ancora vuoto", "the video buffer is still empty"),
    (K::EngAudioEmpty, "il buffer audio è ancora vuoto", "the audio buffer is still empty"),
    (K::EngClipStale, "la clip video finisce {0} s prima della pressione: cattura o encoder fermi", "the video clip ends {0} s before the key press: capture or encoder stalled"),
    (K::EngSaveInternal, "errore interno durante il salvataggio", "internal error while saving"),
    (K::EngSaveThread, "thread di salvataggio: {0}", "saving thread: {0}"),
    (K::EngEncoderError, "encoder video: {0}", "video encoder: {0}"),
    (K::EngEncoderFailed, "encoder {0} fallito ({1}): passo al successivo", "encoder {0} failed ({1}): trying the next one"),
    (K::EngEncoderChanged, "encoder cambiato da {0} a {1}: il buffer video riparte da zero", "encoder changed from {0} to {1}: the video buffer starts over"),
    (K::EngNo444, "il colore 4:4:4 richiede NVENC in H.264 o HEVC: {0} registra in 4:2:0", "4:4:4 colour needs NVENC in H.264 or HEVC: {0} records in 4:2:0"),
    (K::EngStartFailed, "avvio del buffer: {0}", "starting the buffer: {0}"),
    (K::EngBufferStopped, "il buffer è fermo: avvialo dal menu della tray", "the buffer is stopped: start it from the tray menu"),
    (K::EngDeviceLost, "device GPU perso: riavvio cattura ed encoder", "GPU device lost: restarting capture and encoder"),
    (K::EngVideoFallbackAudio, "nessun video nel buffer (encoder non pronto): salvo la clip audio", "no video in the buffer (encoder not ready): saving the audio clip"),
    (K::EngClipShort, "clip di {0} s invece di {1} s: il buffer non copriva ancora tutta la durata", "clip of {0} s instead of {1} s: the buffer did not cover the full duration yet"),
    (K::EngTestVideoOk, "video ok ({0})", "video ok ({0})"),
    (K::EngTestVideoMissing, "video assente, nero o fermo", "video missing, black or frozen"),
    (K::EngTestTrackOk, "{0}: audio ok", "{0}: audio ok"),
    (K::EngTestTrackSilent, "{0}: silenzio", "{0}: silent"),
];

/// Testo grezzo, con i segnaposto.
pub fn raw(lang: Lang, key: K) -> &'static str {
    TABLE.iter().find(|(k, ..)| *k == key).map_or("", |&(_, it, en)| if lang == Lang::It { it } else { en })
}

/// Testo nella lingua corrente con `{0}`, `{1}`… sostituiti da `args`.
pub fn tr(key: K, args: &[&str]) -> String {
    fmt(current(), key, args)
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
