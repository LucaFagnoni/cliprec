//! Server della named pipe usata da `cliprec-settings`. Una riga JSON per richiesta e per risposta.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use cliprec_core::ipc::{IpcRequest, IpcResponse, decode_line, encode_line};
use cliprec_core::{AppCommand, EngineStatus};
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::core::PCWSTR;

use crate::supervisor::spawn_supervised;
use crate::sys::wide;

/// Tutto ciò che serve per rispondere alle richieste.
#[derive(Clone)]
pub struct Context {
    pub status: Arc<Mutex<EngineStatus>>,
    pub commands: Sender<AppCommand>,
    /// Chiamata su `ReloadConfig`, oltre all'inoltro del comando al motore (es. ricarica le hotkey).
    pub on_reload: Arc<dyn Fn() + Send + Sync>,
    /// Chiamata su `Quit`, dopo aver inoltrato `AppCommand::Quit` al motore: chiude il loop della shell.
    pub on_quit: Arc<dyn Fn() + Send + Sync>,
}

/// Risposta a una richiesta (pura rispetto alla pipe, così si prova senza Win32).
pub fn handle_request(ctx: &Context, req: IpcRequest) -> IpcResponse {
    let sent = |c| match ctx.commands.send(c) {
        Ok(()) => IpcResponse::Ok,
        Err(_) => IpcResponse::Error("il motore non è in ascolto".into()),
    };
    match req {
        IpcRequest::Status => match ctx.status.lock() {
            Ok(s) => IpcResponse::Status(s.clone()),
            Err(_) => IpcResponse::Error("stato non disponibile".into()),
        },
        IpcRequest::ReloadConfig => {
            let r = sent(AppCommand::ReloadConfig);
            (ctx.on_reload)();
            r
        }
        IpcRequest::TestClip => sent(AppCommand::SaveTestClip),
        IpcRequest::SaveVideoClip => sent(AppCommand::SaveVideoClip),
        IpcRequest::SaveAudioClip => sent(AppCommand::SaveAudioClip),
        IpcRequest::Quit => {
            let r = sent(AppCommand::Quit);
            (ctx.on_quit)();
            r
        }
    }
}

fn serve_connection(ctx: &Context, file: File) {
    let mut reader = BufReader::new(&file);
    let mut writer = &file;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let resp = match decode_line::<IpcRequest>(&line) {
            // Un panic nel gestore non deve uccidere la connessione: risposta d'errore e si prosegue.
            Ok(req) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle_request(ctx, req)))
                .unwrap_or_else(|_| IpcResponse::Error("errore interno".into())),
            Err(e) => IpcResponse::Error(format!("richiesta non valida: {e}")),
        };
        let Ok(out) = encode_line(&resp) else { return };
        if writer.write_all(out.as_bytes()).is_err() {
            return;
        }
    }
}

/// Crea un'istanza della pipe accessibile solo all'utente corrente e non ai client remoti.
fn create_instance(name: &[u16], sddl: &[u16], first: bool) -> anyhow::Result<RawHandle> {
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `sddl` è NUL-terminata; `sd` viene allocato da Windows e liberato sotto.
    unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut sd, None) }?;
    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: `name` è NUL-terminata; `attrs` e il descrittore vivono per tutta la chiamata.
    let h = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            16,
            16 * 1024,
            16 * 1024,
            0,
            Some(&attrs),
        )
    };
    // SAFETY: il descrittore è stato allocato da ConvertString... e non serve più.
    let _ = unsafe { LocalFree(Some(HLOCAL(sd.0))) };
    if h.is_invalid() {
        anyhow::bail!("CreateNamedPipeW: {}", windows::core::Error::from_thread());
    }
    Ok(h.0)
}

/// Thread in ascolto contemporaneo sulla pipe.
const ACCEPTORS: usize = 4;

fn accept_loop(name: &[u16], sddl: &[u16], ctx: &Context, pending: &mut Option<usize>) {
    loop {
        let raw = match pending.take() {
            Some(h) => h as RawHandle,
            None => match create_instance(name, sddl, false) {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!("pipe: {e}");
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            },
        };
        // SAFETY: `raw` è un handle di pipe valido di cui prendiamo la proprietà.
        let file = unsafe { File::from_raw_handle(raw) };
        // SAFETY: l'handle resta valido finché `file` vive; chiamata sincrona.
        let connected = unsafe { ConnectNamedPipe(windows::Win32::Foundation::HANDLE(raw), None) };
        // Un client può collegarsi tra la creazione e la connect: non è un errore.
        // SAFETY: legge l'ultimo errore del thread.
        if connected.is_err() && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
            continue;
        }
        let ctx = ctx.clone();
        if let Err(e) = std::thread::Builder::new().name("ipc-conn".into()).spawn(move || serve_connection(&ctx, file)) {
            tracing::error!("thread connessione: {e}");
        }
    }
}

/// Avvia il server su `pipe_name` in un thread supervisionato. Se un'altra istanza possiede già
/// la pipe (o la creazione fallisce) restituisce l'errore e non parte.
pub fn spawn(pipe_name: String, ctx: Context) -> anyhow::Result<()> {
    let name = wide(&pipe_name);
    let sddl = wide(&format!("D:P(A;;GA;;;{})", crate::sys::current_user_sid()?));
    // La prima istanza è creata qui per riportare subito un conflitto; poi passa al thread.
    let first = create_instance(&name, &sddl, true)? as usize;
    // Più thread in ascolto: mentre uno crea la sua prossima istanza gli altri hanno già una pipe
    // libera, così un secondo client non trova mai la pipe occupata (m5).
    for i in 0..ACCEPTORS {
        let (name, sddl, ctx) = (name.clone(), sddl.clone(), ctx.clone());
        let mut pending = (i == 0).then_some(first);
        spawn_supervised("ipc", move || accept_loop(&name, &sddl, &ctx, &mut pending))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::channel;

    use super::*;

    fn ctx() -> (Context, std::sync::mpsc::Receiver<AppCommand>, Arc<AtomicU32>) {
        let (tx, rx) = channel();
        let reloads = Arc::new(AtomicU32::new(0));
        let r = reloads.clone();
        let status = Arc::new(Mutex::new(EngineStatus { video_seconds: 42.0, ..Default::default() }));
        let q = reloads.clone();
        let on_quit = Arc::new(move || _ = q.fetch_add(100, Ordering::SeqCst));
        (Context { status, commands: tx, on_reload: Arc::new(move || _ = r.fetch_add(1, Ordering::SeqCst)), on_quit }, rx, reloads)
    }

    #[test]
    fn requests_map_to_commands() {
        let (c, rx, reloads) = ctx();
        assert!(matches!(handle_request(&c, IpcRequest::Status), IpcResponse::Status(s) if s.video_seconds == 42.0));
        assert_eq!(handle_request(&c, IpcRequest::ReloadConfig), IpcResponse::Ok);
        assert_eq!(handle_request(&c, IpcRequest::TestClip), IpcResponse::Ok);
        assert_eq!(handle_request(&c, IpcRequest::Quit), IpcResponse::Ok);
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            [AppCommand::ReloadConfig, AppCommand::SaveTestClip, AppCommand::Quit]
        );
        assert_eq!(reloads.load(Ordering::SeqCst), 101);
    }

    #[test]
    fn real_pipe_round_trip_with_in_process_client() {
        let (c, rx, _) = ctx();
        let name = format!(r"\\.\pipe\cliprec-test-{}", std::process::id());
        spawn(name.clone(), c).unwrap();
        // Un secondo server sullo stesso nome non deve poter rubare la pipe.
        let (c2, ..) = ctx();
        assert!(spawn(name.clone(), c2).is_err());

        let mut pipe = OpenOptions::new().read(true).write(true).open(&name).unwrap();
        let mut reader_pipe = pipe.try_clone().unwrap();
        let mut reader = BufReader::new(&mut reader_pipe);
        for req in [IpcRequest::Status, IpcRequest::TestClip] {
            pipe.write_all(encode_line(&req).unwrap().as_bytes()).unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let resp: IpcResponse = decode_line(&line).unwrap();
            match req {
                IpcRequest::Status => assert!(matches!(resp, IpcResponse::Status(s) if s.video_seconds == 42.0)),
                _ => assert_eq!(resp, IpcResponse::Ok),
            }
        }
        pipe.write_all(b"non json\n").unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(decode_line::<IpcResponse>(&line).unwrap(), IpcResponse::Error(_)));
        assert_eq!(rx.try_recv().unwrap(), AppCommand::SaveTestClip);
    }

    #[test]
    fn m9_panic_in_a_request_does_not_kill_the_connection() {
        let (mut c, _rx, _) = ctx();
        c.on_reload = Arc::new(|| panic!("panic iniettato nel gestore"));
        let name = format!(r"\\.\pipe\cliprec-test-m9-{}", std::process::id());
        spawn(name.clone(), c).unwrap();
        let mut pipe = OpenOptions::new().read(true).write(true).open(&name).unwrap();
        let mut reader_pipe = pipe.try_clone().unwrap();
        let mut reader = BufReader::new(&mut reader_pipe);
        let mut ask = |req: IpcRequest| {
            pipe.write_all(encode_line(&req).unwrap().as_bytes()).unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            line
        };
        let line = ask(IpcRequest::ReloadConfig);
        assert!(matches!(decode_line::<IpcResponse>(&line), Ok(IpcResponse::Error(_))), "risposta all'errore interno: {line:?}");
        let line = ask(IpcRequest::Status);
        assert!(matches!(decode_line::<IpcResponse>(&line), Ok(IpcResponse::Status(_))), "la connessione deve restare viva: {line:?}");
    }

    #[test]
    fn m5_two_concurrent_clients_never_find_the_pipe_busy() {
        let (c, _rx, _) = ctx();
        let name = format!(r"\\.\pipe\cliprec-test-m5-{}", std::process::id());
        spawn(name.clone(), c).unwrap();
        let busy = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let (name, busy) = (name.clone(), busy.clone());
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        match OpenOptions::new().read(true).write(true).open(&name) {
                            Ok(mut p) => {
                                p.write_all(encode_line(&IpcRequest::Status).unwrap().as_bytes()).unwrap();
                                let mut line = String::new();
                                BufReader::new(&p).read_line(&mut line).unwrap();
                            }
                            Err(e) if e.raw_os_error() == Some(231) => _ = busy.fetch_add(1, Ordering::SeqCst),
                            Err(e) => panic!("{e}"),
                        }
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        assert_eq!(busy.load(Ordering::SeqCst), 0, "ERROR_PIPE_BUSY su 2000 connessioni");
    }
}
