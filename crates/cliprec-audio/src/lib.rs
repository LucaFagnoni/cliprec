//! Motore audio: process loopback WASAPI, microfoni, loopback di uscita, silenzio, mix con correzione della deriva.
//!
//! Contratti in `cliprec-core`; vedi SPEC.md, sezione «Sorgenti e tracce audio».

mod beep;
pub mod dsp;
mod mixer;

pub use mixer::{MIX_BLOCK_FRAMES, MixInput, Mixer};

pub use beep::{BEEP_BLOCK_FRAMES, BEEP_FRAMES, BeepSource, render_beep};
