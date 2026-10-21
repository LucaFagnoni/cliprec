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
        let frame = GpuFrame { texture: tex.clone(), width: 1280, height: 720, pts: QpcTime(1_000_000_000).add_hns(i as i64 * HNS_PER_SEC / 60), index: i, repeated: false, hold: None };
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

/// Regressione 0.1.10-0.1.12: l'encoder fermo (qui il sink bloccato 500 ms, in gioco la GPU satura)
/// riempiva la coda, il pacer aspettava fino a 2 s e poi l'encoder era dichiarato guasto, con il
/// passaggio a un altro codec e il buffer svuotato. Ora `encode` non aspetta: salta i frame, senza
/// errori, e riprende ad accodare appena l'encoder si libera.
#[test]
fn full_queue_skips_frames_without_blocking_or_failing() {
    use cliprec_encode::Encoded;
    let Some(gpu) = GpuDevice::create(None).ok() else { return };
    let settings = EncoderSettings::from_config(&Default::default(), 1280, 720);
    let mut enc = cliprec_encode::open_best(&gpu, &settings).unwrap();
    let blocked = Arc::new(Mutex::new(true));
    let gate = blocked.clone();
    enc.set_sink(Box::new(move |_| {
        if std::mem::take(&mut *gate.lock().unwrap()) {
            std::thread::sleep(std::time::Duration::from_millis(500));
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
    let mut tex = None;
    // SAFETY: descrittore e puntatore d'uscita locali.
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.unwrap();
    let tex = tex.unwrap();
    let period = std::time::Duration::from_secs(1) / FPS;
    let t0 = Instant::now();
    let (mut worst, mut skipped, mut queued_after) = (std::time::Duration::ZERO, 0, 0);
    for i in 0..90u64 {
        let frame = GpuFrame { texture: tex.clone(), width: 1280, height: 720, pts: QpcTime(1_000_000_000).add_hns(i as i64 * HNS_PER_SEC / 60), index: i, repeated: false, hold: None };
        let began = Instant::now();
        let outcome = enc.encode(&frame).unwrap();
        worst = worst.max(began.elapsed());
        match outcome {
            Encoded::Skipped => skipped += 1,
            Encoded::Queued if skipped > 0 => queued_after += 1,
            Encoded::Queued => {}
        }
        if let Some(wait) = (t0 + period * (i as u32 + 1)).checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    enc.flush().unwrap();
    eprintln!("sink fermo 500 ms: {skipped} frame saltati, {queued_after} accodati dopo, encode più lenta {:.2} ms", worst.as_secs_f64() * 1000.0);
    assert!(skipped > 0, "il test deve riempire la coda");
    assert!(queued_after > 20, "dopo lo sblocco l'encoder deve riprendere: {queued_after}");
    assert!(worst < period, "encode ha aspettato: {worst:?}");
}

/// Miglioria C: con `hold` la conversione la fa il thread dell'encoder, non chi chiama `encode`
/// (il pacer). Il prestito della superficie lo rilascia quel thread a conversione accodata, e dopo
/// il flush nessun prestito resta aperto.
#[test]
fn frames_with_hold_are_converted_off_the_caller_thread() {
    use cliprec_core::FrameHold;
    use cliprec_encode::Encoded;
    use std::thread::ThreadId;
    struct Guard(Arc<Mutex<Vec<ThreadId>>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.lock().unwrap().push(std::thread::current().id());
        }
    }
    let Some(gpu) = GpuDevice::create(None).ok() else { return };
    let settings = EncoderSettings::from_config(&Default::default(), 1280, 720);
    let mut enc = cliprec_encode::open_best(&gpu, &settings).unwrap();
    let packets = Arc::new(Mutex::new(0u32));
    let seen = packets.clone();
    enc.set_sink(Box::new(move |_| *seen.lock().unwrap() += 1)).unwrap();
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
    let mut tex = None;
    // SAFETY: descrittore e puntatore d'uscita locali.
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.unwrap();
    let tex = tex.unwrap();
    let released = Arc::new(Mutex::new(Vec::new()));
    let period = std::time::Duration::from_secs(1) / FPS;
    let mut queued = 0;
    for i in 0..60u64 {
        let hold = FrameHold::new(Guard(released.clone()));
        let frame = GpuFrame { texture: tex.clone(), width: 1280, height: 720, pts: QpcTime(1_000_000_000).add_hns(i as i64 * HNS_PER_SEC / 60), index: i, repeated: false, hold: Some(hold) };
        queued += u32::from(enc.encode(&frame).unwrap() == Encoded::Queued);
        drop(frame);
        std::thread::sleep(period);
    }
    enc.flush().unwrap();
    let released = released.lock().unwrap();
    let me = std::thread::current().id();
    let elsewhere = released.iter().filter(|&&t| t != me).count();
    eprintln!("{queued} frame accodati, {elsewhere} prestiti rilasciati dalla conversione");
    assert_eq!(released.len(), 60, "prestiti rimasti aperti");
    assert!(queued > 50 && elsewhere > 0, "{queued} accodati, {elsewhere} rilasciati altrove");
    assert_eq!(*packets.lock().unwrap(), queued, "pacchetti persi");
}
