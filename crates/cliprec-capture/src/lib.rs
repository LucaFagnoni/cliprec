//! Cattura video: Windows.Graphics.Capture, Desktop Duplication, pacer a frequenza costante, sorgente sintetica.
//!
//! Contratti in `cliprec-core`; vedi SPEC.md, sezione «Squadra di subagenti».
//!
//! Ingresso per l'integrazione: [`open_source`] crea il device D3D11 sull'adattatore giusto e la
//! sorgente descritta da `VideoConfig`; [`SyntheticSource`] serve ai test e alla misura della sincronia.

mod pacer;
mod source;
mod synthetic;
mod targets;
#[cfg(test)]
mod testutil;
mod wgc;

pub use pacer::SUPPORTED_FPS;
pub use source::{CaptureSource, Target, open_source};
pub use synthetic::{SyntheticSource, is_flash};
pub use targets::device_for_monitor;
