//! Encoder hardware (NVENC, AMF, QSV) via FFmpeg 9 con frame D3D11 senza copie; conversione
//! BGRA→NV12 su GPU.
//!
//! Ogni [`GpuFrame`](cliprec_core::GpuFrame) BGRA viene convertito in NV12 BT.709 a range limitato
//! da `ID3D11VideoProcessor`, scalato nella risoluzione d'uscita fissa con bande nere, dentro una
//! texture del pool hwframes D3D11 di FFmpeg, e passato all'encoder senza passare dalla CPU.
//! Unica eccezione il ripiego software [`SOFTWARE_ENCODER`] (SVT-AV1), ultimo candidato: la
//! conversione resta sulla GPU e solo il frame NV12 finale viene letto dalla CPU.
//!
//! Uso tipico:
//!
//! ```no_run
//! # use cliprec_core::{GpuDevice, config::VideoConfig};
//! # fn f(gpu: &GpuDevice) -> Result<(), cliprec_core::EncodeError> {
//! let settings = cliprec_encode::EncoderSettings::from_config(&VideoConfig::default(), 2560, 1440);
//! let mut enc = cliprec_encode::open_best(gpu, &settings)?;
//! // Se `enc.encode(..)` fallisce: riaprire con `open` sul candidato successivo a
//! // `enc.params().encoder_name` in `candidates(settings.codec)`.
//! # Ok(()) }
//! ```

mod d3d;
mod worker;
mod ffi;
mod nal;
mod yuv;

use cliprec_core::config::{CodecChoice, ColorFormat, EncoderAdvanced, OutputResolution, VideoConfig, default_max_bitrate_kbps};
use cliprec_core::{EncodeError, GpuDevice, VideoCodec};

pub use ffi::HwEncoder as Encoder;
pub use worker::PacketSink;

/// Famiglia di encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    Nvenc,
    Amf,
    Qsv,
    /// SVT-AV1 su CPU ([`SOFTWARE_ENCODER`]), l'unico che legge i frame dalla GPU.
    Software,
}

/// Encoder di riserva software, ultimo di ogni elenco di [`candidates`].
pub const SOFTWARE_ENCODER: &str = "libsvtav1";

impl Vendor {
    const HARDWARE: [Vendor; 3] = [Vendor::Nvenc, Vendor::Amf, Vendor::Qsv];

    fn suffix(self) -> &'static str {
        match self {
            Vendor::Nvenc => "nvenc",
            Vendor::Amf => "amf",
            Vendor::Qsv => "qsv",
            Vendor::Software => "",
        }
    }
}

/// Impostazioni di un encoder. `width`/`height` sono la risoluzione d'uscita fissa (pari).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderSettings {
    pub codec: CodecChoice,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    /// CQ/QP, più basso = migliore.
    pub quality: u8,
    /// Tetto in kbit/s; `None` = [`default_max_bitrate_kbps`] per codec, altezza e fps.
    pub max_bitrate_kbps: Option<u32>,
    /// Formato colore, controllo del bitrate, preset, GOP, B-frame, look-ahead, AQ.
    pub advanced: EncoderAdvanced,
}

impl EncoderSettings {
    /// Impostazioni dalla configurazione e dalla dimensione della prima texture della sorgente.
    pub fn from_config(cfg: &VideoConfig, source_width: u32, source_height: u32) -> Self {
        let (width, height) = output_size(cfg.resolution, source_width, source_height);
        Self {
            codec: cfg.codec,
            fps: cfg.fps,
            width,
            height,
            quality: cfg.quality,
            max_bitrate_kbps: cfg.max_bitrate_kbps,
            advanced: cfg.advanced.clone(),
        }
    }

    /// Bitrate obiettivo per VBR e CBR in kbit/s: quello scelto, altrimenti 2/3 del tetto.
    pub fn target_bitrate_kbps_for(&self, codec: VideoCodec) -> u32 {
        self.advanced.bitrate_kbps.unwrap_or_else(|| self.max_bitrate_kbps_for(codec) * 2 / 3).max(100)
    }

    /// Il 4:4:4 è chiesto e `encoder_name` lo sa fare.
    pub fn chroma444_for(&self, encoder_name: &str) -> bool {
        self.advanced.color_format == ColorFormat::Yuv444 && supports_444(encoder_name)
    }

    /// Tetto effettivo in kbit/s per `codec`.
    pub fn max_bitrate_kbps_for(&self, codec: VideoCodec) -> u32 {
        self.max_bitrate_kbps.unwrap_or_else(|| default_max_bitrate_kbps(codec, self.height, self.fps))
    }
}

/// Encoder che codificano in 4:4:4 da frame BGRA: NVENC in H.264 e HEVC. AMF e AV1 no; QSV solo su
/// alcune generazioni, per ora escluso.
pub fn supports_444(encoder_name: &str) -> bool {
    matches!(encoder_name, "h264_nvenc" | "hevc_nvenc")
}

/// Risoluzione d'uscita: la nativa della sorgente o l'altezza scelta con le proporzioni della
/// sorgente; larghezza e altezza arrotondate per difetto a un numero pari (minimo 2).
pub fn output_size(resolution: OutputResolution, source_width: u32, source_height: u32) -> (u32, u32) {
    let (sw, sh) = (source_width.max(2), source_height.max(2));
    let (w, h) = match resolution.height() {
        None => (sw, sh),
        Some(h) => ((u64::from(sw) * u64::from(h) / u64::from(sh)) as u32, h),
    };
    ((w & !1).max(2), (h & !1).max(2))
}

fn codec_prefix(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
        VideoCodec::Av1 => "av1",
    }
}

/// Encoder da provare per `choice`, in ordine: prima il codec scelto (HEVC per `Auto`), poi gli
/// altri come ripiego; per ogni codec NVENC, AMF, QSV; in fondo, sempre, [`SOFTWARE_ENCODER`]
/// (AV1 su CPU).
pub fn candidates(choice: CodecChoice) -> Vec<(VideoCodec, String)> {
    let codecs: &[VideoCodec] = match choice {
        CodecChoice::Auto | CodecChoice::Hevc => &[VideoCodec::Hevc, VideoCodec::H264],
        CodecChoice::H264 => &[VideoCodec::H264, VideoCodec::Hevc],
        CodecChoice::Av1 => &[VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264],
    };
    codecs
        .iter()
        .flat_map(|&c| Vendor::HARDWARE.iter().map(move |v| (c, format!("{}_{}", codec_prefix(c), v.suffix()))))
        .chain(std::iter::once((VideoCodec::Av1, SOFTWARE_ENCODER.to_owned())))
        .collect()
}

/// Apre l'encoder FFmpeg `encoder_name` (es. `hevc_nvenc`, `h264_qsv`, `libsvtav1`) sul device di
/// sessione. Con `libsvtav1` la conversione resta sulla GPU e solo il frame NV12 finale passa
/// alla CPU.
///
/// Errori: [`EncodeError::Unsupported`] se l'encoder non esiste o non può funzionare su questo
/// device con queste opzioni; [`EncodeError::SessionRejected`] se la sessione è rifiutata (es.
/// sessioni esaurite); [`EncodeError::Failed`] per argomenti non validi.
pub fn open(device: &GpuDevice, settings: &EncoderSettings, encoder_name: &str) -> Result<Encoder, EncodeError> {
    let unsupported = || EncodeError::Unsupported(format!("nome di encoder sconosciuto: {encoder_name}"));
    let (codec, vendor) = if encoder_name == SOFTWARE_ENCODER {
        (VideoCodec::Av1, Vendor::Software)
    } else {
        let (prefix, suffix) = encoder_name.split_once('_').ok_or_else(unsupported)?;
        let codec = [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1]
            .into_iter()
            .find(|&c| codec_prefix(c) == prefix)
            .ok_or_else(unsupported)?;
        (codec, Vendor::HARDWARE.into_iter().find(|v| v.suffix() == suffix).ok_or_else(unsupported)?)
    };
    let auto_b_frames = match vendor {
        // NVENC: FFmpeg tiene almeno 4 × (B + 1) superfici, ognuna con un buffer di bitstream in RAM
        // di ~2,5 byte per pixel che il driver tiene residente; con 2 B-frame sono 12 (84 MB a 1080p
        // contro 30 senza, `tests/memory.rs`). Il costo: ~15% di bitrate in più a qualità costante
        // (DECISIONS 31).
        Vendor::Nvenc => 0,
        // AMF: niente B-frame in HEVC su nessuna generazione VCN; in AV1 da verificare a runtime.
        Vendor::Amf if codec != VideoCodec::H264 => 0,
        Vendor::Software => 0,
        _ => B_FRAMES,
    };
    let b_frames = match (vendor, settings.advanced.b_frames) {
        (Vendor::Software, _) => 0,
        (_, Some(b)) => u32::from(b.min(4)),
        (_, None) => auto_b_frames,
    };
    let enc = with_bframe_fallback(encoder_name, b_frames, |b| ffi::HwEncoder::open(device, settings, encoder_name, codec, vendor, b))
        .or_else(|e| match e {
            // Alcuni driver AMD rifiutano QVBR, pre-analisi o VBAQ (dipende da GPU e versione):
            // si riprova lo stesso encoder in VBR semplice, senza B-frame, prima di scartarlo.
            EncodeError::Unsupported(ref msg) | EncodeError::SessionRejected(ref msg) if vendor == Vendor::Amf => {
                tracing::info!(encoder = encoder_name, motivo = %msg, "AMF rifiutato, riprovo in VBR semplice");
                let mut plain = settings.clone();
                plain.advanced.rate_control = cliprec_core::config::RateControl::Vbr;
                plain.advanced.lookahead = false;
                plain.advanced.adaptive_quantization = false;
                ffi::HwEncoder::open(device, &plain, encoder_name, codec, vendor, 0)
            }
            other => Err(other),
        })?;
    tracing::info!(
        encoder = encoder_name,
        width = settings.width,
        height = settings.height,
        fps = settings.fps,
        colore_444 = settings.chroma444_for(encoder_name),
        "encoder aperto"
    );
    Ok(enc)
}

/// B-frame chiesti alla prima apertura di un encoder che li supporta.
const B_FRAMES: u32 = 2;

/// Apre con `b_frames` B-frame; se `avcodec_open2` rifiuta la configurazione come non supportata
/// (es. un encoder senza B-frame su quella GPU: «Max B-frames 2 exceed 0», ENOSYS), riprova lo stesso encoder
/// senza B-frame prima di lasciare il posto al candidato successivo. FFmpeg scrive il motivo solo
/// nel log, quindi si riprova su ogni rifiuto di `avcodec_open2` di tipo `Unsupported`.
fn with_bframe_fallback<T>(name: &str, b_frames: u32, mut open: impl FnMut(u32) -> Result<T, EncodeError>) -> Result<T, EncodeError> {
    match open(b_frames) {
        Err(EncodeError::Unsupported(msg)) if b_frames > 0 && msg.contains("avcodec_open2") => {
            tracing::info!(encoder = name, motivo = %msg, "apertura rifiutata con B-frame, riprovo senza");
            open(0)
        }
        other => other,
    }
}

/// Apre il primo encoder di [`candidates`]`(settings.codec)` che funziona davvero su `device`.
/// Se nessuno si apre restituisce l'errore dell'ultimo tentativo.
pub fn open_best(device: &GpuDevice, settings: &EncoderSettings) -> Result<Encoder, EncodeError> {
    open_after(device, settings, &[])
}

/// Come [`open_best`], ma salta gli encoder in `failed` (nomi FFmpeg, es. `hevc_nvenc`): per il
/// ripiego a runtime quando un encoder fallisce in modo ripetibile.
pub fn open_after(device: &GpuDevice, settings: &EncoderSettings, failed: &[String]) -> Result<Encoder, EncodeError> {
    let mut last = EncodeError::Unsupported("nessun encoder disponibile".into());
    for (_, name) in candidates(settings.codec).into_iter().filter(|(_, n)| !failed.contains(n)) {
        match open(device, settings, &name) {
            Ok(enc) => return Ok(enc),
            Err(e) => {
                tracing::info!(encoder = %name, error = %e, "encoder scartato");
                last = e;
            }
        }
    }
    Err(last)
}

/// Porta il livello dei log di FFmpeg a ERROR e li inoltra a `tracing` (target `ffmpeg`; ERROR
/// come `warn`, FATAL come `error`) invece di scriverli su stderr. Da chiamare una volta all'avvio.
pub fn quiet_ffmpeg_logs() {
    ffi::quiet_logs();
}

/// Imposta la priorità GPU del processo (`D3DKMTSetProcessSchedulingPriorityClass`): HIGH con
/// `high = true` (`general.gpu_priority = "high"`), altrimenti NORMAL; mai REALTIME. Vale per tutto
/// il processo e per il device di sessione: va chiamata una volta dopo aver creato `device`.
pub fn set_gpu_priority(device: &GpuDevice, high: bool) -> Result<(), EncodeError> {
    d3d::set_gpu_priority(device, high).map_err(|e| EncodeError::Failed(format!("priorità GPU: {e}")))
}

/// Combinazioni codec/encoder hardware che si aprono davvero su `device` (1280×720 a 60 fps,
/// CQ 23). Il ripiego software [`SOFTWARE_ENCODER`] non è incluso: c'è sempre.
pub fn probe(device: &GpuDevice) -> Vec<(VideoCodec, String)> {
    let settings = EncoderSettings { codec: CodecChoice::Auto, fps: 60, width: 1280, height: 720, quality: 23, max_bitrate_kbps: None, advanced: EncoderAdvanced::default() };
    candidates(CodecChoice::Av1)
        .into_iter()
        .filter(|(_, name)| name != SOFTWARE_ENCODER)
        .filter(|(_, name)| match open(device, &settings, name) {
            Ok(_) => true,
            Err(e) => {
                // Nel log il motivo di ogni encoder che non si apre (es. AMF su una GPU AMD).
                tracing::info!(encoder = %name, "encoder non disponibile: {e}");
                false
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_size_keeps_aspect_and_is_even() {
        assert_eq!(output_size(OutputResolution::Native, 1920, 1080), (1920, 1080));
        assert_eq!(output_size(OutputResolution::Native, 1281, 721), (1280, 720));
        assert_eq!(output_size(OutputResolution::P720, 1920, 1080), (1280, 720));
        assert_eq!(output_size(OutputResolution::P1080, 2560, 1080), (2560, 1080));
        assert_eq!(output_size(OutputResolution::P1080, 1600, 1200), (1440, 1080));
        assert_eq!(output_size(OutputResolution::P1440, 3440, 1440), (3440, 1440));
    }

    #[test]
    fn bframe_rejection_retries_without_bframes() {
        let rejected = || EncodeError::Unsupported("hevc_nvenc: avcodec_open2: Function not implemented".into());
        let mut calls = Vec::new();
        let r = with_bframe_fallback("hevc_nvenc", 2, |b| {
            calls.push(b);
            if b > 0 { Err(rejected()) } else { Ok(b) }
        });
        assert_eq!((r.ok(), calls), (Some(0), vec![2, 0]), "rifiuto dei B-frame: stesso encoder senza B-frame");

        // Sessione rifiutata o errore prima di avcodec_open2: nessun nuovo tentativo.
        for err in [EncodeError::SessionRejected("x: avcodec_open2: Out of memory".into()), EncodeError::Unsupported("x: device QSV derivato".into())] {
            let mut calls = Vec::new();
            let mut err = Some(err);
            let r: Result<u32, _> = with_bframe_fallback("x", 2, |b| {
                calls.push(b);
                Err(err.take().unwrap_or_else(rejected))
            });
            assert!(r.is_err());
            assert_eq!(calls, [2]);
        }

        // Encoder senza B-frame: un solo tentativo.
        let mut calls = Vec::new();
        let _ = with_bframe_fallback("hevc_amf", 0, |b| -> Result<u32, _> {
            calls.push(b);
            Err(rejected())
        });
        assert_eq!(calls, [0]);
    }

    #[test]
    fn candidates_order() {
        let names: Vec<String> = candidates(CodecChoice::Auto).into_iter().map(|(_, n)| n).collect();
        assert_eq!(names[..3], ["hevc_nvenc", "hevc_amf", "hevc_qsv"]);
        assert_eq!(names[3], "h264_nvenc");
        assert_eq!(candidates(CodecChoice::H264)[0].1, "h264_nvenc");
        assert_eq!(candidates(CodecChoice::Av1).len(), 10);
        for choice in [CodecChoice::Auto, CodecChoice::H264, CodecChoice::Hevc, CodecChoice::Av1] {
            assert_eq!(candidates(choice).last().map(|(_, n)| n.as_str()), Some(SOFTWARE_ENCODER));
        }
    }
}
