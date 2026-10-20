//! Icone della tray disegnate in codice (RGBA 32x32), una per stato.

use cliprec_core::EngineStatus;

pub const SIZE: u32 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconState {
    /// Buffer attivo.
    Active,
    /// Almeno un salvataggio in corso.
    Saving,
    Error,
    /// Buffer fermato dall'utente.
    Stopped,
}

pub fn state_of(st: &EngineStatus, recent_error: bool) -> IconState {
    if recent_error || st.last_error.is_some() {
        IconState::Error
    } else if st.saving > 0 {
        IconState::Saving
    } else if st.buffer_running {
        IconState::Active
    } else {
        IconState::Stopped
    }
}

fn color(s: IconState) -> [u8; 3] {
    match s {
        IconState::Active => [46, 160, 67],
        IconState::Saving => [240, 170, 20],
        IconState::Error => [215, 50, 45],
        IconState::Stopped => [120, 124, 130],
    }
}

/// Disco colorato con bordo morbido e un simbolo bianco: punto di registrazione, o «!» per l'errore.
pub fn rgba(s: IconState) -> Vec<u8> {
    let [r, g, b] = color(s);
    let c = (SIZE as f32 - 1.0) / 2.0;
    let mut px = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            let d = (dx * dx + dy * dy).sqrt();
            let alpha = ((15.0 - d) * 255.0).clamp(0.0, 255.0) as u8;
            let white = match s {
                IconState::Error => (14..18).contains(&x) && ((7..19).contains(&y) || (22..26).contains(&y)),
                _ => d < 5.5,
            };
            if white {
                px.extend_from_slice(&[255, 255, 255, alpha]);
            } else {
                px.extend_from_slice(&[r, g, b, alpha]);
            }
        }
    }
    px
}

pub fn tray_icon(s: IconState) -> Option<tray_icon::Icon> {
    tray_icon::Icon::from_rgba(rgba(s), SIZE, SIZE).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_valid_and_distinct() {
        let all = [IconState::Active, IconState::Saving, IconState::Error, IconState::Stopped];
        for s in all {
            let px = rgba(s);
            assert_eq!(px.len(), (SIZE * SIZE * 4) as usize);
            assert_eq!(px[3], 0, "angolo trasparente");
            assert!(tray_icon(s).is_some());
        }
        assert_ne!(rgba(IconState::Active), rgba(IconState::Saving));
        assert_ne!(rgba(IconState::Active), rgba(IconState::Error));
    }

    #[test]
    fn state_priority() {
        let mut st = EngineStatus { buffer_running: true, ..Default::default() };
        assert_eq!(state_of(&st, false), IconState::Active);
        st.saving = 1;
        assert_eq!(state_of(&st, false), IconState::Saving);
        assert_eq!(state_of(&st, true), IconState::Error);
        st.saving = 0;
        st.buffer_running = false;
        assert_eq!(state_of(&st, false), IconState::Stopped);
        st.last_error = Some("x".into());
        assert_eq!(state_of(&st, false), IconState::Error);
    }
}
