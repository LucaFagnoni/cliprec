//! Tabella di traduzione italiano/inglese (una tabella semplice al posto di gettext: nessun file
//! `.mo` da distribuire). Ogni riga: setter di `global T` nello Slint, testo italiano, testo inglese.

use crate::model::Problem;
use crate::{SettingsWindow, T};
use slint::ComponentHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    It,
    En,
}

impl Lang {
    /// `choice`: 0 = automatica (lingua di Windows), 1 = italiano, 2 = inglese.
    pub fn resolve(choice: i32) -> Self {
        match choice {
            1 => Self::It,
            2 => Self::En,
            _ => system_lang(),
        }
    }

    pub fn pick<'a>(self, it: &'a str, en: &'a str) -> &'a str {
        match self {
            Self::It => it,
            Self::En => en,
        }
    }
}

fn system_lang() -> Lang {
    let mut buf = [0u16; 85];
    // SAFETY: il buffer è valido per 85 u16 (LOCALE_NAME_MAX_LENGTH) e la lunghezza passata coincide.
    let n = unsafe { windows::Win32::Globalization::GetUserDefaultLocaleName(&mut buf) };
    let len = usize::try_from(n).unwrap_or(0).min(buf.len());
    let name = String::from_utf16_lossy(&buf[..len]);
    if name.to_ascii_lowercase().starts_with("it") { Lang::It } else { Lang::En }
}

macro_rules! strings {
    ($($setter:ident : $it:expr, $en:expr;)*) => {
        /// Scrive tutte le stringhe nella finestra nella lingua scelta.
        pub fn apply(ui: &SettingsWindow, lang: Lang) {
            let t = ui.global::<T>();
            $( t.$setter(lang.pick($it, $en).into()); )*
        }
        #[cfg(test)]
        pub const ALL: &[(&str, &str)] = &[$(($it, $en)),*];
    };
}

strings! {
    set_window_title: "ClipRec · Impostazioni", "ClipRec · Settings";
    set_tab_video: "Video", "Video";
    set_tab_audio: "Audio", "Audio";
    set_tab_hotkeys: "Hotkey", "Hotkeys";
    set_tab_saving: "Salvataggio", "Saving";
    set_tab_advanced: "Avanzate", "Advanced";
    set_tab_about: "Informazioni", "About";
    set_video_enabled: "Registra il video (se disattivo: solo audio)", "Record video (off: audio only)";
    set_video_source: "Sorgente video", "Video source";
    set_video_target: "Indice, titolo o eseguibile", "Index, title or executable";
    set_video_codec: "Codec", "Codec";
    set_video_resolution: "Risoluzione d'uscita", "Output resolution";
    set_video_fps: "Fotogrammi al secondo", "Frames per second";
    set_video_quality: "Qualità (più bassa = migliore)", "Quality (lower = better)";
    set_video_max_bitrate: "Tetto di bitrate (kbit/s)", "Bitrate cap (kbit/s)";
    set_auto: "Automatico", "Automatic";
    set_native: "Nativa", "Native";
    set_src_monitor: "Monitor", "Monitor";
    set_src_window: "Finestra", "Window";
    set_src_game: "Gioco (eseguibile)", "Game (executable)";
    set_pick: "Scegli…", "Pick…";
    set_audio_mode: "Tracce audio", "Audio tracks";
    set_mode_separate: "Una traccia per sorgente", "One track per source";
    set_mode_merged: "Traccia unica (mix)", "Single track (mix)";
    set_add_mix: "Aggiungi in testa una traccia «Mix»", "Add a leading \"Mix\" track";
    set_add_source: "Aggiungi sorgente", "Add source";
    set_sources_hint: "Fino a 8 sorgenti, nell'ordine delle tracce. Volume 100% = invariato.", "Up to 8 sources, in track order. Volume 100% = unchanged.";
    set_kind_app: "App", "App";
    set_kind_input: "Ingresso", "Input";
    set_kind_output: "Uscita", "Output";
    set_placeholder_name: "Nome traccia", "Track name";
    set_placeholder_app: "Discord.exe", "Discord.exe";
    set_placeholder_device: "(predefinito di Windows)", "(Windows default)";
    set_hotkey_video: "Clip video", "Video clip";
    set_hotkey_audio: "Clip audio", "Audio clip";
    set_pad_video: "Controller · clip video (facoltativo)", "Controller · video clip (optional)";
    set_pad_audio: "Controller · clip audio (facoltativo)", "Controller · audio clip (optional)";
    set_pad_hint: "Tasti separati da +: A B X Y LB RB LS RS Back Start DPadUp DPadDown DPadLeft DPadRight (es. Back+A). Vuoto = nessun controller.", "Buttons separated by +: A B X Y LB RB LS RS Back Start DPadUp DPadDown DPadLeft DPadRight (e.g. Back+A). Empty = no controller.";
    set_hotkey_press: "Premi la combinazione…", "Press the combination…";
    set_hotkey_clear: "Cancella", "Clear";
    set_hotkey_hint: "Clicca il campo e premi la combinazione (anche i tasti laterali del mouse).", "Click the field and press the combination (side mouse buttons work too).";
    set_clip_video_seconds: "Durata clip video (s)", "Video clip length (s)";
    set_clip_audio_seconds: "Durata clip audio (s)", "Audio clip length (s)";
    set_folder: "Cartella delle clip", "Clips folder";
    set_folder_default: "Video\\ClipRec (predefinita)", "Videos\\ClipRec (default)";
    set_browse: "Sfoglia…", "Browse…";
    set_template: "Nome del file", "File name";
    set_template_hint: "Segnaposto: {game} {date} {time} {kind}", "Placeholders: {game} {date} {time} {kind}";
    set_audio_format: "Formato clip audio", "Audio clip format";
    set_fmt_flac: "FLAC, un file per traccia", "FLAC, one file per track";
    set_fmt_wav: "WAV, un file per traccia", "WAV, one file per track";
    set_fmt_mka: "MKA unico multitraccia", "Single multi-track MKA";
    set_container: "Contenitore video", "Video container";
    set_ram_max: "Tetto di RAM dei buffer (MB)", "Buffer RAM cap (MB)";
    set_ram_auto: "Automatico (stima)", "Automatic (estimate)";
    set_ram_estimate: "Stima dal vivo", "Live estimate";
    set_lock_pages: "Blocca le pagine dei buffer in RAM", "Lock buffer pages in RAM";
    set_gpu_priority: "Priorità GPU", "GPU priority";
    set_prio_high: "Alta", "High";
    set_prio_normal: "Normale", "Normal";
    set_language: "Lingua", "Language";
    set_lang_auto: "Automatica", "Automatic";
    set_start_windows: "Avvia ClipRec con Windows", "Start ClipRec with Windows";
    set_sound: "Suono al salvataggio", "Sound on save";
    set_notifications: "Notifiche", "Notifications";
    set_capture_cursor: "Cattura il cursore del mouse", "Capture the mouse cursor";
    set_any_fullscreen: "In automatico cattura qualsiasi app a schermo intero", "In automatic mode capture any full-screen app";
    set_games: "Giochi riconosciuti", "Recognised games";
    set_games_hint: "Eseguibili separati da virgola", "Comma-separated executables";
    set_about_text: "ClipRec · registratore replay sempre attivo per Windows.\nIl buffer vive solo in RAM; il disco si tocca solo quando salvi una clip.", "ClipRec · always-on replay recorder for Windows.\nThe buffer lives in RAM only; the disk is touched only when you save a clip.";
    set_save: "Salva", "Save";
    set_cancel: "Annulla", "Cancel";
    set_test: "Prova", "Test";
    set_status_title: "Stato", "Status";
}

/// Testo di un problema di validazione.
pub fn problem_text(lang: Lang, p: &Problem) -> String {
    let t = |it: &str, en: &str| lang.pick(it, en).to_string();
    match p {
        Problem::TooManySources => t("Al massimo 8 sorgenti audio.", "At most 8 audio sources."),
        Problem::VideoSecondsZero => t("La durata della clip video deve essere maggiore di 0.", "The video clip length must be greater than 0."),
        Problem::AudioSecondsZero => t("La durata della clip audio deve essere maggiore di 0.", "The audio clip length must be greater than 0."),
        Problem::HotkeyEmpty(0) => t("Manca la hotkey della clip video.", "The video clip hotkey is missing."),
        Problem::HotkeyEmpty(_) => t("Manca la hotkey della clip audio.", "The audio clip hotkey is missing."),
        Problem::BadPad(_) => t("Combinazione del controller non valida: usa solo A B X Y LB RB LS RS Back Start DPadUp DPadDown DPadLeft DPadRight separati da +.", "Invalid controller combination: use only A B X Y LB RB LS RS Back Start DPadUp DPadDown DPadLeft DPadRight separated by +."),
        Problem::SamePad => t("Le due combinazioni del controller devono essere diverse.", "The two controller combinations must be different."),
        Problem::SameHotkey => t("Le due hotkey devono essere diverse.", "The two hotkeys must be different."),
        Problem::BadBitrate => t("Tetto di bitrate non valido (numero intero in kbit/s, o vuoto).", "Invalid bitrate cap (whole kbit/s, or empty)."),
        Problem::BadMaxMb => t("Tetto di RAM non valido (numero intero in MB, o vuoto).", "Invalid RAM cap (whole MB, or empty)."),
        Problem::BadMonitor => t("L'indice del monitor deve essere un numero (0 = principale).", "The monitor index must be a number (0 = primary)."),
        Problem::EmptyVideoTarget => t("Indica il titolo della finestra o l'eseguibile del gioco.", "Enter the window title or the game executable."),
        Problem::EmptySourceName(i) => format!("{} {}", t("Nome mancante nella sorgente audio", "Missing name in audio source"), i + 1),
        Problem::EmptyAppExe(i) => format!("{} {}", t("Eseguibile mancante nella sorgente audio", "Missing executable in audio source"), i + 1),
        Problem::EmptyTemplate => t("Il nome del file non può essere vuoto.", "The file name cannot be empty."),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_string_is_translated() {
        assert!(super::ALL.len() > 60);
        for (it, en) in super::ALL {
            assert!(!it.is_empty() && !en.is_empty());
        }
    }
}
