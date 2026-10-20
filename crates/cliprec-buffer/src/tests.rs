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
                    prop_assert!(st.video_seconds + 1e-9 >= (video_seconds + RETAIN_MARGIN_SECS).min(pushed_secs) || first == 0);
                    let kf_pos = keyframes.iter().position(|&k| k == first).unwrap();
                    if st.keyframes > 2 {
                        let second = keyframes[kf_pos + 1];
                        prop_assert!((seq + 1 - second) as f64 / 50.0 < video_seconds + RETAIN_MARGIN_SECS + 1e-9);
                    }
                }
            }
        }
        // Taglio: parte dall'ultimo keyframe con pts ≤ end - cut, o dal più vecchio.
        let end = QpcTime(1_000_000_000 + sizes.len() as i64 * FRAME_HNS);
        let s = b.snapshot_video(cut, end).unwrap();
        check_snapshot(&s, &sizes);
        // Lo snapshot ha 1 ms di tolleranza sul punto di taglio.
        let target = end.add_hns(-(cut * HNS_PER_SEC as f64).round() as i64 + HNS_PER_SEC / 1000);
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
fn params_change_clears_the_ring() {
    let b = ReplayBuffer::new(BufferConfig { video_seconds: 1.0, audio_seconds: 1.0, max_bytes: 16 * MB, lock_pages: false });
    b.set_video_params(params());
    push(&b, 0, 1000, true);
    assert!(b.snapshot_video(1.0, QpcTime(i64::MAX)).is_some());
    b.set_video_params(VideoStreamParams { width: 128, ..params() });
    assert!(b.snapshot_video(1.0, QpcTime(i64::MAX)).is_none());
    // Pacchetti prima del primo keyframe ignorati.
    push(&b, 1, 1000, false);
    assert_eq!(b.stats().video_packets, 0);
}

/// Con la coda dei B-frame ancora incompleta la clip si misura dall'ultimo frame completo, che è
/// prima di `newest_end`: il margine di conservazione evita clip di qualche frame sotto N.
#[test]
fn retained_video_always_covers_n_seconds_with_b_frames() {
    let n = 2.0;
    // Ogni snapshot chiude il blocco da 4 MiB in scrittura: tetto largo e snapshot non a ogni frame.
    let b = buffer(n, 256 * MB);
    // Ordine di decodifica P(d+3), B(d+1), B(d+2); keyframe ogni 60 frame mostrati.
    let mut seq = 0i64;
    let mut push_frame = |display: i64, keyframe: bool| {
        let pts = QpcTime(1_000_000_000 + display * FRAME_HNS);
        let dts = QpcTime(1_000_000_000 + (seq - 2) * FRAME_HNS);
        let data = packet_bytes(seq as u64, 100);
        b.push_video(PacketRef { data: &data, pts, dts, duration: FRAME_HNS, keyframe });
        seq += 1;
    };
    push_frame(0, true);
    for g in 0..200 {
        let p = 3 * g + 3;
        for (display, key) in [(p, p % 60 == 0), (p - 2, false), (p - 1, false)] {
            push_frame(display, key);
            if g % 7 != 0 {
                continue;
            }
            let s = b.snapshot_video(n, QpcTime(i64::MAX)).unwrap();
            let first = s.packets.iter().map(|p| p.pts).min().unwrap();
            let last = s.packets.iter().map(|p| p.pts.add_hns(p.duration)).max().unwrap();
            let secs = last.hns_since(first) as f64 / HNS_PER_SEC as f64;
            if display > 200 {
                assert!(secs >= n - 0.001, "clip di {secs} s dopo il frame {display}");
            }
        }
    }
}

/// La memoria segue il flusso reale, non il tetto di bitrate: si prealloca poco e si cresce a
/// blocchi solo se serve, fino al tetto di memoria.
#[test]
fn arena_follows_the_real_bitrate() {
    // Tetto 40 Mbit/s, 10 s: stima 60 MB, preallocati 15 MB.
    let b = ReplayBuffer::new(BufferConfig { video_seconds: 10.0, audio_seconds: 1.0, max_bytes: 512 * MB, lock_pages: false });
    b.set_video_params(VideoStreamParams { bit_rate: 40_000_000, ..params() });
    assert!(b.stats().bytes <= 16 * MB, "{:?}", b.stats());
    // Flusso reale 1 Mbit/s per 20 s (2500 B a frame a 50 fps): resta nella preallocazione.
    for seq in 0..1000 {
        push(&b, seq, 2_500, seq % 50 == 0);
    }
    assert!(b.stats().bytes <= 16 * MB, "{:?}", b.stats());
    // Flusso a 40 Mbit/s: l'arena cresce fino a coprire 10 s e mezzo più un GOP, non oltre.
    for seq in 1000..2000 {
        push(&b, seq, 100_000, seq % 50 == 0);
    }
    let st = b.stats();
    assert!(st.bytes > 50 * MB && st.bytes < 80 * MB, "{st:?}");
    assert!(st.video_seconds >= 10.0, "{st:?}");
}

/// Unico test che blocca pagine (gli altri usano `lock_pages: false`), così può controllare
/// che il working set torni com'era.
#[test]
fn locked_pages_survive_repeated_reloads() {
    let before = crate::lock::working_set().unwrap();
    for i in 0..5 {
        let b = ReplayBuffer::new(BufferConfig { video_seconds: 1.0, audio_seconds: 1.0, max_bytes: 256 * MB, lock_pages: true });
        // 64 MB preallocati e bloccati (un quarto di bitrate × 3 s), più un blocco audio.
        b.set_video_params(VideoStreamParams { bit_rate: 4 * 64 * MB as u64 * 8 / 3, ..params() });
        b.set_tracks(&["A".into()]);
        push_track(&b, 0, 0, 48_000, 480);
        push(&b, 0, 1000, true);
        let st = b.stats();
        assert!(st.locked, "ricarica {i}: pagine non bloccate ({st:?})");
        assert!(st.bytes >= 60 * MB, "{st:?}");
    }
    assert_eq!(crate::lock::working_set().unwrap().0, before.0, "working set minimo ripristinato");
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
fn audio_refs_render_on_another_thread() {
    fn send_static<T: Send + 'static>(_: &T) {}
    let b = buffer(2.0, 64 * MB);
    b.set_tracks(&["A".into(), "B".into()]);
    push_track(&b, 0, 0, 96_000, 480);
    push_track(&b, 1, 0, 96_000, 333);
    let expected = b.snapshot_audio(at(1000), at(90_000)).unwrap();
    let refs = b.snapshot_audio_refs(at(1000), at(90_000)).unwrap();
    send_static(&refs);
    // Il motore continua a scrivere (e a scartare i blocchi vecchi) mentre il thread di
    // salvataggio decomprime: i riferimenti tengono vivi i blocchi.
    let render = std::thread::spawn(move || refs.render());
    push_track(&b, 0, 96_000, 96_000, 480);
    let s = render.join().unwrap();
    assert!(s == expected, "render diverso dallo snapshot diretto");
    assert_eq!(s.start, at(1000));
    for (t, track) in s.tracks.iter().enumerate() {
        assert_eq!(track.samples.len(), 89_000 * 2);
        assert_sample(track.samples[0], t, 1000, 0);
        assert_sample(track.samples[88_999 * 2 + 1], t, 89_999, 1);
    }
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

/// Come `push_track`, ma con `start` spostato di `jitter(k)` hns sul pezzo `k`.
fn push_track_with(b: &ReplayBuffer, track: usize, from: i64, frames: i64, jitter: impl Fn(i64) -> i64) {
    let mut f = from;
    let mut k = 0;
    while f < from + frames {
        let n = (480 + (k * 37) % 300).min(from + frames - f);
        let s: Vec<f32> = (f..f + n).flat_map(|g| [signal(track, g, 0), signal(track, g, 1)]).collect();
        b.push(track, at(f).add_hns(jitter(k)), &s);
        f += n;
        k += 1;
    }
}

/// Ritardo costante (in frame) con cui `got` riproduce il segnale della traccia.
fn lag_of(got: &[f32], track: usize, from: i64) -> i64 {
    let probe = 2000;
    (-96..=96)
        .find(|&lag| {
            (0..64).all(|i| {
                let want = audio::quantize(signal(track, from + probe + i + lag, 0)) as f32 / 8_388_607.0;
                (got[(probe + i) as usize * 2] - want).abs() < 1e-6
            })
        })
        .unwrap_or(i64::MAX)
}

#[test]
fn qpc_jitter_does_not_break_contiguity() {
    let b = buffer(2.0, 64 * MB);
    b.set_tracks(&["A".into(), "B".into()]);
    // ±2 ms di jitter pseudo-casuale sullo start di ogni pacchetto (il primo esatto).
    let jitter = |k: i64| if k == 0 { 0 } else { ((k * 7919) % 41 - 20) * 1_000 };
    push_track_with(&b, 0, 0, 96_000, jitter);
    push_track_with(&b, 1, 0, 96_000, |k| jitter(k + 3) * i64::from(k != 0));
    for from in [0, 30_011] {
        let to = 90_000;
        let s = b.snapshot_audio(at(from), at(to)).unwrap();
        for (t, track) in s.tracks.iter().enumerate() {
            // Nessun buco né campione ripetuto: tutto il tratto è il segnale con un ritardo
            // costante entro 1 ms (esatto quando si parte dal primo campione del segmento).
            let lag = lag_of(&track.samples, t, from);
            assert!(lag.abs() <= 48, "traccia {t} da {from}: ritardo {lag}");
            if from == 0 {
                assert_eq!(lag, 0);
            }
            for i in 0..(to - from - 96) as usize {
                assert_sample(track.samples[i * 2], t, from + i as i64 + lag, 0);
            }
        }
    }
}

#[test]
fn long_gap_opens_a_new_segment_filled_with_silence() {
    let b = buffer(4.0, 64 * MB);
    b.set_tracks(&["A".into(), "B".into()]);
    // A: 1 s, poi un vuoto di 0,5 s (oltre i 100 ms), poi 1 s. B continua senza vuoti.
    push_track(&b, 0, 0, 48_000, 480);
    push_track(&b, 0, 72_000, 48_000, 480);
    push_track(&b, 1, 0, 120_000, 480);
    let s = b.snapshot_audio(at(0), at(120_000)).unwrap();
    assert_eq!(s.tracks[0].samples.len(), s.tracks[1].samples.len());
    let a = &s.tracks[0].samples;
    for f in 0..120_000i64 {
        let i = f as usize * 2;
        if !(48_000 - 32..72_000).contains(&f) {
            assert_sample(a[i], 0, f, 0);
        } else if f >= 48_000 {
            assert_eq!(a[i], 0.0, "silenzio nel vuoto, frame {f}");
        }
        assert_sample(s.tracks[1].samples[i], 1, f, 0);
    }
}

/// Con i B-frame la coda in ordine di decodifica (I0 P3 B1 B2 P6 B4) ha un buco di pts (manca B5):
/// lo snapshot si ferma all'ultimo frame completo (B2), senza P6 né B4 che da P6 dipende.
#[test]
fn snapshot_drops_incomplete_b_frame_tail() {
    let b = buffer(10.0, 64 * MB);
    let t = |f: i64| QpcTime(1_000_000_000 + f * FRAME_HNS);
    for (f, dts, key) in [(0, -2, true), (3, -1, false), (1, 0, false), (2, 1, false), (6, 2, false), (4, 3, false)] {
        let data = packet_bytes(f as u64, 16);
        b.push_video(PacketRef { data: &data, pts: t(f), dts: t(dts), duration: FRAME_HNS, keyframe: key });
    }
    let s = b.snapshot_video(0.01, t(100)).unwrap();
    let frames: Vec<i64> = s.packets.iter().map(|p| (p.pts.0 - 1_000_000_000) / FRAME_HNS).collect();
    assert_eq!(frames, [0, 3, 1, 2]);
}

/// La durata si misura dalla fine dell'ultimo frame, non da `end`: con `end` molto dopo l'ultimo
/// pacchetto la clip copre comunque i secondi chiesti.
#[test]
fn snapshot_seconds_are_measured_from_last_frame() {
    let b = buffer(10.0, 64 * MB);
    for seq in 0..250 {
        push(&b, seq, 32, seq % 50 == 0);
    }
    let last_end = QpcTime(1_000_000_000 + 250 * FRAME_HNS);
    let s = b.snapshot_video(2.0, last_end.add_hns(HNS_PER_SEC / 2)).unwrap();
    let start = s.start().unwrap();
    assert!(last_end.hns_since(start) >= 2 * HNS_PER_SEC, "clip più corta di 2 s");
    assert!(last_end.hns_since(start) <= 3 * HNS_PER_SEC, "clip oltre un GOP in più");
}

/// Un buco interno (pacer bloccato 1 s, molti GOP fa) non tronca la clip: resta un salto di
/// timestamp nel mezzo e la clip arriva fino all'ultimo frame.
#[test]
fn gap_after_last_keyframe_does_not_truncate_snapshot() {
    // Tick del pacer persi (300 ms) dopo l'ultimo keyframe: la clip arriva comunque all'ultimo frame.
    let b = buffer(10.0, 64 * MB);
    for seq in (0..360).chain(375..400) {
        push(&b, seq, 32, seq % 50 == 0);
    }
    let s = b.snapshot_video(5.0, QpcTime(i64::MAX)).unwrap();
    assert_eq!(seq_of(s.packets.last().unwrap()), 399);
    let st = b.stats();
    assert_eq!(st.newest_end, Some(QpcTime(1_000_000_000 + 400 * FRAME_HNS)));
    // Subito dopo il buco, con un solo frame oltre: è nelle ultime 3 posizioni, la clip si
    // ferma prima del buco (potrebbe essere un B-frame mancante).
    let b = buffer(10.0, 64 * MB);
    for seq in (0..360).chain(375..376) {
        push(&b, seq, 32, seq % 50 == 0);
    }
    assert_eq!(seq_of(b.snapshot_video(5.0, QpcTime(i64::MAX)).unwrap().packets.last().unwrap()), 359);
    let empty = buffer(1.0, 64 * MB);
    assert_eq!(empty.stats().newest_end, None);
}

#[test]
fn internal_gap_does_not_truncate_snapshot() {
    let b = buffer(10.0, 64 * MB);
    for seq in (0..100).chain(150..400) {
        push(&b, seq, 32, seq % 50 == 0);
    }
    let s = b.snapshot_video(7.0, QpcTime(1_000_000_000 + 400 * FRAME_HNS)).unwrap();
    assert_eq!(seq_of(s.packets.last().unwrap()), 399);
    assert_eq!(seq_of(&s.packets[0]), 50);
}

/// M4: con N = M l'audio conservato deve coprire tutto il video conservato (N + margine + GOP),
/// altrimenti la clip video parte muta.
#[test]
fn audio_covers_the_whole_video_clip() {
    let b = ReplayBuffer::new(BufferConfig { video_seconds: 5.0, audio_seconds: 5.0, max_bytes: 64 * MB, lock_pages: false });
    b.set_video_params(params());
    b.set_tracks(&["A".into()]);
    // Audio a pezzi da 10 ms, video a 50 fps con un keyframe al secondo, stesso orologio.
    // 20,9 s: la fine non cade su un keyframe, quindi la clip parte fino a un GOP prima di end - N.
    for step in 0..2090i64 {
        let f = step * 480;
        let s: Vec<f32> = (f..f + 480).flat_map(|g| [signal(0, g, 0), signal(0, g, 1)]).collect();
        b.push(0, at(f), &s);
        if step % 2 == 0 {
            let frame = step / 2;
            let pts = at(frame * 960);
            let data = packet_bytes(frame as u64, 100);
            b.push_video(PacketRef { data: &data, pts, dts: pts, duration: FRAME_HNS, keyframe: frame % 50 == 0 });
        }
    }
    let end = at(2090 * 480);
    let video = b.snapshot_video(5.0, end).unwrap();
    let start = video.start().unwrap();
    let audio = b.snapshot_audio(start, end).unwrap();
    let missing_ms = audio.start.hns_since(start) as f64 / 10_000.0;
    assert!(missing_ms <= 0.0, "{missing_ms} ms senza audio a inizio clip");
}

/// Le tracce di un tipo di clip si scelgono per indice e nell'ordine del suo elenco.
#[test]
fn audio_refs_select_tracks_in_the_given_order() {
    let b = buffer(2.0, 64 * MB);
    b.set_tracks(&["Mix".into(), "A".into(), "B".into()]);
    for t in 0..3 {
        push_track(&b, t, 0, 48_000, 480);
    }
    let refs = b.snapshot_audio_refs(at(1000), at(40_000)).unwrap().select_tracks(&[2, 0, 7]);
    assert_eq!(refs.track_count(), 2);
    assert_eq!((refs.track_name(0), refs.track_name(1)), (Some("B"), Some("Mix")));
}
