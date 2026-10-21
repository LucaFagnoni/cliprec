//! Memoria privata residente (working set privato) per fase: device D3D11, encoder aperto, dopo
//! 120 frame a 1080p60, encoder chiuso. Binario a parte con un solo test: la misura è di tutto il
//! processo. Stampa la tabella con `--nocapture`.
//!
//! Con NVENC quasi tutto il costo dell'encoder sono i buffer di bitstream in RAM di sistema, uno per
//! superficie, circa 2,5 byte per pixel ciascuno (5,2 MB a 1080p), che il driver tiene residenti.
//! QSV non ha limite: il driver Intel tiene una cache che si stabilizza solo dopo alcune aperture.
#![allow(clippy::unwrap_used)]

use cliprec_core::gpu::luid_to_i64;
use cliprec_core::{GpuDevice, GpuFrame, HNS_PER_SEC, QpcTime};
use cliprec_encode::EncoderSettings;
use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2};
use windows::Win32::System::Threading::GetCurrentProcess;

const MB: f64 = 1e6;

fn private_ws() -> f64 {
    let mut c = PROCESS_MEMORY_COUNTERS_EX2 { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32, ..Default::default() };
    // SAFETY: struttura locale della dimensione dichiarata in `cb`; pseudo-handle sempre valido.
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), std::ptr::from_mut(&mut c).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb) }.unwrap();
    c.PrivateWorkingSetSize as f64 / MB
}

fn device(vendor_id: u32) -> Option<GpuDevice> {
    // SAFETY: nessuna precondizione.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    // SAFETY: factory valida; EnumAdapters1 fallisce oltre l'ultimo adattatore.
    let luid = (0..).map_while(|i| unsafe { factory.EnumAdapters1(i) }.ok()).find_map(|a| {
        // SAFETY: adattatore valido.
        let d = unsafe { a.GetDesc1() }.ok()?;
        (d.VendorId == vendor_id).then(|| luid_to_i64(d.AdapterLuid))
    })?;
    GpuDevice::create(Some(luid)).ok()
}

/// (encoder aperto, dopo 120 frame, dopo la chiusura) in MB sopra il device già creato, e fps di
/// codifica alla massima velocità.
fn stages(gpu: &GpuDevice, name: &str) -> (f64, f64, f64, f64) {
    let settings = EncoderSettings::from_config(&Default::default(), 1920, 1080);
    let desc = D3D11_TEXTURE2D_DESC {
        Width: 1920,
        Height: 1080,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..Default::default()
    };
    let mut tex = None;
    // SAFETY: descrittore e puntatore d'uscita locali.
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.unwrap();
    let tex = tex.unwrap();
    let base = private_ws();
    let mut enc = cliprec_encode::open(gpu, &settings, name).unwrap();
    let opened = private_ws() - base;
    let t0 = std::time::Instant::now();
    for i in 0..120u64 {
        let pts = QpcTime(1_000_000_000).add_hns(i as i64 * HNS_PER_SEC / 60);
        let frame = GpuFrame { texture: tex.clone(), width: 1920, height: 1080, pts, index: i, repeated: false };
        enc.encode(&frame).unwrap();
    }
    enc.flush().unwrap();
    let fps = 120.0 / t0.elapsed().as_secs_f64();
    let encoded = private_ws() - base;
    drop(enc);
    (opened, encoded, private_ws() - base, fps)
}

#[test]
fn encoder_memory_by_stage() {
    cliprec_encode::quiet_ffmpeg_logs();
    let start = private_ws();
    for (vendor, name) in [(0x10de, "hevc_nvenc"), (0x10de, "h264_nvenc"), (0x8086, "hevc_qsv"), (0x10de, "libsvtav1")] {
        let before = private_ws();
        let Some(gpu) = device(vendor) else {
            eprintln!("{name}: adattatore assente, salto");
            continue;
        };
        let dev = private_ws() - before;
        // Una prima apertura a vuoto: il driver alloca una volta le sue strutture per il device.
        drop(cliprec_encode::open(&gpu, &EncoderSettings::from_config(&Default::default(), 1920, 1080), name).unwrap());
        let warm = private_ws() - before - dev;
        let (opened, encoded, closed, fps) = stages(&gpu, name);
        eprintln!(
            "{name:<11} device {dev:5.1} MB, prima apertura {warm:5.1}, aperto {opened:5.1}, dopo 120 frame {encoded:5.1}, chiuso {closed:5.1} ({fps:.0} fps)"
        );
        // Misurati su RTX 3080: hevc_nvenc 30 MB (84 con 2 B-frame, cioè 12 superfici invece di 4);
        // libsvtav1 660 MB con lp=4 (circa 1 GB con il parallelismo automatico su 28 thread).
        let limit = match name {
            "hevc_nvenc" | "h264_nvenc" => 45.0,
            "libsvtav1" => 800.0,
            _ => f64::INFINITY,
        };
        assert!(encoded < limit, "{name} a 1080p: {encoded:.1} MB residenti dopo la codifica (limite {limit})");
    }
    eprintln!("processo: {start:.1} MB all'inizio, {:.1} MB alla fine", private_ws());
}
