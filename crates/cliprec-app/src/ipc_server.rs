//! Server della named pipe usata da `cliprec-settings`. Una riga JSON per richiesta e per risposta.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::os::windows::io::AsRawHandle;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cliprec_core::ipc::{IpcRequest, IpcResponse, decode_line, encode_line};
use cliprec_core::{AppCommand, EngineStatus};
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::IO::CancelIoEx;
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

#[derive(Debug, PartialEq, Eq)]
enum LineResult {
    Line,
    Eof,
    TooLong,
    TimedOut,
}

/// Legge una riga in `out` (compreso il `\n`) senza superare `max` byte. `wait` è chiamata quando il
/// buffer è vuoto, prima di leggere: restituisce `false` se è scaduto il tempo o il client è sparito.
fn read_bounded_line<R: std::io::Read>(r: &mut BufReader<R>, out: &mut Vec<u8>, max: usize, mut wait: impl FnMut() -> bool) -> LineResult {
    loop {
        if r.buffer().is_empty() && !wait() {
            return LineResult::TimedOut;
        }
        let buf = match r.fill_buf() {
            Ok([]) | Err(_) => return LineResult::Eof,
            Ok(b) => b,
        };
        let newline = buf.iter().position(|&b| b == b'\n');
        let take = newline.map_or(buf.len(), |i| i + 1);
        // Si copia al massimo ciò che manca al limite (+1 per accorgersi dello sforamento).
        let room = (max + 1).saturating_sub(out.len()).min(take);
        out.extend_from_slice(&buf[..room]);
        r.consume(take);
        if out.len() > max {
            return LineResult::TooLong;
        }
        if newline.is_some() {
            return LineResult::Line;
        }
    }
}

/// Limiti di una connessione: riga massima e tempo massimo per riceverla.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_line: usize,
    pub idle: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_line: 64 * 1024, idle: Duration::from_secs(30) }
    }
}

/// Guardiano di una connessione: se la richiesta non arriva entro `deadline` annulla la lettura
/// bloccata (`CancelIoEx`) e alza `expired`. Al `drop` si ferma e viene atteso, quindi non usa mai
/// l'handle dopo la sua chiusura.
struct Watchdog {
    done: Option<Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    fn start(handle: usize, deadline: Arc<Mutex<Instant>>, expired: Arc<AtomicBool>) -> Self {
        let (done, done_rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("ipc-watchdog".into())
            .spawn(move || {
                while matches!(done_rx.recv_timeout(Duration::from_millis(50)), Err(RecvTimeoutError::Timeout)) {
                    if deadline.lock().map(|d| Instant::now() >= *d).unwrap_or(true) {
                        expired.store(true, Ordering::SeqCst);
                        // SAFETY: l'handle è quello della connessione, che aspetta questo thread prima di chiuderlo.
                        let _ = unsafe { CancelIoEx(HANDLE(handle as *mut _), None) };
                    }
                }
            })
            .ok();
        Self { done: Some(done), thread }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        drop(self.done.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Limita la frequenza di un messaggio (log «tutte le istanze impegnate»).
#[derive(Debug)]
pub struct RateLimit {
    every: Duration,
    last: Option<Instant>,
}

impl RateLimit {
    pub fn new(every: Duration) -> Self {
        Self { every, last: None }
    }

    pub fn allow(&mut self, now: Instant) -> bool {
        if self.last.is_some_and(|t| now.duration_since(t) < self.every) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

fn serve_connection(ctx: &Context, file: File, limits: Limits) {
    let mut reader = BufReader::new(&file);
    let mut writer = &file;
    let mut line = Vec::new();
    let deadline = Arc::new(Mutex::new(Instant::now() + limits.idle));
    let expired = Arc::new(AtomicBool::new(false));
    let _watchdog = Watchdog::start(file.as_raw_handle() as usize, Arc::clone(&deadline), Arc::clone(&expired));
    loop {
        line.clear();
        if let Ok(mut d) = deadline.lock() {
            *d = Instant::now() + limits.idle;
        }
        match read_bounded_line(&mut reader, &mut line, limits.max_line, || !expired.load(Ordering::SeqCst)) {
            LineResult::Line => {}
            LineResult::Eof => return,
            LineResult::TooLong => {
                tracing::warn!("pipe: riga oltre {} byte, connessione chiusa", limits.max_line);
                return;
            }
            LineResult::TimedOut => {
                tracing::info!("pipe: connessione inattiva, chiusa");
                return;
            }
        }
        let resp = match std::str::from_utf8(&line).map_err(|e| e.to_string()).and_then(|l| decode_line::<IpcRequest>(l).map_err(|e| e.to_string())) {
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

fn accept_loop(name: &[u16], sddl: &[u16], ctx: &Context, limits: Limits, pending: &mut Option<usize>) {
    let mut log_limit = RateLimit::new(Duration::from_secs(60));
    loop {
        let raw = match pending.take() {
            Some(h) => h as RawHandle,
            None => match create_instance(name, sddl, false) {
                Ok(h) => h,
                Err(e) => {
                    // Con tutte le istanze impegnate (ERROR_PIPE_BUSY) si riprova ogni secondo, ma si logga di rado.
                    if log_limit.allow(Instant::now()) {
                        tracing::error!("pipe: nessuna istanza disponibile ({e})");
                    }
                    std::thread::sleep(Duration::from_secs(1));
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
        if let Err(e) = std::thread::Builder::new().name("ipc-conn".into()).spawn(move || serve_connection(&ctx, file, limits)) {
            tracing::error!("thread connessione: {e}");
        }
    }
}

/// Avvia il server su `pipe_name` in un thread supervisionato. Se un'altra istanza possiede già
/// la pipe (o la creazione fallisce) restituisce l'errore e non parte.
pub fn spawn(pipe_name: String, ctx: Context) -> anyhow::Result<()> {
    spawn_with_limits(pipe_name, ctx, Limits::default())
}

pub fn spawn_with_limits(pipe_name: String, ctx: Context, limits: Limits) -> anyhow::Result<()> {
    let name = wide(&pipe_name);
    let sddl = wide(&format!("D:P(A;;GA;;;{})", crate::sys::current_user_sid()?));
    // La prima istanza è creata qui per riportare subito un conflitto; poi passa al thread.
    let first = create_instance(&name, &sddl, true)? as usize;
    // Più thread in ascolto: mentre uno crea la sua prossima istanza gli altri hanno già una pipe
    // libera, così un secondo client non trova mai la pipe occupata (m5).
    for i in 0..ACCEPTORS {
        let (name, sddl, ctx) = (name.clone(), sddl.clone(), ctx.clone());
        let mut pending = (i == 0).then_some(first);
        spawn_supervised("ipc", move || accept_loop(&name, &sddl, &ctx, limits, &mut pending))?;
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

    #[test]
    fn m6_oversized_line_is_cut_and_memory_stays_bounded() {
        use std::io::Read;
        // 150 MB senza a capo, come un client ostile.
        let mut r = BufReader::new(std::io::repeat(b'a').take(150 * 1024 * 1024));
        let mut out = Vec::new();
        let res = read_bounded_line(&mut r, &mut out, 64 * 1024, || true);
        assert_eq!(res, LineResult::TooLong);
        assert!(out.len() <= 64 * 1024 + 8 * 1024, "buffer cresciuto fino a {} byte", out.len());
    }

    #[test]
    fn m6_idle_or_partial_connection_is_closed_after_the_timeout() {
        let (c, ..) = ctx();
        let name = format!(r"\\.\pipe\cliprec-test-m6-{}", std::process::id());
        spawn_with_limits(name.clone(), c, Limits { max_line: 64 * 1024, idle: Duration::from_millis(300) }).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut pipe = OpenOptions::new().read(true).write(true).open(&name).unwrap();
            pipe.write_all(b"\"Stat").unwrap(); // riga a metà, senza a capo
            let mut buf = [0u8; 16];
            let n = std::io::Read::read(&mut pipe, &mut buf);
            let _ = tx.send(n);
        });
        let r = rx.recv_timeout(Duration::from_secs(5)).expect("il server deve chiudere la connessione inattiva");
        assert!(matches!(r, Ok(0) | Err(_)));
    }

    #[test]
    fn m6_log_rate_limit() {
        let mut l = RateLimit::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(l.allow(t0));
        assert!(!l.allow(t0 + Duration::from_secs(1)));
        assert!(l.allow(t0 + Duration::from_secs(61)));
    }

    #[test]
    fn m6_normal_lines_and_closed_peers() {
        let mut r = BufReader::new(&b"\"Status\"\n\"Quit\"\n"[..]);
        let mut out = Vec::new();
        assert_eq!(read_bounded_line(&mut r, &mut out, 64, || true), LineResult::Line);
        assert_eq!(out, b"\"Status\"\n");
        out.clear();
        assert_eq!(read_bounded_line(&mut r, &mut out, 64, || true), LineResult::Line);
        out.clear();
        assert_eq!(read_bounded_line(&mut r, &mut out, 64, || true), LineResult::Eof);
        let mut r = BufReader::new(&b"x"[..]);
        assert_eq!(read_bounded_line(&mut r, &mut Vec::new(), 64, || false), LineResult::TimedOut);
    }
}
