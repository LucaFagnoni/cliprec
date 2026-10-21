//! Modulo FFI: thread dell'encoder. Il thread della conversione ([`crate::Encoder`]) converte il
//! frame su una superficie dell'encoder e lo mette in coda; questo thread lo manda a FFmpeg,
//! raccoglie i pacchetti e li consegna direttamente al sink (il buffer di replay). Se l'encoder si
//! ferma un attimo (GPU satura nei caricamenti) la coda assorbe fino a [`QUEUE`] frame.
//! Con la coda piena il pacer non aspetta: il frame si salta e il buco si riempie ripetendo il
//! precedente; solo una coda ferma per [`STUCK`] è un guasto dell'encoder.
//!
//! Niente allocazioni a regime: i frame della coda girano tra i due thread, i pacchetti passano al
//! sink in prestito.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use cliprec_core::{HNS_PER_SEC, PacketRef, QpcTime, VideoCodec};
use ffmpeg_the_third::ffi;
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};

/// Frame che possono aspettare in coda oltre a quelli che l'encoder tiene già (200 ms a 60 fps): assorbe
/// i picchi della GPU durante il gioco. Ognuno tiene una superficie dell'encoder (circa 3 MB a 1080p
/// 4:2:0, 6 MB in 4:4:4).
pub(crate) const QUEUE: usize = 12;
/// Attesa massima della fine dello svuotamento.
pub(crate) const STALL: Duration = Duration::from_secs(2);
/// Coda ferma da tanto: l'encoder è bloccato davvero, non solo indietro perché la GPU è satura (un
/// gioco che carica la tiene occupata anche per qualche secondo). Si passa al candidato successivo.
pub(crate) const STUCK: Duration = Duration::from_secs(10);

/// Dove vanno i pacchetti codificati, in ordine di decodifica, dal thread dell'encoder.
pub type PacketSink = Box<dyn FnMut(PacketRef<'_>) + Send>;

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

/// Esito del thread verso il pacer.
pub(crate) enum Done {
    Flushed,
    /// Passo di FFmpeg e messaggio: il thread è terminato.
    Failed(&'static str, String),
}

enum Job {
    /// Frame pieno e istante QPC del primo frame (pts 0), per i tempi dei pacchetti.
    Frame(Ptr<ffi::AVFrame>, QpcTime),
    Sink(PacketSink),
    Flush,
}

/// Tempi e tipo dei pacchetti, fissi per l'encoder.
#[derive(Clone, Copy)]
pub(crate) struct Stream {
    pub fps: u32,
    pub codec: VideoCodec,
}

/// Posto libero nella coda dell'encoder (un frame vuoto), da [`Worker::wait_slot`].
pub(crate) struct Slot(Ptr<ffi::AVFrame>);

pub(crate) struct Worker {
    jobs: Option<SyncSender<Job>>,
    done: Receiver<Done>,
    free: Receiver<Ptr<ffi::AVFrame>>,
    /// Per restituire un posto non usato.
    free_back: SyncSender<Ptr<ffi::AVFrame>>,
    stop: Arc<AtomicBool>,
    /// Posti liberi in coda, letti senza aspettare da chi decide se saltare un frame.
    free_count: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
    /// Tutti i frame della coda, liberati in Drop dopo la fine del thread.
    frames: Vec<*mut ffi::AVFrame>,
}

impl Worker {
    /// Avvia il thread su `ctx` (aperto) e `pkt`, che da qui in poi usa solo lui finché `Worker` vive.
    /// Finché non arriva un sink ([`Self::set_sink`]) i pacchetti si scartano.
    ///
    /// # Safety
    /// `ctx` e `pkt` devono restare validi finché `Worker` non è distrutto, e nessun altro li usa.
    pub unsafe fn start(ctx: *mut ffi::AVCodecContext, pkt: *mut ffi::AVPacket, stream: Stream) -> std::io::Result<Self> {
        let (jobs_tx, jobs) = sync_channel::<Job>(QUEUE + 2);
        let (done_tx, done) = sync_channel::<Done>(4);
        let (free_tx, free) = sync_channel::<Ptr<ffi::AVFrame>>(QUEUE);
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
        let free_back = free_tx.clone();
        let free_count = Arc::new(AtomicUsize::new(QUEUE));
        let counter = free_count.clone();
        let thread = std::thread::Builder::new().name("cliprec-encode".into()).spawn(move || {
            // SAFETY: pseudo-handle del thread corrente, sempre valida.
            let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
            run(ctx, pkt, stream, &jobs, &done_tx, &free_tx, &counter, &flag);
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
        Ok(Self { jobs: Some(jobs_tx), done, free, free_back, stop, free_count, thread: Some(thread), frames })
    }

    /// Da qui i pacchetti vanno a `sink`, chiamato dal thread dell'encoder.
    pub fn set_sink(&self, sink: PacketSink) -> Result<(), Done> {
        self.send(Job::Sink(sink))
    }

    /// Un posto libero, aspettandolo al più `timeout` (dal thread della conversione, non dal pacer,
    /// che con la coda piena salta il frame guardando [`Self::free_count`]).
    pub fn wait_slot(&self, timeout: Duration) -> Option<Slot> {
        let slot = self.free.recv_timeout(timeout).ok().map(Slot)?;
        self.free_count.fetch_sub(1, Ordering::Relaxed);
        Some(slot)
    }

    /// Contatore dei posti liberi in coda, da leggere senza aspettare.
    pub fn free_count(&self) -> Arc<AtomicUsize> {
        self.free_count.clone()
    }

    /// Restituisce un posto preso con [`Self::wait_slot`] e non usato (conversione fallita).
    pub fn give_back(&self, slot: Slot) {
        if self.free_back.try_send(slot.0).is_ok() {
            self.free_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Accoda nel posto `slot` un riferimento a `src` con il pts dato (`base` = istante del pts 0).
    ///
    /// # Safety
    /// `src` è un frame valido con un buffer (hw o in memoria) di cui l'encoder può prendere un riferimento.
    pub unsafe fn submit(&self, slot: Slot, src: *const ffi::AVFrame, pts: i64, base: QpcTime) -> Result<(), Done> {
        let f = slot.0;
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
            self.give_back(slot);
            return Err(Done::Failed("av_frame_ref", crate::ffi::av_error_string(ret)));
        }
        self.send(Job::Frame(f, base))
    }

    /// Errore riportato dal thread dopo l'ultima chiamata, se c'è.
    pub fn failed(&self) -> Result<(), Done> {
        match self.done.try_recv() {
            Ok(Done::Failed(step, e)) => Err(Done::Failed(step, e)),
            Ok(Done::Flushed) | Err(TryRecvError::Empty) => Ok(()),
            Err(TryRecvError::Disconnected) => Err(Done::Failed("thread dell'encoder", "terminato".into())),
        }
    }

    /// Svuota l'encoder: al ritorno tutti i pacchetti sono passati dal sink.
    pub fn flush(&self) -> Result<(), Done> {
        self.send(Job::Flush)?;
        match self.done.recv_timeout(STALL) {
            Ok(Done::Flushed) => Ok(()),
            Ok(failed) => Err(failed),
            Err(_) => Err(Done::Failed("flush", "l'encoder non risponde".into())),
        }
    }

    fn send(&self, job: Job) -> Result<(), Done> {
        match self.jobs.as_ref().map(|j| j.send(job)) {
            Some(Ok(())) => Ok(()),
            _ => Err(self.exit_reason()),
        }
    }

    /// Il thread è uscito dopo un errore: il motivo è in `done`.
    fn exit_reason(&self) -> Done {
        self.done.try_recv().unwrap_or(Done::Failed("thread dell'encoder", "terminato".into()))
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

#[allow(clippy::too_many_arguments)]
fn run(
    ctx: Ptr<ffi::AVCodecContext>,
    pkt: Ptr<ffi::AVPacket>,
    stream: Stream,
    jobs: &Receiver<Job>,
    done: &SyncSender<Done>,
    free: &SyncSender<Ptr<ffi::AVFrame>>,
    free_count: &AtomicUsize,
    stop: &AtomicBool,
) {
    let mut sink: PacketSink = Box::new(|_| {});
    let mut base = QpcTime(0);
    while let Ok(job) = jobs.recv() {
        let (ret, flush) = match job {
            Job::Sink(s) => {
                sink = s;
                continue;
            }
            Job::Frame(f, b) => {
                base = b;
                // SAFETY: contesto aperto, usato solo da questo thread; `f` è un frame pieno della coda.
                let ret = if stop.load(Ordering::Acquire) { 0 } else { unsafe { ffi::avcodec_send_frame(ctx.0, f.0) } };
                // SAFETY: l'encoder ha il suo riferimento; il nostro si toglie prima di restituire il frame.
                unsafe { ffi::av_frame_unref(f.0) };
                if free.send(f).is_ok() {
                    free_count.fetch_add(1, Ordering::Relaxed);
                }
                (ret, false)
            }
            // SAFETY: un frame nullo chiede lo svuotamento.
            Job::Flush => match unsafe { ffi::avcodec_send_frame(ctx.0, std::ptr::null()) } {
                r if r == ffi::AVERROR_EOF => (0, true),
                r => (r, true),
            },
        };
        if ret < 0 {
            let _ = done.send(Done::Failed("avcodec_send_frame", crate::ffi::av_error_string(ret)));
            return;
        }
        if let Err((step, e)) = drain(ctx, pkt, stream, base, &mut sink) {
            let _ = done.send(Done::Failed(step, e));
            return;
        }
        if flush {
            let _ = done.send(Done::Flushed);
        }
    }
}

/// Consegna a `sink` tutti i pacchetti pronti.
fn drain(
    ctx: Ptr<ffi::AVCodecContext>,
    pkt: Ptr<ffi::AVPacket>,
    stream: Stream,
    base: QpcTime,
    sink: &mut PacketSink,
) -> Result<(), (&'static str, String)> {
    let fps = i64::from(stream.fps.max(1));
    let to_qpc = |ts: i64| base.add_hns((i128::from(ts) * i128::from(HNS_PER_SEC) / i128::from(fps)) as i64);
    loop {
        // SAFETY: contesto e pacchetto usati solo da questo thread.
        let ret = unsafe { ffi::avcodec_receive_packet(ctx.0, pkt.0) };
        if ret == ffi::AVERROR(crate::ffi::EAGAIN) || ret == ffi::AVERROR_EOF {
            return Ok(());
        }
        if ret < 0 {
            return Err(("avcodec_receive_packet", crate::ffi::av_error_string(ret)));
        }
        // SAFETY: dopo un receive riuscito `pkt` contiene dati validi per `size` byte fino a
        // av_packet_unref; la fetta non sopravvive alla chiamata di `sink`.
        unsafe {
            let p = &*pkt.0;
            let data = if p.data.is_null() || p.size <= 0 { &[][..] } else { std::slice::from_raw_parts(p.data, p.size as usize) };
            let dts = if p.dts == ffi::AV_NOPTS_VALUE { p.pts } else { p.dts };
            sink(PacketRef {
                data,
                pts: to_qpc(p.pts),
                dts: to_qpc(dts),
                duration: HNS_PER_SEC / fps,
                // Solo gli IDR aprono un GOP chiuso: un I-frame non IDR o un CRA non bastano.
                keyframe: p.flags & ffi::AV_PKT_FLAG_KEY != 0 && crate::nal::first_vcl_is_idr(stream.codec, data).unwrap_or(true),
            });
            ffi::av_packet_unref(pkt.0);
        }
    }
}
