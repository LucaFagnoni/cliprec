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
    set_app_subtitle: "Impostazioni", "Settings";
    set_tab_video: "Clip video", "Video clips";
    set_tab_audio_clip: "Clip audio", "Audio clips";
    set_tab_audio: "Audio", "Audio";
    set_tab_general: "Generale", "General";
    set_tab_about: "Informazioni", "About";
    set_video_subtitle: "Cosa registrare, con quale qualità, con quale audio e con quali scorciatoie.", "What to record, at what quality, with which audio and which shortcuts.";
    set_audio_clip_subtitle: "Clip solo audio: durata, formato, tracce, sorgenti e scorciatoie.", "Audio-only clips: length, format, tracks, sources and shortcuts.";
    set_audio_subtitle: "Opzioni comuni a tutte le sorgenti audio.", "Options shared by all audio sources.";
    set_general_subtitle: "Cartella, avvio, notifiche, memoria e lingua.", "Folder, startup, notifications, memory and language.";
    set_about_subtitle: "Registratore replay sempre attivo per Windows.", "Always-on replay recorder for Windows.";
    set_sec_recording: "Registrazione", "Recording";
    set_sec_quality: "Qualità", "Quality";
    set_sec_capture: "Cattura", "Capture";
    set_sec_sources: "Sorgenti audio", "Audio sources";
    set_sec_apps: "App e dispositivi", "Apps and devices";
    set_sec_tracks_short: "Tracce", "Tracks";
    set_sec_length_files: "Durata e file", "Length and file";
    set_sec_length_format: "Durata e formato", "Length and format";
    set_sec_shortcuts: "Scorciatoie", "Shortcuts";
    set_sec_files: "File", "Files";
    set_sec_startup: "Avvio", "Startup";
    set_sec_notify: "Notifiche", "Notifications";
    set_sec_memory: "Memoria", "Memory";
    set_sec_performance: "Prestazioni", "Performance";
    set_sec_language: "Lingua", "Language";
    set_video_enabled: "Registra il video", "Record video";
    set_video_enabled_desc: "Spento: ClipRec registra solo l'audio.", "Off: ClipRec records audio only.";
    set_video_source: "Sorgente", "Source";
    set_video_source_desc: "Automatico segue il gioco in primo piano, altrimenti il monitor principale.", "Automatic follows the game in the foreground, otherwise the main monitor.";
    set_video_target: "Bersaglio", "Target";
    set_target_monitor_desc: "Numero del monitor (0 = principale).", "Monitor number (0 = main).";
    set_target_window_desc: "Titolo della finestra, o sceglila tra quelle aperte.", "Window title, or pick one of the open windows.";
    set_target_game_desc: "Eseguibile del gioco, o sceglilo tra le app aperte.", "Game executable, or pick it from the running apps.";
    set_video_codec: "Codec", "Codec";
    set_video_codec_desc: "Automatico sceglie il migliore che la scheda video supporta.", "Automatic picks the best one your graphics card supports.";
    set_video_resolution: "Risoluzione d'uscita", "Output resolution";
    set_video_resolution_desc: "Nativa è quella del monitor; una più bassa dà file più leggeri.", "Native is the monitor's; a lower one gives lighter files.";
    set_video_fps: "Fotogrammi al secondo", "Frames per second";
    set_video_quality: "Qualità", "Quality";
    set_video_quality_desc: "Più a sinistra: immagine migliore e file più grandi.", "Further left: better picture and larger files.";
    set_video_max_bitrate: "Tetto di bitrate", "Bitrate cap";
    set_video_max_bitrate_desc: "In kbit/s. Vuoto: nessun tetto.", "In kbit/s. Empty: no cap.";
    set_auto: "Automatico", "Automatic";
    set_native: "Nativa", "Native";
    set_src_monitor: "Monitor", "Monitor";
    set_src_window: "Finestra", "Window";
    set_src_game: "Gioco", "Game";
    set_pick: "Scegli…", "Pick…";
    set_sec_encoding: "Codifica avanzata", "Advanced encoding";
    set_color_format: "Formato colore", "Color format";
    set_color_format_desc: "4:4:4 elimina gli aloni colorati attorno al testo. Solo schede NVIDIA in H.264 o HEVC; alcuni lettori e siti non lo riproducono.", "4:4:4 removes colored halos around text. NVIDIA cards only, in H.264 or HEVC; some players and sites cannot play it.";
    set_fmt_420: "4:2:0 (compatibile)", "4:2:0 (compatible)";
    set_fmt_444: "4:4:4 (testo nitido)", "4:4:4 (sharp text)";
    set_rate_control: "Controllo del bitrate", "Rate control";
    set_rate_control_desc: "Qualità costante usa la qualità scelta sopra; VBR e CBR puntano a un bitrate.", "Constant quality uses the quality above; VBR and CBR aim at a bitrate.";
    set_rc_quality: "Qualità costante", "Constant quality";
    set_rc_vbr: "Bitrate variabile (VBR)", "Variable bitrate (VBR)";
    set_rc_cbr: "Bitrate costante (CBR)", "Constant bitrate (CBR)";
    set_target_bitrate: "Bitrate obiettivo", "Target bitrate";
    set_target_bitrate_desc: "In kbit/s, per VBR e CBR. Vuoto: due terzi del tetto.", "In kbit/s, for VBR and CBR. Empty: two thirds of the cap.";
    set_preset: "Preset dell'encoder", "Encoder preset";
    set_preset_desc: "Più qualità a parità di bitrate costa più lavoro alla scheda video.", "More quality at the same bitrate costs the graphics card more work.";
    set_preset_fastest: "Velocissimo", "Fastest";
    set_preset_fast: "Veloce", "Fast";
    set_preset_balanced: "Bilanciato", "Balanced";
    set_preset_quality: "Qualità", "Quality";
    set_preset_best: "Massima qualità", "Best quality";
    set_keyframe: "Intervallo dei fotogrammi chiave", "Keyframe interval";
    set_keyframe_desc: "Più lungo: file più piccoli, ma la clip può iniziare fino a questo intervallo prima.", "Longer: smaller files, but the clip may start up to this interval earlier.";
    set_b_frames: "B-frame", "B-frames";
    set_b_frames_desc: "Riducono il bitrate; con NVIDIA aumentano la memoria usata.", "They lower the bitrate; on NVIDIA they increase memory use.";
    set_lookahead: "Look-ahead", "Look-ahead";
    set_lookahead_desc: "Analizza i fotogrammi in anticipo per distribuire meglio i bit.", "Analyses frames ahead to spread the bits better.";
    set_aq: "Quantizzazione adattiva", "Adaptive quantization";
    set_aq_desc: "Più dettaglio dove l'occhio lo nota (ottimizzazione psicovisiva).", "More detail where the eye notices it (psycho-visual tuning).";
    set_audio_bitrate: "Bitrate audio", "Audio bitrate";
    set_audio_bitrate_desc: "AAC, per ogni traccia.", "AAC, per track.";
    set_capture_cursor: "Mostra il cursore del mouse", "Show the mouse cursor";
    set_any_fullscreen: "Qualsiasi app a schermo intero", "Any full-screen app";
    set_any_fullscreen_desc: "In automatico registra anche le app a schermo intero che non sono giochi riconosciuti.", "In automatic mode, also record full-screen apps that are not recognised games.";
    set_games: "Giochi riconosciuti", "Recognised games";
    set_games_hint: "Eseguibili separati da virgola", "Comma-separated executables";
    set_audio_mode: "Disposizione", "Layout";
    set_audio_mode_desc: "Separate: ogni sorgente nella sua traccia, da mixare dopo.", "Separate: each source on its own track, to mix later.";
    set_mode_separate: "Una traccia per sorgente", "One track per source";
    set_mode_merged: "Traccia unica (mix)", "Single track (mix)";
    set_add_mix: "Traccia «Mix» in testa", "Leading \"Mix\" track";
    set_add_mix_desc: "Aggiunge una prima traccia con la somma di tutte le sorgenti, comoda per i lettori che ne riproducono una sola.", "Adds a first track with all sources summed, handy for players that play only one.";
    set_process_events: "Aggancio immediato delle app", "Hook apps immediately";
    set_process_events_desc: "Usa gli eventi di Windows quando un'app si apre o si chiude. Serve ClipRec avviato come amministratore; altrimenti controlla ogni mezzo secondo.", "Uses Windows events when an app opens or closes. Needs ClipRec running as administrator; otherwise it checks every half second.";
    set_add_source: "Aggiungi sorgente", "Add source";
    set_add_router: "Aggiungi i canali del router", "Add the router channels";
    set_block_exclusive: "Blocca la modalità esclusiva", "Block exclusive mode";
    set_block_exclusive_desc: "Toglie ai dispositivi registrati il permesso di essere presi in esclusiva (Windows → Audio → Avanzate). Un gioco in modalità esclusiva scavalca il mixer e nessun programma può registrarlo; così passa in modalità condivisa.", "Removes the exclusive-control permission from the recorded devices (Windows → Sound → Advanced). A game in exclusive mode bypasses the mixer and no program can record it; this way it uses shared mode.";
    set_sources_hint: "In ordine di traccia, fino a 8. «Tutto tranne un'app» registra l'audio del PC senza quel programma, per esempio il gioco senza Discord. «Mono» unisce i canali di un microfono che si sente da un lato solo. Con SteelSeries Sonar, Voicemeeter o Wave Link «Aggiungi i canali del router» registra ogni canale (gioco, chat, media) in una traccia, senza il microfono.", "In track order, up to 8. \"Everything except an app\" records the PC audio without that program, for example the game without Discord. \"Mono\" merges the channels of a microphone heard on one side only. With SteelSeries Sonar, Voicemeeter or Wave Link \"Add the router channels\" records each channel (game, chat, media) on its own track, without the microphone.";
    set_kind_app: "App", "App";
    set_kind_input: "Microfono / ingresso", "Microphone / input";
    set_kind_output: "Uscita (ciò che senti)", "Output (what you hear)";
    set_kind_except: "Tutto tranne un'app", "Everything except an app";
    set_col_volume: "Volume", "Volume";
    set_col_delay: "Ritardo", "Delay";
    set_mono: "Mono", "Mono";
    set_pick_app: "Scegli un'app aperta…", "Pick a running app…";
    set_device_missing: "Non collegato", "Not connected";
    set_placeholder_name: "Nome della traccia", "Track name";
    set_placeholder_app: "Discord.exe", "Discord.exe";
    set_placeholder_device: "Predefinito di Windows", "Windows default";
    set_move_up: "Su", "Up";
    set_move_down: "Giù", "Down";
    set_remove: "Rimuovi", "Remove";
    set_hotkey_video: "Tastiera e mouse", "Keyboard and mouse";
    set_hotkey_video_desc: "Salva gli ultimi secondi di video e audio.", "Saves the last seconds of video and audio.";
    set_hotkey_audio: "Tastiera e mouse", "Keyboard and mouse";
    set_hotkey_audio_desc: "Salva solo l'audio, anche più lungo del video.", "Saves the audio only, even longer than the video.";
    set_hotkey_hint: "Clicca il campo e premi un tasto, da solo o con Ctrl, Alt, Shift o Win; vanno bene anche F13-F24 e i tasti laterali del mouse. Esc annulla.", "Click the field and press a key, alone or with Ctrl, Alt, Shift or Win; F13-F24 and the side mouse buttons work too. Esc cancels.";
    set_hotkey_press: "Premi la combinazione…", "Press the shortcut…";
    set_hotkey_clear: "Cancella", "Clear";
    set_hotkey_empty: "Nessuna", "None";
    set_pad_video: "Controller", "Controller";
    set_pad_audio: "Controller", "Controller";
    set_pad_hint: "Facoltativo. Tasti separati da +: A B X Y LB RB LS RS Back Start DPadUp DPadDown DPadLeft DPadRight.", "Optional. Buttons separated by +: A B X Y LB RB LS RS Back Start DPadUp DPadDown DPadLeft DPadRight.";
    set_clip_video_seconds: "Durata", "Length";
    set_clip_video_seconds_desc: "Secondi salvati dalla scorciatoia video.", "Seconds saved by the video shortcut.";
    set_clip_audio_seconds: "Durata", "Length";
    set_clip_audio_seconds_desc: "Secondi salvati dalla scorciatoia audio.", "Seconds saved by the audio shortcut.";
    set_seconds: "s", "s";
    set_folder: "Cartella", "Folder";
    set_folder_default: "Video\\ClipRec (predefinita)", "Videos\\ClipRec (default)";
    set_browse: "Sfoglia…", "Browse…";
    set_template: "Nome del file", "File name";
    set_template_hint: "Segnaposto: {game} {date} {time} {kind}", "Placeholders: {game} {date} {time} {kind}";
    set_container: "Formato video", "Video format";
    set_container_desc: "MP4 si apre ovunque; MKV regge meglio un'interruzione.", "MP4 opens everywhere; MKV copes better with an interruption.";
    set_audio_format: "Formato", "Format";
    set_fmt_flac: "FLAC, un file per traccia", "FLAC, one file per track";
    set_fmt_wav: "WAV, un file per traccia", "WAV, one file per track";
    set_fmt_mka: "MKA unico multitraccia", "Single multi-track MKA";
    set_fmt_polywav: "WAV polifonico unico", "Single polyphonic WAV";
    set_fmt_opus: "Opus, un file per traccia", "Opus, one file per track";
    set_fmt_aac: "AAC, un file per traccia", "AAC, one file per track";
    set_fmt_mp3: "MP3, un file per traccia", "MP3, one file per track";
    set_start_windows: "Avvia con Windows", "Start with Windows";
    set_start_windows_desc: "ClipRec parte all'accesso, nella barra delle applicazioni.", "ClipRec starts at sign-in, in the notification area.";
    set_start_admin: "Avvia come amministratore", "Start as administrator";
    set_start_admin_desc: "Serve per registrare i giochi avviati come amministratore e per l'aggancio immediato delle app. Windows chiede conferma quando salvi.", "Needed to record games running as administrator and to hook apps immediately. Windows asks for confirmation when you save.";
    set_notifications: "Notifiche", "Notifications";
    set_notifications_desc: "Avviso con i pulsanti Apri e Cartella dopo ogni clip.", "A notice with Open and Folder buttons after each clip.";
    set_sound: "Suono di conferma", "Confirmation sound";
    set_sound_desc: "Un suono breve appena premi la scorciatoia, prima che la clip sia scritta.", "A short sound as soon as you press the shortcut, before the clip is written.";
    set_ram_max: "Tetto di RAM", "RAM cap";
    set_ram_max_desc: "In MB, per i buffer. Vuoto: automatico.", "In MB, for the buffers. Empty: automatic.";
    set_ram_auto: "Automatico", "Automatic";
    set_ram_estimate: "Stima", "Estimate";
    set_lock_pages: "Blocca i buffer in RAM", "Lock the buffers in RAM";
    set_lock_pages_desc: "Windows non li sposta nel file di paging.", "Windows does not move them to the page file.";
    set_gpu_priority: "Priorità GPU", "GPU priority";
    set_gpu_priority_desc: "Alta evita fotogrammi persi quando il gioco carica la scheda video.", "High avoids dropped frames when the game loads the graphics card.";
    set_prio_high: "Alta", "High";
    set_prio_normal: "Normale", "Normal";
    set_language: "Lingua", "Language";
    set_lang_auto: "Automatica", "Automatic";
    set_about_text: "Il buffer vive solo in RAM: il disco si usa solo quando salvi una clip. Due scorciatoie, una per la clip video e una per la clip audio, e una traccia separata per ogni sorgente.", "The buffer lives in RAM only: the disk is used only when you save a clip. Two shortcuts, one for the video clip and one for the audio clip, and a separate track for each source.";
    set_version: "Versione", "Version";
    set_sec_updates: "Aggiornamenti", "Updates";
    set_check_now: "Cerca aggiornamenti", "Check for updates";
    set_check_updates: "Controlla automaticamente", "Check automatically";
    set_check_updates_desc: "All'avvio e ogni giorno ClipRec cerca una versione nuova e ti chiede se installarla.", "At startup and every day ClipRec looks for a new version and asks whether to install it.";
    set_save: "Salva", "Save";
    set_cancel: "Annulla", "Cancel";
    set_test: "Clip di prova", "Test clip";
    set_status_running: "In esecuzione", "Running";
    set_status_offline: "Non in esecuzione", "Not running";
}

/// Testo di un problema di validazione.
/// «delle clip video» / «delle clip audio» per i problemi di un elenco di sorgenti.
fn list_name(lang: Lang, list: u8) -> &'static str {
    match list {
        0 => lang.pick("delle clip video.", "of the video clips."),
        _ => lang.pick("delle clip audio.", "of the audio clips."),
    }
}

pub fn problem_text(lang: Lang, p: &Problem) -> String {
    let t = |it: &str, en: &str| lang.pick(it, en).to_string();
    match p {
        Problem::TooManySources(l) => format!("{} {}", t("Al massimo 8 sorgenti audio", "At most 8 audio sources"), list_name(lang, *l)),
        Problem::VideoSecondsZero => t("La durata della clip video deve essere maggiore di 0.", "The video clip length must be greater than 0."),
        Problem::AudioSecondsZero => t("La durata della clip audio deve essere maggiore di 0.", "The audio clip length must be greater than 0."),
        Problem::HotkeyEmpty(0) => t("Manca la hotkey della clip video.", "The video clip hotkey is missing."),
        Problem::HotkeyEmpty(_) => t("Manca la hotkey della clip audio.", "The audio clip hotkey is missing."),
        Problem::BadPad(_) => t("Combinazione del controller non valida: usa solo A B X Y LB RB LS RS Back Start DPadUp DPadDown DPadLeft DPadRight separati da +.", "Invalid controller combination: use only A B X Y LB RB LS RS Back Start DPadUp DPadDown DPadLeft DPadRight separated by +."),
        Problem::SamePad => t("Le due combinazioni del controller devono essere diverse.", "The two controller combinations must be different."),
        Problem::BadDelay(l, i) => format!("{} {} {}", t("Il ritardo deve stare tra -1000 e 1000 ms nella sorgente audio", "The delay must be between -1000 and 1000 ms in audio source"), i + 1, list_name(lang, *l)),
        Problem::SameHotkey => t("Le due hotkey devono essere diverse.", "The two hotkeys must be different."),
        Problem::BadTargetBitrate => t("Bitrate obiettivo non valido (numero intero in kbit/s, o vuoto).", "Invalid target bitrate (whole kbit/s, or empty)."),
        Problem::BadBitrate => t("Tetto di bitrate non valido (numero intero in kbit/s, o vuoto).", "Invalid bitrate cap (whole kbit/s, or empty)."),
        Problem::BadMaxMb => t("Tetto di RAM non valido (numero intero in MB, o vuoto).", "Invalid RAM cap (whole MB, or empty)."),
        Problem::BadMonitor => t("L'indice del monitor deve essere un numero (0 = principale).", "The monitor index must be a number (0 = primary)."),
        Problem::EmptyVideoTarget => t("Indica il titolo della finestra o l'eseguibile del gioco.", "Enter the window title or the game executable."),
        Problem::EmptySourceName(l, i) => format!("{} {} {}", t("Nome mancante nella sorgente audio", "Missing name in audio source"), i + 1, list_name(lang, *l)),
        Problem::EmptyAppExe(l, i) => format!("{} {} {}", t("Eseguibile mancante nella sorgente audio", "Missing executable in audio source"), i + 1, list_name(lang, *l)),
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
