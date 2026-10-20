//! Conversione degli eventi di tastiera/mouse di Slint nel testo delle hotkey ("Ctrl+Alt+F10",
//! "Ctrl+Mouse4") e avvisi sui conflitti con programmi noti.

#[derive(Clone, Copy, Debug, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

fn compose(m: Mods, key: &str) -> String {
    let mut s = String::new();
    for (on, name) in [(m.ctrl, "Ctrl"), (m.alt, "Alt"), (m.shift, "Shift"), (m.meta, "Win")] {
        if on {
            s.push_str(name);
            s.push('+');
        }
    }
    s.push_str(key);
    s
}

/// Nome del tasto dal testo dell'evento Slint, o `None` per i soli modificatori e i tasti non gestiti.
fn key_name(text: &str) -> Option<String> {
    let c = text.chars().next()?;
    let code = c as u32;
    Some(match c {
        '\u{f704}'..='\u{f71b}' => format!("F{}", code - 0xf704 + 1),
        '\u{f700}' => "Up".into(),
        '\u{f701}' => "Down".into(),
        '\u{f702}' => "Left".into(),
        '\u{f703}' => "Right".into(),
        '\u{f727}' => "Insert".into(),
        '\u{f729}' => "Home".into(),
        '\u{f72b}' => "End".into(),
        '\u{f72c}' => "PageUp".into(),
        '\u{f72d}' => "PageDown".into(),
        ' ' => "Space".into(),
        '\t' => "Tab".into(),
        '\n' | '\r' => "Enter".into(),
        '\u{7f}' => "Delete".into(),
        // Ctrl+lettera può arrivare come carattere di controllo. Da 0x10 a 0x18 sono i codici dei
        // soli modificatori di Slint (Shift, Control, Alt...): non formano una combinazione.
        '\u{1}'..='\u{f}' if c != '\u{8}' => char::from_u32(code + 64)?.to_string(),
        c if c.is_ascii_alphanumeric() => c.to_ascii_uppercase().to_string(),
        _ => return None,
    })
}

/// `Some(testo)` se l'evento completa una combinazione valida.
pub fn from_key(text: &str, m: Mods) -> Option<String> {
    key_name(text).map(|k| compose(m, &k))
}

/// Tasti laterali del mouse: 4 = indietro, 5 = avanti.
pub fn from_mouse(button: i32, m: Mods) -> Option<String> {
    matches!(button, 4 | 5).then(|| compose(m, &format!("Mouse{button}")))
}

/// Avvisa se la combinazione coincide con una nota di altri programmi.
pub fn conflict(hotkey: &str) -> Option<&'static str> {
    const KNOWN: &[(&str, &str)] = &[
        ("Alt+F10", "NVIDIA App (Instant Replay: salva)"),
        ("Alt+F9", "NVIDIA App (registrazione)"),
        ("Alt+F12", "NVIDIA App (screenshot)"),
        ("Alt+F1", "NVIDIA App (filtri foto)"),
        ("Alt+Z", "NVIDIA App (overlay)"),
        ("Win+G", "Xbox Game Bar"),
        ("Win+Alt+R", "Xbox Game Bar (registra)"),
        ("Win+Alt+G", "Xbox Game Bar (ultimi 30 s)"),
        ("Win+Alt+PrintScreen", "Xbox Game Bar (screenshot)"),
        ("Ctrl+Shift+Esc", "Gestione attività di Windows"),
        ("Alt+F4", "chiusura finestra di Windows"),
        ("Alt+Tab", "cambio finestra di Windows"),
    ];
    KNOWN.iter().find(|(k, _)| k.eq_ignore_ascii_case(hotkey)).map(|(_, who)| *who)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CA: Mods = Mods { ctrl: true, alt: true, shift: false, meta: false };

    #[test]
    fn keys_and_mouse() {
        assert_eq!(from_key("\u{f70d}", CA).as_deref(), Some("Ctrl+Alt+F10"));
        assert_eq!(from_key("\u{f704}", Mods::default()).as_deref(), Some("F1"));
        assert_eq!(from_key("f", CA).as_deref(), Some("Ctrl+Alt+F"));
        assert_eq!(from_key("\u{6}", Mods { ctrl: true, ..Mods::default() }).as_deref(), Some("Ctrl+F"));
        assert_eq!(from_key("9", Mods { shift: true, ..Mods::default() }).as_deref(), Some("Shift+9"));
        assert_eq!(from_key("\u{11}", CA), None);
        // Tasti senza modificatori, anche F13-F24 (tasti macro).
        assert_eq!(from_key("\u{f711}", Mods::default()).as_deref(), Some("F14"));
        assert_eq!(from_key("\u{f71b}", Mods::default()).as_deref(), Some("F24"));
        assert_eq!(from_key("\u{f72c}", Mods::default()).as_deref(), Some("PageUp"));
        assert_eq!(from_mouse(5, Mods { ctrl: true, ..Mods::default() }).as_deref(), Some("Ctrl+Mouse5"));
        assert_eq!(from_mouse(1, CA), None);
    }

    #[test]
    fn known_conflicts() {
        assert!(conflict("alt+f10").is_some());
        assert!(conflict("Ctrl+Alt+F10").is_none());
        assert!(conflict("Win+Alt+R").is_some());
    }
}
