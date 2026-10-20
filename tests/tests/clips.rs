//! Genera clip di riferimento con ffmpeg e verifica `check` e `measure_sync`.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use cliprec_tests::generate::{GenOptions, gen_test_clip};
use cliprec_tests::ffmpeg;
use cliprec_tests::probe::{Expect, check, check_audio_dir, probe};
use cliprec_tests::sync::measure_sync;

fn out(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("target").join("test-output").join(name)
}

fn clip(name: &str, offset: f64) -> PathBuf {
    let p = out(name);
    gen_test_clip(&p, &GenOptions { av_offset_ms: offset, ..GenOptions::default() }).unwrap();
    p
}

fn good() -> Expect {
    Expect {
        min_duration: Some(5.0),
        audio_tracks: Some(2),
        track_names: Some(vec!["Mix".into(), "Discord".into()]),
        fps: Some(60.0),
        ..Expect::default()
    }
}

#[test]
fn check_accepts_reference_clip_and_flags_violations() {
    let p = clip("ref-0.mp4", 0.0);
    let info = probe(&p).unwrap();
    assert_eq!(info.audio.len(), 2);
    assert_eq!(check(&p, &good()).unwrap(), Vec::<String>::new());

    let e = Expect { audio_tracks: Some(3), ..good() };
    assert_eq!(check(&p, &e).unwrap().len(), 1);
    let e = Expect { track_names: Some(vec!["Mix".into(), "Gioco".into()]), ..good() };
    assert_eq!(check(&p, &e).unwrap().len(), 1);
    let e = Expect { min_duration: Some(7.0), ..good() };
    assert_eq!(check(&p, &e).unwrap().len(), 1);
    let e = Expect { min_duration: Some(3.0), ..good() };
    assert_eq!(check(&p, &e).unwrap().len(), 1, "oltre un GOP in più");
    let e = Expect { fps: Some(30.0), ..good() };
    assert_eq!(check(&p, &e).unwrap().len(), 1);
}

#[test]
fn measure_sync_recovers_offset() {
    for (name, off) in [("sync-0.mp4", 0.0), ("sync-40.mp4", 40.0)] {
        let r = measure_sync(&clip(name, off)).unwrap();
        println!("{name}: {r:?}");
        assert_eq!(r.flashes, 5);
        for s in &r.av {
            assert!(s.matched >= 4);
            assert!((s.mean_ms - off).abs() < 2.0, "{name}: {s:?}");
            assert!((s.max_abs_ms - off).abs() < 2.0, "{name}: {s:?}");
        }
        assert!(r.inter_track[1].max_abs_ms < 2.0, "{name}: {:?}", r.inter_track);
        assert!(r.violations(20.0).is_empty() == (off < 20.0));
    }
}

#[test]
fn audio_only_mka_and_folder() {
    let p = out("tracks.mp4");
    let o = GenOptions { track_offsets_ms: vec![0.0, 25.0], ..GenOptions::default() };
    gen_test_clip(&p, &o).unwrap();
    // Solo audio multitraccia (MKA, copia dei flussi) e cartella con un FLAC per traccia.
    let mka = out("tracks.mka");
    let mut c = ffmpeg::ffmpeg();
    ffmpeg::run(c.arg("-y").arg("-i").arg(&p).args(["-vn", "-map", "0:a", "-c", "copy"]).arg(&mka)).unwrap();
    let r = measure_sync(&mka).unwrap();
    println!("mka: {r:?}");
    assert_eq!(r.flashes, 0);
    assert!((r.inter_track[1].mean_ms - 25.0).abs() < 2.0, "{:?}", r.inter_track);

    let dir = out("tracks-flac");
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..2 {
        let mut c = ffmpeg::ffmpeg();
        let f = dir.join(format!("traccia{i}.flac"));
        ffmpeg::run(c.arg("-y").arg("-i").arg(&p).args(["-map", &format!("0:a:{i}"), "-c:a", "flac"]).arg(&f)).unwrap();
    }
    let e = Expect { audio_tracks: Some(2), min_duration: Some(5.0), ..Expect::default() };
    assert_eq!(check_audio_dir(&dir, &e).unwrap(), Vec::<String>::new());
    let r = measure_sync(&dir).unwrap();
    assert!((r.inter_track[1].mean_ms - 25.0).abs() < 2.0, "{:?}", r.inter_track);
    // Una traccia più corta viene segnalata.
    let c = ffmpeg::ffmpeg().arg("-y").arg("-i").arg(dir.join("traccia1.flac")).args(["-t", "4"]).arg(dir.join("traccia1b.flac")).status().unwrap();
    assert!(c.success());
    std::fs::rename(dir.join("traccia1b.flac"), dir.join("traccia1.flac")).unwrap();
    assert!(!check_audio_dir(&dir, &e).unwrap().is_empty());
}
