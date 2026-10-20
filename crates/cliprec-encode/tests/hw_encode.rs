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
use cliprec_core::{GpuDevice, GpuFrame, HNS_PER_SEC, QpcTime, VideoEncoder};
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
fn run(gpu: &GpuDevice, enc: &mut dyn VideoEncoder, sizes: &[(u32, u32)], noise: bool) -> (Vec<Pkt>, f64) {
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

fn check_packets(pkts: &[Pkt]) {
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
    let ffmpeg = std::env::var("FFMPEG_DIR").unwrap_or_else(|_| r"D:\Repositories\cliprec\third_party\ffmpeg".into());
    let out = Command::new(PathBuf::from(ffmpeg).join("bin").join("ffprobe.exe"))
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=codec_name,width,height,nb_read_frames", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
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
    };
    let mut enc = cliprec_encode::open(&gpu, &settings, encoder).unwrap();
    let params = enc.params().clone();
    assert!(!params.extradata.is_empty(), "extradata vuoto");
    assert_eq!((params.width, params.height), out);
    let (pkts, fps) = run(&gpu, enc.as_mut(), sizes, noise);
    check_packets(&pkts);
    let bytes: usize = pkts.iter().map(|p| p.data.len()).sum();
    let bps = bytes as f64 * 8.0 / (FRAMES as f64 / f64::from(FPS));
    let key_bytes = pkts.iter().filter(|p| p.keyframe).map(|p| p.data.len()).max().unwrap_or(0);
    let path = write_stream(file, &params.extradata, &pkts);
    let probe = ffprobe(&path);
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
    // VBV di 1 s pari al tetto: su 3 s la media resta sotto il tetto più il buffer iniziale.
    assert!(bps <= cap as f64 * 1.35, "{bps} bit/s oltre il tetto {cap}");
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
fn com_references_are_balanced() {
    let Some(gpu) = device(NVIDIA).or_else(|| device(INTEL)) else { return };
    let name = if gpu.adapter_name.contains("NVIDIA") { "hevc_nvenc" } else { "hevc_qsv" };
    let settings = EncoderSettings::from_config(&Default::default(), 1920, 1080);
    // Un giro a vuoto: il runtime D3D/driver può tenere oggetti alla prima apertura.
    drop(cliprec_encode::open(&gpu, &settings, name).unwrap());
    let before = refcount(&gpu.device);
    for _ in 0..3 {
        let mut enc = cliprec_encode::open(&gpu, &settings, name).unwrap();
        run(&gpu, enc.as_mut(), &[(1920, 1080), (1280, 1024)], false);
        assert!(refcount(&gpu.device) > before, "l'encoder deve tenere riferimenti al device");
    }
    assert_eq!(refcount(&gpu.device), before, "riferimenti al device non rilasciati");
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
