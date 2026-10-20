//! Ingresso dell'audio (M7): il thread di cattura copia il PCM in una coda `rtrb` preallocata per
//! traccia, senza allocare; un thread di compressione, non in tempo reale, la svuota nei blocchi
//! FLAC. Snapshot e statistiche svuotano prima la coda, così non restano buchi.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::{JoinHandle, Thread};
use std::time::Duration;

use cliprec_core::{CHANNELS, QpcTime, SAMPLE_RATE};
use flacenc::config::Encoder;
use flacenc::error::Verified;
use rtrb::{Consumer, Producer, RingBuffer};

use crate::audio::{Keep, Track};


/// Secondi di PCM che la coda di una traccia può tenere prima che il thread di cattura debba
/// comprimere da sé (solo se il compressore resta indietro di tanto).
const QUEUE_SECS: usize = 2;
/// Pezzi (`push`) in coda al massimo: 2 s a pezzi da 1 ms.
const QUEUE_CHUNKS: usize = 2048;
/// Attesa massima del compressore tra un risveglio e l'altro.
const IDLE: Duration = Duration::from_millis(50);

struct QueueIn {
    samples: Producer<f32>,
    chunks: Producer<(QpcTime, usize)>,
}

struct QueueOut {
    samples: Consumer<f32>,
    chunks: Consumer<(QpcTime, usize)>,
    scratch: Vec<f32>,
}

pub(crate) struct Slot {
    pub track: Mutex<Track>,
    input: Mutex<QueueIn>,
    output: Mutex<QueueOut>,
}

impl Slot {
    fn new(track: Track) -> Self {
        let (sp, sc) = RingBuffer::new(QUEUE_SECS * SAMPLE_RATE as usize * CHANNELS);
        let (cp, cc) = RingBuffer::new(QUEUE_CHUNKS);
        Self {
            track: Mutex::new(track),
            input: Mutex::new(QueueIn { samples: sp, chunks: cp }),
            output: Mutex::new(QueueOut { samples: sc, chunks: cc, scratch: Vec::new() }),
        }
    }
}

/// Stato audio condiviso tra `ReplayBuffer`, thread di cattura e compressore.
pub(crate) struct Intake {
    pub slots: RwLock<Vec<Slot>>,
    /// Memoria delle arene audio, letta dal video per il tetto comune.
    pub bytes: AtomicUsize,
    /// pts del keyframe più vecchio conservato (`i64::MAX` senza video): l'audio lo copre (M4).
    pub video_from: AtomicI64,
    flac: Option<Verified<Encoder>>,
    keep_audio_hns: i64,
    keep_video_max_hns: i64,
    stop: AtomicBool,
    compressor: OnceLock<Thread>,
}

impl Intake {
    pub fn new(flac: Option<Verified<Encoder>>, keep_audio_hns: i64, keep_video_max_hns: i64) -> Self {
        Self {
            slots: RwLock::new(Vec::new()),
            bytes: AtomicUsize::new(0),
            video_from: AtomicI64::new(i64::MAX),
            flac,
            keep_audio_hns,
            keep_video_max_hns,
            stop: AtomicBool::new(false),
            compressor: OnceLock::new(),
        }
    }

    pub fn flac_ok(&self) -> bool {
        self.flac.is_some()
    }

    /// Avvia il thread di compressione; si ferma con [`Intake::shutdown`].
    pub fn spawn(self: &Arc<Self>) -> Option<JoinHandle<()>> {
        let me = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("cliprec-flac".into())
            .spawn(move || {
                while !me.stop.load(Ordering::Acquire) {
                    me.drain_all();
                    std::thread::park_timeout(IDLE);
                }
            })
            .ok()?;
        let _ = self.compressor.set(handle.thread().clone());
        Some(handle)
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.compressor.get() {
            t.unpark();
        }
    }

    pub fn set_tracks(&self, tracks: Vec<Track>) {
        let mut slots = self.slots.write().unwrap_or_else(|e| e.into_inner());
        *slots = tracks.into_iter().map(Slot::new).collect();
        self.bytes.store(0, Ordering::Relaxed);
    }

    /// Thread di cattura: copia nella coda (nessuna allocazione). Solo se il compressore è
    /// indietro di più di [`QUEUE_SECS`] la traccia viene compressa qui, per non perdere audio.
    pub fn push(&self, track: usize, start: QpcTime, samples: &[f32]) {
        if self.flac.is_none() || samples.is_empty() {
            return;
        }
        let slots = self.slots.read().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = slots.get(track) else { return };
        let frames = samples.len() / CHANNELS;
        let samples = &samples[..frames * CHANNELS];
        for attempt in 0..2 {
            {
                let mut q = lock(&slot.input);
                if q.samples.slots() >= samples.len() && q.chunks.slots() >= 1 {
                    if q.samples.push_entire_slice(samples).is_ok() {
                        let _ = q.chunks.push((start, frames));
                    }
                    break;
                }
            }
            if attempt == 0 {
                tracing::warn!("compressore FLAC in ritardo: compressione sul thread di cattura");
                self.drain(slot);
            } else {
                tracing::error!("coda audio piena: {frames} campioni persi");
            }
        }
        if let Some(t) = self.compressor.get() {
            t.unpark();
        }
    }

    fn keep(&self) -> Keep {
        let video_from = self.video_from.load(Ordering::Relaxed);
        Keep {
            audio_hns: self.keep_audio_hns,
            video_from: (video_from != i64::MAX).then_some(QpcTime(video_from)),
            video_max_hns: self.keep_video_max_hns,
        }
    }

    /// Comprime tutto ciò che è in coda per una traccia.
    pub fn drain(&self, slot: &Slot) {
        let Some(flac) = &self.flac else { return };
        let mut out = lock(&slot.output);
        let out = &mut *out;
        while let Ok((start, frames)) = out.chunks.pop() {
            let Ok(chunk) = out.samples.read_chunk(frames * CHANNELS) else { break };
            let (a, b) = chunk.as_slices();
            out.scratch.clear();
            out.scratch.extend_from_slice(a);
            out.scratch.extend_from_slice(b);
            chunk.commit_all();
            let mut t = lock(&slot.track);
            let before = t.ring.allocated_bytes();
            t.push(flac, start, &out.scratch, self.keep());
            let after = t.ring.allocated_bytes();
            if after >= before {
                self.bytes.fetch_add(after - before, Ordering::Relaxed);
            } else {
                self.bytes.fetch_sub(before - after, Ordering::Relaxed);
            }
        }
    }

    pub fn drain_all(&self) {
        let slots = self.slots.read().unwrap_or_else(|e| e.into_inner());
        for slot in slots.iter() {
            self.drain(slot);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
