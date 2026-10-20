//! Client della named pipe del core: una richiesta, una riga JSON di risposta.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::core::PCWSTR;

use cliprec_core::EngineStatus;
use cliprec_core::ipc::{IpcRequest, IpcResponse, decode_line, encode_line, pipe_name};

use crate::i18n::Lang;

/// Nome della pipe; `CLIPREC_PIPE` lo sovrascrive (prove con un core finto).
pub fn pipe_path() -> String {
    std::env::var("CLIPREC_PIPE").unwrap_or_else(|_| pipe_name())
}

const ERROR_PIPE_BUSY: i32 = 231;
const BUSY_WAIT_MS: u32 = 2000;

/// Apre la pipe; se tutte le istanze sono occupate attende con `WaitNamedPipeW` e riprova
/// (il core serve un client alla volta: «Salva» non deve coincidere con un errore di stato).
fn open_pipe(path: &str) -> std::io::Result<std::fs::File> {
    let deadline = std::time::Instant::now() + Duration::from_millis(u64::from(BUSY_WAIT_MS));
    loop {
        match OpenOptions::new().read(true).write(true).open(path) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && std::time::Instant::now() < deadline => {
                let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
                // SAFETY: `wide` è terminata da NUL e vive per tutta la chiamata. L'esito non conta:
                // se l'attesa scade o fallisce si ritenta finché non scade la scadenza complessiva.
                let _ = unsafe { WaitNamedPipeW(PCWSTR(wide.as_ptr()), BUSY_WAIT_MS) };
            }
            other => return other,
        }
    }
}

/// Invia una richiesta e legge la risposta. Errore se il core non c'è o non risponde bene.
pub fn request_on(path: &str, req: &IpcRequest) -> anyhow::Result<IpcResponse> {
    let mut pipe = open_pipe(path)?;
    pipe.write_all(encode_line(req)?.as_bytes())?;
    pipe.flush()?;
    let mut line = String::new();
    BufReader::new(pipe).read_line(&mut line)?;
    if line.is_empty() {
        anyhow::bail!("pipe chiusa senza risposta");
    }
    Ok(decode_line(&line)?)
}

pub fn request(req: &IpcRequest) -> anyhow::Result<IpcResponse> {
    request_on(&pipe_path(), req)
}

/// Riga del pannello di stato.
pub fn format_status(lang: Lang, s: &EngineStatus) -> String {
    let yn = |b: bool| lang.pick(if b { "sì" } else { "no" }, if b { "yes" } else { "no" });
    let mb = s.buffer_bytes as f64 / 1_048_576.0;
    let mut t = if s.video_enabled {
        format!(
            "{}: {} · {:.0} fps\n{}: {}\n{}: {:.0} MB · {}: {:.0} s\n{}: {}",
            lang.pick("Encoder", "Encoder"),
            if s.encoder.is_empty() { "-" } else { &s.encoder },
            s.fps,
            lang.pick("frame persi", "dropped frames"),
            s.frames_dropped,
            "RAM",
            mb,
            lang.pick("coperti", "covered"),
            s.video_seconds,
            lang.pick("pagine bloccate", "pages locked"),
            yn(s.ram_locked),
        )
    } else {
        format!(
            "{}\nRAM: {:.0} MB · {}: {:.0} s\n{}: {}",
            lang.pick("Solo audio", "Audio only"),
            mb,
            lang.pick("coperti", "covered"),
            s.audio_seconds,
            lang.pick("pagine bloccate", "pages locked"),
            yn(s.ram_locked),
        )
    };
    if !s.buffer_running {
        t.push_str(lang.pick("\nBuffer fermo", "\nBuffer stopped"));
    }
    if let Some(e) = &s.last_error {
        t.push_str(&format!("\n{e}"));
    }
    t
}

pub fn offline_text(lang: Lang) -> &'static str {
    lang.pick("ClipRec non è in esecuzione", "ClipRec is not running")
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::{FromRawHandle, RawHandle};

    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT};
    use windows::core::PCWSTR;

    use super::*;

    #[test]
    fn talks_to_a_fake_core_and_handles_absence() {
        let name = format!(r"\\.\pipe\cliprec-settings-test-{}", std::process::id());
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` è terminata da NUL e vive oltre la chiamata; gli altri parametri sono valori semplici.
        let h = unsafe {
            CreateNamedPipeW(PCWSTR(wide.as_ptr()), PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE | PIPE_WAIT, 1, 4096, 4096, 0, None)
        };
        assert!(!h.is_invalid());
        let raw = h.0 as usize;
        let server = std::thread::spawn(move || {
            // SAFETY: l'handle è la pipe appena creata e passa a questo thread, che ne diventa l'unico proprietario.
            unsafe {
                let h = windows::Win32::Foundation::HANDLE(raw as *mut _);
                let _ = ConnectNamedPipe(h, None);
                let mut f = std::fs::File::from_raw_handle(raw as RawHandle);
                let mut line = String::new();
                BufReader::new(&mut f).read_line(&mut line).unwrap();
                let req: IpcRequest = decode_line(&line).unwrap();
                let resp = match req {
                    IpcRequest::Status => IpcResponse::Status(EngineStatus { encoder: "NVENC".into(), fps: 60.0, ..Default::default() }),
                    _ => IpcResponse::Ok,
                };
                f.write_all(encode_line(&resp).unwrap().as_bytes()).unwrap();
                f.flush().unwrap();
                // Attende che il client abbia letto prima di chiudere.
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        });
        match request_on(&name, &IpcRequest::Status).unwrap() {
            IpcResponse::Status(s) => assert_eq!(s.encoder, "NVENC"),
            other => panic!("risposta inattesa: {other:?}"),
        }
        server.join().unwrap();
        // Nessun server: errore, non blocco.
        assert!(request_on(r"\\.\pipe\cliprec-settings-test-assente", &IpcRequest::Status).is_err());
    }

    #[test]
    fn status_line_mentions_key_fields() {
        let s = EngineStatus { buffer_running: true, video_enabled: true, encoder: "NVENC HEVC".into(), fps: 60.0, ram_locked: true, ..Default::default() };
        let t = format_status(Lang::It, &s);
        assert!(t.contains("NVENC HEVC") && t.contains("60 fps") && t.contains("pagine bloccate: sì"));
    }

    #[test]
    fn busy_pipe_is_waited_for_not_reported_as_offline() {
        use windows::Win32::System::Pipes::DisconnectNamedPipe;
        let name = format!(r"\\.\pipe\cliprec-settings-test-busy-{}", std::process::id());
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` è terminata da NUL e vive oltre la chiamata; un'unica istanza consentita.
        let h = unsafe {
            CreateNamedPipeW(PCWSTR(wide.as_ptr()), PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE | PIPE_WAIT, 1, 4096, 4096, 0, None)
        };
        assert!(!h.is_invalid());
        let raw = h.0 as usize;
        let server = std::thread::spawn(move || {
            // SAFETY: l'handle è la pipe creata dal test e appartiene solo a questo thread.
            unsafe {
                let h = windows::Win32::Foundation::HANDLE(raw as *mut _);
                let _ = ConnectNamedPipe(h, None); // primo client: occupa l'unica istanza
                std::thread::sleep(std::time::Duration::from_millis(200));
                let _ = DisconnectNamedPipe(h);
                let _ = ConnectNamedPipe(h, None); // il client che ha atteso
                let mut f = std::fs::File::from_raw_handle(raw as RawHandle);
                let mut line = String::new();
                BufReader::new(&mut f).read_line(&mut line).unwrap();
                f.write_all(encode_line(&IpcResponse::Ok).unwrap().as_bytes()).unwrap();
                f.flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        });
        let hold = std::fs::OpenOptions::new().read(true).write(true).open(&name).unwrap();
        let res = request_on(&name, &IpcRequest::ReloadConfig);
        drop(hold);
        assert_eq!(res.unwrap(), IpcResponse::Ok);
        server.join().unwrap();
    }
}
