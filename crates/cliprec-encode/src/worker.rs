//! Modulo FFI: thread dell'encoder. Il pacer converte il frame su una superficie dell'encoder e lo
//! mette in coda; questo thread lo manda a FFmpeg e raccoglie i pacchetti. Se l'encoder si ferma un
//! attimo (GPU satura nei caricamenti) la coda assorbe fino a [`QUEUE`] frame invece di far perdere
//! tick al pacer; a coda piena il pacer aspetta, come quando la codifica era sul suo thread.
//!
//! Niente allocazioni a regime: i frame della coda e i buffer dei pacchetti girano tra i due thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ffmpeg_the_third::ffi;
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};

/// Frame che possono aspettare in coda oltre a quelli che l'encoder tiene già (100 ms a 60 fps).
pub(crate) const QUEUE: usize = 6;
/// Pacchetti pronti non ancora ritirati prima che il thread aspetti il pacer.
const PACKETS: usize = 64;
/// Attesa massima di un posto in coda: oltre, l'encoder è bloccato.
pub(crate) const STALL: Duration = Duration::from_secs(2);

/// Puntatore FFmpeg passato tra i due thread: chi lo riceve è il solo a usarlo fino alla restituzione.
struct Ptr<T>(*mut T);
impl<T> Clone for Ptr<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Ptr<T> {}
// SAFETY: i frame e il contesto passano di mano tramite i canali; mai usati da due thread insieme.
unsafe impl<T> Send for Ptr<T> {}

/// Pacchetto codificato; `data` torna al thread con [`Worker::recycle`].
pub(crate) struct Packet {
    pub data: Vec<u8>,
    pub pts: i64,
    pub dts: i64,
    pub key: bool,
}

pub(crate) enum Done {
    Packet(Packet),
    Flushed,
    /// Passo di FFmpeg e messaggio: il thread è terminato.
    Failed(&'static str, String),
}

enum Job {
    Frame(Ptr<ffi::AVFrame>),
    Flush,
}

pub(crate) struct Worker {
    jobs: Option<SyncSender<Job>>,
    done: Receiver<Done>,
    free: Receiver<Ptr<ffi::AVFrame>>,
    buffers: SyncSender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Tutti i frame della coda, liberati in Drop dopo la fine del thread.
    frames: Vec<*mut ffi::AVFrame>,
}

impl Worker {
    /// Avvia il thread su `ctx` (aperto) e `pkt`, che da qui in poi usa solo lui finché `Worker` vive.
    ///
    /// # Safety
    /// `ctx` e `pkt` devono restare validi finché `Worker` non è distrutto, e nessun altro li usa.
    pub unsafe fn start(ctx: *mut ffi::AVCodecContext, pkt: *mut ffi::AVPacket) -> std::io::Result<Self> {
        let (jobs_tx, jobs) = sync_channel::<Job>(QUEUE + 1);
        let (done_tx, done) = sync_channel::<Done>(PACKETS);
        let (free_tx, free) = sync_channel::<Ptr<ffi::AVFrame>>(QUEUE);
        let (buffers, buffers_rx) = sync_channel::<Vec<u8>>(PACKETS);
        let mut frames = Vec::with_capacity(QUEUE);
        for _ in 0..QUEUE {
            // SAFETY: allocazione semplice; nullo solo senza memoria.
            let f = unsafe { ffi::av_frame_alloc() };
            if f.is_null() {
                for mut f in frames {
                    // SAFETY: frame allocati qui sopra.
                    unsafe { ffi::av_frame_free(&mut f) };
                }
                return Err(std::io::Error::other("av_frame_alloc"));
            }
            frames.push(f);
            let _ = free_tx.send(Ptr(f));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (ctx, pkt, flag) = (Ptr(ctx), Ptr(pkt), stop.clone());
        let thread = std::thread::Builder::new().name("cliprec-encode".into()).spawn(move || {
            // SAFETY: pseudo-handle del thread corrente, sempre valida.
            let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
            run(ctx, pkt, &jobs, &done_tx, &free_tx, &buffers_rx, &flag);
        });
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                for mut f in frames {
                    // SAFETY: nessun thread li usa.
                    unsafe { ffi::av_frame_free(&mut f) };
                }
                return Err(e);
            }
        };
        Ok(Self { jobs: Some(jobs_tx), done, free, buffers, stop, thread: Some(thread), frames })
    }

    /// Accoda un riferimento a `src` con il pts dato. Aspetta un posto libero (al più [`STALL`])
    /// consegnando intanto a `ready` ciò che il thread produce.
    ///
    /// # Safety
    /// `src` è un frame valido con un buffer (hw o in memoria) di cui l'encoder può prendere un riferimento.
    pub unsafe fn queue(&self, src: *const ffi::AVFrame, pts: i64, ready: &mut dyn FnMut(Done)) -> Result<(), Done> {
        let deadline = Instant::now() + STALL;
        let f = loop {
            self.poll(ready);
            match self.free.recv_timeout(Duration::from_millis(1)) {
                Ok(f) => break f,
                Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                Err(_) => return Err(Done::Failed("coda dell'encoder", format!("ferma da {} s", STALL.as_secs()))),
            }
        };
        // SAFETY: `f` è un frame vuoto della coda; `src` è valido (contratto). L'encoder prende poi
        // un riferimento proprio in avcodec_send_frame.
        let ret = unsafe {
            let ret = ffi::av_frame_ref(f.0, src);
            let d = &mut *f.0;
            d.pts = pts;
            d.duration = 1;
            d.color_primaries = ffi::AVColorPrimaries::BT709;
            d.color_trc = ffi::AVColorTransferCharacteristic::BT709;
            d.colorspace = ffi::AVColorSpace::BT709;
            d.color_range = ffi::AVColorRange::MPEG;
            ret
        };
        if ret < 0 {
            return Err(Done::Failed("av_frame_ref", crate::ffi::av_error_string(ret)));
        }
        self.send(Job::Frame(f))
    }

    /// Consegna a `ready` ciò che il thread ha prodotto finora, senza aspettare.
    pub fn poll(&self, ready: &mut dyn FnMut(Done)) {
        loop {
            match self.done.try_recv() {
                Ok(d) => ready(d),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Svuota l'encoder e consegna tutti i pacchetti rimasti.
    pub fn flush(&self, ready: &mut dyn FnMut(Done)) -> Result<(), Done> {
        self.send(Job::Flush)?;
        loop {
            match self.done.recv_timeout(STALL) {
                Ok(Done::Flushed) => return Ok(()),
                Ok(Done::Failed(step, e)) => return Err(Done::Failed(step, e)),
                Ok(d) => ready(d),
                Err(_) => return Err(Done::Failed("flush", "l'encoder non risponde".into())),
            }
        }
    }

    /// Restituisce al thread il buffer di un pacchetto già consegnato.
    pub fn recycle(&self, data: Vec<u8>) {
        let _ = self.buffers.try_send(data);
    }

    fn send(&self, job: Job) -> Result<(), Done> {
        match self.jobs.as_ref().map(|j| j.send(job)) {
            Some(Ok(())) => Ok(()),
            // Il thread è uscito dopo un errore: il motivo è in `done`.
            _ => Err(self.done.try_recv().unwrap_or(Done::Failed("thread dell'encoder", "terminato".into()))),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.jobs = None;
        // Il thread può essere fermo su un canale pieno: svuotarlo lo sblocca.
        while self.thread.as_ref().is_some_and(|t| !t.is_finished()) {
            while self.done.try_recv().is_ok() {}
            std::thread::sleep(Duration::from_millis(1));
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        for f in &mut self.frames {
            // SAFETY: il thread è finito; ogni frame è nostro e senza altri utenti.
            unsafe { ffi::av_frame_free(f) };
        }
    }
}

fn run(
    ctx: Ptr<ffi::AVCodecContext>,
    pkt: Ptr<ffi::AVPacket>,
    jobs: &Receiver<Job>,
    done: &SyncSender<Done>,
    free: &SyncSender<Ptr<ffi::AVFrame>>,
    buffers: &Receiver<Vec<u8>>,
    stop: &AtomicBool,
) {
    while let Ok(job) = jobs.recv() {
        let flush = matches!(job, Job::Flush);
        let ret = match job {
            Job::Frame(f) => {
                // SAFETY: contesto aperto, usato solo da questo thread; `f` è un frame pieno della coda.
                let ret = if stop.load(Ordering::Acquire) { 0 } else { unsafe { ffi::avcodec_send_frame(ctx.0, f.0) } };
                // SAFETY: l'encoder ha il suo riferimento; il nostro si toglie prima di restituire il frame.
                unsafe { ffi::av_frame_unref(f.0) };
                let _ = free.send(f);
                ret
            }
            // SAFETY: un frame nullo chiede lo svuotamento.
            Job::Flush => match unsafe { ffi::avcodec_send_frame(ctx.0, std::ptr::null()) } {
                r if r == ffi::AVERROR_EOF => 0,
                r => r,
            },
        };
        if ret < 0 {
            let _ = done.send(Done::Failed("avcodec_send_frame", crate::ffi::av_error_string(ret)));
            return;
        }
        if let Err((step, e)) = drain(ctx, pkt, done, buffers) {
            let _ = done.send(Done::Failed(step, e));
            return;
        }
        if flush {
            let _ = done.send(Done::Flushed);
        }
    }
}

/// Manda a `done` tutti i pacchetti pronti.
fn drain(
    ctx: Ptr<ffi::AVCodecContext>,
    pkt: Ptr<ffi::AVPacket>,
    done: &SyncSender<Done>,
    buffers: &Receiver<Vec<u8>>,
) -> Result<(), (&'static str, String)> {
    loop {
        // SAFETY: contesto e pacchetto usati solo da questo thread.
        let ret = unsafe { ffi::avcodec_receive_packet(ctx.0, pkt.0) };
        if ret == ffi::AVERROR(crate::ffi::EAGAIN) || ret == ffi::AVERROR_EOF {
            return Ok(());
        }
        if ret < 0 {
            return Err(("avcodec_receive_packet", crate::ffi::av_error_string(ret)));
        }
        let mut data = buffers.try_recv().unwrap_or_default();
        data.clear();
        // SAFETY: dopo un receive riuscito `pkt` contiene dati validi per `size` byte fino a av_packet_unref.
        let p = unsafe {
            let p = &*pkt.0;
            if !p.data.is_null() && p.size > 0 {
                data.extend_from_slice(std::slice::from_raw_parts(p.data, p.size as usize));
            }
            let dts = if p.dts == ffi::AV_NOPTS_VALUE { p.pts } else { p.dts };
            let packet = Packet { data, pts: p.pts, dts, key: p.flags & ffi::AV_PKT_FLAG_KEY != 0 };
            ffi::av_packet_unref(pkt.0);
            packet
        };
        if done.send(Done::Packet(p)).is_err() {
            return Ok(()); // chiusura
        }
    }
}
