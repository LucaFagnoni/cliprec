//! Cattura video: Windows.Graphics.Capture, Desktop Duplication, pacer a frequenza costante, sorgente sintetica.
//!
//! Contratti in `cliprec-core`; vedi SPEC.md, sezione «Squadra di subagenti».

mod pacer;
mod synthetic;
#[cfg(test)]
mod testutil;

pub use pacer::SUPPORTED_FPS;
pub use synthetic::{SyntheticSource, is_flash};
