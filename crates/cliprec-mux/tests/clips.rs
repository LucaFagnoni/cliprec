//! Pacchetti H.264/HEVC veri (libopenh264/libkvazaar di ffmpeg.exe, software) → ReplayBuffer → snapshot → file, validati con ffprobe.
#![allow(clippy::unwrap_used, clippy::expect_used)] // file di test: un panic è l'esito di un controllo fallito

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use cliprec_buffer::{BufferConfig, ReplayBuffer};
use cliprec_core::config::{AudioClipFormat, VideoContainer};
use cliprec_core::time::frames_to_hns;
use cliprec_core::{AudioTrackSink, ClipKind, HNS_PER_SEC, PacketRef, QpcTime, VideoCodec, VideoStreamParams};
use cliprec_mux::{VideoClipOptions, write_audio_clip, write_video_clip};
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
    for ext in ["", ".mp4", ".mkv", ".flac", ".wav", ".mka", ".opus", ".m4a", ".part", ".mp4.part"] {
        let p = PathBuf::from(format!("{}{ext}", base.display()));
        let _ = std::fs::remove_dir_all(&p);
        let _ = std::fs::remove_file(&p);
    }
}

fn pts(frame: i64) -> QpcTime {
    QpcTime(ORIGIN + frame * HNS_PER_SEC / FPS)
}

/// Encoder software della build FFmpeg completa: libopenh264 (H.264 senza B-frame) o libkvazaar
/// (HEVC con GOP gerarchico: frame riordinati e dts negativi come NVENC con B-frame).
#[derive(Clone, Copy)]
enum Sw {
    OpenH264,
    Kvazaar,
}

/// Video sintetico codificato: extradata Annex B e pacchetti (dati, pts, dts in frame, keyframe).
struct Encoded {
    extradata: Vec<u8>,
    packets: Vec<(Vec<u8>, i64, i64, bool)>,
}

const FRAMES: i64 = 6 * FPS;

/// Codifica con `ffmpeg.exe` della build completa (`FFMPEG_TOOLS_DIR`): la build ridotta che ClipRec
/// linka non ha encoder software H.264/HEVC. Il NUT conserva pacchetti Annex B ed extradata come
/// escono dall'encoder (header globali); `ffprobe -show_data` li rilegge. Una volta per encoder.
fn encoded(sw: Sw) -> &'static Encoded {
    static H264: OnceLock<Encoded> = OnceLock::new();
    static HEVC: OnceLock<Encoded> = OnceLock::new();
    let (cell, name, extra): (_, _, &[&str]) = match sw {
        Sw::OpenH264 => (&H264, "libopenh264", &[]),
        // GOP chiuso (IDR) ogni 32 frame, GOP gerarchico da 8 con riordino.
        Sw::Kvazaar => (&HEVC, "libkvazaar", &["-kvazaar-params", "preset=ultrafast,gop=8,period=32,open-gop=0"]),
    };
    cell.get_or_init(|| {
        let bin = Path::new(option_env!("FFMPEG_TOOLS_DIR").unwrap_or(r"D:\Repositories\cliprec\third_party\ffmpeg")).join("bin");
        let file = out_dir().join(format!("sorgente-{name}.nut"));
        let src = format!("testsrc2=size={W}x{H}:rate={FPS}");
        let st = Command::new(bin.join("ffmpeg.exe"))
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-f", "lavfi", "-i", &src])
            .args(["-frames:v", &FRAMES.to_string(), "-pix_fmt", "yuv420p", "-c:v", name])
            .args(["-g", &FPS.to_string(), "-bf", "0", "-b:v", "1M"])
            .args(extra)
            .arg(&file)
            .status()
            .unwrap();
        assert!(st.success(), "ffmpeg {name}");
        let out = Command::new(bin.join("ffprobe.exe"))
            .args(["-v", "error", "-show_streams", "-show_packets", "-show_data", "-of", "json"])
            .arg(&file)
            .output()
            .unwrap();
        assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        let extradata = unhex(v["streams"][0]["extradata"].as_str().unwrap_or(""));
        assert!(!extradata.is_empty(), "extradata (SPS/PPS) attesa");
        // pts in frame da 0; il NUT non conserva i dts iniziali: si ricostruiscono come sequenza
        // crescente di un frame, anticipata quanto basta perché dts <= pts.
        let raw: Vec<(Vec<u8>, i64, bool)> = v["packets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                let t: f64 = p["pts_time"].as_str().unwrap().parse().unwrap();
                (unhex(p["data"].as_str().unwrap()), (t * FPS as f64).round() as i64, p["flags"].as_str().unwrap().starts_with('K'))
            })
            .collect();
        let first = raw.iter().map(|p| p.1).min().unwrap();
        let delay = raw.iter().enumerate().map(|(i, p)| i as i64 - (p.1 - first)).max().unwrap().max(0);
        let packets = raw.into_iter().enumerate().map(|(i, (d, p, k))| (d, p - first, i as i64 - delay, k)).collect();
        Encoded { extradata, packets }
    })
}

/// Byte di un dump esadecimale di ffprobe: "00000000: 0000 0001 6742 ...  ascii".
fn unhex(dump: &str) -> Vec<u8> {
    let hex: String = dump.lines().flat_map(|l| l.get(10..51).unwrap_or("").split_whitespace()).collect();
    (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect()
}

/// Passa al buffer i pacchetti del video sintetico.
fn encode_into(buffer: &ReplayBuffer, sw: Sw) {
    let e = encoded(sw);
    let (name, codec) = match sw {
        Sw::OpenH264 => ("libopenh264", VideoCodec::H264),
        Sw::Kvazaar => ("libkvazaar", VideoCodec::Hevc),
    };
    buffer.set_video_params(VideoStreamParams {
        codec,
        width: W as u32,
        height: H as u32,
        fps: FPS as u32,
        extradata: e.extradata.clone(),
        bit_rate: 1_000_000,
        encoder_name: name.into(),
    });
    for (data, p, d, keyframe) in &e.packets {
        buffer.push_video(PacketRef { data, pts: pts(*p), dts: pts(*d), duration: HNS_PER_SEC / FPS, keyframe: *keyframe });
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
    encode_into(&b, sw);
    // «Gioco» da 0,5 s prima del video, «Discord» da 4 s: allineamenti diversi.
    push_tone(&b, 0, 440.0, -24_000, 6 * 48_000 + 24_000);
    push_tone(&b, 1, 660.0, 4 * 48_000, 2 * 48_000);
    b
}

fn ffprobe(path: &Path, extra: &[&str]) -> Value {
    let dir = option_env!("FFMPEG_TOOLS_DIR").unwrap_or(r"D:\Repositories\cliprec\third_party\ffmpeg");
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
    let dir = option_env!("FFMPEG_TOOLS_DIR").unwrap_or(r"D:\Repositories\cliprec\third_party\ffmpeg");
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
    let refs = b.snapshot_audio_refs(video.start().unwrap(), end).unwrap();
    let saved = write_video_clip(&base, &video, Some(&refs), &VideoClipOptions::default()).unwrap();
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
    let saved = write_video_clip(&base, &video, None::<&cliprec_buffer::AudioRefs>, &VideoClipOptions::default()).unwrap();
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

/// m27: WAV polifonico, Opus e AAC (M4A).
#[test]
fn audio_clips_polywav_opus_aac() {
    let b = filled_buffer();
    let end = pts(6 * FPS);
    let refs = b.snapshot_audio_refs(end.add_hns(-4 * HNS_PER_SEC), end).unwrap();

    // WAV polifonico: un solo file, 2 canali per traccia, nomi in iXML.
    let base = out_dir().join("audio_poly");
    clean(&base);
    let saved = write_audio_clip(&base, &refs, AudioClipFormat::PolyWav).unwrap();
    assert_eq!(saved.path.extension().unwrap(), "wav");
    let p = ffprobe(&saved.path, &[]);
    println!("polywav: {}", summary(&p));
    let s = &p["streams"][0];
    assert_eq!(s["codec_name"], "pcm_s24le");
    assert_eq!(s["channels"], 4);
    assert_eq!(s["sample_rate"], "48000");
    assert!((f(&p["format"]["duration"]) - 4.0).abs() < 1e-3);
    let bytes = std::fs::read(&saved.path).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    for name in ["Gioco L", "Gioco R", "Discord L", "Discord R"] {
        assert!(text.contains(&format!("<NAME>{name}</NAME>")), "iXML senza {name}");
    }
    // I campioni sono quelli dello snapshot: canali 0-1 = Gioco, 2-3 = Discord.
    let pcm = decode_s32(&saved.path);
    let snap = b.snapshot_audio(end.add_hns(-4 * HNS_PER_SEC), end).unwrap();
    for (i, frame) in pcm.chunks_exact(4).enumerate().step_by(997) {
        for (ch, got) in frame.iter().enumerate() {
            let want = (snap.tracks[ch / 2].samples[i * 2 + ch % 2] * 8_388_607.0).round() as i32;
            assert_eq!(got >> 8, want, "frame {i} canale {ch}");
        }
    }

    for (format, ext, codec) in [(AudioClipFormat::Opus, "opus", "opus"), (AudioClipFormat::Aac, "m4a", "aac"), (AudioClipFormat::Mp3, "mp3", "mp3")] {
        let base = out_dir().join(format!("audio_{ext}"));
        clean(&base);
        let saved = write_audio_clip(&base, &refs, format).unwrap();
        assert!(saved.path.is_dir());
        for (file, name) in [(format!("01 - Gioco.{ext}"), "Gioco"), (format!("02 - Discord.{ext}"), "Discord")] {
            let p = ffprobe(&saved.path.join(&file), &[]);
            println!("{file}: {}", summary(&p));
            let s = &p["streams"][0];
            assert_eq!(s["codec_name"], codec);
            assert_eq!(s["channels"], 2);
            let d = f(&p["format"]["duration"]);
            assert!((d - 4.0).abs() < 0.05, "{file}: durata {d}");
            let title = [&p["format"]["tags"]["title"], &s["tags"]["title"], &s["tags"]["name"], &s["tags"]["handler_name"]]
                .into_iter()
                .find_map(|v| v.as_str().map(str::to_owned));
            assert_eq!(title.as_deref(), Some(name), "{file}: nome della traccia");
        }
        // Una sola traccia: un file, niente cartella.
        let base = out_dir().join(format!("audio_single_{ext}"));
        clean(&base);
        let one = cliprec_core::AudioSnapshot { start: refs.start(), tracks: vec![snap.tracks[0].clone()] };
        let saved = write_audio_clip(&base, &one, format).unwrap();
        assert!(saved.path.is_file());
        assert_eq!(saved.path.extension().unwrap(), ext);
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
        // Dai riferimenti ai blocchi FLAC, in streaming (come il motore); il confronto è con lo snapshot.
        let refs = b.snapshot_audio_refs(end.add_hns(-4 * HNS_PER_SEC), end).unwrap();
        let saved = write_audio_clip(&base, &refs, format).unwrap();
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
