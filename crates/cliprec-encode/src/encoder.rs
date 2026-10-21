//! Encoder visto da chi lo usa (il pacer). La conversione BGRA→NV12 e l'accodamento all'encoder
//! FFmpeg li fa un thread dedicato, proprietario di [`HwEncoder`]: sul pacer restano un paio di
//! contatori e l'invio del frame. Con la GPU satura `VideoProcessorBlt` può bloccare nel driver per
//! decine di millisecondi: prima bloccava il pacer, che perdeva tick; ora blocca solo questo thread
//! e il pacer salta il frame (che si riempie ripetendo il precedente, come un tick perso).
//!
//! Il frame viaggia con il suo [`cliprec_core::FrameHold`]: la sorgente non riscrive la superficie
//! finché la conversione non l'ha letta. Ne converte al più uno alla volta, perché il triplo buffer
//! della cattura regge un solo prestito oltre a quello del pacer. Un frame senza `hold` (test,
//! sorgenti che non prestano la superficie) si converte prima che `encode` torni, come prima.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Instant;

use cliprec_core::{EncodeError, FrameHold, GpuFrame, QpcTime, VideoStreamParams};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};

use crate::ffi::HwEncoder;
use crate::worker::{PacketSink, QUEUE, STUCK};

/// Esito di [`Encoder::encode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoded {
    Queued,
    /// Encoder indietro (GPU satura): il frame non è stato convertito né codificato.
    Skipped,
}

/// Un frame da convertire (o da ripetere) e accodare all'encoder.
pub(crate) struct FrameJob {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    pub pts: i64,
    /// Istante QPC del pts 0.
    pub base: QpcTime,
    /// Stesso contenuto del frame precedente: si rimanda la sua superficie senza convertire.
    pub reuse: bool,
    /// Tiene la superficie della sorgente fino alla fine della conversione.
    pub hold: Option<FrameHold>,
}

enum Job {
    /// Con `ack` il thread risponde a conversione finita (frame senza `hold`).
    Frame(FrameJob, bool),
    Sink(PacketSink),
    Flush,
}

/// Encoder hardware (o SVT-AV1) con conversione e codifica su thread propri.
pub struct Encoder {
    params: VideoStreamParams,
    device: ID3D11Device,
    jobs: Option<SyncSender<Job>>,
    /// Risposte del thread: esito di flush e conferme dei frame senza `hold`; un errore (dopo il
    /// quale il thread termina) arriva anche senza domanda.
    done: Receiver<Result<(), EncodeError>>,
    /// Posti liberi nella coda del thread dell'encoder.
    free: Arc<AtomicUsize>,
    /// Frame inviati alla conversione e non ancora accodati all'encoder.
    pending: Arc<AtomicUsize>,
    /// Una conversione inviata e non ancora finita.
    converting: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Indice e pts QPC del primo frame: pts dell'encoder = indice − primo indice (time_base 1/fps).
    first: Option<(u64, QpcTime)>,
    last_pts: i64,
    /// Da quando i frame si saltano, se succede.
    full_since: Option<Instant>,
    /// L'ultimo frame con contenuto nuovo è stato saltato: il prossimo «ripetuto» va convertito,
    /// non deve ripetere la superficie precedente, che è più vecchia.
    content_skipped: bool,
}

impl Encoder {
    /// Avvia il thread della conversione, che da qui possiede `hw`.
    pub(crate) fn start(hw: HwEncoder, device: ID3D11Device) -> Result<Self, EncodeError> {
        let params = hw.params().clone();
        let name = params.encoder_name.clone();
        let free = hw.free_count().ok_or_else(|| EncodeError::Failed(format!("{name}: encoder chiuso")))?;
        // Un frame per posto della coda dell'encoder, più sink e flush.
        let (jobs_tx, jobs) = sync_channel::<Job>(QUEUE + 2);
        let (done_tx, done) = sync_channel(2);
        let pending = Arc::new(AtomicUsize::new(0));
        let converting = Arc::new(AtomicBool::new(false));
        let (p, c) = (pending.clone(), converting.clone());
        let thread = std::thread::Builder::new()
            .name("cliprec-convert".into())
            .spawn(move || {
                // SAFETY: pseudo-handle del thread corrente, sempre valida.
                let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
                run(hw, &jobs, &done_tx, &p, &c);
            })
            .map_err(|e| EncodeError::Failed(format!("{name}: thread della conversione: {e}")))?;
        Ok(Self {
            params,
            device,
            jobs: Some(jobs_tx),
            done,
            free,
            pending,
            converting,
            thread: Some(thread),
            first: None,
            last_pts: -1,
            full_since: None,
            content_skipped: false,
        })
    }

    pub fn params(&self) -> &VideoStreamParams {
        &self.params
    }

    /// Da qui i pacchetti vanno a `sink`, in ordine di decodifica, chiamato dal thread dell'encoder
    /// (mai da quello che chiama `encode`). Senza sink si scartano.
    pub fn set_sink(&mut self, sink: PacketSink) -> Result<(), EncodeError> {
        self.send(Job::Sink(sink))
    }

    /// Manda il frame alla conversione e all'encoder; i pacchetti vanno al sink ([`Self::set_sink`]).
    /// Con `frame.hold` torna subito, altrimenti a conversione finita. Con l'encoder indietro (coda
    /// piena o conversione precedente non finita) non aspetta: [`Encoded::Skipped`]. Un errore
    /// della conversione o dell'encoder si riporta qui alla chiamata dopo.
    pub fn encode(&mut self, frame: &GpuFrame) -> Result<Encoded, EncodeError> {
        match self.done.try_recv() {
            Ok(Err(e)) => return Err(e),
            Ok(Ok(())) | Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Err(self.closed()),
        }
        let (first_index, base) = *self.first.get_or_insert((frame.index, frame.pts));
        // `last_pts` avanza solo quando il frame parte: un frame saltato non consuma un pts.
        let pts = (frame.index.saturating_sub(first_index) as i64).max(self.last_pts + 1);
        // Frame ripetuto (stesso contenuto del precedente, o riempitivo di un tick perso): si rimanda
        // la superficie già convertita. Niente conversione sulla GPU, che sotto carico compete col
        // gioco; come i frame duplicati di `ffmpeg -r`, che gli encoder accettano.
        let reuse = frame.repeated && !self.content_skipped && self.last_pts >= 0;
        let full = self.free.load(Ordering::Acquire) <= self.pending.load(Ordering::Acquire);
        if full || (!reuse && self.converting.load(Ordering::Acquire)) {
            return self.skip(frame.repeated);
        }
        let ack = frame.hold.is_none();
        let job = FrameJob {
            texture: frame.texture.clone(),
            width: frame.width,
            height: frame.height,
            pts,
            base,
            reuse,
            hold: frame.hold.clone(),
        };
        self.pending.fetch_add(1, Ordering::AcqRel);
        if !reuse {
            self.converting.store(true, Ordering::Release);
        }
        let sent = match self.jobs.as_ref().map(|j| j.try_send(Job::Frame(job, ack))) {
            Some(Ok(())) => true,
            // Non succede: i frame in viaggio sono meno dei posti liberi.
            Some(Err(TrySendError::Full(_))) => false,
            _ => return Err(self.closed()),
        };
        if !sent {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            if !reuse {
                self.converting.store(false, Ordering::Release);
            }
            return self.skip(frame.repeated);
        }
        self.last_pts = pts;
        self.full_since = None;
        if !reuse {
            self.content_skipped = false;
        }
        if ack {
            self.reply()?;
        }
        Ok(Encoded::Queued)
    }

    /// Svuota l'encoder: al ritorno gli ultimi pacchetti sono passati dal sink.
    pub fn flush(&mut self) -> Result<(), EncodeError> {
        self.send(Job::Flush)?;
        self.reply()
    }

    /// Frame saltato perché l'encoder è indietro. Errore solo se succede da [`STUCK`].
    fn skip(&mut self, repeated: bool) -> Result<Encoded, EncodeError> {
        let since = *self.full_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= STUCK {
            return Err(self.error(format!("coda dell'encoder ferma da {} s", STUCK.as_secs())));
        }
        if !repeated {
            self.content_skipped = true;
        }
        Ok(Encoded::Skipped)
    }

    fn send(&self, job: Job) -> Result<(), EncodeError> {
        match self.jobs.as_ref().map(|j| j.send(job)) {
            Some(Ok(())) => Ok(()),
            _ => Err(self.closed()),
        }
    }

    /// Risposta del thread a una domanda (flush o frame senza `hold`).
    fn reply(&self) -> Result<(), EncodeError> {
        match self.done.recv_timeout(STUCK) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => Err(self.error("il thread della conversione non risponde".into())),
            Err(RecvTimeoutError::Disconnected) => Err(self.closed()),
        }
    }

    /// Il thread è uscito: il motivo, se l'ha lasciato, è in `done`.
    fn closed(&self) -> EncodeError {
        match self.done.try_recv() {
            Ok(Err(e)) => e,
            _ => self.error("thread della conversione terminato".into()),
        }
    }

    /// Device perso se D3D11 lo dice, altrimenti errore dell'encoder.
    fn error(&self, detail: String) -> EncodeError {
        if crate::d3d::device_removed(&self.device) {
            EncodeError::DeviceLost
        } else {
            EncodeError::Failed(format!("{}: {detail}", self.params.encoder_name))
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Senza mittente il thread esce dal ciclo e distrugge `HwEncoder`, che ferma il suo thread
        // prima di liberare FFmpeg.
        self.jobs = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    mut hw: HwEncoder,
    jobs: &Receiver<Job>,
    done: &SyncSender<Result<(), EncodeError>>,
    pending: &AtomicUsize,
    converting: &AtomicBool,
) {
    while let Ok(job) = jobs.recv() {
        let result = match job {
            Job::Sink(sink) => hw.set_sink(sink),
            Job::Flush => {
                let r = hw.flush();
                let failed = r.is_err();
                let _ = done.send(r);
                if failed {
                    return;
                }
                continue;
            }
            Job::Frame(job, ack) => {
                let reuse = job.reuse;
                let r = hw.process(job);
                // Il posto in coda è già preso (o il thread esce): il pacer può contare sul prossimo.
                pending.fetch_sub(1, Ordering::AcqRel);
                if !reuse {
                    converting.store(false, Ordering::Release);
                }
                if r.is_ok() && ack {
                    let _ = done.send(Ok(()));
                }
                r
            }
        };
        if let Err(e) = result {
            let _ = done.send(Err(e));
            return;
        }
    }
}
