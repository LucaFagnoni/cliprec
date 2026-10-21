//! Il sink dei pacchetti lo chiama il thread dell'encoder: in un file a sé perché cargo esegue i
//! file di test uno alla volta, e in parallelo con gli altri test GPU l'encoder resta indietro e la
//! coda si riempie comunque (misurato: 87 ms invece di 0,35).

// Test: unwrap ammesso anche negli helper.
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Instant;

use cliprec_core::{GpuDevice, GpuFrame, HNS_PER_SEC, QpcTime};
use cliprec_encode::EncoderSettings;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

const FPS: u32 = 60;
/// Il sink (il buffer di replay) può fermarsi, per esempio per un blocco nuovo dell'arena o uno
/// snapshot: lo chiama il thread dell'encoder, quindi `encode` sul thread del pacer non aspetta.
/// Ogni 30 pacchetti il sink si ferma 80 ms; `encode` deve restare sotto un periodo a 60 fps.
#[test]
fn slow_sink_does_not_block_encode() {
    let Some(gpu) = GpuDevice::create(None).ok() else { return };
    let settings = EncoderSettings::from_config(&Default::default(), 1280, 720);
    let mut enc = cliprec_encode::open_best(&gpu, &settings).unwrap();
    let count = Arc::new(Mutex::new(0u32));
    let seen = count.clone();
    enc.set_sink(Box::new(move |_| {
        let mut n = seen.lock().unwrap();
        *n += 1;
        if n.is_multiple_of(30) {
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
    }))
    .unwrap();
    let desc = D3D11_TEXTURE2D_DESC {
        Width: 1280,
        Height: 720,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        ..Default::default()
    };
    let (mut tex, mut rtv) = (None, None);
    // SAFETY: descrittore e puntatori d'uscita locali; texture valida con BIND_RENDER_TARGET.
    unsafe {
        gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)).unwrap();
        gpu.device.CreateRenderTargetView(tex.as_ref().unwrap(), None, Some(&mut rtv)).unwrap();
    }
    let tex = tex.unwrap();
    let period = std::time::Duration::from_secs(1) / FPS;
    let t0 = Instant::now();
    let mut worst = std::time::Duration::ZERO;
    for i in 0..180u64 {
        // SAFETY: vista valida sul device del contesto, protetto per l'uso da più thread.
        unsafe { gpu.context.ClearRenderTargetView(rtv.as_ref().unwrap(), &[(i % 60) as f32 / 60.0, 0.2, 0.4, 1.0]) };
        let frame = GpuFrame { texture: tex.clone(), width: 1280, height: 720, pts: QpcTime(1_000_000_000).add_hns(i as i64 * HNS_PER_SEC / 60), index: i, repeated: false };
        let began = Instant::now();
        enc.encode(&frame).unwrap();
        worst = worst.max(began.elapsed());
        if let Some(wait) = (t0 + period * (i as u32 + 1)).checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    enc.flush().unwrap();
    let packets = *count.lock().unwrap();
    eprintln!("{}: encode più lenta {:.2} ms con il sink fermo 80 ms ogni 30 pacchetti; {packets} pacchetti", enc.params().encoder_name, worst.as_secs_f64() * 1000.0);
    assert!(worst < period, "encode ha aspettato il sink: {worst:?}");
    assert_eq!(packets, 180, "pacchetti persi");
}
