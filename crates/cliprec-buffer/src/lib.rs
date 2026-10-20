//! Ring buffer in RAM: pacchetti video a GOP interi, audio FLAC per traccia, pagine bloccate, snapshot senza copie.
//!
//! Contratti in `cliprec-core`; vedi SPEC.md, sezione «Buffer in RAM e salvataggio clip».
//!
//! - Video: i byte dei pacchetti vanno in un'arena di blocchi da 4 MiB preallocati e riciclati
//!   ([`arena`]); il buffer tiene `video_seconds` più un GOP (sempre almeno due keyframe) e scarta
//!   i GOP più vecchi, interi, quando supera la durata o il tetto di memoria.
//! - Audio: ogni traccia è compressa in blocchi FLAC indipendenti (4096 frame, 24 bit), ognuno con
//!   l'istante QPC del primo campione; si tengono `max(video_seconds, audio_seconds)`.
//! - Gli snapshot prendono i lock solo per clonare riferimenti `Arc` ai blocchi (video e audio);
//!   la decompressione avviene fuori dai lock. Il buffer non tocca mai il disco.

mod arena;
mod audio;
mod lock;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock};

use cliprec_core::time::hns_to_frames;
use cliprec_core::{
    AudioSnapshot, AudioTrackSink, EncodedPacket, HNS_PER_SEC, PacketRef, QpcTime, VideoSnapshot, VideoStreamParams,
};
use flacenc::config::Encoder;
use flacenc::error::{Verified, Verify};

use crate::arena::ChunkRing;
use crate::audio::{Keep, Track};
pub use crate::audio::TrackReader;

const VIDEO_CHUNK: usize = 4 << 20;
/// Video conservato oltre `video_seconds`, oltre al GOP in corso.
const RETAIN_MARGIN_SECS: f64 = 0.5;
/// GOP più lungo che l'audio segue: oltre, l'audio conservato non insegue il video.
const MAX_GOP_SECS: f64 = 10.0;
/// Posizioni di pts finali (in ordine di presentazione) in cui un buco indica B-frame non ancora
/// usciti dall'encoder: profondità dei B-frame (2) più uno.
pub const TAIL_DEPTH: usize = 3;
/// Frazione della stima dal tetto di bitrate preallocata subito.
const PREALLOC_DIVISOR: usize = 4;

#[derive(Clone, Debug, PartialEq)]
pub struct BufferConfig {
    /// N: secondi di video da garantire (il buffer ne tiene in più fino a un GOP).
    pub video_seconds: f64,
    /// M: secondi della clip audio. L'audio copre `max(video_seconds, audio_seconds)`.
    pub audio_seconds: f64,
    /// Tetto della memoria di video e audio insieme, in byte.
    pub max_bytes: usize,
    /// Blocca le pagine in RAM con `VirtualLock`.
    pub lock_pages: bool,
}

impl BufferConfig {
    /// Stima della memoria necessaria: bitrate × (N + 2 s di margine per il GOP) più l'audio
    /// FLAC a 24 bit (stimato a 2/3 del PCM: circa 190 kB/s per traccia) per `max(N, M)`.
    pub fn estimate_bytes(bit_rate: u64, video_seconds: f64, audio_seconds: f64, tracks: usize) -> usize {
        let video = bit_rate as f64 / 8.0 * (video_seconds + 2.0);
        let audio = 192_000.0 * video_seconds.max(audio_seconds) * tracks as f64;
        (video + audio) as usize
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BufferStats {
    /// Secondi di video coperti, dal keyframe più vecchio alla fine dell'ultimo pacchetto.
    pub video_seconds: f64,
    /// Secondi di audio coperti da tutte le tracce (la più corta).
    pub audio_seconds: f64,
    /// Memoria delle arene (video e audio, blocchi in uso e liberi).
    pub bytes: usize,
    /// Pagine bloccate in RAM; `false` se `VirtualLock` non è riuscito o non è richiesto.
    pub locked: bool,
    /// Il tetto di memoria ha scartato video: i secondi coperti sono meno di `video_seconds`.
    pub memory_limited: bool,
    pub video_packets: usize,
    pub keyframes: usize,
    /// Fine (pts + durata) dell'ultimo frame video nel buffer; `None` se è vuoto. Se è molto
    /// prima della pressione dell'hotkey, la clip salvata sarebbe vecchia (encoder fermo).
    pub newest_end: Option<QpcTime>,
}

#[derive(Clone, Copy)]
struct VideoMeta {
    pts: QpcTime,
    dts: QpcTime,
    duration: i64,
    keyframe: bool,
}

struct Video {
    params: Option<VideoStreamParams>,
    ring: ChunkRing<VideoMeta>,
    /// Numeri progressivi dei keyframe nel ring, dal più vecchio.
    keyframes: VecDeque<u64>,
    /// Fine (pts + durata) più avanzata vista.
    newest_end: QpcTime,
    memory_limited: bool,
}

impl Video {
    /// Scarta il GOP più vecchio, intero.
    fn evict_gop(&mut self) {
        let Some(&next) = self.keyframes.get(1) else { return };
        while self.ring.first_seq() < next && self.ring.pop_front().is_some() {}
        self.keyframes.pop_front();
    }

    fn pts(&self, seq: u64) -> Option<QpcTime> {
        self.ring.get(seq).map(|e| e.meta.pts)
    }

    fn covered_secs(&self) -> f64 {
        let first = self.keyframes.front().and_then(|&k| self.pts(k));
        first.map_or(0.0, |f| self.newest_end.hns_since(f) as f64 / HNS_PER_SEC as f64)
    }
}

/// Audio di una clip non ancora decompresso: riferimenti ai blocchi FLAC di ogni traccia.
pub struct AudioRefs {
    start: QpcTime,
    frames: usize,
    tracks: Vec<audio::TrackCopy>,
}

impl AudioRefs {
    /// Istante QPC del primo campione di ogni traccia.
    pub fn start(&self) -> QpcTime {
        self.start
    }

    /// Lunghezza di ogni traccia, in frame stereo a 48 kHz.
    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    pub fn track_name(&self, track: usize) -> Option<&str> {
        self.tracks.get(track).map(|t| t.name())
    }

    /// Lettore in streaming della traccia `track`: decomprime un blocco FLAC alla volta, così il
    /// salvataggio non tiene mai in memoria il PCM intero (M5).
    pub fn reader(&self, track: usize) -> Option<TrackReader<'_>> {
        self.tracks.get(track).map(|t| t.reader(self.start, self.frames))
    }

    /// Decomprime e taglia tutte le tracce allo stesso campione e alla stessa lunghezza, con
    /// silenzio dove mancano dati. Tiene tutto il PCM in memoria: per salvare meglio `reader`.
    pub fn render(self) -> AudioSnapshot {
        let (start, frames) = (self.start, self.frames);
        AudioSnapshot { start, tracks: self.tracks.iter().map(|c| audio::render(c, start, frames)).collect() }
    }
}

/// Buffer di replay condiviso (`Arc<ReplayBuffer>`) tra encoder, motore audio e salvataggio.
pub struct ReplayBuffer {
    config: BufferConfig,
    video: Mutex<Video>,
    tracks: RwLock<Vec<Mutex<Track>>>,
    audio_bytes: AtomicUsize,
    flac: Option<Verified<Encoder>>,
    /// pts del keyframe più vecchio conservato (`i64::MAX` senza video): l'audio lo copre (M4).
    video_from: AtomicI64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ReplayBuffer {
    pub fn new(config: BufferConfig) -> Self {
        let flac = Encoder::default().into_verified().ok();
        if flac.is_none() {
            tracing::error!("configurazione FLAC non valida: audio disattivato");
        }
        let video = Video {
            params: None,
            ring: ChunkRing::new(VIDEO_CHUNK, config.lock_pages, config.max_bytes),
            keyframes: VecDeque::new(),
            newest_end: QpcTime(0),
            memory_limited: false,
        };
        Self {
            config,
            video: Mutex::new(video),
            tracks: RwLock::new(Vec::new()),
            audio_bytes: AtomicUsize::new(0),
            flac,
            video_from: AtomicI64::new(i64::MAX),
        }
    }

    pub fn config(&self) -> &BufferConfig {
        &self.config
    }

    /// Imposta i parametri dello stream video. Se cambiano, i pacchetti vecchi (non più
    /// compatibili) vengono scartati. Prealloca un quarto della stima dal tetto di bitrate (con
    /// qualità costante il flusso reale sta di solito molto sotto il tetto); oltre, l'arena cresce a
    /// blocchi da 4 MiB, bloccati e poi riciclati, fino a `max_bytes`.
    pub fn set_video_params(&self, params: VideoStreamParams) {
        let mut v = lock(&self.video);
        if v.params.as_ref() == Some(&params) {
            return;
        }
        v.ring.clear();
        v.keyframes.clear();
        v.memory_limited = false;
        v.newest_end = QpcTime(0);
        self.video_from.store(i64::MAX, Ordering::Relaxed);
        let estimate = BufferConfig::estimate_bytes(params.bit_rate, self.config.video_seconds, 0.0, 0);
        v.ring.reserve((estimate / PREALLOC_DIVISOR).min(self.config.max_bytes));
        v.params = Some(params);
    }

    /// Copia un pacchetto nel buffer. I pacchetti prima del primo keyframe vengono ignorati.
    pub fn push_video(&self, p: PacketRef<'_>) {
        let mut v = lock(&self.video);
        if v.keyframes.is_empty() && !p.keyframe {
            return;
        }
        // Tetto di memoria: scarta GOP interi, ma tiene sempre il GOP corrente.
        let cap = self.config.max_bytes.saturating_sub(self.audio_bytes.load(Ordering::Relaxed));
        while v.ring.in_use_bytes() + v.ring.extra_needed(p.data.len()) > cap && v.keyframes.len() > 1 {
            v.evict_gop();
            v.memory_limited = true;
        }
        let seq = v.ring.next_seq();
        v.ring.push(p.data, VideoMeta { pts: p.pts, dts: p.dts, duration: p.duration, keyframe: p.keyframe });
        if p.keyframe {
            v.keyframes.push_back(seq);
        }
        v.newest_end = v.newest_end.max(p.pts.add_hns(p.duration.max(0)));
        // Durata: scarta il GOP più vecchio se il successivo copre già i secondi richiesti più
        // mezzo secondo di margine (la clip si misura dall'ultimo frame completo, che può essere
        // qualche frame prima di `newest_end`).
        let oldest = v.newest_end.add_hns(-secs_to_hns(self.config.video_seconds + RETAIN_MARGIN_SECS));
        while v.keyframes.len() > 2 && v.keyframes.get(1).and_then(|&k| v.pts(k)).is_some_and(|t| t <= oldest) {
            v.evict_gop();
            v.memory_limited = false;
        }
        let from = v.keyframes.front().and_then(|&k| v.pts(k)).map_or(i64::MAX, |t| t.0);
        self.video_from.store(from, Ordering::Relaxed);
    }

    /// Pacchetti degli ultimi `seconds` prima di `end`, misurati dalla fine dell'ultimo frame
    /// completo: dall'ultimo keyframe con pts ≤ `fine - seconds` (o dal più vecchio disponibile)
    /// all'ultimo frame completo, così la clip non è mai più corta di `seconds` se il buffer li ha.
    ///
    /// Con i B-frame la coda in ordine di decodifica può contenere un P-frame i cui B-frame
    /// precedenti non sono ancora usciti dall'encoder: se tra le ultime [`TAIL_DEPTH`] posizioni
    /// in ordine di presentazione c'è un buco di pts, la clip si ferma prima del buco e finisce su
    /// un frame decodificabile senza salti. Un buco più vecchio (tick del pacer persi) resta nel
    /// file come salto di timestamp.
    pub fn snapshot_video(&self, seconds: f64, end: QpcTime) -> Option<VideoSnapshot> {
        let mut v = lock(&self.video);
        let params = v.params.clone()?;
        let frame = HNS_PER_SEC / i64::from(params.fps.max(1));
        // Il buco si cerca solo dopo l'ultimo keyframe: i buchi interni (pacer bloccato) restano nel
        // file come salto di timestamp, non troncano la clip.
        let tail_from = v.keyframes.back().copied()?;
        let mut shown: Vec<QpcTime> = v
            .ring
            .iter()
            .filter(|(seq, e)| *seq >= tail_from && e.meta.dts <= end)
            .map(|(_, e)| e.meta.pts)
            .collect();
        shown.sort_unstable();
        let tail = &shown[shown.len().saturating_sub(TAIL_DEPTH)..];
        let cut = tail.windows(2).find(|w| w[1].hns_since(w[0]) > frame * 3 / 2).map_or(*shown.last()?, |w| w[0]);
        // Ultimo pacchetto tenuto (ordine di decodifica) e fine dell'ultimo frame tenuto.
        let mut stop = None;
        let mut last_end = None;
        for (seq, e) in v
            .ring
            .iter()
            .take_while(|(seq, e)| e.meta.dts <= end && (*seq < tail_from || e.meta.pts <= cut))
        {
            stop = Some(seq);
            let frame_end = e.meta.pts.add_hns(e.meta.duration.max(frame));
            last_end = Some(last_end.map_or(frame_end, |t: QpcTime| t.max(frame_end)));
        }
        let (stop, last_end) = (stop?, last_end?);
        // 1 ms di tolleranza: i pts sulla griglia CFR sono interi troncati e la fine dell'ultimo
        // frame può cadere un'unità prima del secondo esatto.
        let target = last_end.add_hns(-secs_to_hns(seconds) + HNS_PER_SEC / 1000);
        let first = v
            .keyframes
            .iter()
            .rev()
            .copied()
            .find(|&k| v.pts(k).is_some_and(|t| t <= target))
            .or_else(|| v.keyframes.front().copied())?;
        v.ring.seal();
        let packets: Vec<EncodedPacket> = v
            .ring
            .iter()
            .skip_while(|(seq, _)| *seq < first)
            .take_while(|(seq, _)| *seq <= stop)
            .map(|(_, e)| EncodedPacket {
                data: v.ring.share(e),
                pts: e.meta.pts,
                dts: e.meta.dts,
                duration: e.meta.duration,
                keyframe: e.meta.keyframe,
            })
            .collect();
        drop(v);
        (!packets.is_empty()).then_some(VideoSnapshot { params, packets })
    }

    /// Tracce audio da `start` a `end`: scorciatoia per `snapshot_audio_refs(..)?.render()`.
    pub fn snapshot_audio(&self, start: QpcTime, end: QpcTime) -> Option<AudioSnapshot> {
        Some(self.snapshot_audio_refs(start, end)?.render())
    }

    /// Riferimenti ai blocchi FLAC da `start` a `end`, presi sotto lock senza copiare né
    /// decomprimere (da chiamare sul thread del motore); la decompressione la fa
    /// [`AudioRefs::render`] sul thread di salvataggio. Se nessuna traccia ha dati così vecchi,
    /// `start` avanza al primo campione disponibile. `None` senza tracce o dati.
    pub fn snapshot_audio_refs(&self, start: QpcTime, end: QpcTime) -> Option<AudioRefs> {
        let tracks = self.tracks.read().unwrap_or_else(|e| e.into_inner());
        let earliest = tracks.iter().filter_map(|t| lock(t).first()).min()?;
        let start = start.max(earliest);
        if end <= start {
            return None;
        }
        let frames = usize::try_from(hns_to_frames(end.hns_since(start))).ok()?;
        let tracks = tracks.iter().map(|t| lock(t).copy(start, end)).collect();
        Some(AudioRefs { start, frames, tracks })
    }

    pub fn stats(&self) -> BufferStats {
        let v = lock(&self.video);
        let tracks = self.tracks.read().unwrap_or_else(|e| e.into_inner());
        let mut audio_locked = true;
        let mut audio_bytes = 0;
        let mut audio_seconds: Option<f64> = None;
        for t in tracks.iter() {
            let t = lock(t);
            audio_locked &= t.ring.locked();
            audio_bytes += t.ring.allocated_bytes();
            let secs = match (t.first(), t.end()) {
                (Some(a), Some(b)) => b.hns_since(a) as f64 / HNS_PER_SEC as f64,
                _ => 0.0,
            };
            audio_seconds = Some(audio_seconds.map_or(secs, |s| s.min(secs)));
        }
        BufferStats {
            video_seconds: v.covered_secs(),
            audio_seconds: audio_seconds.unwrap_or(0.0),
            bytes: v.ring.allocated_bytes() + audio_bytes,
            locked: self.config.lock_pages && v.ring.locked() && audio_locked,
            memory_limited: v.memory_limited && v.covered_secs() < self.config.video_seconds,
            video_packets: v.ring.len(),
            keyframes: v.keyframes.len(),
            newest_end: (v.ring.len() > 0).then_some(v.newest_end),
        }
    }

    /// Per i test: (primi 8 byte, keyframe) di ogni pacchetto nel ring, senza chiudere blocchi.
    #[cfg(test)]
    fn retained(&self) -> Vec<(u64, bool)> {
        let v = lock(&self.video);
        v.ring
            .iter()
            .map(|(_, e)| (u64::from_le_bytes(v.ring.bytes(e)[..8].try_into().unwrap_or_default()), e.meta.keyframe))
            .collect()
    }

    /// Audio da conservare: max(N, M) secondi e tutto il video conservato (N + margine + un GOP), con un
    /// limite se il video si ferma o ha GOP enormi.
    fn audio_keep(&self) -> Keep {
        let video_from = self.video_from.load(Ordering::Relaxed);
        Keep {
            audio_hns: secs_to_hns(self.config.audio_seconds.max(self.config.video_seconds)),
            video_from: (video_from != i64::MAX).then_some(QpcTime(video_from)),
            video_max_hns: secs_to_hns(self.config.video_seconds + RETAIN_MARGIN_SECS + MAX_GOP_SECS),
        }
    }
}

fn secs_to_hns(secs: f64) -> i64 {
    (secs * HNS_PER_SEC as f64).round() as i64
}

impl AudioTrackSink for ReplayBuffer {
    /// Sostituisce le tracce (e ne scarta i dati) se i nomi cambiano.
    fn set_tracks(&self, names: &[String]) {
        let mut tracks = self.tracks.write().unwrap_or_else(|e| e.into_inner());
        if tracks.len() == names.len() && tracks.iter().zip(names).all(|(t, n)| lock(t).name == *n) {
            return;
        }
        *tracks = names
            .iter()
            .filter_map(|n| Track::new(n.clone(), self.config.lock_pages, self.config.max_bytes).map(Mutex::new))
            .collect();
        if tracks.len() != names.len() {
            tracing::error!("encoder FLAC non inizializzabile: tracce audio disattivate");
            tracks.clear();
        }
        self.audio_bytes.store(0, Ordering::Relaxed);
    }

    fn push(&self, track: usize, start: QpcTime, samples: &[f32]) {
        let Some(flac) = &self.flac else { return };
        let tracks = self.tracks.read().unwrap_or_else(|e| e.into_inner());
        let Some(t) = tracks.get(track) else { return };
        let mut t = lock(t);
        let before = t.ring.allocated_bytes();
        t.push(flac, start, samples, self.audio_keep());
        let after = t.ring.allocated_bytes();
        if after >= before {
            self.audio_bytes.fetch_add(after - before, Ordering::Relaxed);
        } else {
            self.audio_bytes.fetch_sub(before - after, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests;
