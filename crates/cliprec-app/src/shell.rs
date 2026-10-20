//! Shell Win32: finestra nascosta, Raw Input per le hotkey, tray con menu, notifiche, IPC.
//! Gira tutta sul thread principale; gli eventi del motore arrivano da un thread di appoggio
//! che sveglia il loop con un messaggio.

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use cliprec_core::ipc::pipe_name;
use cliprec_core::{AppCommand, Config, EngineEvent, EngineStatus};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIcon, TrayIconBuilder};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, KillTimer, MSG, PostMessageW, PostQuitMessage,
    RegisterClassExW, SetTimer, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_INPUT, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPED,
};
use windows::core::w;

use crate::hotkey::{self, Chord, Hotkeys, InputEvent, Trigger};
use crate::i18n::{self, K, Lang, fmt};
use crate::icon::{self, IconState};
use crate::notify::{self, Notifier};
use crate::rawinput;
use crate::supervisor::spawn_supervised;

/// Tutto ciò che la shell riceve all'avvio.
pub struct ShellIo {
    pub commands: Sender<AppCommand>,
    pub events: Receiver<EngineEvent>,
    pub status: Arc<Mutex<EngineStatus>>,
    pub config: Config,
}

const WM_REFRESH: u32 = WM_APP + 1;
const WM_RELOAD: u32 = WM_APP + 2;
/// Chiusura richiesta via IPC: il motore ha già ricevuto `Quit`, qui esce il loop.
const WM_QUIT_REQUEST: u32 = WM_APP + 3;
const TIMER_ID: usize = 1;
const RECENT_MAX: usize = 5;

/// `CLIPREC_CONFIG` se impostata, altrimenti il percorso predefinito.
pub fn config_path() -> PathBuf {
    std::env::var_os("CLIPREC_CONFIG").map_or_else(Config::default_path, PathBuf::from)
}

/// Testo della voce di stato: secondi coperti e RAM.
pub fn status_line(lang: Lang, st: &EngineStatus) -> String {
    if !st.buffer_running {
        return fmt(lang, K::StatusStopped, &[]);
    }
    let mb = (st.buffer_bytes / 1_000_000).to_string();
    let audio = format!("{:.0}", st.audio_seconds);
    if st.video_enabled {
        fmt(lang, K::StatusVideo, &[&format!("{:.0}", st.video_seconds), &audio, &mb])
    } else {
        fmt(lang, K::StatusAudio, &[&audio, &mb])
    }
}

/// Stato condiviso tra thread (menu, relay degli eventi, IPC).
struct App {
    commands: Sender<AppCommand>,
    status: Arc<Mutex<EngineStatus>>,
    config: Mutex<Config>,
    notifier: Mutex<Notifier>,
    error_at: Mutex<Option<Instant>>,
    settings: Mutex<Option<Child>>,
    hwnd: AtomicIsize,
}

impl App {
    fn wake(&self, msg: u32) {
        let h = self.hwnd.load(Ordering::Relaxed);
        if h != 0 {
            // SAFETY: l'handle appartiene alla nostra finestra, che vive quanto il loop; se è già
            // distrutta PostMessage fallisce e basta.
            let _ = unsafe { PostMessageW(Some(HWND(h as *mut _)), msg, WPARAM(0), LPARAM(0)) };
        }
    }

    fn process_event(&self, ev: &EngineEvent) {
        if let Ok(mut e) = self.error_at.lock() {
            match ev {
                EngineEvent::ClipFailed { .. } | EngineEvent::Error(_) => *e = Some(Instant::now()),
                EngineEvent::ClipSaved(_) => *e = None,
                EngineEvent::Warning(_) => {}
            }
        }
        if let Ok(mut n) = self.notifier.lock() {
            n.handle(ev);
        }
        self.wake(WM_REFRESH);
    }

    fn recent_error(&self) -> bool {
        self.error_at.lock().ok().and_then(|e| *e).is_some_and(|t| t.elapsed() < Duration::from_secs(60))
    }

    fn send(&self, cmd: AppCommand) {
        if self.commands.send(cmd).is_err() {
            tracing::error!(?cmd, "il motore non è in ascolto");
        }
    }

    fn status(&self) -> EngineStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn launch_settings(&self) {
        let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("cliprec-settings.exe")));
        let Some(exe) = exe.filter(|p| p.exists()) else {
            self.process_event(&EngineEvent::Warning(fmt(i18n::current(), K::SettingsMissing, &[])));
            return;
        };
        let Ok(mut slot) = self.settings.lock() else { return };
        if slot.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None))) {
            return; // già aperta
        }
        match std::process::Command::new(&exe).spawn() {
            Ok(child) => *slot = Some(child),
            Err(e) => tracing::error!("avvio impostazioni: {e}"),
        }
    }

    fn open_clips_folder(&self) {
        let folder = self.config.lock().map(|c| c.clips.folder()).unwrap_or_default();
        let _ = std::fs::create_dir_all(&folder);
        crate::sys::open_path(&folder);
    }
}

fn parse_chord(label: &str, text: &str, lang: Lang) -> Option<Chord> {
    match hotkey::parse(text) {
        Ok(c) => {
            for who in hotkey::known_conflicts(&c, lang) {
                tracing::warn!("hotkey {label} «{text}» coincide con {who}");
            }
            Some(c)
        }
        Err(e) => {
            tracing::error!("hotkey {label} «{text}» non valida: {e}");
            None
        }
    }
}

/// Avvisi per le hotkey non valide o in conflitto, da mostrare come `Warning`.
fn hotkey_warnings(cfg: &Config, lang: Lang) -> Vec<String> {
    let mut out = Vec::new();
    for (key, text) in [(K::HotkeyVideo, &cfg.hotkeys.video), (K::HotkeyAudio, &cfg.hotkeys.audio)] {
        let label = fmt(lang, key, &[]);
        match hotkey::parse(text) {
            Ok(c) => out.extend(hotkey::known_conflicts(&c, lang).into_iter().map(|w| fmt(lang, K::HotkeyConflict, &[&label, text, w]))),
            Err(e) => out.push(fmt(lang, K::HotkeyInvalid, &[&label, text, &e.to_string()])),
        }
    }
    out
}

/// Parte dell'interfaccia che vive solo sul thread principale.
struct Ui {
    app: Arc<App>,
    hotkeys: Hotkeys,
    tray: TrayIcon,
    status_item: MenuItem,
    toggle_item: MenuItem,
    folder_item: MenuItem,
    settings_item: MenuItem,
    quit_item: MenuItem,
    restart_item: Option<MenuItem>,
    recent_menu: Submenu,
    recent_items: Vec<MenuItem>,
    recent_paths: Arc<Mutex<Vec<PathBuf>>>,
    shown_state: Option<IconState>,
    shown_text: String,
    shown_recent: Vec<PathBuf>,
    shown_running: Option<bool>,
    last_elevation_pid: Option<u32>,
    elevation_tick: u32,
    self_elevated: bool,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

impl Ui {
    fn refresh(&mut self) {
        let st = self.app.status();
        let text = status_line(i18n::current(), &st);
        if text != self.shown_text {
            self.status_item.set_text(&text);
            let _ = self.tray.set_tooltip(Some(format!("ClipRec · {text}")));
            self.shown_text = text;
        }
        let state = icon::state_of(&st, self.app.recent_error());
        if self.shown_state != Some(state) {
            if let Some(i) = icon::tray_icon(state) {
                let _ = self.tray.set_icon(Some(i));
            }
            self.shown_state = Some(state);
        }
        if self.shown_running != Some(st.buffer_running) {
            self.toggle_item.set_text(fmt(i18n::current(), if st.buffer_running { K::MenuStop } else { K::MenuStart }, &[]));
            self.shown_running = Some(st.buffer_running);
        }
        let recent: Vec<PathBuf> = st.recent_clips.iter().take(RECENT_MAX).cloned().collect();
        if recent != self.shown_recent {
            for it in self.recent_items.drain(..) {
                let _ = self.recent_menu.remove(&it);
            }
            if recent.is_empty()
                && let Ok(it) = self.empty_item()
            {
                self.recent_items.push(it);
            }
            for (i, p) in recent.iter().enumerate() {
                let name = p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
                let it = MenuItem::with_id(format!("clip{i}"), name, true, None);
                if self.recent_menu.append(&it).is_ok() {
                    self.recent_items.push(it);
                }
            }
            if let Ok(mut r) = self.recent_paths.lock() {
                r.clone_from(&recent);
            }
            self.shown_recent = recent;
        }
    }

    /// Cambio lingua a caldo: riscrive le voci fisse e forza il ridisegno di quelle dinamiche.
    fn relabel(&mut self, l: Lang) {
        self.recent_menu.set_text(fmt(l, K::MenuRecent, &[]));
        self.folder_item.set_text(fmt(l, K::MenuFolder, &[]));
        self.settings_item.set_text(fmt(l, K::MenuSettings, &[]));
        self.quit_item.set_text(fmt(l, K::MenuQuit, &[]));
        if let Some(it) = &self.restart_item {
            it.set_text(fmt(l, K::MenuRestartAdmin, &[]));
        }
        self.shown_text.clear();
        self.shown_running = None;
        self.shown_recent = vec![PathBuf::from("\0")];
    }

    fn empty_item(&self) -> Result<MenuItem, tray_icon::menu::Error> {
        let it = MenuItem::new(fmt(i18n::current(), K::MenuNoClips, &[]), false, None);
        self.recent_menu.append(&it)?;
        Ok(it)
    }

    fn check_elevated_game(&mut self) {
        if self.self_elevated {
            return;
        }
        self.elevation_tick += 1;
        if !self.elevation_tick.is_multiple_of(2) {
            return; // ogni 2 s
        }
        let games = self.app.config.lock().map(|c| c.video.games.clone()).unwrap_or_default();
        match crate::sys::elevated_foreground_game(&games) {
            Some(g) if self.last_elevation_pid != Some(g.pid) => {
                self.last_elevation_pid = Some(g.pid);
                self.app.process_event(&EngineEvent::Warning(fmt(i18n::current(), K::ElevatedGame, &[&g.exe])));
            }
            Some(_) => {}
            None => self.last_elevation_pid = None,
        }
    }

    fn reload_config(&mut self) {
        match Config::load(&config_path()) {
            Ok(cfg) => {
                i18n::set_from_config(&cfg.general.language);
                for e in crate::gamepad::set_from_config(&cfg) {
                    self.app.process_event(&EngineEvent::Warning(e));
                }
                let l = i18n::current();
                self.relabel(l);
                self.hotkeys.video = parse_chord("video", &cfg.hotkeys.video, l);
                self.hotkeys.audio = parse_chord("audio", &cfg.hotkeys.audio, l);
                for w in hotkey_warnings(&cfg, i18n::current()) {
                    self.app.process_event(&EngineEvent::Warning(w));
                }
                if let Ok(mut n) = self.app.notifier.lock() {
                    n.enabled = cfg.general.notifications;
                    n.sound = cfg.general.sound_on_save;
                }
                // «Avvio con Windows» cambiato dalle impostazioni: si applica subito, non al prossimo avvio.
                if !notify::headless() {
                    crate::autostart::apply(&cfg);
                }
                if let Ok(mut c) = self.app.config.lock() {
                    *c = cfg;
                }
                tracing::info!("configurazione ricaricata");
            }
            Err(e) => tracing::error!("ricarica configurazione: {e}"),
        }
    }

    fn on_input(&mut self, ev: InputEvent) {
        let t0 = Instant::now();
        let trigger = match ev {
            InputEvent::Key { vk, down: true, .. } if !is_modifier(vk) => Some(Trigger::Key(vk)),
            InputEvent::Mouse { button, down: true } => Some(Trigger::Mouse(button)),
            _ => None,
        };
        if let Some(t) = trigger {
            // Riallinea con lo stato reale se un «su» è andato perso (es. cambio di desktop).
            if self.hotkeys.is_held(t) && !rawinput::async_trigger_down(t) {
                self.hotkeys.forget_trigger(t);
            }
            self.hotkeys.set_mods(rawinput::async_mods());
        }
        if let Some(cmd) = self.hotkeys.feed(ev) {
            self.app.send(cmd);
            tracing::info!(?cmd, elapsed_us = t0.elapsed().as_micros() as u64, "hotkey");
        }
    }
}

fn is_modifier(vk: u16) -> bool {
    matches!(vk, hotkey::VK_SHIFT | hotkey::VK_CONTROL | hotkey::VK_MENU | hotkey::VK_LWIN | hotkey::VK_RWIN)
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_INPUT => {
            let events = rawinput::decode(lparam);
            UI.with(|u| {
                if let Ok(mut u) = u.try_borrow_mut()
                    && let Some(ui) = u.as_mut()
                {
                    for ev in events {
                        ui.on_input(ev);
                    }
                }
            });
            // Windows chiede di chiamare DefWindowProc per la pulizia di WM_INPUT.
            // SAFETY: parametri passati tali e quali dal sistema.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_QUIT_REQUEST => {
            // SAFETY: siamo nel wndproc, sul thread del loop di messaggi.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_TIMER | WM_REFRESH | WM_RELOAD => {
            UI.with(|u| {
                if let Ok(mut u) = u.try_borrow_mut()
                    && let Some(ui) = u.as_mut()
                {
                    match msg {
                        WM_RELOAD => ui.reload_config(),
                        WM_TIMER => {
                            ui.check_elevated_game();
                            ui.refresh();
                        }
                        _ => ui.refresh(),
                    }
                }
            });
            LRESULT(0)
        }
        m if m == crate::sys::second_instance_msg() => {
            UI.with(|u| {
                if let Ok(u) = u.try_borrow()
                    && let Some(ui) = u.as_ref()
                {
                    tracing::info!("seconda istanza avviata");
                    ui.app.process_event(&EngineEvent::Warning(fmt(i18n::current(), K::AlreadyRunning, &[])));
                }
            });
            LRESULT(0)
        }
        // SAFETY: parametri passati tali e quali dal sistema.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn create_window() -> anyhow::Result<HWND> {
    // SAFETY: nessun argomento; restituisce il modulo corrente.
    let hinst = unsafe { GetModuleHandleW(None) }?;
    let class = w!("ClipRecShell");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst.into(),
        lpszClassName: class,
        ..Default::default()
    };
    // SAFETY: `wc` è completa e la classe vive quanto il processo.
    if unsafe { RegisterClassExW(&wc) } == 0 {
        anyhow::bail!("RegisterClassExW: {}", windows::core::Error::from_thread());
    }
    // SAFETY: finestra solo-messaggi, senza genitore visibile; i parametri sono validi.
    let hwnd = unsafe {
        CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("ClipRec"), WS_OVERLAPPED, 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hinst.into()), None)
    }?;
    // Se ClipRec gira come amministratore, UIPI scarterebbe l'avviso di una seconda istanza normale.
    // SAFETY: finestra appena creata da questo thread; messaggio registrato valido.
    let _ = unsafe {
        windows::Win32::UI::WindowsAndMessaging::ChangeWindowMessageFilterEx(
            hwnd,
            crate::sys::second_instance_msg(),
            windows::Win32::UI::WindowsAndMessaging::MSGFLT_ALLOW,
            None,
        )
    };
    Ok(hwnd)
}

fn build_ui(app: &Arc<App>, hwnd: HWND) -> anyhow::Result<Ui> {
    let status_item = MenuItem::new("ClipRec", false, None);
    let l = i18n::current();
    let toggle_item = MenuItem::with_id("toggle", fmt(l, K::MenuStop, &[]), true, None);
    let recent_menu = Submenu::new(fmt(l, K::MenuRecent, &[]), true);
    let folder_item = MenuItem::with_id("folder", fmt(l, K::MenuFolder, &[]), true, None);
    let settings_item = MenuItem::with_id("settings", fmt(l, K::MenuSettings, &[]), true, None);
    let quit_item = MenuItem::with_id("quit", fmt(l, K::MenuQuit, &[]), true, None);
    let restart_item = crate::sys::restart_menu_visible(crate::sys::self_elevated())
        .then(|| MenuItem::with_id("restart", fmt(l, K::MenuRestartAdmin, &[]), true, None));
    let sep = PredefinedMenuItem::separator();
    let mut items: Vec<&dyn tray_icon::menu::IsMenuItem> = vec![&status_item, &sep, &toggle_item, &recent_menu, &folder_item, &settings_item];
    if let Some(r) = &restart_item {
        items.push(r);
    }
    let sep2 = PredefinedMenuItem::separator();
    items.extend([&sep2 as &dyn tray_icon::menu::IsMenuItem, &quit_item]);
    let menu = Menu::new();
    menu.append_items(&items)?;
    let icon = icon::tray_icon(IconState::Stopped).context("icona della tray")?;
    let tray = TrayIconBuilder::new().with_menu(Box::new(menu)).with_icon(icon).with_tooltip("ClipRec").build()?;

    let recent_paths: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    let (h_app, h_recent) = (Arc::clone(app), Arc::clone(&recent_paths));
    // muda chiama il gestore dal loop del thread principale.
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| match e.id.0.as_str() {
        "toggle" => {
            let running = h_app.status().buffer_running;
            h_app.send(if running { AppCommand::StopBuffer } else { AppCommand::StartBuffer });
        }
        "folder" => h_app.open_clips_folder(),
        "settings" => h_app.launch_settings(),
        "restart" => match crate::sys::restart_elevated() {
            Ok(()) => {
                h_app.send(AppCommand::Quit);
                // SAFETY: il gestore gira sul thread del loop di messaggi.
                unsafe { PostQuitMessage(0) };
            }
            Err(e) => {
                tracing::warn!("riavvio come amministratore: {e}");
                h_app.process_event(&EngineEvent::Warning(fmt(i18n::current(), K::RestartFailed, &[&e.to_string()])));
            }
        },
        "quit" => {
            h_app.send(AppCommand::Quit);
            // SAFETY: il gestore gira sul thread del loop di messaggi.
            unsafe { PostQuitMessage(0) };
        }
        id => {
            if let Some(i) = id.strip_prefix("clip").and_then(|n| n.parse::<usize>().ok()) {
                let path = h_recent.lock().ok().and_then(|r| r.get(i).cloned());
                if let Some(p) = path {
                    crate::sys::open_path(&p);
                }
            }
        }
    }));

    let cfg = app.config.lock().map(|c| c.clone()).unwrap_or_default();
    let hotkeys = Hotkeys::new(parse_chord("video", &cfg.hotkeys.video, l), parse_chord("audio", &cfg.hotkeys.audio, l));
    rawinput::register(hwnd).context("registrazione Raw Input")?;
    Ok(Ui {
        app: Arc::clone(app),
        hotkeys,
        tray,
        status_item,
        toggle_item,
        folder_item,
        settings_item,
        quit_item,
        restart_item,
        recent_menu,
        recent_items: Vec::new(),
        recent_paths,
        shown_state: None,
        shown_text: String::new(),
        shown_recent: vec![PathBuf::from("\0")], // forza il primo disegno
        shown_running: None,
        last_elevation_pid: None,
        elevation_tick: 0,
        self_elevated: crate::sys::self_elevated(),
    })
}

/// Esegue la shell sul thread corrente fino all'uscita (voce «Esci» o `WM_QUIT`).
pub fn run(io: ShellIo) -> anyhow::Result<()> {
    let ShellIo { commands, events, status, config } = io;
    let wide_id = crate::sys::wide(notify::APP_ID);
    // SAFETY: stringa terminata da NUL, copiata da Windows.
    let _ = unsafe { SetCurrentProcessExplicitAppUserModelID(windows::core::PCWSTR(wide_id.as_ptr())) };

    let app = Arc::new(App {
        commands: commands.clone(),
        status: Arc::clone(&status),
        notifier: Mutex::new(Notifier::new(config.general.notifications, config.general.sound_on_save)),
        config: Mutex::new(config.clone()),
        error_at: Mutex::new(None),
        settings: Mutex::new(None),
        hwnd: AtomicIsize::new(0),
    });

    i18n::set_from_config(&config.general.language);
    let hwnd = create_window()?;
    app.hwnd.store(hwnd.0 as isize, Ordering::Relaxed);
    let ui = build_ui(&app, hwnd)?;
    UI.with(|u| *u.borrow_mut() = Some(ui));
    for w in hotkey_warnings(&config, i18n::current()) {
        app.process_event(&EngineEvent::Warning(w));
    }

    // Relay degli eventi del motore: notifiche e risveglio del loop.
    let events = Arc::new(Mutex::new(events));
    let relay_app = Arc::clone(&app);
    spawn_supervised("eventi", move || {
        loop {
            let ev = match events.lock() {
                Ok(rx) => rx.recv(),
                Err(_) => return,
            };
            match ev {
                Ok(ev) => relay_app.process_event(&ev),
                Err(_) => return, // il motore ha chiuso il canale
            }
        }
    })?;

    let (reload_app, quit_app) = (Arc::clone(&app), Arc::clone(&app));
    let ctx = crate::ipc_server::Context {
        status,
        commands,
        on_reload: Arc::new(move || reload_app.wake(WM_RELOAD)),
        on_quit: Arc::new(move || quit_app.wake(WM_QUIT_REQUEST)),
    };
    if let Err(e) = crate::ipc_server::spawn(pipe_name(), ctx) {
        tracing::error!("server IPC non avviato: {e}");
    }

    if !notify::headless() {
        crate::autostart::apply(&config);
    }

    // SAFETY: finestra valida; il timer è sul thread corrente e viene tolto all'uscita.
    unsafe { SetTimer(Some(hwnd), TIMER_ID, 1000, None) };
    app.wake(WM_REFRESH);
    tracing::info!("shell avviata");

    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` è un'uscita valida; 0 = WM_QUIT, -1 = errore.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break;
        }
        // SAFETY: messaggio appena ottenuto da GetMessageW.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // SAFETY: stesso thread e stessa finestra del SetTimer.
    let _ = unsafe { KillTimer(Some(hwnd), TIMER_ID) };
    UI.with(|u| *u.borrow_mut() = None);
    tracing::info!("shell terminata");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_texts() {
        let mut st = EngineStatus::default();
        assert_eq!(status_line(i18n::current(), &st), "Buffer fermo");
        st.buffer_running = true;
        st.video_enabled = true;
        st.video_seconds = 58.4;
        st.audio_seconds = 120.0;
        st.buffer_bytes = 412_000_000;
        assert_eq!(status_line(i18n::current(), &st), "Buffer: 58 s video · 120 s audio · 412 MB");
        st.video_enabled = false;
        assert_eq!(status_line(i18n::current(), &st), "Buffer: 120 s audio · 412 MB");
    }

    #[test]
    fn hotkey_warnings_cover_invalid_and_conflicts() {
        let mut cfg = Config::default();
        assert!(hotkey_warnings(&cfg, i18n::current()).is_empty());
        cfg.hotkeys.video = "Alt+F10".into();
        cfg.hotkeys.audio = "Ctrl+Pippo".into();
        let w = hotkey_warnings(&cfg, i18n::current());
        assert_eq!(w.len(), 2);
        assert!(w[0].contains("NVIDIA") && w[1].contains("non valida"));
    }
}
