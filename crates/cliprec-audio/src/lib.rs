//! Motore audio: process loopback WASAPI, microfoni, loopback di uscita, silenzio, mix con correzione della deriva.
//!
//! Contratti in `cliprec-core`; vedi SPEC.md, sezione «Sorgenti e tracce audio».

mod beep;
pub mod dsp;
mod engine;
mod mixer;
mod wasapi_source;

pub use engine::{AudioEngine, MAX_DELAY_MS, MAX_MIX_LATENCY_HNS, MIX_LATENCY_HNS, MIX_TRACK_NAME};
pub use mixer::{MIX_BLOCK_FRAMES, MixInput, Mixer};
pub use wasapi_source::{CaptureMode, OPENING, WasapiSource, find_root_pid, list_devices};

/// Testo di un panic catturato con `catch_unwind`.
pub(crate) fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic senza messaggio".into())
}

pub use beep::{BEEP_BLOCK_FRAMES, BEEP_FRAMES, BeepSource, render_beep};
