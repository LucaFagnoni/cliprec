//! Ring audio per traccia: blocchi FLAC (24 bit, 4096 frame) con l'istante QPC del primo campione.

use std::io::Cursor;

use cliprec_core::time::{frames_to_hns, hns_to_frames};
use cliprec_core::{AudioTrackPcm, CHANNELS, PacketData, QpcTime, SAMPLE_RATE};
use flacenc::bitsink::MemSink;
use flacenc::component::{BitRepr, StreamInfo};
use flacenc::config::Encoder;
use flacenc::error::Verified;
use flacenc::source::{Fill, FrameBuf};

use crate::arena::ChunkRing;

pub(crate) const BLOCK_FRAMES: usize = 4096;
const BITS: usize = 24;
const SCALE: f32 = 8_388_607.0; // 2^23 - 1
/// Scarto massimo tra l'istante QPC di un blocco e quello atteso per considerarlo contiguo.
const CONTIGUOUS_HNS: i64 = 200_000; // 20 ms
const AUDIO_CHUNK: usize = 1 << 20;

pub(crate) struct BlockMeta {
    start: QpcTime,
    frames: usize,
}

pub(crate) struct Track {
    pub name: String,
    stream_info: StreamInfo,
    framebuf: FrameBuf,
    sink: MemSink<u8>,
    frame_no: usize,
    /// Campioni interleaved a 24 bit non ancora compressi (meno di un blocco).
    pending: Vec<i32>,
    pending_start: QpcTime,
    /// Istante subito dopo l'ultimo campione ricevuto; `None` prima del primo.
    end: Option<QpcTime>,
    pub ring: ChunkRing<BlockMeta>,
}

/// Riferimenti ai blocchi di una traccia presi sotto lock (senza copiarne i byte), da
/// decomprimere fuori.
pub(crate) struct TrackCopy {
    name: String,
    blocks: Vec<(QpcTime, usize, PacketData)>,
    pending: Vec<i32>,
    pending_start: QpcTime,
}

pub(crate) fn quantize(x: f32) -> i32 {
    (x.clamp(-1.0, 1.0) * SCALE).round() as i32
}

impl Track {
    pub fn new(name: String, lock_pages: bool, max_bytes: usize) -> Option<Self> {
        let stream_info = StreamInfo::new(SAMPLE_RATE as usize, CHANNELS, BITS).ok()?;
        let framebuf = FrameBuf::with_size(CHANNELS, BLOCK_FRAMES).ok()?;
        Some(Self {
            name,
            stream_info,
            framebuf,
            sink: MemSink::with_capacity(BLOCK_FRAMES * CHANNELS * 32),
            frame_no: 0,
            pending: Vec::with_capacity(BLOCK_FRAMES * CHANNELS),
            pending_start: QpcTime(0),
            end: None,
            ring: ChunkRing::new(AUDIO_CHUNK, lock_pages, max_bytes),
        })
    }

    /// Accoda `samples` (f32 stereo interleaved) che iniziano a `start`; tiene `keep_hns` di storia.
    pub fn push(&mut self, enc: &Verified<Encoder>, start: QpcTime, samples: &[f32], keep_hns: i64) {
        let frames = samples.len() / CHANNELS;
        if frames == 0 {
            return;
        }
        // Un salto nei timestamp (riavvio della sorgente): chiude il blocco parziale e riparte.
        if let Some(end) = self.end
            && start.hns_since(end).abs() > CONTIGUOUS_HNS
        {
            self.encode_pending(enc);
        }
        for (i, frame) in samples.chunks_exact(CHANNELS).enumerate() {
            if self.pending.is_empty() {
                self.pending_start = start.add_hns(frames_to_hns(i as i64));
            }
            self.pending.extend(frame.iter().map(|&x| quantize(x)));
            if self.pending.len() == BLOCK_FRAMES * CHANNELS {
                self.encode_pending(enc);
            }
        }
        let end = start.add_hns(frames_to_hns(frames as i64));
        self.end = Some(end);
        let oldest = end.add_hns(-keep_hns);
        while self.ring.front().is_some_and(|b| b.meta.start.add_hns(frames_to_hns(b.meta.frames as i64)) <= oldest) {
            self.ring.pop_front();
        }
    }

    fn encode_pending(&mut self, enc: &Verified<Encoder>) {
        let frames = self.pending.len() / CHANNELS;
        // Meno di 32 frame (< 1 ms) non fanno un blocco FLAC valido: si scartano.
        if frames >= flacenc::constant::MIN_BLOCK_SIZE {
            let mut short;
            let fb = if frames == BLOCK_FRAMES {
                &mut self.framebuf
            } else {
                match FrameBuf::with_size(CHANNELS, frames) {
                    Ok(f) => {
                        short = f;
                        &mut short
                    }
                    Err(_) => return self.pending.clear(),
                }
            };
            let frame = fb
                .fill_interleaved(&self.pending)
                .ok()
                .and_then(|()| flacenc::encode_fixed_size_frame(enc, fb, self.frame_no, &self.stream_info).ok());
            self.sink.clear();
            match frame.map(|f| f.write(&mut self.sink)) {
                Some(Ok(())) => {
                    self.ring.push(self.sink.as_slice(), BlockMeta { start: self.pending_start, frames });
                    self.frame_no = (self.frame_no + 1) % (1 << 31);
                }
                _ => tracing::error!(track = %self.name, "compressione FLAC fallita: blocco perso"),
            }
        }
        self.pending.clear();
    }

    /// Istante del primo campione conservato.
    pub fn first(&self) -> Option<QpcTime> {
        self.ring.front().map(|b| b.meta.start).or((!self.pending.is_empty()).then_some(self.pending_start))
    }

    /// Istante subito dopo l'ultimo campione conservato.
    pub fn end(&self) -> Option<QpcTime> {
        self.end
    }

    /// Riferimenti ai blocchi che toccano `[start, end)` e copia dei campioni non ancora compressi.
    pub fn copy(&mut self, start: QpcTime, end: QpcTime) -> TrackCopy {
        self.ring.seal();
        let blocks = self
            .ring
            .iter()
            .filter(|(_, b)| b.meta.start.add_hns(frames_to_hns(b.meta.frames as i64)) > start && b.meta.start < end)
            .map(|(_, b)| (b.meta.start, b.meta.frames, self.ring.share(b)))
            .collect();
        TrackCopy {
            name: self.name.clone(),
            blocks,
            pending: self.pending.clone(),
            pending_start: self.pending_start,
        }
    }
}

impl TrackCopy {
    /// Decomprime e posiziona i campioni su `frames` frame a partire da `start`; silenzio dove
    /// mancano dati. I blocchi contigui si accodano al campione, senza ricalcolare la posizione
    /// dal loro QPC (che ha il jitter dei pacchetti WASAPI).
    pub fn render(self, start: QpcTime, frames: usize) -> AudioTrackPcm {
        let mut out = Placer { out: vec![0.0f32; frames * CHANNELS], start, next: None };
        let mut buf = Vec::new();
        for (at, n, data) in &self.blocks {
            match claxon::frame::FrameReader::new(Cursor::new(&**data)).read_next_or_eof(std::mem::take(&mut buf)) {
                Ok(Some(block)) if block.duration() as usize == *n && block.channels() as usize == CHANNELS => {
                    out.place(*at, *n, |i, ch| block.sample(ch as u32, i as u32));
                    buf = block.into_buffer();
                }
                _ => {
                    tracing::error!(track = %self.name, "blocco FLAC illeggibile: sostituito da silenzio");
                    out.next = None;
                }
            }
        }
        let pending = &self.pending;
        out.place(self.pending_start, pending.len() / CHANNELS, |i, ch| pending[i * CHANNELS + ch]);
        AudioTrackPcm { name: self.name, samples: out.out }
    }
}

struct Placer {
    out: Vec<f32>,
    start: QpcTime,
    /// Istante e posizione (in frame) attesi per il prossimo blocco contiguo.
    next: Option<(QpcTime, i64)>,
}

impl Placer {
    fn place(&mut self, at: QpcTime, n: usize, sample: impl Fn(usize, usize) -> i32) {
        let offset = match self.next {
            Some((t, off)) if at.hns_since(t).abs() <= CONTIGUOUS_HNS => off,
            _ => hns_to_frames(at.hns_since(self.start)),
        };
        let frames = self.out.len() / CHANNELS;
        for i in 0..n {
            let Ok(dst) = usize::try_from(offset + i as i64) else { continue };
            if dst >= frames {
                break;
            }
            for ch in 0..CHANNELS {
                self.out[dst * CHANNELS + ch] = sample(i, ch) as f32 / SCALE;
            }
        }
        self.next = Some((at.add_hns(frames_to_hns(n as i64)), offset + n as i64));
    }
}
