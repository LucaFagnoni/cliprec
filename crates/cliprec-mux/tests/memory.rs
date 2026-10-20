//! M5: picco di memoria (heap Rust) del salvataggio, con un allocatore che conta.
//! Binario a parte: i contatori sono globali. Le allocazioni interne di FFmpeg (av_malloc) non
//! passano di qui.
#![allow(clippy::unwrap_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use cliprec_buffer::{BufferConfig, ReplayBuffer};
use cliprec_core::config::AudioClipFormat;
use cliprec_core::time::frames_to_hns;
use cliprec_core::{AudioTrackSink, EncodedPacket, HNS_PER_SEC, PacketData, QpcTime, VideoCodec, VideoSnapshot, VideoStreamParams};
use cliprec_mux::{VideoClipOptions, write_audio_clip, write_video_clip};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: delega a `System` e tiene il conto dei byte vivi e del massimo.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(now, Ordering::Relaxed);
        // SAFETY: stesso contratto di `GlobalAlloc::alloc`.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: stesso contratto di `GlobalAlloc::dealloc`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const SECS: i64 = 60;
const ORIGIN: i64 = 50_000_000_000;
const LIMIT: usize = 2 << 20;

/// Picco di heap sopra quello di partenza durante `f`.
fn peak_during(f: impl FnOnce()) -> usize {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    f();
    PEAK.load(Ordering::Relaxed) - base
}

fn at(frame: i64) -> QpcTime {
    QpcTime(ORIGIN + frames_to_hns(frame))
}

#[test]
fn saving_keeps_memory_bounded() {
    let b = ReplayBuffer::new(BufferConfig { video_seconds: 1.0, audio_seconds: SECS as f64 + 1.0, max_bytes: 512 << 20, lock_pages: false });
    b.set_tracks(&["Gioco".into(), "Discord".into()]);
    let chunk: Vec<f32> = (0..9600).map(|i| ((i / 2) as f32 * 0.03).sin() * 0.5).collect();
    for step in 0..(SECS + 1) * 10 {
        for t in 0..2 {
            b.push(t, at(step * 4800), &chunk);
        }
    }
    let (start, end) = (at(0), at((SECS + 1) * 48_000));
    // Video finto (i byte non si decodificano) di SECS secondi sullo stesso orologio.
    let data = PacketData::from(vec![0u8, 0, 0, 1, 0x65, 0x88, 0x84, 0x00]);
    let packets = (0..SECS * 30)
        .map(|i| {
            let pts = QpcTime(ORIGIN + i * HNS_PER_SEC / 30);
            EncodedPacket { data: data.clone(), pts, dts: pts, duration: HNS_PER_SEC / 30, keyframe: i % 30 == 0 }
        })
        .collect();
    let params = VideoStreamParams {
        codec: VideoCodec::H264,
        width: 64,
        height: 64,
        fps: 30,
        extradata: vec![0, 0, 0, 1, 0x67, 0x42, 0, 0x0a, 0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80],
        bit_rate: 1_000_000,
        encoder_name: "finto".into(),
    };
    let video = VideoSnapshot { params, packets };
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/mux/memory");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let refs = b.snapshot_audio_refs(start, end).unwrap();
    let mut saved = None;
    // Salvataggio dai riferimenti ai blocchi FLAC, decompressi in streaming.
    let video_peak = peak_during(|| {
        saved = Some(write_video_clip(&dir.join("video"), &video, Some(&refs), &VideoClipOptions::default()).unwrap());
    });
    let audio_peak = peak_during(|| {
        write_audio_clip(&dir.join("audio"), &refs, AudioClipFormat::FlacPerTrack).unwrap();
    });
    println!("picco heap: clip video {} kB, clip audio {} kB", video_peak >> 10, audio_peak >> 10);
    assert!((saved.unwrap().duration_secs - SECS as f64).abs() < 0.1);
    assert!(video_peak < LIMIT, "clip video: picco di {} MB", video_peak >> 20);
    assert!(audio_peak < LIMIT, "clip audio: picco di {} MB", audio_peak >> 20);
}
