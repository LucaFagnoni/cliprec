//! Finestra impostazioni di ClipRec (Slint, processo separato lanciato dalla tray).
//!
//! Variabili d'ambiente per le prove: `CLIPREC_CONFIG` (percorso di config.toml al posto di
//! `Config::default_path()`), `CLIPREC_PIPE` (nome della pipe), `CLIPREC_TAB` (scheda iniziale, 0-5), `CLIPREC_SMOKE_MS` (chiude la
//! finestra da sola dopo N millisecondi).
#![windows_subsystem = "windows"]

mod hotkey;
mod i18n;
mod model;
mod pipe;
mod sys;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use cliprec_core::Config;
use cliprec_core::ipc::{IpcRequest, IpcResponse};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use i18n::Lang;
use sys::Choice;

// Il codice generato da Slint non rispetta i lint del progetto (unwrap, unsafe senza SAFETY).
#[allow(clippy::all, clippy::pedantic, clippy::unwrap_used, clippy::expect_used, clippy::undocumented_unsafe_blocks)]
mod generated {
    slint::include_modules!();
}
use generated::*;

/// Lingua corrente per il thread dello stato (`true` = inglese).
static LANG_EN: AtomicBool = AtomicBool::new(false);
/// Millisecondi dall'avvio dell'ultimo tentativo di stato concluso (con o senza risposta).
static LAST_POLL_MS: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<Instant> = OnceLock::new();

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn current_lang() -> Lang {
    if LANG_EN.load(Ordering::Relaxed) { Lang::En } else { Lang::It }
}

fn config_path() -> PathBuf {
    std::env::var_os("CLIPREC_CONFIG").map(PathBuf::from).unwrap_or_else(Config::default_path)
}

fn labels(list: &[Choice]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(list.iter().map(|c| SharedString::from(c.label.as_str())).collect::<Vec<_>>()))
}

/// Liste d'aiuto lette una volta all'avvio.
struct Lists {
    monitors: Vec<Choice>,
    windows: Vec<Choice>,
    apps: Vec<Choice>,
    inputs: Vec<Choice>,
    outputs: Vec<Choice>,
}

fn set_message(ui: &SettingsWindow, text: &str, error: bool) {
    ui.set_message(text.into());
    ui.set_message_error(error);
}

fn set_lang(ui: &SettingsWindow, lang: Lang) {
    LANG_EN.store(lang == Lang::En, Ordering::Relaxed);
    i18n::apply(ui, lang);
}

fn current_sources(rows: &VecModel<SourceRow>) -> Vec<SourceRow> {
    rows.iter().collect()
}

/// Testo della stima di RAM con l'avviso se supera il tetto scelto.
fn ram_text(lang: Lang, c: &Config) -> String {
    let est = model::estimate_ram_mb(c);
    let mut t = format!("≈ {est:.0} MB");
    if let Some(max) = c.memory.max_mb {
        t.push_str(&format!(" ({} {max} MB)", lang.pick("tetto", "cap")));
        if est > f64::from(max) {
            t.push_str(lang.pick(" · oltre il tetto: i buffer copriranno meno secondi", " · over the cap: buffers will cover fewer seconds"));
        }
    }
    t
}

fn hotkey_warning(lang: Lang, c: &Config) -> String {
    let mut parts = Vec::new();
    for (label, key) in [(lang.pick("Clip video", "Video clip"), &c.hotkeys.video), (lang.pick("Clip audio", "Audio clip"), &c.hotkeys.audio)] {
        if let Some(who) = hotkey::conflict(key) {
            parts.push(format!(
                "{label}: {key} {} {who}.",
                lang.pick("è usata anche da", "is also used by")
            ));
        }
    }
    parts.join("\n")
}

fn main() -> anyhow::Result<()> {
    // Renderer software: niente driver GPU nel processo, quindi meno RAM. Se non disponibile
    // resta la scelta predefinita di Slint.
    let _ = slint::BackendSelector::new().renderer_name("software".into()).select();
    sys::init_com();
    let _ = now_ms();

    let path = config_path();
    let (base, load_error) = match Config::load(&path) {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e.to_string())),
    };

    let ui = SettingsWindow::new()?;
    let (form, rows_vec) = model::to_form(&base);
    let lang = Lang::resolve(form.language);
    set_lang(&ui, lang);
    ui.set_f(form);
    let rows = Rc::new(VecModel::from(rows_vec));
    ui.set_sources(ModelRc::from(rows.clone()));
    if let Some(e) = load_error {
        set_message(&ui, &e, true);
    }

    let lists = Rc::new(Lists {
        monitors: sys::monitors(),
        windows: sys::window_titles(),
        apps: sys::running_apps(),
        inputs: sys::input_devices(),
        outputs: sys::output_devices(),
    });
    ui.set_monitors(labels(&lists.monitors));
    ui.set_windows_list(labels(&lists.windows));
    ui.set_apps(labels(&lists.apps));
    ui.set_inputs(labels(&lists.inputs));
    ui.set_outputs(labels(&lists.outputs));

    let base = Rc::new(RefCell::new(base));

    ui.on_language_changed({
        let w = ui.as_weak();
        move || {
            if let Some(ui) = w.upgrade() {
                set_lang(&ui, Lang::resolve(ui.get_f().language));
            }
        }
    });

    ui.on_source_changed({
        let rows = rows.clone();
        move |i, row| {
            if let Ok(i) = usize::try_from(i) {
                rows.set_row_data(i, row);
            }
        }
    });
    ui.on_source_add({
        let rows = rows.clone();
        move || {
            if rows.row_count() < cliprec_core::config::MAX_AUDIO_SOURCES {
                rows.push(SourceRow { name: "".into(), kind: 0, target: "".into(), volume: 1.0 });
            }
        }
    });
    ui.on_source_remove({
        let rows = rows.clone();
        move |i| {
            if let Ok(i) = usize::try_from(i)
                && i < rows.row_count()
            {
                rows.remove(i);
            }
        }
    });
    ui.on_source_move({
        let rows = rows.clone();
        move |i, delta| {
            let (Ok(a), Ok(b)) = (usize::try_from(i), usize::try_from(i + delta)) else { return };
            if let (Some(ra), Some(rb)) = (rows.row_data(a), rows.row_data(b)) {
                rows.set_row_data(a, rb);
                rows.set_row_data(b, ra);
            }
        }
    });
    ui.on_source_pick({
        let rows = rows.clone();
        let lists = lists.clone();
        move |i, kind, idx| {
            let list = match kind {
                0 => &lists.apps,
                1 => &lists.inputs,
                _ => &lists.outputs,
            };
            let (Ok(i), Ok(idx)) = (usize::try_from(i), usize::try_from(idx)) else { return };
            let (Some(mut row), Some(choice)) = (rows.row_data(i), list.get(idx)) else { return };
            row.target = choice.value.as_str().into();
            if row.name.trim().is_empty() {
                let stem = choice.label.strip_suffix(".exe").unwrap_or(&choice.label);
                row.name = stem.into();
            }
            rows.set_row_data(i, row);
        }
    });
    ui.on_video_target_pick({
        let w = ui.as_weak();
        let lists = lists.clone();
        move |idx| {
            let Some(ui) = w.upgrade() else { return };
            let mut f = ui.get_f();
            let list = match f.source_kind {
                1 => &lists.monitors,
                2 => &lists.windows,
                _ => &lists.apps,
            };
            if let Some(c) = usize::try_from(idx).ok().and_then(|i| list.get(i)) {
                f.source_target = c.value.as_str().into();
                ui.set_f(f);
            }
        }
    });
    ui.on_browse_folder({
        let w = ui.as_weak();
        move || {
            if let (Some(ui), Some(dir)) = (w.upgrade(), sys::pick_folder()) {
                let mut f = ui.get_f();
                f.folder = dir.into();
                ui.set_f(f);
            }
        }
    });

    let capture = |which: i32, key: Option<String>, ui: &SettingsWindow| {
        if let Some(k) = key {
            let mut f = ui.get_f();
            if which == 0 {
                f.hotkey_video = k.into();
            } else {
                f.hotkey_audio = k.into();
            }
            ui.set_f(f);
        }
    };
    ui.on_hotkey_key({
        let w = ui.as_weak();
        move |which, text, ctrl, alt, shift, meta| {
            if let Some(ui) = w.upgrade() {
                capture(which, hotkey::from_key(&text, hotkey::Mods { ctrl, alt, shift, meta }), &ui);
            }
        }
    });
    ui.on_hotkey_mouse({
        let w = ui.as_weak();
        move |which, button, ctrl, alt, shift, meta| {
            if let Some(ui) = w.upgrade() {
                capture(which, hotkey::from_mouse(button, hotkey::Mods { ctrl, alt, shift, meta }), &ui);
            }
        }
    });

    ui.on_save({
        let w = ui.as_weak();
        let rows = rows.clone();
        let base = base.clone();
        let path = path.clone();
        move || {
            let Some(ui) = w.upgrade() else { return };
            let lang = current_lang();
            let cfg = match model::build_config(&ui.get_f(), &current_sources(&rows), &base.borrow()) {
                Ok(c) => c,
                Err(problems) => {
                    let text: Vec<String> = problems.iter().map(|p| i18n::problem_text(lang, p)).collect();
                    set_message(&ui, &text.join(" "), true);
                    return;
                }
            };
            if let Err(e) = cfg.save(&path) {
                set_message(&ui, &e.to_string(), true);
                return;
            }
            *base.borrow_mut() = cfg;
            set_message(&ui, lang.pick("Salvato.", "Saved."), false);
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                let applied = matches!(pipe::request(&IpcRequest::ReloadConfig), Ok(IpcResponse::Ok));
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    let lang = current_lang();
                    let text = if applied {
                        lang.pick("Salvato e applicato.", "Saved and applied.")
                    } else {
                        lang.pick("Salvato. ClipRec non è in esecuzione: leggerà le impostazioni al prossimo avvio.", "Saved. ClipRec is not running: it will read the settings on next start.")
                    };
                    set_message(&ui, text, false);
                });
            });
        }
    });
    ui.on_cancel({
        let w = ui.as_weak();
        move || {
            if let Some(ui) = w.upgrade() {
                let _ = ui.hide();
            }
        }
    });
    ui.on_test_clip({
        let w = ui.as_weak();
        move || {
            let weak = w.clone();
            std::thread::spawn(move || {
                let res = pipe::request(&IpcRequest::TestClip);
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    let lang = current_lang();
                    match res {
                        Ok(IpcResponse::Error(e)) => set_message(&ui, &e, true),
                        Ok(_) => set_message(&ui, lang.pick("Clip di prova richiesta.", "Test clip requested."), false),
                        Err(_) => set_message(&ui, pipe::offline_text(lang), true),
                    }
                });
            });
        }
    });

    // Stima della RAM e avvisi sulle hotkey, ricalcolati mentre l'utente modifica i campi.
    let tick = Timer::default();
    tick.start(TimerMode::Repeated, Duration::from_millis(400), {
        let w = ui.as_weak();
        let rows = rows.clone();
        let base = base.clone();
        move || {
            let Some(ui) = w.upgrade() else { return };
            let lang = current_lang();
            let (c, _) = model::build_lenient(&ui.get_f(), &current_sources(&rows), &base.borrow());
            ui.set_ram_estimate(ram_text(lang, &c).into());
            ui.set_hotkey_warning(hotkey_warning(lang, &c).into());
            // Il thread di stato è fermo (core che non risponde): mostra «non in esecuzione».
            if now_ms().saturating_sub(LAST_POLL_MS.load(Ordering::Relaxed)) > 4000 {
                ui.set_status_online(false);
                ui.set_status_text(pipe::offline_text(lang).into());
            }
        }
    });

    // Pannello di stato: una richiesta Status al secondo su un thread, mai sul thread della UI.
    LAST_POLL_MS.store(now_ms(), Ordering::Relaxed);
    ui.set_status_text(pipe::offline_text(lang).into());
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        loop {
            let res = pipe::request(&IpcRequest::Status);
            LAST_POLL_MS.store(now_ms(), Ordering::Relaxed);
            let alive = weak
                .upgrade_in_event_loop(move |ui| {
                    let lang = current_lang();
                    match res {
                        Ok(IpcResponse::Status(s)) => {
                            ui.set_status_text(pipe::format_status(lang, &s).into());
                            ui.set_status_online(true);
                        }
                        _ => {
                            ui.set_status_text(pipe::offline_text(lang).into());
                            ui.set_status_online(false);
                        }
                    }
                })
                .is_ok();
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });

    if let Some(tab) = std::env::var("CLIPREC_TAB").ok().and_then(|v| v.parse::<i32>().ok()) {
        ui.set_start_tab(tab);
    }
    let smoke = Timer::default();
    if let Some(ms) = std::env::var("CLIPREC_SMOKE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        smoke.start(TimerMode::SingleShot, Duration::from_millis(ms), || {
            let _ = slint::quit_event_loop();
        });
    }

    ui.run()?;
    Ok(())
}
