//! Controller XInput (facoltativo): «Clip video» e «Clip audio» anche da un tasto o da una
//! combinazione del controller (`hotkeys.video_pad`, `hotkeys.audio_pad`). Scatta sul fronte di
//! pressione, come le hotkey: tenere premuto non genera clip multiple.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cliprec_core::{AppCommand, Config};
use windows::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};

/// Maschere dei tasti XInput (`XINPUT_GAMEPAD_*`).
const BUTTONS: &[(&str, u16)] = &[
    ("DPadUp", 0x0001),
    ("DPadDown", 0x0002),
    ("DPadLeft", 0x0004),
    ("DPadRight", 0x0008),
    ("Start", 0x0010),
    ("Back", 0x0020),
    ("LS", 0x0040),
    ("RS", 0x0080),
    ("LB", 0x0100),
    ("RB", 0x0200),
    ("A", 0x1000),
    ("B", 0x2000),
    ("X", 0x4000),
    ("Y", 0x8000),
];

static VIDEO: AtomicU32 = AtomicU32::new(0);
static AUDIO: AtomicU32 = AtomicU32::new(0);

/// «Back+A» → maschera; stringa vuota → 0 (nessun tasto).
pub fn parse(text: &str) -> Result<u16, String> {
    if text.trim().is_empty() {
        return Ok(0);
    }
    text.split('+').try_fold(0u16, |mask, part| {
        let part = part.trim();
        BUTTONS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(part))
            .map(|(_, bit)| mask | bit)
            .ok_or_else(|| format!("tasto del controller sconosciuto: «{part}»"))
    })
}

/// Aggiorna le combinazioni dalla configurazione; restituisce gli errori di sintassi da mostrare.
pub fn set_from_config(cfg: &Config) -> Vec<String> {
    let mut errors = Vec::new();
    for (text, slot) in [(&cfg.hotkeys.video_pad, &VIDEO), (&cfg.hotkeys.audio_pad, &AUDIO)] {
        let mask = parse(text).unwrap_or_else(|e| {
            errors.push(e);
            0
        });
        slot.store(u32::from(mask), Ordering::Relaxed);
    }
    errors
}

/// Tutti i tasti della combinazione premuti adesso e non tutti un attimo prima.
fn pressed(prev: u16, now: u16, mask: u16) -> bool {
    mask != 0 && now & mask == mask && prev & mask != mask
}

/// Thread che legge i controller ogni 16 ms; i controller scollegati si riprovano ogni 2 s,
/// perché `XInputGetState` su un indice vuoto è lento. Senza combinazioni configurate dorme.
pub fn spawn(commands: Sender<AppCommand>) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name("gamepad".into()).spawn(move || {
        let mut prev = [0u16; 4];
        let mut next_probe = [Instant::now(); 4];
        loop {
            let (video, audio) = (VIDEO.load(Ordering::Relaxed) as u16, AUDIO.load(Ordering::Relaxed) as u16);
            if video == 0 && audio == 0 {
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            let now = Instant::now();
            for (i, (prev, probe)) in prev.iter_mut().zip(next_probe.iter_mut()).enumerate() {
                if now < *probe {
                    continue;
                }
                let mut state = XINPUT_STATE::default();
                // SAFETY: `state` è una struttura locale valida; l'indice è 0..4 come richiesto.
                if unsafe { XInputGetState(i as u32, &mut state) } != 0 {
                    *prev = 0;
                    *probe = now + Duration::from_secs(2);
                    continue;
                }
                let buttons = state.Gamepad.wButtons.0;
                for (mask, cmd) in [(video, AppCommand::SaveVideoClip), (audio, AppCommand::SaveAudioClip)] {
                    if pressed(*prev, buttons, mask) && commands.send(cmd).is_err() {
                        return;
                    }
                }
                *prev = buttons;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_edges() {
        assert_eq!(parse(""), Ok(0));
        assert_eq!(parse("back + a"), Ok(0x1020));
        assert_eq!(parse("LB+RB+Y"), Ok(0x8300));
        assert!(parse("A+Turbo").is_err());
        let combo = 0x1020;
        assert!(pressed(0x0020, 0x1020, combo), "secondo tasto premuto: scatta");
        assert!(!pressed(0x1020, 0x1020, combo), "tenuto premuto: non scatta di nuovo");
        assert!(!pressed(0, 0x1000, combo), "solo uno dei due");
        assert!(pressed(0x1000, 0x1020 | 0x0001, combo), "altri tasti in più non contano");
        assert!(!pressed(0, 0xffff, 0), "nessuna combinazione configurata");
    }
}
