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
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
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

/// Texture BGRA con render target, come quelle che produce la cattura.
fn texture(gpu: &GpuDevice, w: u32, h: u32) -> (ID3D11Texture2D, ID3D11RenderTargetView) {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        ..Default::default()
    };
    let mut tex = None;
    // SAFETY: descrittore e puntatore d'uscita locali.
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.unwrap();
    let tex = tex.unwrap();
    let mut rtv = None;
    // SAFETY: texture valida con BIND_RENDER_TARGET; puntatore d'uscita locale.
    unsafe { gpu.device.CreateRenderTargetView(&tex, None, Some(&mut rtv)) }.unwrap();
    (tex, rtv.unwrap())
}

#[derive(Debug)]
struct Pkt {
    data: Vec<u8>,
    pts: QpcTime,
    dts: QpcTime,
    keyframe: bool,
}

/// Codifica `FRAMES` frame; le dimensioni della sorgente cambiano a metà se `sizes` ne ha due.
fn run(gpu: &GpuDevice, enc: &mut dyn VideoEncoder, sizes: &[(u32, u32)]) -> (Vec<Pkt>, f64) {
    let targets: Vec<_> = sizes.iter().map(|&(w, h)| [texture(gpu, w, h), texture(gpu, w, h)]).collect();
    let start = QpcTime(1_000_000_000);
    let mut out = Vec::new();
    let mut sink = |p: cliprec_core::PacketRef<'_>| {
        out.push(Pkt { data: p.data.to_vec(), pts: p.pts, dts: p.dts, keyframe: p.keyframe });
    };
    let t0 = Instant::now();
    for i in 0..FRAMES {
        let part = (i as usize * sizes.len()) / FRAMES as usize;
        let (w, h) = sizes[part];
        let (tex, rtv) = &targets[part][i as usize % 2];
        let t = i as f32 / FRAMES as f32;
        // SAFETY: vista valida sul device del contesto, protetto per l'uso da più thread.
        unsafe { gpu.context.ClearRenderTargetView(rtv, &[t, 1.0 - t, (i % 60) as f32 / 60.0, 1.0]) };
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

fn encode_and_probe(vendor_id: u32, encoder: &str, sizes: &[(u32, u32)], out: (u32, u32), file: &str, probe_codec: &str) {
    let Some(gpu) = device(vendor_id) else {
        eprintln!("SALTATO {encoder}: nessun adattatore {vendor_id:#x}");
        return;
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
    let (pkts, fps) = run(&gpu, enc.as_mut(), sizes);
    check_packets(&pkts);
    let bytes: usize = pkts.iter().map(|p| p.data.len()).sum();
    let key_bytes = pkts.iter().filter(|p| p.keyframe).map(|p| p.data.len()).max().unwrap_or(0);
    let path = write_stream(file, &params.extradata, &pkts);
    let probe = ffprobe(&path);
    eprintln!(
        "{encoder} su {}: {fps:.0} fps di encode, {} pacchetti, {:.1} KB/pacchetto medi, keyframe max {:.1} KB, {:.2} Mbit/s, extradata {} B (inizio {:02x?}); ffprobe: {probe}",
        gpu.adapter_name,
        pkts.len(),
        bytes as f64 / pkts.len() as f64 / 1024.0,
        key_bytes as f64 / 1024.0,
        bytes as f64 * 8.0 / (FRAMES as f64 / f64::from(FPS)) / 1e6,
        params.extradata.len(),
        &params.extradata[..params.extradata.len().min(6)],
    );
    assert_eq!(probe, format!("{probe_codec},{},{},{FRAMES}", out.0, out.1));
}

const NVIDIA: u32 = 0x10de;
const INTEL: u32 = 0x8086;

#[test]
fn hevc_nvenc_1080p_native() {
    encode_and_probe(NVIDIA, "hevc_nvenc", &[(1920, 1080)], (1920, 1080), "hevc_nvenc.hevc", "hevc");
}

#[test]
fn h264_nvenc_scaled_with_source_resize() {
    // 1080p e poi 4:3: scala in 1280×720, la seconda metà con bande laterali.
    encode_and_probe(NVIDIA, "h264_nvenc", &[(1920, 1080), (1600, 1200)], (1280, 720), "h264_nvenc.h264", "h264");
}

#[test]
fn hevc_qsv_on_intel() {
    encode_and_probe(INTEL, "hevc_qsv", &[(1920, 1080)], (1920, 1080), "hevc_qsv.hevc", "hevc");
}

#[test]
fn h264_qsv_on_intel() {
    encode_and_probe(INTEL, "h264_qsv", &[(2560, 1440)], (1920, 1080), "h264_qsv.h264", "h264");
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
