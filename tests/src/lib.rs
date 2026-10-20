//! Strumenti di validazione delle clip di ClipRec (posseduti dal subagente test-harness):
//! validazione ffprobe, misura della sincronia, clip di riferimento, report dei benchmark.
//! Condivisi da `cargo xtask` e dai test.

pub mod bench;
pub mod ffmpeg;
pub mod generate;
pub mod probe;
pub mod sync;
