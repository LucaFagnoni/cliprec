use std::sync::Arc;

use cliprec_core::time::frames_to_hns;
use cliprec_core::{VideoCodec, VideoStreamParams};
use proptest::prelude::*;

use super::*;

const MB: usize = 1 << 20;

fn params() -> VideoStreamParams {
    VideoStreamParams {
        codec: VideoCodec::H264,
        width: 64,
        height: 64,
        fps: 50,
        extradata: vec![1, 2, 3],
        bit_rate: 1_000_000,
        encoder_name: "test".into(),
    }
}

fn buffer(video_seconds: f64, max_bytes: usize) -> ReplayBuffer {
    let b = ReplayBuffer::new(BufferConfig { video_seconds, audio_seconds: 2.0, max_bytes, lock_pages: false });
    b.set_video_params(params());
    b
}

/// Pacchetto `seq` a 50 fps: i byte iniziano con il numero progressivo, per riconoscerlo.
fn packet_bytes(seq: u64, size: usize) -> Vec<u8> {
    let mut v = vec![(seq % 251) as u8; size.max(8)];
    v[..8].copy_from_slice(&seq.to_le_bytes());
    v
}

const FRAME_HNS: i64 = HNS_PER_SEC / 50;

fn push(b: &ReplayBuffer, seq: u64, size: usize, keyframe: bool) {
    let pts = QpcTime(1_000_000_000 + seq as i64 * FRAME_HNS);
    let data = packet_bytes(seq, size);
    b.push_video(PacketRef { data: &data, pts, dts: pts, duration: FRAME_HNS, keyframe });
}

fn seq_of(p: &EncodedPacket) -> u64 {
    u64::from_le_bytes(p.data[..8].try_into().unwrap())
}

/// Controlla che lo snapshot sia una sequenza contigua di pacchetti integri che parte da un keyframe.
fn check_snapshot(s: &VideoSnapshot, sizes: &[usize]) {
    assert!(s.packets[0].keyframe, "lo snapshot deve partire da un keyframe");
    let first = seq_of(&s.packets[0]);
    for (i, p) in s.packets.iter().enumerate() {
        let seq = first + i as u64;
        assert_eq!(seq_of(p), seq);
        assert_eq!(&*p.data, &packet_bytes(seq, sizes[seq as usize])[..]);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Scarto a GOP interi, durata coperta, tetto di memoria e taglio al keyframe.
    #[test]
    fn ring_invariants(
        gops in prop::collection::vec((1usize..40, 1usize..300_000), 1..40),
        video_seconds in 0.5f64..4.0,
        cap_mb in prop::sample::select(vec![8usize, 12, 64]),
        cut in 0.0f64..5.0,
    ) {
        let max_bytes = cap_mb * MB;
        let b = buffer(video_seconds, max_bytes);
        let mut sizes = Vec::new();
        let mut keyframes = Vec::new();
        for (len, size) in &gops {
            for i in 0..*len {
                let seq = sizes.len() as u64;
                let size = if i == 0 { *size } else { size / 4 };
                sizes.push(size);
                if i == 0 { keyframes.push(seq); }
                push(&b, seq, size, i == 0);

                let st = b.stats();
                prop_assert!(st.keyframes >= keyframes.len().min(1));
                // Mai sopra il tetto, salvo un unico GOP più grande del tetto.
                prop_assert!(st.bytes <= max_bytes || st.keyframes == 1, "{} > {}", st.bytes, max_bytes);
                let kept = b.retained();
                let first = kept[0].0;
                // Il ring è un suffisso di quanto spinto che parte da un keyframe: GOP interi.
                prop_assert!(kept[0].1 && keyframes.contains(&first));
                prop_assert!(kept.iter().enumerate().all(|(i, &(s, _))| s == first + i as u64));
                prop_assert_eq!(kept.last().unwrap().0, seq);
                if cap_mb == 64 {
                    prop_assert!(!st.memory_limited);
                    // Senza pressione di memoria: almeno due keyframe e N secondi coperti
                    // (se già spinti), mai più di N + un GOP.
                    let pushed_kf = keyframes.len();
                    prop_assert!(st.keyframes >= pushed_kf.min(2));
                    let pushed_secs = (seq + 1) as f64 / 50.0;
                    prop_assert!(st.video_seconds + 1e-9 >= video_seconds.min(pushed_secs) || first == 0);
                    let kf_pos = keyframes.iter().position(|&k| k == first).unwrap();
                    if st.keyframes > 2 {
                        let second = keyframes[kf_pos + 1];
                        prop_assert!((seq + 1 - second) as f64 / 50.0 < video_seconds + 1e-9);
                    }
                }
            }
        }
        // Taglio: parte dall'ultimo keyframe con pts ≤ end - cut, o dal più vecchio.
        let end = QpcTime(1_000_000_000 + sizes.len() as i64 * FRAME_HNS);
        let s = b.snapshot_video(cut, end).unwrap();
        check_snapshot(&s, &sizes);
        let target = end.add_hns(-(cut * HNS_PER_SEC as f64).round() as i64);
        let start = s.packets[0].pts;
        let oldest = b.snapshot_video(1e9, end).unwrap().packets[0].pts;
        prop_assert!(start <= target || start == oldest);
        // Nessun keyframe dopo il primo ha ancora pts ≤ target.
        prop_assert!(s.packets.iter().skip(1).all(|p| !p.keyframe || p.pts > target));
    }
}

#[test]
fn memory_cap_reports_fewer_seconds() {
    // 20 s richiesti, 1 MB/s di pacchetti, tetto 8 MB: il buffer copre meno e lo dice.
    let b = buffer(20.0, 8 * MB);
    let sizes = vec![20_000; 50 * 20];
    for (seq, &size) in sizes.iter().enumerate() {
        push(&b, seq as u64, size, seq % 25 == 0);
    }
    let st = b.stats();
    assert!(st.memory_limited, "{st:?}");
    assert!(st.video_seconds < 10.0, "{st:?}");
    assert!(st.bytes <= 8 * MB, "{st:?}");
    check_snapshot(&b.snapshot_video(60.0, QpcTime(i64::MAX)).unwrap(), &sizes);
}

#[test]
fn snapshot_while_writing() {
    let b = Arc::new(buffer(1.0, 64 * MB));
    let n = 3000u64;
    let sizes: Vec<usize> = (0..n).map(|i| 1_000 + (i as usize * 7919) % 60_000).collect();
    let writer = {
        let b = Arc::clone(&b);
        let sizes = sizes.clone();
        std::thread::spawn(move || {
            for seq in 0..n {
                push(&b, seq, sizes[seq as usize], seq % 30 == 0);
            }
        })
    };
    let mut held = Vec::new();
    let mut taken = 0;
    while !writer.is_finished() {
        if let Some(s) = b.snapshot_video(0.5, QpcTime(i64::MAX)) {
            check_snapshot(&s, &sizes);
            taken += 1;
            // Tiene vivi alcuni snapshot mentre il writer ricicla i blocchi.
            if held.len() < 8 {
                held.push(s);
            }
        }
    }
    writer.join().unwrap();
    assert!(taken > 0);
    // I byte degli snapshot tenuti non sono stati sovrascritti.
    for s in &held {
        check_snapshot(s, &sizes);
    }
}

#[test]
fn params_change_clears_and_lock_pages_does_not_fail() {
    let b = ReplayBuffer::new(BufferConfig { video_seconds: 1.0, audio_seconds: 1.0, max_bytes: 16 * MB, lock_pages: true });
    b.set_video_params(params());
    push(&b, 0, 1000, true);
    assert!(b.snapshot_video(1.0, QpcTime(i64::MAX)).is_some());
    println!("locked = {}", b.stats().locked); // dipende dai privilegi; non deve fallire.
    b.set_video_params(VideoStreamParams { width: 128, ..params() });
    assert!(b.snapshot_video(1.0, QpcTime(i64::MAX)).is_none());
    // Pacchetti prima del primo keyframe ignorati.
    push(&b, 1, 1000, false);
    assert_eq!(b.stats().video_packets, 0);
}

// ---- audio ----

const ORIGIN: i64 = 5_000_000_000;

fn at(frame: i64) -> QpcTime {
    QpcTime(ORIGIN + frames_to_hns(frame))
}

/// Segnale funzione del campione assoluto, diverso per canale e traccia.
fn signal(track: usize, frame: i64, ch: usize) -> f32 {
    let x = frame as f32 * (0.01 + track as f32 * 0.003) + ch as f32;
    0.8 * x.sin()
}

/// Spinge `frames` campioni dalla posizione `from` a pezzi di dimensione variabile.
fn push_track(b: &ReplayBuffer, track: usize, from: i64, frames: i64, chunk: i64) {
    let mut f = from;
    let mut k = 0;
    while f < from + frames {
        let n = (chunk + (k * 37) % 300).min(from + frames - f);
        let s: Vec<f32> = (f..f + n).flat_map(|g| [signal(track, g, 0), signal(track, g, 1)]).collect();
        b.push(track, at(f), &s);
        f += n;
        k += 1;
    }
}

fn assert_sample(got: f32, track: usize, frame: i64, ch: usize) {
    let want = audio::quantize(signal(track, frame, ch)) as f32 / 8_388_607.0;
    assert!((got - want).abs() < 1e-6, "traccia {track} frame {frame} ch {ch}: {got} != {want}");
}

#[test]
fn flac_round_trip() {
    let b = buffer(1.0, 64 * MB);
    b.set_tracks(&["Sistema".into()]);
    let total = 48_000 + 1234; // blocchi pieni più un parziale non compresso
    push_track(&b, 0, 0, total, 480);
    let s = b.snapshot_audio(at(0), at(total)).unwrap();
    assert_eq!(s.start, at(0));
    let t = &s.tracks[0];
    assert_eq!(t.name, "Sistema");
    assert_eq!(t.samples.len(), total as usize * 2);
    for f in 0..total {
        for ch in 0..2 {
            assert_sample(t.samples[f as usize * 2 + ch], 0, f, ch);
        }
    }
    // Compressione: meno del PCM a 24 bit.
    assert!(b.stats().audio_seconds > 1.0);
}

#[test]
fn tracks_aligned_to_the_sample() {
    let b = buffer(2.0, 64 * MB);
    b.set_tracks(&["A".into(), "B".into()]);
    // B parte 505 campioni dopo A, con pezzi di altra dimensione.
    push_track(&b, 0, 0, 96_000, 441);
    push_track(&b, 1, 505, 96_000 - 505, 960);
    let (from, to) = (300, 90_000);
    let s = b.snapshot_audio(at(from), at(to)).unwrap();
    assert_eq!(s.start, at(from));
    assert_eq!(s.tracks.len(), 2);
    let n = (to - from) as usize;
    for t in &s.tracks {
        assert_eq!(t.samples.len(), n * 2);
    }
    for i in 0..n {
        let g = from + i as i64;
        for ch in 0..2 {
            assert_sample(s.tracks[0].samples[i * 2 + ch], 0, g, ch);
            if g < 505 {
                assert_eq!(s.tracks[1].samples[i * 2 + ch], 0.0, "silenzio prima che B inizi");
            } else {
                assert_sample(s.tracks[1].samples[i * 2 + ch], 1, g, ch);
            }
        }
    }
    // Richiesta più vecchia dei dati: parte dal primo campione disponibile.
    let s = b.snapshot_audio(at(-48_000), at(1000)).unwrap();
    assert_eq!(s.start, at(0));
}

#[test]
fn audio_keeps_only_the_longest_duration() {
    let b = buffer(1.0, 64 * MB); // audio_seconds 2.0
    b.set_tracks(&["A".into()]);
    push_track(&b, 0, 0, 48_000 * 6, 480);
    let st = b.stats();
    assert!(st.audio_seconds >= 2.0 && st.audio_seconds < 2.5, "{st:?}");
}
