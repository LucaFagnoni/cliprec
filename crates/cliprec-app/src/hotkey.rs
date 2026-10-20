//! Logica pura delle hotkey: parser delle stringhe di `HotkeyConfig`, stato dei modificatori,
//! scatto sul fronte di pressione e soppressione dell'autorepeat. Niente Win32 qui.

use std::collections::HashSet;

use cliprec_core::AppCommand;

// Codici VK usati dal parser e dal decoder Raw Input.
pub const VK_SHIFT: u16 = 0x10;
pub const VK_CONTROL: u16 = 0x11;
pub const VK_MENU: u16 = 0x12;
pub const VK_LWIN: u16 = 0x5B;
pub const VK_RWIN: u16 = 0x5C;

/// Modificatori attivi.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
}

/// Ciò che scatta: un tasto della tastiera (VK) o un pulsante del mouse (3 = centrale, 4 e 5 = laterali).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Trigger {
    Key(u16),
    Mouse(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub mods: Mods,
    pub trigger: Trigger,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    UnknownKey(String),
    NoTrigger,
    TooManyTriggers,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "hotkey vuota"),
            Self::UnknownKey(k) => write!(f, "tasto sconosciuto: {k}"),
            Self::NoTrigger => write!(f, "manca il tasto principale"),
            Self::TooManyTriggers => write!(f, "più di un tasto principale"),
        }
    }
}

impl std::error::Error for ParseError {}

fn key_vk(name: &str) -> Option<u16> {
    let n = name.to_ascii_lowercase();
    let b = n.as_bytes();
    Some(match n.as_str() {
        "space" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "insert" | "ins" => 0x2D,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "pause" => 0x13,
        "printscreen" | "prtsc" => 0x2C,
        "scrolllock" => 0x91,
        _ if b.len() == 1 && b[0].is_ascii_alphabetic() => b[0].to_ascii_uppercase() as u16,
        _ if b.len() == 1 && b[0].is_ascii_digit() => b[0] as u16,
        _ if n.starts_with('f') && n.len() <= 3 => match n[1..].parse::<u16>() {
            Ok(k @ 1..=24) => 0x70 + k - 1,
            _ => return None,
        },
        _ if n.starts_with("num") && n.len() == 4 && b[3].is_ascii_digit() => 0x60 + u16::from(b[3] - b'0'),
        _ => return None,
    })
}

/// «Ctrl+Alt+F10», «Mouse5», «Ctrl+Mouse4», «Shift+F12»…
pub fn parse(text: &str) -> Result<Chord, ParseError> {
    if text.trim().is_empty() {
        return Err(ParseError::Empty);
    }
    let mut mods = Mods::default();
    let mut trigger = None;
    for part in text.split('+').map(str::trim) {
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => mods.ctrl = true,
            "alt" => mods.alt = true,
            "shift" => mods.shift = true,
            "win" | "meta" | "super" => mods.win = true,
            _ => {
                let t = if let Some(n) = lower.strip_prefix("mouse") {
                    match n {
                        "3" | "4" | "5" => Trigger::Mouse(n.as_bytes()[0] - b'0'),
                        _ => return Err(ParseError::UnknownKey(part.to_string())),
                    }
                } else {
                    Trigger::Key(key_vk(&lower).ok_or_else(|| ParseError::UnknownKey(part.to_string()))?)
                };
                if trigger.replace(t).is_some() {
                    return Err(ParseError::TooManyTriggers);
                }
            }
        }
    }
    Ok(Chord { mods, trigger: trigger.ok_or(ParseError::NoTrigger)? })
}

/// Hotkey di altri programmi note; `(combinazione, chi la usa)`.
const KNOWN: &[(&str, &str, &str)] = &[
    ("Alt+F10", "NVIDIA App (salva replay istantaneo)", "NVIDIA App (instant replay)"),
    ("Alt+F9", "NVIDIA App (avvia/ferma registrazione)", "NVIDIA App (start/stop recording)"),
    ("Alt+F12", "NVIDIA App (mostra FPS)", "NVIDIA App (show FPS)"),
    ("Alt+Z", "NVIDIA App (overlay)", "NVIDIA App (overlay)"),
    ("Win+Alt+G", "Xbox Game Bar (registra gli ultimi secondi)", "Xbox Game Bar (record last moments)"),
    ("Win+Alt+R", "Xbox Game Bar (avvia/ferma registrazione)", "Xbox Game Bar (start/stop recording)"),
    ("Win+G", "Xbox Game Bar", "Xbox Game Bar"),
    ("F12", "Steam (screenshot)", "Steam (screenshot)"),
    ("Shift+Tab", "Steam (overlay)", "Steam (overlay)"),
    ("Alt+F1", "AMD Software (replay istantaneo)", "AMD Software (instant replay)"),
    ("Ctrl+Shift+S", "AMD Software (screenshot)", "AMD Software (screenshot)"),
];

/// Descrizioni dei programmi che usano già la stessa combinazione.
pub fn known_conflicts(chord: &Chord, lang: crate::i18n::Lang) -> Vec<&'static str> {
    KNOWN
        .iter()
        .filter(|(c, ..)| parse(c).is_ok_and(|k| k == *chord))
        .map(|&(_, it, en)| if lang == crate::i18n::Lang::It { it } else { en })
        .collect()
}

/// Serve ascoltare il mouse? Solo se almeno una hotkey usa un pulsante `MouseN`: altrimenti
/// Raw Input consegnerebbe un `WM_INPUT` per ogni movimento (1-8 kHz) senza motivo.
pub fn needs_mouse(video: Option<Chord>, audio: Option<Chord>) -> bool {
    [video, audio].into_iter().flatten().any(|c| matches!(c.trigger, Trigger::Mouse(_)))
}

/// Evento d'ingresso già decodificato da Raw Input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// `vk` generico di Raw Input (Ctrl = 0x11 ecc.); `e0` distingue destra/sinistra.
    Key { vk: u16, e0: bool, down: bool },
    Mouse { button: u8, down: bool },
}

/// Eventi di pulsanti del mouse da `RAWMOUSE.usButtonFlags` (solo pulsanti 3, 4 e 5).
pub fn mouse_events(button_flags: u16) -> Vec<InputEvent> {
    const BITS: [(u16, u8, bool); 6] = [
        (0x0010, 3, true),
        (0x0020, 3, false),
        (0x0040, 4, true),
        (0x0080, 4, false),
        (0x0100, 5, true),
        (0x0200, 5, false),
    ];
    BITS.iter().filter(|(bit, ..)| button_flags & bit != 0).map(|&(_, button, down)| InputEvent::Mouse { button, down }).collect()
}

/// Stato delle due hotkey: modificatori premessi, trigger tenuti, scatto sul fronte.
#[derive(Debug)]
pub struct Hotkeys {
    pub video: Option<Chord>,
    pub audio: Option<Chord>,
    /// Modificatori fisici tenuti, come `vk | (e0 << 8)`.
    held_mods: HashSet<u16>,
    held_triggers: HashSet<Trigger>,
}

impl Hotkeys {
    pub fn new(video: Option<Chord>, audio: Option<Chord>) -> Self {
        Self { video, audio, held_mods: HashSet::new(), held_triggers: HashSet::new() }
    }

    pub fn mods(&self) -> Mods {
        let has = |vk: u16| self.held_mods.iter().any(|m| m & 0xFF == vk);
        Mods { ctrl: has(VK_CONTROL), alt: has(VK_MENU), shift: has(VK_SHIFT), win: has(VK_LWIN) || has(VK_RWIN) }
    }

    pub fn is_held(&self, t: Trigger) -> bool {
        self.held_triggers.contains(&t)
    }

    /// Dimentica un trigger che risulta rilasciato (un evento «su» perso): evita che una
    /// pressione vera sia scambiata per autorepeat.
    pub fn forget_trigger(&mut self, t: Trigger) {
        self.held_triggers.remove(&t);
    }

    /// Riallinea i modificatori allo stato reale quando un «su» è andato perso.
    pub fn set_mods(&mut self, m: Mods) {
        self.held_mods.clear();
        for (on, vk) in [(m.ctrl, VK_CONTROL), (m.alt, VK_MENU), (m.shift, VK_SHIFT), (m.win, VK_LWIN)] {
            if on {
                self.held_mods.insert(vk);
            }
        }
    }

    /// Elabora un evento; restituisce il comando se è una pressione nuova di una hotkey.
    pub fn feed(&mut self, ev: InputEvent) -> Option<AppCommand> {
        let (trigger, down) = match ev {
            InputEvent::Key { vk, e0, down } => {
                if matches!(vk, VK_SHIFT | VK_CONTROL | VK_MENU | VK_LWIN | VK_RWIN) {
                    // Shift «finti» di NumLock arrivano con E0: non sono tasti veri.
                    if vk == VK_SHIFT && e0 {
                        return None;
                    }
                    let id = vk | (u16::from(e0) << 8);
                    if down {
                        self.held_mods.insert(id);
                    } else {
                        self.held_mods.remove(&id);
                    }
                    return None;
                }
                (Trigger::Key(vk), down)
            }
            InputEvent::Mouse { button, down } => (Trigger::Mouse(button), down),
        };
        if !down {
            self.held_triggers.remove(&trigger);
            return None;
        }
        if !self.held_triggers.insert(trigger) {
            return None; // autorepeat: il tasto era già premuto
        }
        let now = self.mods();
        let hit = |c: &Option<Chord>| c.is_some_and(|c| c.trigger == trigger && c.mods == now);
        if hit(&self.video) {
            Some(AppCommand::SaveVideoClip)
        } else if hit(&self.audio) {
            Some(AppCommand::SaveAudioClip)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(vk: u16, down: bool) -> InputEvent {
        InputEvent::Key { vk, e0: false, down }
    }

    /// Un tasto da solo, anche F13-F24 (tasti macro), è una hotkey valida; con un modificatore
    /// premuto non scatta.
    #[test]
    fn single_key_without_modifiers() {
        let f14 = parse("F14").unwrap();
        assert_eq!(f14, Chord { mods: Mods::default(), trigger: Trigger::Key(0x7D) });
        let mut h = Hotkeys::new(Some(f14), None);
        assert_eq!(h.feed(key(0x7D, true)), Some(AppCommand::SaveVideoClip));
        h.feed(key(0x7D, false));
        h.feed(key(VK_CONTROL, true));
        assert_eq!(h.feed(key(0x7D, true)), None);
    }

    #[test]
    fn parses_strings() {
        let c = parse("Ctrl+Alt+F10").unwrap();
        assert_eq!(c, Chord { mods: Mods { ctrl: true, alt: true, ..Default::default() }, trigger: Trigger::Key(0x79) });
        assert_eq!(parse("Mouse5").unwrap(), Chord { mods: Mods::default(), trigger: Trigger::Mouse(5) });
        let c = parse("ctrl + mouse4").unwrap();
        assert!(c.mods.ctrl && c.trigger == Trigger::Mouse(4));
        assert_eq!(parse("Shift+F12").unwrap().trigger, Trigger::Key(0x7B));
        assert_eq!(parse("Win+A").unwrap().trigger, Trigger::Key(b'A' as u16));
        assert_eq!(parse("").unwrap_err(), ParseError::Empty);
        assert_eq!(parse("Ctrl+Alt").unwrap_err(), ParseError::NoTrigger);
        assert_eq!(parse("F1+F2").unwrap_err(), ParseError::TooManyTriggers);
        assert!(matches!(parse("Ctrl+Foo"), Err(ParseError::UnknownKey(_))));
        assert!(matches!(parse("Mouse1"), Err(ParseError::UnknownKey(_))));
        assert!(matches!(parse("F25"), Err(ParseError::UnknownKey(_))));
    }

    fn setup() -> Hotkeys {
        Hotkeys::new(parse("Ctrl+Alt+F10").ok(), parse("Mouse5").ok())
    }

    #[test]
    fn fires_on_press_edge_only_and_ignores_autorepeat() {
        let mut h = setup();
        assert_eq!(h.feed(key(VK_CONTROL, true)), None);
        assert_eq!(h.feed(key(VK_MENU, true)), None);
        assert_eq!(h.feed(key(0x79, true)), Some(AppCommand::SaveVideoClip));
        for _ in 0..5 {
            assert_eq!(h.feed(key(0x79, true)), None, "autorepeat");
        }
        assert_eq!(h.feed(key(0x79, false)), None);
        assert_eq!(h.feed(key(0x79, true)), Some(AppCommand::SaveVideoClip), "nuova pressione");
    }

    #[test]
    fn modifiers_must_match_exactly() {
        let mut h = setup();
        assert_eq!(h.feed(key(0x79, true)), None, "senza modificatori");
        h.feed(key(0x79, false));
        h.feed(key(VK_CONTROL, true));
        h.feed(key(VK_MENU, true));
        h.feed(key(VK_SHIFT, true));
        assert_eq!(h.feed(key(0x79, true)), None, "Shift in più");
        h.feed(key(0x79, false));
        h.feed(key(VK_SHIFT, false));
        assert_eq!(h.feed(key(0x79, true)), Some(AppCommand::SaveVideoClip));
        h.feed(key(VK_CONTROL, false));
        h.feed(key(0x79, false));
        assert_eq!(h.feed(key(0x79, true)), None, "Ctrl rilasciato");
    }

    #[test]
    fn right_and_left_modifier_tracked_independently() {
        let mut h = Hotkeys::new(parse("Ctrl+F1").ok(), None);
        h.feed(InputEvent::Key { vk: VK_CONTROL, e0: false, down: true });
        h.feed(InputEvent::Key { vk: VK_CONTROL, e0: true, down: true });
        h.feed(InputEvent::Key { vk: VK_CONTROL, e0: false, down: false });
        assert!(h.mods().ctrl, "il Ctrl destro è ancora premuto");
        // Shift finto di NumLock (E0) ignorato.
        h.feed(InputEvent::Key { vk: VK_SHIFT, e0: true, down: true });
        assert!(!h.mods().shift);
    }

    #[test]
    fn mouse_buttons_and_flags() {
        let mut h = setup();
        for ev in mouse_events(0x0100) {
            assert_eq!(h.feed(ev), Some(AppCommand::SaveAudioClip));
        }
        assert_eq!(h.feed(InputEvent::Mouse { button: 5, down: true }), None, "ripetizione");
        for ev in mouse_events(0x0200) {
            h.feed(ev);
        }
        assert_eq!(h.feed(InputEvent::Mouse { button: 5, down: true }), Some(AppCommand::SaveAudioClip));
        assert_eq!(mouse_events(0x0040 | 0x0200).len(), 2);
        assert!(mouse_events(0x0001).is_empty(), "il tasto sinistro non conta");
    }

    #[test]
    fn stale_state_can_be_resynced() {
        let mut h = setup();
        h.feed(key(VK_CONTROL, true));
        h.feed(key(VK_MENU, true));
        assert!(h.feed(key(0x79, true)).is_some());
        // «su» perso: il fronte successivo sarebbe scambiato per autorepeat finché non si riallinea.
        assert_eq!(h.feed(key(0x79, true)), None);
        h.forget_trigger(Trigger::Key(0x79));
        assert!(h.feed(key(0x79, true)).is_some());
        h.set_mods(Mods::default());
        h.forget_trigger(Trigger::Key(0x79));
        assert_eq!(h.feed(key(0x79, true)), None);
    }

    #[test]
    fn conflicts_are_reported() {
        assert!(!known_conflicts(&parse("Alt+F10").unwrap(), crate::i18n::Lang::En).is_empty());
        assert!(known_conflicts(&parse("Ctrl+Alt+F10").unwrap(), crate::i18n::Lang::En).is_empty());
        assert!(!known_conflicts(&parse("Alt+F9").unwrap(), crate::i18n::Lang::En).is_empty());
    }

    #[test]
    fn m21_mouse_registered_only_when_a_hotkey_uses_it() {
        let k = |t: &str| parse(t).ok();
        assert!(!needs_mouse(k("Ctrl+Alt+F10"), k("Ctrl+Alt+F9")), "solo tastiera");
        assert!(!needs_mouse(None, None));
        assert!(needs_mouse(k("Mouse5"), k("Ctrl+Alt+F9")));
        assert!(needs_mouse(k("Ctrl+F1"), k("Ctrl+Mouse4")));
    }
}
