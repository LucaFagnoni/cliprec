//! Motore audio: process loopback WASAPI, microfoni, loopback di uscita, silenzio, mix con correzione della deriva.
//!
//! Contratti in `cliprec-core`; vedi SPEC.md, sezione «Sorgenti e tracce audio».

mod beep;
pub mod dsp;
mod engine;
mod mixer;
mod wasapi_source;

pub use engine::{AudioEngine, MIX_LATENCY_HNS, MIX_TRACK_NAME};
pub use mixer::{MIX_BLOCK_FRAMES, MixInput, Mixer};
pub use wasapi_source::{OPENING, WasapiSource, find_root_pid, list_devices};

pub use beep::{BEEP_BLOCK_FRAMES, BEEP_FRAMES, BeepSource, render_beep};
