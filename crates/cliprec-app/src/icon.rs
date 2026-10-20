//! Icone della tray: il logo di ClipRec (32x32, da `assets/icon/tray-32.rgba`) con un indicatore
//! di stato, una per stato.

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

/// Logo in pixel RGBA non premoltiplicati, generato da `scripts/icone.py`.
const LOGO: &[u8; (SIZE * SIZE * 4) as usize] = include_bytes!("../../../assets/icon/tray-32.rgba");

/// Colori dell'indicatore: (cerchio grande semitrasparente, cerchio pieno più chiaro sopra).
/// `None` = solo il logo.
fn badge(s: IconState) -> Option<([u8; 3], [u8; 3])> {
    match s {
        IconState::Stopped => None,
        IconState::Active => Some(([46, 160, 67], [120, 225, 130])),
        IconState::Saving => Some(([235, 160, 20], [255, 214, 100])),
        IconState::Error => Some(([215, 50, 45], [255, 125, 115])),
    }
}

/// Centro dell'indicatore (in alto a sinistra) e raggi dei due cerchi, in pixel.
const BADGE_CENTER: f32 = 8.0;
const OUTER_RADIUS: f32 = 7.5;
const INNER_RADIUS: f32 = 4.5;
const OUTER_ALPHA: f32 = 0.45;

/// Copertura di un pixel da parte di un cerchio, con il bordo sfumato su un pixel.
fn coverage(x: u32, y: u32, radius: f32) -> f32 {
    let d = ((x as f32 + 0.5 - BADGE_CENTER).powi(2) + (y as f32 + 0.5 - BADGE_CENTER).powi(2)).sqrt();
    (radius + 0.5 - d).clamp(0.0, 1.0)
}

/// Compone `color` con opacità `alpha` sopra il pixel RGBA `p` (non premoltiplicato).
fn blend(p: &mut [u8], color: [u8; 3], alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    let dst_a = f32::from(p[3]) / 255.0;
    let out_a = alpha + dst_a * (1.0 - alpha);
    for c in 0..3 {
        let v = (f32::from(color[c]) * alpha + f32::from(p[c]) * dst_a * (1.0 - alpha)) / out_a;
        p[c] = v.round().clamp(0.0, 255.0) as u8;
    }
    p[3] = (out_a * 255.0).round() as u8;
}

/// Logo con lo stato, indicatore in alto a sinistra: verde col buffer attivo, arancione mentre
/// salva, rosso in errore; logo grigio senza indicatore se il buffer è fermo.
pub fn rgba(s: IconState) -> Vec<u8> {
    let mut px = LOGO.to_vec();
    if s == IconState::Stopped {
        for p in px.chunks_exact_mut(4) {
            let l = ((u32::from(p[0]) * 3 + u32::from(p[1]) * 6 + u32::from(p[2])) / 10 * 3 / 4) as u8;
            p[..3].fill(l);
        }
    }
    if let Some((outer, inner)) = badge(s) {
        for y in 0..SIZE {
            for x in 0..SIZE {
                let i = ((y * SIZE + x) * 4) as usize;
                let p = &mut px[i..i + 4];
                blend(p, outer, coverage(x, y, OUTER_RADIUS) * OUTER_ALPHA);
                blend(p, inner, coverage(x, y, INNER_RADIUS));
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
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output");
        let _ = std::fs::create_dir_all(&out);
        for s in all {
            // Pixel grezzi per controllarle a occhio (ffmpeg -f rawvideo -pix_fmt rgba -s 32x32).
            let _ = std::fs::write(out.join(format!("tray-{s:?}.rgba")), rgba(s));
            let px = rgba(s);
            assert_eq!(px.len(), (SIZE * SIZE * 4) as usize);
            assert_eq!(px[3], 0, "angolo trasparente");
            assert!(tray_icon(s).is_some());
        }
        assert_ne!(rgba(IconState::Active), rgba(IconState::Saving));
        assert_ne!(rgba(IconState::Active), rgba(IconState::Error));
        assert_ne!(rgba(IconState::Active), rgba(IconState::Stopped));
        assert_ne!(rgba(IconState::Saving), rgba(IconState::Error));
        // Indicatore verde in alto a sinistra col buffer attivo: centro pieno, bordo sfumato.
        let active = rgba(IconState::Active);
        let at = |x: u32, y: u32| &active[((y * SIZE + x) * 4) as usize..((y * SIZE + x) * 4 + 4) as usize];
        assert_eq!(at(7, 7), &[120, 225, 130, 255]);
        assert!(at(0, 8)[3] < 255, "il cerchio grande è semitrasparente");
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
