//! Client della named pipe del core: una richiesta, una riga JSON di risposta.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};

use cliprec_core::EngineStatus;
use cliprec_core::ipc::{IpcRequest, IpcResponse, decode_line, encode_line, pipe_name};

use crate::i18n::Lang;

/// Nome della pipe; `CLIPREC_PIPE` lo sovrascrive (prove con un core finto).
pub fn pipe_path() -> String {
    std::env::var("CLIPREC_PIPE").unwrap_or_else(|_| pipe_name())
}

/// Invia una richiesta e legge la risposta. Errore se il core non c'è o non risponde bene.
pub fn request_on(path: &str, req: &IpcRequest) -> anyhow::Result<IpcResponse> {
    let mut pipe = OpenOptions::new().read(true).write(true).open(path)?;
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
            "{}: {} · {:.0} fps · {}: {} · {}: {:.0} MB · {}: {:.0} s · {}: {}",
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
            "{} · RAM: {:.0} MB · {}: {:.0} s · {}: {}",
            lang.pick("Solo audio", "Audio only"),
            mb,
            lang.pick("coperti", "covered"),
            s.audio_seconds,
            lang.pick("pagine bloccate", "pages locked"),
            yn(s.ram_locked),
        )
    };
    if !s.buffer_running {
        t.push_str(lang.pick(" · buffer fermo", " · buffer stopped"));
    }
    if let Some(e) = &s.last_error {
        t.push_str(&format!(" · {e}"));
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
}
