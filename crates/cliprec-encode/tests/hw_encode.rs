//! Codifica 3 s a 60 fps di texture BGRA sintetiche sul device D3D11 e verifica con ffprobe.
//!
//! I test saltano (con un messaggio) se manca l'adattatore del vendor.

// Test: unwrap ammesso anche negli helper.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use cliprec_core::config::CodecChoice;
use cliprec_core::gpu::luid_to_i64;
use cliprec_core::{GpuDevice, GpuFrame, HNS_PER_SEC, QpcTime};
use cliprec_encode::Encoder;
use cliprec_encode::EncoderSettings;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
    ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

const FPS: u32 = 60;
const FRAMES: u64 = 180;

fn adapter_luid(vendor_id: u32) -> Option<i64> {
    // SAFETY: nessuna precondizione.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    // SAFETY: factory valida; EnumAdapters1 fallisce oltre l'ultimo adattatore.
    (0..).map_while(|i| unsafe { factory.EnumAdapters1(i) }.ok()).find_map(|a| {
        // SAFETY: adattatore valido.
        let d = unsafe { a.GetDesc1() }.ok()?;
        (d.VendorId == vendor_id).then(|| luid_to_i64(d.AdapterLuid))
    })
}

fn device(vendor_id: u32) -> Option<GpuDevice> {
    let dev = GpuDevice::create(Some(adapter_luid(vendor_id)?)).ok()?;
    Some(dev)
}

/// Texture BGRA con render target, come quelle che produce la cattura; con `seed` piena di rumore
/// e solo `BIND_SHADER_RESOURCE` (verifica che il video processor accetti texture non render target).
fn texture(gpu: &GpuDevice, w: u32, h: u32, seed: Option<u32>) -> (ID3D11Texture2D, Option<ID3D11RenderTargetView>) {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: match seed {
            Some(_) => D3D11_BIND_SHADER_RESOURCE.0 as u32,
            None => (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        },
        ..Default::default()
    };
    let pixels: Vec<u32> = match seed {
        Some(mut x) => (0..w * h)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x | 0xff00_0000
            })
            .collect(),
        None => Vec::new(),
    };
    let init = D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr().cast(), SysMemPitch: w * 4, SysMemSlicePitch: 0 };
    let mut tex = None;
    // SAFETY: descrittore, dati iniziali (w×h pixel da 4 byte) e puntatore d'uscita locali.
    unsafe { gpu.device.CreateTexture2D(&desc, seed.map(|_| &init as *const _), Some(&mut tex)) }.unwrap();
    let tex = tex.unwrap();
    if seed.is_some() {
        return (tex, None);
    }
    let mut rtv = None;
    // SAFETY: texture valida con BIND_RENDER_TARGET; puntatore d'uscita locale.
    unsafe { gpu.device.CreateRenderTargetView(&tex, None, Some(&mut rtv)) }.unwrap();
    (tex, rtv)
}

#[derive(Debug)]
struct Pkt {
    data: Vec<u8>,
    pts: QpcTime,
    dts: QpcTime,
    keyframe: bool,
}

/// Codifica `FRAMES` frame; le dimensioni della sorgente cambiano a metà se `sizes` ne ha due.
/// Con `noise` ruota 4 texture di rumore (caso peggiore per il bitrate), altrimenti colori pieni.
fn run(gpu: &GpuDevice, enc: &mut Encoder, sizes: &[(u32, u32)], noise: bool) -> (Vec<Pkt>, f64) {
    let targets: Vec<Vec<_>> = sizes
        .iter()
        .map(|&(w, h)| match noise {
            true => (1..=4).map(|s| texture(gpu, w, h, Some(s * 7919))).collect(),
            false => (0..2).map(|_| texture(gpu, w, h, None)).collect(),
        })
        .collect();
    let start = QpcTime(1_000_000_000);
    let mut out = Vec::new();
    let mut sink = |p: cliprec_core::PacketRef<'_>| {
        out.push(Pkt { data: p.data.to_vec(), pts: p.pts, dts: p.dts, keyframe: p.keyframe });
    };
    let t0 = Instant::now();
    for i in 0..FRAMES {
        let part = (i as usize * sizes.len()) / FRAMES as usize;
        let (w, h) = sizes[part];
        let (tex, rtv) = &targets[part][i as usize % targets[part].len()];
        let t = i as f32 / FRAMES as f32;
        if let Some(rtv) = rtv {
            // SAFETY: vista valida sul device del contesto, protetto per l'uso da più thread.
            unsafe { gpu.context.ClearRenderTargetView(rtv, &[t, 1.0 - t, (i % 60) as f32 / 60.0, 1.0]) };
        }
        let frame = GpuFrame {
            texture: tex.clone(),
            width: w,
            height: h,
            pts: start.add_hns(i as i64 * HNS_PER_SEC / i64::from(FPS)),
            index: i,
            repeated: false,
        };
        enc.encode(&frame, &mut sink).unwrap();
    }
    enc.flush(&mut sink).unwrap();
    let secs = t0.elapsed().as_secs_f64();
    (out, FRAMES as f64 / secs)
}

/// Tipi dei NAL di un pacchetto Annex B (parser indipendente da quello del crate).
fn nal_types(data: &[u8], hevc: bool) -> Vec<u8> {
    (0..data.len().saturating_sub(3))
        .filter(|&i| data[i..i + 3] == [0, 0, 1])
        .map(|i| if hevc { (data[i + 3] >> 1) & 0x3f } else { data[i + 3] & 0x1f })
        .collect()
}

/// `codec`: "h264", "hevc" o "av1" (per AV1 niente controllo dei NAL: lo fa ffprobe sui frame).
fn check_packets(pkts: &[Pkt], codec: &str) {
    let hevc = codec == "hevc";
    for p in pkts.iter().filter(|_| codec != "av1") {
        let idr = nal_types(&p.data, hevc).iter().any(|&t| if hevc { t == 19 || t == 20 } else { t == 5 });
        assert_eq!(p.keyframe, idr, "keyframe se e solo se il pacchetto contiene un IDR");
    }
    assert_eq!(pkts.len() as u64, FRAMES, "un pacchetto per frame");
    assert!(pkts[0].keyframe, "il primo pacchetto deve essere un keyframe");
    for w in pkts.windows(2) {
        assert!(w[1].dts > w[0].dts, "dts strettamente crescenti in ordine di decodifica");
    }
    let mut pts: Vec<QpcTime> = pkts.iter().map(|p| p.pts).collect();
    pts.sort();
    pts.dedup();
    assert_eq!(pts.len() as u64, FRAMES, "pts unici");
    for p in pkts {
        assert!(p.pts >= p.dts, "pts >= dts");
    }
    let start = QpcTime(1_000_000_000);
    let key_frames: Vec<i64> = pkts
        .iter()
        .filter(|p| p.keyframe)
        .map(|p| (p.pts.hns_since(start) * i64::from(FPS) + HNS_PER_SEC / 2) / HNS_PER_SEC)
        .collect();
    assert_eq!(key_frames, vec![0, 60, 120], "un keyframe ogni 60 frame");
}

fn ffprobe(path: &PathBuf) -> String {
    let ffmpeg = std::env::var("FFMPEG_TOOLS_DIR").unwrap_or_else(|_| "D:/Repositories/cliprec/third_party/ffmpeg".into());
    let out = Command::new(PathBuf::from(ffmpeg).join("bin").join("ffprobe.exe"))
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=codec_name,width,height,nb_read_frames", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Decodifica il file con ffmpeg e confronta il pixel centrale di ogni frame con il colore pieno
/// disegnato in `run`: rileva superfici riscritte mentre l'encoder le legge e conversioni sbagliate.
/// `lag`: frame di scarto ammessi (prima o dopo), tranne che sui keyframe (0, 60, 120), che devono
/// coincidere. SVT-AV1 a CRF 28 nei livelli temporali alti non codifica variazioni di 1-4 livelli
/// su un campo piatto e ripete un frame di riferimento, anche successivo: misurato fino a 4.
fn check_colors(path: &PathBuf, w: u32, h: u32, lag: usize, tol: i32) {
    let ffmpeg = std::env::var("FFMPEG_TOOLS_DIR").unwrap_or_else(|_| "D:/Repositories/cliprec/third_party/ffmpeg".into());
    let out = Command::new(PathBuf::from(ffmpeg).join("bin").join("ffmpeg.exe"))
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-vf", &format!("crop=2:2:{}:{}", w / 2, h / 2), "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .unwrap();
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout.len() as u64, FRAMES * 12);
    let want = |i: usize| {
        let t = i as f32 / FRAMES as f32;
        [t, 1.0 - t, (i % 60) as f32 / 60.0].map(|c| (c * 255.0).round() as i32)
    };
    for (i, px) in out.stdout.chunks(12).enumerate() {
        let diff = |j: usize| (0..3).map(|c| (i32::from(px[c]) - want(j)[c]).abs()).max().unwrap();
        let lag = if i % 60 == 0 { 0 } else { lag };
        let ok = (i.saturating_sub(lag)..=(i + lag).min(FRAMES as usize - 1)).any(|j| diff(j) <= tol);
        assert!(ok, "frame {i}: {:?} invece di {:?}", &px[..3], want(i));
    }
}

/// Contenitore IVF minimo per AV1 (ffprobe non riconosce OBU nudi senza delimitatori temporali).
/// Se i pacchetti non portano il sequence header, lo prende da `extradata` (OBU grezzi).
fn write_ivf(name: &str, w: u32, h: u32, extradata: &[u8], pkts: &[Pkt]) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let mut b = Vec::new();
    b.extend_from_slice(b"DKIF");
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(b"AV01");
    b.extend_from_slice(&(w as u16).to_le_bytes());
    b.extend_from_slice(&(h as u16).to_le_bytes());
    b.extend_from_slice(&FPS.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&(pkts.len() as u32).to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    let start = QpcTime(1_000_000_000);
    // Un OBU sequence header (tipo 1) ha il primo byte 0x0a o 0x08: si aggiunge al primo pacchetto
    // se extradata è un OBU grezzo che il pacchetto non contiene già.
    let raw_obu = matches!(extradata.first(), Some(0x0a | 0x08));
    let needs_header = raw_obu && pkts.first().is_some_and(|p| !p.data.windows(extradata.len()).any(|w| w == extradata));
    for (i, p) in pkts.iter().enumerate() {
        let data = if i == 0 && needs_header { [extradata, &p.data].concat() } else { p.data.clone() };
        let pts = (p.pts.hns_since(start) * i64::from(FPS) + HNS_PER_SEC / 2) / HNS_PER_SEC;
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&(pts as u64).to_le_bytes());
        b.extend_from_slice(&data);
    }
    std::fs::write(&path, b).unwrap();
    path
}

/// Indici dei frame decodificati marcati come chiave da ffprobe.
fn probe_key_frames(path: &PathBuf) -> Vec<usize> {
    let ffmpeg = std::env::var("FFMPEG_TOOLS_DIR").unwrap_or_else(|_| "D:/Repositories/cliprec/third_party/ffmpeg".into());
    let out = Command::new(PathBuf::from(ffmpeg).join("bin").join("ffprobe.exe"))
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "frame=key_frame", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).lines().enumerate().filter(|(_, l)| l.trim() == "1").map(|(i, _)| i).collect()
}

/// Encoder di riserva SVT-AV1: aperto da `open_after` quando tutti gli hardware sono esclusi.
fn svtav1(size: (u32, u32), noise: bool, file: &str) -> Option<(f64, u64, f64)> {
    let gpu = device(NVIDIA).or_else(|| device(INTEL))?;
    let settings = EncoderSettings::from_config(&Default::default(), size.0, size.1);
    let hardware: Vec<String> =
        cliprec_encode::candidates(settings.codec).into_iter().map(|(_, n)| n).filter(|n| n != "libsvtav1").collect();
    let mut enc = cliprec_encode::open_after(&gpu, &settings, &hardware).unwrap();
    let params = enc.params().clone();
    assert_eq!(params.encoder_name, "libsvtav1");
    assert_eq!(params.codec, cliprec_core::VideoCodec::Av1);
    assert_eq!((params.width, params.height), size);
    let (pkts, fps) = run(&gpu, &mut enc, &[size], noise);
    check_packets(&pkts, "av1");
    let path = write_ivf(file, size.0, size.1, &params.extradata, &pkts);
    assert_eq!(ffprobe(&path), format!("av1,{},{},{FRAMES}", size.0, size.1));
    assert_eq!(probe_key_frames(&path), vec![0, 60, 120], "un keyframe ogni 60 frame (ffprobe)");
    if !noise {
        // CRF 28 su campi piatti: errore di quantizzazione fino a 8 livelli.
        check_colors(&path, size.0, size.1, 4, 10);
    }
    let bytes: usize = pkts.iter().map(|p| p.data.len()).sum();
    let bps = bytes as f64 * 8.0 / (FRAMES as f64 / f64::from(FPS));
    eprintln!(
        "libsvtav1 {}x{}{}: {fps:.0} fps di encode, {:.2} Mbit/s (tetto {:.2}), extradata {} B",
        size.0,
        size.1,
        if noise { " (rumore)" } else { "" },
        bps / 1e6,
        params.bit_rate as f64 / 1e6,
        params.extradata.len()
    );
    Some((bps, params.bit_rate, fps))
}

#[test]
fn open_after_falls_back_to_svtav1() {
    svtav1((1920, 1080), false, "libsvtav1_1080p.ivf");
}

#[test]
fn svtav1_1080p_noise_respects_bitrate_cap() {
    let Some((bps, cap, _)) = svtav1((1920, 1080), true, "libsvtav1_1080p_noise.ivf") else { return };
    assert!(bps <= cap as f64 * 1.35, "{bps} bit/s oltre il tetto {cap}");
}

#[test]
fn svtav1_1440p_colors() {
    svtav1((2560, 1440), false, "libsvtav1_1440p.ivf");
}

#[test]
fn svtav1_1440p_noise_respects_bitrate_cap() {
    let Some((bps, cap, _)) = svtav1((2560, 1440), true, "libsvtav1_1440p_noise.ivf") else { return };
    assert!(bps <= cap as f64 * 1.35, "{bps} bit/s oltre il tetto {cap}");
}

fn write_stream(name: &str, extradata: &[u8], pkts: &[Pkt]) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-output");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let mut bytes = extradata.to_vec();
    for p in pkts {
        bytes.extend_from_slice(&p.data);
    }
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Codifica, verifica i pacchetti e il file con ffprobe; restituisce (bit/s medi, tetto in bit/s).
fn encode_and_probe(
    vendor_id: u32,
    encoder: &str,
    sizes: &[(u32, u32)],
    out: (u32, u32),
    file: &str,
    probe_codec: &str,
    noise: bool,
) -> Option<(f64, u64)> {
    let Some(gpu) = device(vendor_id) else {
        eprintln!("SALTATO {encoder}: nessun adattatore {vendor_id:#x}");
        return None;
    };
    let settings = EncoderSettings {
        codec: CodecChoice::Auto,
        fps: FPS,
        width: out.0,
        height: out.1,
        quality: 23,
        max_bitrate_kbps: None,
        advanced: Default::default(),
    };
    let mut enc = cliprec_encode::open(&gpu, &settings, encoder).unwrap();
    let params = enc.params().clone();
    assert!(!params.extradata.is_empty(), "extradata vuoto");
    assert_eq!((params.width, params.height), out);
    let (pkts, fps) = run(&gpu, &mut enc, sizes, noise);
    check_packets(&pkts, probe_codec);
    let bytes: usize = pkts.iter().map(|p| p.data.len()).sum();
    let bps = bytes as f64 * 8.0 / (FRAMES as f64 / f64::from(FPS));
    let key_bytes = pkts.iter().filter(|p| p.keyframe).map(|p| p.data.len()).max().unwrap_or(0);
    let path = write_stream(file, &params.extradata, &pkts);
    let probe = ffprobe(&path);
    if !noise {
        // 8 livelli: senza B-frame (NVENC, DECISIONS 31) h264_nvenc a 720p non codifica alcune
        // variazioni di 1-4 livelli per frame del campo piatto: misurato fino a 7.
        check_colors(&path, out.0, out.1, 0, 8);
    }
    eprintln!(
        "{encoder}{} su {}: {fps:.0} fps di encode, {} pacchetti, {:.1} KB/pacchetto medi, keyframe max {:.1} KB, {:.2} Mbit/s (tetto {:.2}), extradata {} B (inizio {:02x?}); ffprobe: {probe}",
        if noise { " (rumore)" } else { "" },
        gpu.adapter_name,
        pkts.len(),
        bytes as f64 / pkts.len() as f64 / 1024.0,
        key_bytes as f64 / 1024.0,
        bps / 1e6,
        params.bit_rate as f64 / 1e6,
        params.extradata.len(),
        &params.extradata[..params.extradata.len().min(6)],
    );
    assert_eq!(probe, format!("{probe_codec},{},{},{FRAMES}", out.0, out.1));
    Some((bps, params.bit_rate))
}

const NVIDIA: u32 = 0x10de;
const INTEL: u32 = 0x8086;

/// Media su 3 s di rumore rispetto al tetto. Con un VBV di 1 s pari al tetto il limite teorico è
/// 4/3 (tetto più buffer iniziale); senza B-frame (DECISIONS 31) NVENC lo supera di circa il 2% sul
/// rumore puro, con un I-frame da 7 Mbit ogni secondo: misurato 1,36 a 1080p e 1,35 a 1440p (con
/// 2 B-frame 1,30 e 1,29).
const NVENC_NOISE_CAP: f64 = 1.40;

#[test]
fn hevc_nvenc_1080p_native() {
    encode_and_probe(NVIDIA, "hevc_nvenc", &[(1920, 1080)], (1920, 1080), "hevc_nvenc.hevc", "hevc", false);
}

#[test]
fn h264_nvenc_scaled_with_source_resize() {
    // 1080p e poi 4:3: scala in 1280×720, la seconda metà con bande laterali.
    encode_and_probe(NVIDIA, "h264_nvenc", &[(1920, 1080), (1600, 1200)], (1280, 720), "h264_nvenc.h264", "h264", false);
}

#[test]
fn hevc_nvenc_noise_respects_bitrate_cap() {
    let Some((bps, cap)) =
        encode_and_probe(NVIDIA, "hevc_nvenc", &[(1920, 1080)], (1920, 1080), "hevc_nvenc_noise.hevc", "hevc", true)
    else {
        return;
    };
    assert!(bps <= cap as f64 * NVENC_NOISE_CAP, "{bps} bit/s oltre il tetto {cap}");
}

#[test]
fn hevc_nvenc_1440p_noise() {
    let Some((bps, cap)) =
        encode_and_probe(NVIDIA, "hevc_nvenc", &[(2560, 1440)], (2560, 1440), "hevc_nvenc_1440p.hevc", "hevc", true)
    else {
        return;
    };
    assert!(bps <= cap as f64 * NVENC_NOISE_CAP, "{bps} bit/s oltre il tetto {cap}");
}

#[test]
fn hevc_qsv_on_intel() {
    encode_and_probe(INTEL, "hevc_qsv", &[(1920, 1080)], (1920, 1080), "hevc_qsv.hevc", "hevc", false);
}

#[test]
fn h264_qsv_on_intel() {
    encode_and_probe(INTEL, "h264_qsv", &[(2560, 1440)], (1920, 1080), "h264_qsv.h264", "h264", false);
}

#[test]
fn gpu_priority_high_then_normal() {
    let Some(gpu) = device(NVIDIA).or_else(|| device(INTEL)) else { return };
    cliprec_encode::set_gpu_priority(&gpu, true).unwrap();
    cliprec_encode::set_gpu_priority(&gpu, false).unwrap();
}

#[test]
fn open_errors_are_classified() {
    let Some(gpu) = device(NVIDIA) else { return };
    let settings = EncoderSettings::from_config(&Default::default(), 1920, 1080);
    // RTX 30: niente AV1 in encode.
    assert!(matches!(cliprec_encode::open(&gpu, &settings, "av1_nvenc").err(), Some(cliprec_core::EncodeError::Unsupported(_))));
    assert!(matches!(cliprec_encode::open(&gpu, &settings, "vp9_nvenc").err(), Some(cliprec_core::EncodeError::Unsupported(_))));
    // QSV su un device NVIDIA: il device QSV derivato non si crea.
    assert!(matches!(cliprec_encode::open(&gpu, &settings, "hevc_qsv").err(), Some(cliprec_core::EncodeError::Unsupported(_))));
}

/// Conteggio dei riferimenti COM del device (AddRef + Release).
fn refcount(dev: &windows::Win32::Graphics::Direct3D11::ID3D11Device) -> u32 {
    use windows::core::Interface;
    // SAFETY: AddRef seguito da Release sullo stesso puntatore valido: il conteggio non cambia.
    unsafe {
        let vt = dev.vtable();
        (vt.base__.AddRef)(dev.as_raw());
        (vt.base__.Release)(dev.as_raw())
    }
}

#[test]
fn com_references_are_balanced_nvenc() {
    com_references_are_balanced(NVIDIA, "hevc_nvenc");
}

#[test]
fn com_references_are_balanced_qsv() {
    com_references_are_balanced(INTEL, "hevc_qsv");
}

fn com_references_are_balanced(vendor_id: u32, name: &str) {
    let Some(gpu) = device(vendor_id) else { return };
    let settings = EncoderSettings::from_config(&Default::default(), 1920, 1080);
    // Un giro a vuoto: il runtime D3D/driver può tenere oggetti alla prima apertura.
    drop(cliprec_encode::open(&gpu, &settings, name).unwrap());
    let before = refcount(&gpu.device);
    for _ in 0..3 {
        let mut enc = cliprec_encode::open(&gpu, &settings, name).unwrap();
        run(&gpu, &mut enc, &[(1920, 1080), (1280, 1024)], false);
        assert!(refcount(&gpu.device) > before, "l'encoder deve tenere riferimenti al device");
    }
    assert_eq!(refcount(&gpu.device), before, "riferimenti al device non rilasciati");
}

#[test]
fn open_after_skips_failed_encoders() {
    let Some(gpu) = device(NVIDIA) else { return };
    let settings = EncoderSettings::from_config(&Default::default(), 1920, 1080);
    let enc = cliprec_encode::open_after(&gpu, &settings, &["hevc_nvenc".to_owned()]).unwrap();
    assert_eq!(enc.params().encoder_name, "h264_nvenc");
    let all: Vec<String> = cliprec_encode::candidates(settings.codec).into_iter().map(|(_, n)| n).collect();
    assert!(cliprec_encode::open_after(&gpu, &settings, &all).is_err());
}

/// Writer condiviso per catturare l'uscita di tracing.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn ffmpeg_logs_go_to_tracing() {
    let Some(gpu) = device(NVIDIA) else { return };
    let out = Captured::default();
    let writer = out.clone();
    let subscriber = tracing_subscriber::fmt().with_writer(move || writer.clone()).with_ansi(false).finish();
    // Globale: con un subscriber per thread l'interesse dei callsite, già registrati da altri test
    // in parallelo, può restare in cache come «mai». Nessun altro test installa un subscriber.
    tracing::subscriber::set_global_default(subscriber).unwrap();
    cliprec_encode::quiet_ffmpeg_logs();
    // QSV su un device NVIDIA: libvpl non crea la sessione e FFmpeg lo scrive a livello ERROR.
    let settings = EncoderSettings::from_config(&Default::default(), 1280, 720);
    assert!(cliprec_encode::open(&gpu, &settings, "hevc_qsv").is_err());
    let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    eprintln!("{text}");
    assert!(text.contains("ffmpeg") && text.contains("QSV"), "log FFmpeg non inoltrato: {text:?}");
}

#[test]
fn open_best_and_probe() {
    let Some(gpu) = device(NVIDIA) else { return };
    let found = cliprec_encode::probe(&gpu);
    eprintln!("probe su {}: {found:?}", gpu.adapter_name);
    assert!(found.iter().any(|(_, n)| n == "hevc_nvenc"));
    let settings = EncoderSettings::from_config(&Default::default(), 2560, 1440);
    let enc = cliprec_encode::open_best(&gpu, &settings).unwrap();
    assert_eq!(enc.params().encoder_name, "hevc_nvenc");
    assert_eq!(enc.params().bit_rate, 16_666_000);
}

/// Testo colorato: linee rosse larghe 1 pixel su nero, una colonna ogni 4. Restituisce il rosso
/// massimo (0-255) finito sulla colonna accanto dopo codifica NVENC e decodifica.
fn red_line_bleed(color_format: cliprec_core::config::ColorFormat) -> Option<(i32, i32, String)> {
    let Some(gpu) = device(NVIDIA) else {
        eprintln!("SALTATO: nessun adattatore NVIDIA");
        return None;
    };
    let (w, h) = (256u32, 64u32);
    // BGRA in little-endian: 0xAARRGGBB.
    let pixels: Vec<u32> = (0..w * h).map(|i| if i % w % 4 == 1 { 0xffff_0000 } else { 0xff00_0000 }).collect();
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let init = D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr().cast(), SysMemPitch: w * 4, SysMemSlicePitch: 0 };
    let mut tex = None;
    // SAFETY: descrittore, dati iniziali (w×h pixel da 4 byte) e puntatore d'uscita locali.
    unsafe { gpu.device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) }.unwrap();
    let tex = tex.unwrap();
    let mut settings = EncoderSettings::from_config(&Default::default(), w, h);
    settings.quality = 18;
    settings.advanced.color_format = color_format;
    let mut enc = cliprec_encode::open(&gpu, &settings, "hevc_nvenc").unwrap();
    let extradata = enc.params().extradata.clone();
    let mut pkts = Vec::new();
    let mut sink = |p: cliprec_core::PacketRef<'_>| pkts.push(Pkt { data: p.data.to_vec(), pts: p.pts, dts: p.dts, keyframe: p.keyframe });
    let start = QpcTime(1_000_000_000);
    for i in 0..30u64 {
        let frame = GpuFrame { texture: tex.clone(), width: w, height: h, pts: start.add_hns(i as i64 * HNS_PER_SEC / 60), index: i, repeated: false };
        enc.encode(&frame, &mut sink).unwrap();
    }
    enc.flush(&mut sink).unwrap();
    let path = write_stream(&format!("righe-rosse-{color_format:?}.hevc"), &extradata, &pkts);
    let ffmpeg = std::env::var("FFMPEG_TOOLS_DIR").unwrap_or_else(|_| "D:/Repositories/cliprec/third_party/ffmpeg".into());
    let bin = PathBuf::from(ffmpeg).join("bin");
    let out = Command::new(bin.join("ffmpeg.exe")).args(["-v", "error", "-i"]).arg(&path).args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"]).output().unwrap();
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    let size = (w * h * 3) as usize;
    let last = &out.stdout[out.stdout.len() - size..];
    let red = |x: u32, y: u32| i32::from(last[((y * w + x) * 3) as usize]);
    let (mut bleed, mut line) = (0, 255);
    for y in 8..h - 8 {
        for x in (4..w - 4).step_by(4) {
            bleed = bleed.max(red(x, y)).max(red(x + 2, y));
            line = line.min(red(x + 1, y));
        }
    }
    let probe = Command::new(bin.join("ffprobe.exe"))
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=pix_fmt", "-of", "csv=p=0"])
        .arg(&path)
        .output()
        .unwrap();
    Some((bleed, line, String::from_utf8_lossy(&probe.stdout).trim().to_owned()))
}

/// 4:4:4 (impostazioni avanzate): niente alone di colore attorno al testo. In 4:2:0 il rosso di una
/// linea da 1 pixel sbava sulla colonna accanto; in 4:4:4 resta nella sua.
#[test]
fn nvenc_444_keeps_colored_text_sharp() {
    use cliprec_core::config::ColorFormat;
    let Some((bleed420, line420, fmt420)) = red_line_bleed(ColorFormat::Yuv420) else { return };
    let Some((bleed444, line444, fmt444)) = red_line_bleed(ColorFormat::Yuv444) else { return };
    eprintln!("4:2:0 ({fmt420}): alone {bleed420}, linea {line420}; 4:4:4 ({fmt444}): alone {bleed444}, linea {line444}");
    assert_eq!(fmt444, "yuv444p");
    assert!(bleed420 >= 40, "il test deve vedere l'alone del 4:2:0 ({bleed420})");
    assert!(bleed444 <= 16, "alone in 4:4:4: {bleed444}");
    assert!(line444 >= 200, "la linea rossa deve restare rossa: {line444}");
}

/// 4:4:4: i colori pieni restano quelli disegnati (la conversione RGB→YUV è dell'encoder).
#[test]
fn hevc_nvenc_444_colors() {
    let Some(gpu) = device(NVIDIA) else { return };
    let mut settings = EncoderSettings::from_config(&Default::default(), 1920, 1080);
    settings.advanced.color_format = cliprec_core::config::ColorFormat::Yuv444;
    let mut enc = cliprec_encode::open(&gpu, &settings, "hevc_nvenc").unwrap();
    let extradata = enc.params().extradata.clone();
    let (pkts, fps) = run(&gpu, &mut enc, &[(1920, 1080)], false);
    eprintln!("hevc_nvenc 4:4:4: {fps:.0} fps di encode");
    let path = write_stream("hevc_nvenc_444.hevc", &extradata, &pkts);
    check_colors(&path, 1920, 1080, 0, 8);
}

/// Frame ripetuti (pacer senza frame nuovi, riempitivi dei tick persi): l'encoder rimanda l'ultima
/// superficie convertita senza rifare la conversione. Qui ogni frame dispari è marcato ripetuto ma
/// porta una texture magenta: nel file deve restare il colore del frame precedente.
fn repeated_frames_skip_conversion(gpu: &GpuDevice, encoder: &str, file: &str) {
    let (w, h) = (640, 360);
    let settings = EncoderSettings::from_config(&Default::default(), w, h);
    let mut enc = cliprec_encode::open(gpu, &settings, encoder).unwrap();
    let params = enc.params().clone();
    let (fresh, fresh_rtv) = texture(gpu, w, h, None);
    let (magenta, magenta_rtv) = texture(gpu, w, h, None);
    // SAFETY: vista valida sul device del contesto, protetto per l'uso da più thread.
    unsafe { gpu.context.ClearRenderTargetView(magenta_rtv.as_ref().unwrap(), &[1.0, 0.0, 1.0, 1.0]) };
    let color = |i: u64| [(i * 4) as f32 / 255.0, 0.5, 1.0 - (i * 4) as f32 / 255.0, 1.0];
    let mut pkts = Vec::new();
    let mut sink = |p: cliprec_core::PacketRef<'_>| {
        pkts.push(Pkt { data: p.data.to_vec(), pts: p.pts, dts: p.dts, keyframe: p.keyframe });
    };
    let n = 60u64;
    for i in 0..n {
        let repeated = i % 2 == 1;
        if !repeated {
            // SAFETY: come sopra.
            unsafe { gpu.context.ClearRenderTargetView(fresh_rtv.as_ref().unwrap(), &color(i)) };
        }
        let frame = GpuFrame {
            texture: if repeated { magenta.clone() } else { fresh.clone() },
            width: w,
            height: h,
            pts: QpcTime(1_000_000_000).add_hns(i as i64 * HNS_PER_SEC / i64::from(FPS)),
            index: i,
            repeated,
        };
        enc.encode(&frame, &mut sink).unwrap();
    }
    enc.flush(&mut sink).unwrap();
    let path = match params.codec {
        cliprec_core::VideoCodec::Av1 => write_ivf(file, w, h, &params.extradata, &pkts),
        _ => write_stream(file, &params.extradata, &pkts),
    };
    let ffmpeg = std::env::var("FFMPEG_TOOLS_DIR").unwrap_or_else(|_| "D:/Repositories/cliprec/third_party/ffmpeg".into());
    let out = Command::new(PathBuf::from(ffmpeg).join("bin").join("ffmpeg.exe"))
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-vf", &format!("crop=2:2:{}:{}", w / 2, h / 2), "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .unwrap();
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout.len() as u64, n * 12, "{encoder}: frame decodificati");
    for (i, px) in out.stdout.chunks(12).enumerate() {
        let want = color(i as u64 & !1).map(|c| (c * 255.0).round() as i32);
        let diff = (0..3).map(|c| (i32::from(px[c]) - want[c]).abs()).max().unwrap();
        assert!(diff <= 12, "{encoder}, frame {i}: {:?} invece di {want:?} (magenta = conversione rifatta)", &px[..3]);
    }
    eprintln!("{encoder}: {n} frame, i ripetuti rimandano la superficie precedente");
}

#[test]
fn repeated_frames_skip_conversion_nvenc() {
    let Some(gpu) = device(NVIDIA) else { return };
    repeated_frames_skip_conversion(&gpu, "hevc_nvenc", "repeated_nvenc.hevc");
}

#[test]
fn repeated_frames_skip_conversion_qsv() {
    let Some(gpu) = device(INTEL) else { return };
    repeated_frames_skip_conversion(&gpu, "hevc_qsv", "repeated_qsv.hevc");
}

#[test]
fn repeated_frames_skip_conversion_svtav1() {
    let Some(gpu) = device(NVIDIA).or_else(|| device(INTEL)) else { return };
    repeated_frames_skip_conversion(&gpu, "libsvtav1", "repeated_svtav1.ivf");
}
