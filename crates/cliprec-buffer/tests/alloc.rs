//! Allocazioni nel percorso caldo, contate con un allocatore globale (binario di test a parte).
//! Si contano solo le allocazioni del thread che chiama `push` (il thread di cattura), non
//! quelle del thread di compressione FLAC.
#![allow(clippy::unwrap_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use cliprec_buffer::{BufferConfig, ReplayBuffer};
use cliprec_core::time::frames_to_hns;
use cliprec_core::{AudioTrackSink, HNS_PER_SEC, PacketRef, QpcTime, VideoCodec, VideoStreamParams};

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

// SAFETY: delega tutto a `System`, aggiunge solo contatori atomici e un flag per thread.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        // SAFETY: stesso contratto di `GlobalAlloc::alloc`.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: stesso contratto di `GlobalAlloc::dealloc`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocazioni fatte da questo thread durante `f`.
fn counted(f: impl FnOnce()) -> (usize, usize) {
    let (a, b) = (ALLOCS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed));
    COUNTING.with(|c| c.set(true));
    f();
    COUNTING.with(|c| c.set(false));
    (ALLOCS.load(Ordering::Relaxed) - a, BYTES.load(Ordering::Relaxed) - b)
}

/// Un solo test per binario: i contatori sono globali.
#[test]
fn hot_path_allocations() {
    let b = ReplayBuffer::new(BufferConfig { video_seconds: 1.0, audio_seconds: 1.0, max_bytes: 64 << 20, lock_pages: false });
    b.set_video_params(VideoStreamParams {
        codec: VideoCodec::H264,
        width: 64,
        height: 64,
        fps: 60,
        extradata: vec![1],
        bit_rate: 8_000_000,
        encoder_name: "test".into(),
    });
    let data = vec![7u8; 16_000];
    let frame = HNS_PER_SEC / 60;
    let push = |i: i64| {
        let pts = QpcTime(i * frame);
        b.push_video(PacketRef { data: &data, pts, dts: pts, duration: frame, keyframe: i % 60 == 0 });
    };
    // A regime (ring pieno, blocchi riciclati) il video non alloca.
    (0..600).for_each(push);
    let (video_allocs, _) = counted(|| (600..1200).for_each(push));
    println!("video: {video_allocs} allocazioni in 600 pacchetti a regime");
    assert_eq!(video_allocs, 0);

    // Audio (M7): il thread di cattura non deve allocare a regime, nemmeno quando si completa un
    // blocco FLAC da 4096 frame.
    b.set_tracks(&["A".into()]);
    let chunk: Vec<f32> = (0..960).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
    let mut f = 0i64;
    let mut push_audio = |n: usize| {
        for _ in 0..n {
            b.push(0, QpcTime(frames_to_hns(f)), &chunk);
            f += 480;
            // Ritmo reale (10 ms) non serve: basta lasciare al compressore il tempo di svuotare.
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    };
    push_audio(400); // 4 s: il ring audio è pieno e ricicla i blocchi
    let blocks = 100 * 480 / 4096;
    let (allocs, bytes) = counted(|| push_audio(100));
    println!("audio: {allocs} allocazioni ({bytes} B) sul thread di cattura in {blocks} blocchi FLAC");
    assert_eq!(allocs, 0, "allocazioni sul thread di cattura");
}
