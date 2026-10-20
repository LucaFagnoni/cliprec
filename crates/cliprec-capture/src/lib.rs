//! Cattura video: Windows.Graphics.Capture, Desktop Duplication, pacer a frequenza costante, sorgente sintetica.
//!
//! Contratti in `cliprec-core`; vedi SPEC.md, sezione «Squadra di subagenti».
//!
//! Ingresso per l'integrazione:
//!
//! - [`open_source`]: crea il `GpuDevice` sull'adattatore che mostra il bersaglio e la sorgente
//!   descritta da `VideoConfig` (Auto/Monitor/Window/Process). Lo stesso device va all'encoder.
//! - [`CaptureSource::new`] con un [`Target`] esplicito e un device da [`device_for_monitor`].
//! - [`SyntheticSource::new`]: test e misura della sincronia (lampo al secondo QPC, contatore).
//!
//! Ogni sorgente consegna `GpuFrame` BGRA (`BIND_SHADER_RESOURCE | BIND_RENDER_TARGET`) a
//! frequenza costante dal thread del pacer. La texture resta intatta per tutta la callback (triplo
//! buffer: la cattura scrive sempre in un altro slot). `index` è il numero del tick: se la callback
//! resta occupata per un intero periodo il tick è perso (`frames_dropped`) e l'indice salta, così
//! `pts == start + index * HNS_PER_SEC / fps` vale sempre. Un tick scaduto mentre la callback era
//! occupata ma prima del successivo viene consegnato in ritardo, mai più di uno.

mod dda;
mod health;
mod pacer;
mod source;
mod synthetic;
mod targets;
#[cfg(test)]
mod testutil;
mod wgc;

pub use pacer::SUPPORTED_FPS;
pub use source::{CaptureSource, Target, open_source};
pub use synthetic::{
    BACKGROUND, BIT_OFF, BIT_ON, COUNTER_BITS, FLASH, SyntheticSource, counter_block, counter_rect, is_flash,
};
pub use targets::device_for_monitor;
