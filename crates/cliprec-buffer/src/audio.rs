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
/// Oltre questo scarto tra lo `start` ricevuto e quello atteso c'è una discontinuità (vuoto,
/// sospensione): si apre un nuovo segmento. Sotto, i campioni sono contigui e lo scarto è
/// jitter del QPC dei pacchetti (±2 ms) o deriva del clock del dispositivo.
const DISCONTINUITY_HNS: i64 = 1_000_000; // 100 ms
/// Peso di ogni nuovo scarto nella media mobile della deriva (jitter ±2 ms → ~0,2 ms residui).
const DRIFT_SMOOTHING: f64 = 1.0 / 64.0;
const AUDIO_CHUNK: usize = 1 << 20;

/// Quanto audio conservare: almeno `audio_hns` prima della fine, e comunque tutto il video
/// conservato (da `video_from`), ma non più di `video_max_hns` prima della fine se il video si
/// ferma. Più un blocco di margine.
#[derive(Clone, Copy)]
pub(crate) struct Keep {
    pub audio_hns: i64,
    pub video_from: Option<QpcTime>,
    pub video_max_hns: i64,
}

impl Keep {
    /// I blocchi che finiscono entro questo istante si possono scartare.
    pub fn cutoff(self, end: QpcTime) -> QpcTime {
        let block = frames_to_hns(BLOCK_FRAMES as i64);
        let audio = end.add_hns(-self.audio_hns - block);
        match self.video_from {
            Some(v) => audio.min(v.max(end.add_hns(-self.video_max_hns)).add_hns(-block)),
            None => audio,
        }
    }
}

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
    /// Segmento corrente: istante del primo campione (`None` prima del primo `push`), campioni
    /// ricevuti da allora e scarto medio (hns) tra gli `start` ricevuti e il conteggio.
    anchor: Option<QpcTime>,
    count: i64,
    drift: f64,
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
            anchor: None,
            count: 0,
            drift: 0.0,
            ring: ChunkRing::new(AUDIO_CHUNK, lock_pages, max_bytes),
        })
    }

    /// Accoda `samples` (f32 stereo interleaved) che iniziano a `start`; tiene `keep_hns` di storia.
    pub fn push(&mut self, enc: &Verified<Encoder>, start: QpcTime, samples: &[f32], keep: Keep) {
        let frames = samples.len() / CHANNELS;
        if frames == 0 {
            return;
        }
        // I campioni sono contigui: l'istante di ognuno viene dal conteggio dall'inizio del
        // segmento, corretto dalla media degli scarti (deriva); `start` serve solo ad ancorare.
        match self.anchor {
            Some(anchor) => {
                let error = start.hns_since(anchor.add_hns(frames_to_hns(self.count))) as f64;
                if (error - self.drift).abs() > DISCONTINUITY_HNS as f64 {
                    // Vuoto o salto: nuovo segmento; allo snapshot il vuoto diventa silenzio.
                    self.encode_pending(enc);
                    self.anchor = Some(start);
                    self.count = 0;
                    self.drift = 0.0;
                } else {
                    self.drift += (error - self.drift) * DRIFT_SMOOTHING;
                }
            }
            None => self.anchor = Some(start),
        }
        for frame in samples.chunks_exact(CHANNELS) {
            if self.pending.is_empty() {
                self.pending_start = self.time_at(self.count);
            }
            self.pending.extend(frame.iter().map(|&x| quantize(x)));
            self.count += 1;
            if self.pending.len() == BLOCK_FRAMES * CHANNELS {
                self.encode_pending(enc);
            }
        }
        let end = self.time_at(self.count);
        let oldest = keep.cutoff(end);
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

    /// Istante del campione `n` del segmento corrente.
    fn time_at(&self, n: i64) -> QpcTime {
        let anchor = self.anchor.unwrap_or_default();
        anchor.add_hns(frames_to_hns(n) + self.drift.round() as i64)
    }

    /// Istante del primo campione conservato.
    pub fn first(&self) -> Option<QpcTime> {
        self.ring.front().map(|b| b.meta.start).or((!self.pending.is_empty()).then_some(self.pending_start))
    }

    /// Istante subito dopo l'ultimo campione conservato.
    pub fn end(&self) -> Option<QpcTime> {
        self.anchor.map(|_| self.time_at(self.count))
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
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Lettore in streaming di `frames` frame a partire da `start`: decomprime un blocco alla
    /// volta, silenzio dove mancano dati.
    pub fn reader(&self, start: QpcTime, frames: usize) -> TrackReader<'_> {
        // Posizioni calcolate dai soli metadati: il primo blocco di ogni tratto contiguo si
        // posiziona dal suo istante, i successivi si accodano al campione (niente buchi né
        // sovrapposizioni da arrotondamenti).
        let mut places = Vec::with_capacity(self.blocks.len() + 1);
        let mut next: Option<(QpcTime, i64)> = None;
        let pending = (self.pending_start, self.pending.len() / CHANNELS, Source::Pending);
        let blocks = self.blocks.iter().enumerate().map(|(i, (at, n, _))| (*at, *n, Source::Block(i)));
        for (at, n, source) in blocks.chain(std::iter::once(pending)) {
            let offset = match next {
                Some((t, off)) if at.hns_since(t).abs() <= DISCONTINUITY_HNS => off,
                _ => hns_to_frames(at.hns_since(start)),
            };
            places.push(Place { offset, frames: n, source });
            next = Some((at.add_hns(frames_to_hns(n as i64)), offset + n as i64));
        }
        TrackReader { copy: self, places, idx: 0, pos: 0, total: frames, decoded: None, spare: Vec::new() }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Source {
    Block(usize),
    Pending,
}

struct Place {
    /// Primo frame del blocco nell'uscita (può essere negativo).
    offset: i64,
    frames: usize,
    source: Source,
}

/// Legge una traccia di uno snapshot audio a pezzi, decomprimendo un blocco FLAC alla volta.
pub struct TrackReader<'a> {
    copy: &'a TrackCopy,
    places: Vec<Place>,
    idx: usize,
    pos: usize,
    total: usize,
    /// Ultimo blocco decompresso (indice in `places`), `None` se illeggibile.
    decoded: Option<(usize, Option<claxon::frame::Block>)>,
    spare: Vec<i32>,
}

impl TrackReader<'_> {
    /// Frame ancora da leggere.
    pub fn remaining(&self) -> usize {
        self.total - self.pos
    }

    /// Riempie `out` (f32 stereo interleaved) con i frame successivi; restituisce quanti frame
    /// ha scritto (0 alla fine).
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        let want = (out.len() / CHANNELS).min(self.remaining());
        let mut w = 0;
        while w < want {
            let p = (self.pos + w) as i64;
            while self.places.get(self.idx).is_some_and(|pl| pl.offset + pl.frames as i64 <= p) {
                self.idx += 1;
            }
            let Some(pl) = self.places.get(self.idx) else {
                out[w * CHANNELS..want * CHANNELS].fill(0.0);
                break;
            };
            if pl.offset > p {
                let gap = ((pl.offset - p) as usize).min(want - w);
                out[w * CHANNELS..(w + gap) * CHANNELS].fill(0.0);
                w += gap;
                continue;
            }
            let from = (p - pl.offset) as usize;
            let k = (pl.frames - from).min(want - w);
            let dst = &mut out[w * CHANNELS..(w + k) * CHANNELS];
            match pl.source {
                Source::Pending => {
                    for (d, &x) in dst.iter_mut().zip(&self.copy.pending[from * CHANNELS..]) {
                        *d = x as f32 / SCALE;
                    }
                }
                Source::Block(b) => {
                    let block = self.decode(b);
                    match block {
                        Some(block) => {
                            for (i, frame) in dst.chunks_exact_mut(CHANNELS).enumerate() {
                                for (ch, d) in frame.iter_mut().enumerate() {
                                    *d = block.sample(ch as u32, (from + i) as u32) as f32 / SCALE;
                                }
                            }
                        }
                        None => dst.fill(0.0),
                    }
                }
            }
            w += k;
        }
        self.pos += want;
        want
    }

    /// Blocco `b` decompresso (tenuto finché non serve il successivo).
    fn decode(&mut self, b: usize) -> Option<&claxon::frame::Block> {
        if self.decoded.as_ref().is_none_or(|(i, _)| *i != b) {
            if let Some((_, Some(old))) = self.decoded.take() {
                self.spare = old.into_buffer();
            }
            let (_, n, data) = &self.copy.blocks[b];
            let block = match claxon::frame::FrameReader::new(Cursor::new(&**data)).read_next_or_eof(std::mem::take(&mut self.spare)) {
                Ok(Some(block)) if block.duration() as usize == *n && block.channels() as usize == CHANNELS => Some(block),
                _ => {
                    tracing::error!(track = %self.copy.name, "blocco FLAC illeggibile: sostituito da silenzio");
                    None
                }
            };
            self.decoded = Some((b, block));
        }
        self.decoded.as_ref().and_then(|(_, block)| block.as_ref())
    }

    /// Legge tutto in memoria.
    pub fn read_all(mut self) -> Vec<f32> {
        let mut out = vec![0.0f32; self.remaining() * CHANNELS];
        self.read(&mut out);
        out
    }
}

/// Traccia intera decompressa (per `AudioRefs::render`).
pub(crate) fn render(copy: &TrackCopy, start: QpcTime, frames: usize) -> AudioTrackPcm {
    AudioTrackPcm { name: copy.name.clone(), samples: copy.reader(start, frames).read_all() }
}
