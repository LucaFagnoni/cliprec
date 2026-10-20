//! Pacchetti H.264 veri (libopenh264, software) → ReplayBuffer → snapshot → file, validati con ffprobe.
#![allow(clippy::unwrap_used, clippy::expect_used)] // file di test: un panic è l'esito di un controllo fallito

use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;

use cliprec_buffer::{BufferConfig, ReplayBuffer};
use cliprec_core::config::{AudioClipFormat, VideoContainer};
use cliprec_core::time::frames_to_hns;
use cliprec_core::{AudioTrackSink, ClipKind, HNS_PER_SEC, PacketRef, QpcTime, VideoCodec, VideoStreamParams};
use cliprec_mux::{VideoClipOptions, write_audio_clip, write_video_clip};
use ffmpeg_the_third::ffi::*;
use serde_json::Value;

const FPS: i64 = 30;
const W: i32 = 320;
const H: i32 = 240;
const ORIGIN: i64 = 50_000_000_000;

fn out_dir() -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output/mux");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn clean(base: &Path) {
    for ext in ["", ".mp4", ".mkv", ".flac", ".wav", ".mka", ".part", ".mp4.part"] {
        let p = PathBuf::from(format!("{}{ext}", base.display()));
        let _ = std::fs::remove_dir_all(&p);
        let _ = std::fs::remove_file(&p);
    }
}

fn pts(frame: i64) -> QpcTime {
    QpcTime(ORIGIN + frame * HNS_PER_SEC / FPS)
}

/// Encoder software della build LGPL: libopenh264 (H.264 senza B-frame) o libkvazaar (HEVC con
/// GOP gerarchico: frame riordinati e dts negativi come NVENC con B-frame).
#[derive(Clone, Copy)]
enum Sw {
    OpenH264,
    Kvazaar,
}

/// Codifica `frames` frame sintetici e li passa al buffer.
fn encode_into(buffer: &ReplayBuffer, frames: i64, sw: Sw) {
    let (name, video_codec) = match sw {
        Sw::OpenH264 => (c"libopenh264", VideoCodec::H264),
        Sw::Kvazaar => (c"libkvazaar", VideoCodec::Hevc),
    };
    // SAFETY: uso standard dell'API di encoding; ogni oggetto viene liberato alla fine.
    unsafe {
        let codec = avcodec_find_encoder_by_name(name.as_ptr());
        assert!(!codec.is_null(), "{name:?} assente");
        let mut ctx = avcodec_alloc_context3(codec);
        (*ctx).width = W;
        (*ctx).height = H;
        (*ctx).pix_fmt = AVPixelFormat::YUV420P;
        (*ctx).time_base = AVRational { num: 1, den: FPS as i32 };
        (*ctx).framerate = AVRational { num: FPS as i32, den: 1 };
        (*ctx).gop_size = FPS as i32;
        (*ctx).max_b_frames = 0;
        (*ctx).bit_rate = 1_000_000;
        (*ctx).flags |= AV_CODEC_FLAG_GLOBAL_HEADER as i32;
        let mut opts: *mut AVDictionary = ptr::null_mut();
        if let Sw::Kvazaar = sw {
            // GOP chiuso (IDR) ogni 32 frame, GOP gerarchico da 8 con riordino.
            av_dict_set(&mut opts, c"kvazaar-params".as_ptr(), c"preset=ultrafast,gop=8,period=32,open-gop=0".as_ptr(), 0);
        }
        assert!(avcodec_open2(ctx, codec, &mut opts) >= 0);
        av_dict_free(&mut opts);
        let extradata = std::slice::from_raw_parts((*ctx).extradata, (*ctx).extradata_size as usize).to_vec();
        assert!(!extradata.is_empty(), "extradata (SPS/PPS) attesa");
        buffer.set_video_params(VideoStreamParams {
            codec: video_codec,
            width: W as u32,
            height: H as u32,
            fps: FPS as u32,
            extradata,
            bit_rate: 1_000_000,
            encoder_name: name.to_string_lossy().into_owned(),
        });

        let mut frame = av_frame_alloc();
        (*frame).width = W;
        (*frame).height = H;
        (*frame).format = AVPixelFormat::YUV420P.0;
        assert!(av_frame_get_buffer(frame, 0) >= 0);
        let mut pkt = av_packet_alloc();
        let drain = |ctx: *mut AVCodecContext| loop {
            if avcodec_receive_packet(ctx, pkt) < 0 {
                break;
            }
            let p = &*pkt;
            let data = std::slice::from_raw_parts(p.data, p.size as usize);
            buffer.push_video(PacketRef {
                data,
                pts: pts(p.pts),
                dts: pts(p.dts),
                duration: HNS_PER_SEC / FPS,
                keyframe: p.flags & AV_PKT_FLAG_KEY != 0,
            });
            av_packet_unref(pkt);
        };
        for i in 0..frames {
            assert!(av_frame_make_writable(frame) >= 0);
            for plane in 0..3 {
                let (w, h) = if plane == 0 { (W, H) } else { (W / 2, H / 2) };
                let stride = (*frame).linesize[plane] as usize;
                let data = (*frame).data[plane];
                for y in 0..h as usize {
                    for x in 0..w as usize {
                        // Barra che scorre: ogni frame è diverso.
                        let v = if plane == 0 { ((x as i64 + i * 4) % 256) as u8 } else { 128 };
                        *data.add(y * stride + x) = v;
                    }
                }
            }
            (*frame).pts = i;
            assert!(avcodec_send_frame(ctx, frame) >= 0);
            drain(ctx);
        }
        avcodec_send_frame(ctx, ptr::null());
        drain(ctx);
        av_packet_free(&mut pkt);
        av_frame_free(&mut frame);
        avcodec_free_context(&mut ctx);
    }
}

/// Seno a `freq` Hz da `from_frame` (relativo a ORIGIN) per `frames` frame, a pezzi da 10 ms.
fn push_tone(buffer: &ReplayBuffer, track: usize, freq: f32, from_frame: i64, frames: i64) {
    let mut f = from_frame;
    while f < from_frame + frames {
        let n = 480.min(from_frame + frames - f);
        let s: Vec<f32> = (f..f + n)
            .flat_map(|g| {
                let x = 0.5 * (g as f32 * freq * std::f32::consts::TAU / 48_000.0).sin();
                [x, x]
            })
            .collect();
        buffer.push(track, QpcTime(ORIGIN + frames_to_hns(f)), &s);
        f += n;
    }
}

fn filled_buffer() -> ReplayBuffer {
    filled_buffer_with(Sw::OpenH264)
}

fn filled_buffer_with(sw: Sw) -> ReplayBuffer {
    let b = ReplayBuffer::new(BufferConfig { video_seconds: 3.0, audio_seconds: 4.0, max_bytes: 256 << 20, lock_pages: false });
    b.set_tracks(&["Gioco".into(), "Discord".into()]);
    encode_into(&b, 6 * FPS, sw);
    // «Gioco» da 0,5 s prima del video, «Discord» da 4 s: allineamenti diversi.
    push_tone(&b, 0, 440.0, -24_000, 6 * 48_000 + 24_000);
    push_tone(&b, 1, 660.0, 4 * 48_000, 2 * 48_000);
    b
}

fn ffprobe(path: &Path, extra: &[&str]) -> Value {
    let dir = option_env!("FFMPEG_DIR").unwrap_or(r"D:\Repositories\cliprec	hird_partyfmpeg");
    let exe = Path::new(dir).join("bin").join("ffprobe.exe");
    let out = Command::new(exe)
        .args(["-v", "error", "-show_streams", "-show_format", "-of", "json"])
        .args(extra)
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Decodifica con ffmpeg in PCM s32 interleaved.
fn decode_s32(path: &Path) -> Vec<i32> {
    let dir = option_env!("FFMPEG_DIR").unwrap_or(r"D:\Repositories\cliprec	hird_partyfmpeg");
    let out = Command::new(Path::new(dir).join("bin").join("ffmpeg.exe"))
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "s32le", "-acodec", "pcm_s32le", "-"])
        .output()
        .unwrap();
    assert!(out.status.success());
    out.stdout.chunks_exact(4).map(|b| i32::from_le_bytes(b.try_into().unwrap())).collect()
}

fn f(v: &Value) -> f64 {
    v.as_str().unwrap().parse().unwrap()
}

fn audio_streams(p: &Value) -> Vec<&Value> {
    p["streams"].as_array().unwrap().iter().filter(|s| s["codec_type"] == "audio").collect()
}

fn check_video_file(path: &Path, expect_secs: f64, codec: &str) {
    let p = ffprobe(path, &["-show_packets", "-select_streams", "v", "-count_frames"]);
    println!("{}: {}", path.display(), summary(&p));
    let streams = p["streams"].as_array().unwrap();
    // -select_streams v limita gli stream mostrati: rileggiamo senza filtro per l'audio.
    let all = ffprobe(path, &[]);
    let v = &streams[0];
    assert_eq!(v["codec_name"], codec);
    // Tutti i frame si decodificano.
    assert_eq!(f(&v["nb_read_frames"]), (expect_secs * FPS as f64).round());
    // Il primo frame mostrato e l'audio partono entrambi da zero: audio allineato al video.
    for s in all["streams"].as_array().unwrap() {
        let t = f(&s["start_time"]);
        assert!(t.abs() < 1e-3, "{} start_time {t}", s["codec_type"]);
    }
    assert_eq!(v["r_frame_rate"], "30/1");
    assert_eq!(v["avg_frame_rate"], "30/1");
    assert_eq!(v["disposition"]["default"], 1);
    let dur = f(&all["format"]["duration"]);
    assert!((dur - expect_secs).abs() < 0.1, "durata {dur}");
    // fps costanti: tutti i pacchetti video durano uguale e sono a passo costante.
    let pkts = p["packets"].as_array().unwrap();
    assert_eq!(pkts.len() as f64, (expect_secs * FPS as f64).round());
    let mut times: Vec<f64> = pkts.iter().map(|k| f(&k["pts_time"])).collect();
    times.sort_by(f64::total_cmp);
    assert!(times[0].abs() < 1e-3, "il video parte da zero: {}", times[0]);
    for w in times.windows(2) {
        assert!((w[1] - w[0] - 1.0 / FPS as f64).abs() < 1e-3, "passo non costante: {w:?}");
    }
    let audio = audio_streams(&all);
    assert_eq!(audio.len(), 2);
    for (s, name) in audio.iter().zip(["Gioco", "Discord"]) {
        assert_eq!(s["codec_name"], "aac");
        assert_eq!(s["sample_rate"], "48000");
        assert_eq!(s["channels"], 2);
        // MP4: FFmpeg scrive `title` nel box udta «name», che ffprobe rilegge come tag `name`.
        let title = if s["tags"]["title"].is_null() { &s["tags"]["name"] } else { &s["tags"]["title"] };
        assert_eq!(title, name);
        if path.extension().is_some_and(|e| e == "mp4") {
            assert_eq!(s["tags"]["handler_name"], name);
            let d = f(&s["duration"]);
            assert!((d - expect_secs).abs() < 0.05, "durata audio {d}");
        }
    }
    assert_eq!(audio[0]["disposition"]["default"], 1);
    assert_eq!(audio[1]["disposition"]["default"], 0);
}

fn summary(p: &Value) -> String {
    let streams: Vec<String> = p["streams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            format!(
                "{}:{}[{}]{}",
                s["codec_type"].as_str().unwrap_or(""),
                s["codec_name"].as_str().unwrap_or(""),
                s["tags"]["title"].as_str().or(s["tags"]["name"].as_str()).unwrap_or("-"),
                if s["disposition"]["default"] == 1 { "*" } else { "" }
            )
        })
        .collect();
    format!("durata {} s, {}", p["format"]["duration"].as_str().unwrap_or("?"), streams.join(" "))
}

#[test]
fn video_clip_mp4_and_mkv() {
    let b = filled_buffer();
    let end = pts(6 * FPS);
    let video = b.snapshot_video(3.0, end).unwrap();
    assert!(video.packets[0].keyframe);
    assert_eq!(video.start(), Some(pts(3 * FPS)));
    let audio = b.snapshot_audio(video.start().unwrap(), end).unwrap();
    assert_eq!(audio.tracks.len(), 2);

    let base = out_dir().join("video_clip");
    clean(&base);
    let saved = write_video_clip(&base, &video, Some(&audio), &VideoClipOptions::default()).unwrap();
    assert_eq!(saved.kind, ClipKind::Video);
    assert_eq!(saved.path.extension().unwrap(), "mp4");
    assert!((saved.duration_secs - 3.0).abs() < 1e-6);
    assert!(saved.bytes > 0);
    assert!(!PathBuf::from(format!("{}.mp4.part", base.display())).exists());
    check_video_file(&saved.path, 3.0, "h264");

    // Stesso nome: non sovrascrive, aggiunge « (2)».
    let again = write_video_clip(&base, &video, Some(&audio), &VideoClipOptions::default()).unwrap();
    assert!(again.path.to_string_lossy().ends_with("video_clip (2).mp4"));
    let _ = std::fs::remove_file(&again.path);

    let mkv = write_video_clip(
        &base,
        &video,
        Some(&audio),
        &VideoClipOptions { container: VideoContainer::Mkv, ..Default::default() },
    )
    .unwrap();
    check_video_file(&mkv.path, 3.0, "h264");

    // Senza audio: solo video.
    let base = out_dir().join("video_only");
    clean(&base);
    let saved = write_video_clip(&base, &video, None, &VideoClipOptions::default()).unwrap();
    assert_eq!(ffprobe(&saved.path, &[])["streams"].as_array().unwrap().len(), 1);
}

/// Due clip nello stesso istante con lo stesso nome: due file distinti e validi, nessun `.part` comune.
#[test]
fn parallel_clips_with_the_same_name() {
    let b = filled_buffer();
    let end = pts(6 * FPS);
    let video = b.snapshot_video(3.0, end).unwrap();
    let audio = b.snapshot_audio(video.start().unwrap(), end).unwrap();
    let base = out_dir().join("parallel");
    clean(&base);
    let _ = std::fs::remove_file(PathBuf::from(format!("{} (2).mp4", base.display())));
    let saved: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..2)
            .map(|_| s.spawn(|| write_video_clip(&base, &video, Some(&audio), &VideoClipOptions::default()).unwrap()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_ne!(saved[0].path, saved[1].path);
    for c in &saved {
        check_video_file(&c.path, 3.0, "h264");
    }
    let leftovers = std::fs::read_dir(out_dir()).unwrap().filter_map(|e| e.ok()).any(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        n.starts_with("parallel") && n.ends_with(".part")
    });
    assert!(!leftovers, "nessun .part rimasto");
    for c in saved {
        let _ = std::fs::remove_file(c.path);
    }
}

/// Cartella annidata con un percorso totale oltre MAX_PATH (260): la clip si salva.
#[test]
fn long_path_clip() {
    let b = filled_buffer();
    let end = pts(6 * FPS);
    let video = b.snapshot_video(3.0, end).unwrap();
    let audio = b.snapshot_audio(video.start().unwrap(), end).unwrap();
    let root = std::path::absolute(out_dir().join("lungo")).unwrap();
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("a".repeat(120)).join("b".repeat(120));
    std::fs::create_dir_all(&dir).unwrap();
    let base = dir.join(format!("clip_{}", "c".repeat(40)));
    let saved = write_video_clip(&base, &video, Some(&audio), &VideoClipOptions::default()).unwrap();
    assert!(saved.path.as_os_str().len() > 260, "{}", saved.path.as_os_str().len());
    assert!(std::fs::metadata(&saved.path).unwrap().len() > 10_000);
    let flac = write_audio_clip(&base, &audio, AudioClipFormat::FlacPerTrack).unwrap();
    assert_eq!(std::fs::read_dir(&flac.path).unwrap().count(), 2);
    std::fs::remove_dir_all(&root).unwrap();
}

/// HEVC con frame riordinati: dopo aver portato a zero il primo pts, i primi dts sono negativi.
#[test]
fn video_clip_with_reordered_frames() {
    let b = filled_buffer_with(Sw::Kvazaar);
    let end = pts(6 * FPS);
    let video = b.snapshot_video(3.0, end).unwrap();
    assert!(video.packets[0].keyframe);
    let start = video.start().unwrap();
    assert!(video.packets.iter().any(|p| p.dts < start), "attesi dts prima del primo pts");
    assert!(video.packets.windows(2).any(|w| w[1].pts < w[0].pts), "attesi frame riordinati");
    let audio = b.snapshot_audio(start, end).unwrap();
    for (container, ext) in [(VideoContainer::Mp4, "mp4"), (VideoContainer::Mkv, "mkv")] {
        let base = out_dir().join("video_bframes");
        clean(&base);
        let saved = write_video_clip(&base, &video, Some(&audio), &VideoClipOptions { container, ..Default::default() }).unwrap();
        assert_eq!(saved.path.extension().unwrap(), ext);
        let secs = saved.duration_secs;
        assert!(secs >= 3.0 - 1e-9 && secs < 3.0 + 32.0 / FPS as f64, "durata {secs}");
        check_video_file(&saved.path, secs, "hevc");
    }
}

#[test]
fn audio_clips_flac_wav_mka() {
    let b = filled_buffer();
    let end = pts(6 * FPS);
    let audio = b.snapshot_audio(end.add_hns(-4 * HNS_PER_SEC), end).unwrap();
    assert_eq!(audio.tracks[0].samples.len(), 4 * 48_000 * 2);

    for (format, ext, codec) in [
        (AudioClipFormat::FlacPerTrack, "flac", "flac"),
        (AudioClipFormat::WavPerTrack, "wav", "pcm_s24le"),
    ] {
        let base = out_dir().join(format!("audio_{ext}"));
        clean(&base);
        let saved = write_audio_clip(&base, &audio, format).unwrap();
        assert_eq!(saved.kind, ClipKind::Audio);
        assert!(saved.path.is_dir());
        let files = [format!("01 - Gioco.{ext}"), format!("02 - Discord.{ext}")];
        for file in &files {
            let p = ffprobe(&saved.path.join(file), &[]);
            println!("{file}: {}", summary(&p));
            let s = &p["streams"][0];
            assert_eq!(s["codec_name"], codec);
            assert_eq!(s["sample_rate"], "48000");
            assert_eq!(s["channels"], 2);
            assert_eq!(s["bits_per_raw_sample"].as_str().or(s["bits_per_sample"].as_i64().map(|_| "24")), Some("24"));
            assert!((f(&p["format"]["duration"]) - 4.0).abs() < 1e-3);
        }
        assert_eq!(std::fs::read_dir(&saved.path).unwrap().count(), 2);
        // Senza perdita: i campioni decodificati sono quelli dello snapshot (24 bit).
        let pcm = decode_s32(&saved.path.join(&files[0]));
        assert_eq!(pcm.len(), audio.tracks[0].samples.len());
        for (got, x) in pcm.iter().zip(&audio.tracks[0].samples) {
            assert_eq!(got >> 8, (x * 8_388_607.0).round() as i32);
        }

        // Una sola traccia: un file, niente cartella.
        let base = out_dir().join(format!("audio_single_{ext}"));
        clean(&base);
        let one = cliprec_core::AudioSnapshot { start: audio.start, tracks: vec![audio.tracks[0].clone()] };
        let saved = write_audio_clip(&base, &one, format).unwrap();
        assert!(saved.path.is_file());
        assert_eq!(saved.path.extension().unwrap(), ext);
    }

    let base = out_dir().join("audio_mka");
    clean(&base);
    let saved = write_audio_clip(&base, &audio, AudioClipFormat::Mka).unwrap();
    let p = ffprobe(&saved.path, &[]);
    println!("mka: {}", summary(&p));
    let streams = audio_streams(&p);
    assert_eq!(streams.len(), 2);
    for (s, name) in streams.iter().zip(["Gioco", "Discord"]) {
        assert_eq!(s["codec_name"], "flac");
        assert_eq!(s["tags"]["title"], name);
    }
    assert_eq!(streams[0]["disposition"]["default"], 1);
    assert!((f(&p["format"]["duration"]) - 4.0).abs() < 0.01);
}
