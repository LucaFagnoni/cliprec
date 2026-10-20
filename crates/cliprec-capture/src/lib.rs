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
//!
//! Primo frame: l'integrazione apre l'encoder sulla dimensione del primo frame ricevuto. Per questo
//! [`CaptureSource`] aspetta fino a [`FIRST_FRAME_WAIT`] il primo frame vero prima del primo tick
//! (`start` è l'istante in cui l'attesa finisce); se non arriva consegna un nero della dimensione
//! del bersaglio (elemento WGC, modalità DDA, oppure il monitor principale se la finestra o il
//! gioco non ci sono ancora).
//!
//! Stato: `Waiting` per finestra minimizzata o assente, frame neri o fermi (avviso sul fullscreen
//! esclusivo) e [`ADAPTER_CHANGED`] (bersaglio su un monitor di un'altra GPU: conviene riaprire
//! device e sorgente con [`open_source`]); `Error` per device perso o 10 panic della callback in
//! 10 s (i panic isolati vengono superati).
//!
//! pts e istante di cattura: vedi `CAPTURE_PTS_OFFSET` in `source.rs` e
//! [`CaptureSource::latency_ms`].

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
pub use source::{ADAPTER_CHANGED, CaptureSource, FIRST_FRAME_WAIT, Target, open_source};
pub use synthetic::{
    BACKGROUND, BIT_OFF, BIT_ON, COUNTER_BITS, FLASH, SyntheticSource, counter_block, counter_rect, is_flash,
};
pub use targets::device_for_monitor;
