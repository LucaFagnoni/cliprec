//! CPU del processo durante la cattura, con una callback vuota (nessun encoder). Non legge pixel.
//!
//! `cargo run -p cliprec-capture --release --example capture_cpu -- [wgc|dda|synthetic] [fps] [secondi]`

use std::error::Error;
use std::time::{Duration, Instant};

use cliprec_capture::{CaptureSource, SyntheticSource, Target, device_for_monitor};
use cliprec_core::VideoSource;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

fn cpu_seconds() -> Result<f64, Box<dyn Error>> {
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: pseudo-handle del processo corrente; puntatori locali.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u)? };
    let t = |f: FILETIME| ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) as f64 / 1e7;
    Ok(t(k) + t(u))
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let kind = args.first().map_or("wgc", String::as_str);
    let fps: u32 = args.get(1).map_or(Ok(60), |s| s.parse())?;
    let secs: u64 = args.get(2).map_or(Ok(5), |s| s.parse())?;
    let device = device_for_monitor(0)?;
    let mut src: Box<dyn VideoSource> = match kind {
        "synthetic" => Box::new(SyntheticSource::new(device, 1920, 1080)),
        "dda" => Box::new(CaptureSource::new(device, Target::Monitor(0), true).prefer_desktop_duplication(true)),
        _ => Box::new(CaptureSource::new(device, Target::Monitor(0), true)),
    };
    let (cpu0, wall0) = (cpu_seconds()?, Instant::now());
    src.start(fps, Box::new(|_| {}))?;
    std::thread::sleep(Duration::from_secs(secs));
    src.stop();
    let cpu = cpu_seconds()? - cpu0;
    let wall = wall0.elapsed().as_secs_f64();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    println!(
        "{} a {fps} fps per {secs} s: CPU {:.2}% di un core ({:.3}% del totale su {cores} thread)",
        src.describe(),
        100.0 * cpu / wall,
        100.0 * cpu / wall / cores
    );
    println!("{:?}", src.stats());
    Ok(())
}
