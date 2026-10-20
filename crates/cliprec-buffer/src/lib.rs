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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock};

use cliprec_core::time::{frames_to_hns, hns_to_frames};
use cliprec_core::{
    AudioSnapshot, AudioTrackSink, EncodedPacket, HNS_PER_SEC, PacketRef, QpcTime, VideoSnapshot, VideoStreamParams,
};
use flacenc::config::Encoder;
use flacenc::error::{Verified, Verify};

use crate::arena::ChunkRing;
use crate::audio::Track;

const VIDEO_CHUNK: usize = 4 << 20;

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

/// Buffer di replay condiviso (`Arc<ReplayBuffer>`) tra encoder, motore audio e salvataggio.
pub struct ReplayBuffer {
    config: BufferConfig,
    video: Mutex<Video>,
    tracks: RwLock<Vec<Mutex<Track>>>,
    audio_bytes: AtomicUsize,
    flac: Option<Verified<Encoder>>,
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
        Self { config, video: Mutex::new(video), tracks: RwLock::new(Vec::new()), audio_bytes: AtomicUsize::new(0), flac }
    }

    pub fn config(&self) -> &BufferConfig {
        &self.config
    }

    /// Imposta i parametri dello stream video. Se cambiano, i pacchetti vecchi (non più
    /// compatibili) vengono scartati. Prealloca l'arena stimata dal bitrate.
    pub fn set_video_params(&self, params: VideoStreamParams) {
        let mut v = lock(&self.video);
        if v.params.as_ref() == Some(&params) {
            return;
        }
        v.ring.clear();
        v.keyframes.clear();
        v.memory_limited = false;
        let estimate = BufferConfig::estimate_bytes(params.bit_rate, self.config.video_seconds, 0.0, 0);
        v.ring.reserve(estimate.min(self.config.max_bytes));
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
        // Durata: scarta il GOP più vecchio se il successivo copre già i secondi richiesti.
        let oldest = v.newest_end.add_hns(-secs_to_hns(self.config.video_seconds));
        while v.keyframes.len() > 2 && v.keyframes.get(1).and_then(|&k| v.pts(k)).is_some_and(|t| t <= oldest) {
            v.evict_gop();
            v.memory_limited = false;
        }
    }

    /// Pacchetti degli ultimi `seconds` prima di `end`, misurati dalla fine dell'ultimo frame
    /// completo: dall'ultimo keyframe con pts ≤ `fine - seconds` (o dal più vecchio disponibile)
    /// all'ultimo frame completo, così la clip non è mai più corta di `seconds` se il buffer li ha.
    ///
    /// Con i B-frame la coda in ordine di decodifica può contenere un P-frame i cui B-frame
    /// precedenti non sono ancora usciti dall'encoder: la clip si tronca al primo pacchetto oltre
    /// il primo buco di pts, e finisce su un frame decodificabile senza salti.
    pub fn snapshot_video(&self, seconds: f64, end: QpcTime) -> Option<VideoSnapshot> {
        let mut v = lock(&self.video);
        let params = v.params.clone()?;
        let frame = HNS_PER_SEC / i64::from(params.fps.max(1));
        let mut shown: Vec<QpcTime> =
            v.ring.iter().filter(|(_, e)| e.meta.dts <= end).map(|(_, e)| e.meta.pts).collect();
        shown.sort_unstable();
        let cut = shown.windows(2).find(|w| w[1].hns_since(w[0]) > frame * 3 / 2).map_or(*shown.last()?, |w| w[0]);
        // Ultimo pacchetto tenuto (ordine di decodifica) e fine dell'ultimo frame tenuto.
        let mut stop = None;
        let mut last_end = None;
        for (seq, e) in v.ring.iter().take_while(|(_, e)| e.meta.dts <= end && e.meta.pts <= cut) {
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

    /// Tracce audio da `start` a `end`, tagliate tutte allo stesso campione e lunghe uguali,
    /// silenzio dove mancano dati. Se nessuna traccia ha dati così vecchi, `start` avanza al
    /// primo campione disponibile (lo dice `AudioSnapshot::start`). `None` senza tracce o dati.
    pub fn snapshot_audio(&self, start: QpcTime, end: QpcTime) -> Option<AudioSnapshot> {
        let tracks = self.tracks.read().unwrap_or_else(|e| e.into_inner());
        let earliest = tracks.iter().filter_map(|t| lock(t).first()).min()?;
        let start = start.max(earliest);
        if end <= start {
            return None;
        }
        // Sotto lock solo i riferimenti ai blocchi compressi, una traccia alla volta.
        let copies: Vec<_> = tracks.iter().map(|t| lock(t).copy(start, end)).collect();
        drop(tracks);
        let frames = usize::try_from(hns_to_frames(end.hns_since(start))).ok()?;
        let tracks = copies.into_iter().map(|c| c.render(start, frames)).collect();
        Some(AudioSnapshot { start, tracks })
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

    fn audio_keep_hns(&self) -> i64 {
        secs_to_hns(self.config.video_seconds.max(self.config.audio_seconds)) + frames_to_hns(audio::BLOCK_FRAMES as i64)
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
        t.push(flac, start, samples, self.audio_keep_hns());
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
