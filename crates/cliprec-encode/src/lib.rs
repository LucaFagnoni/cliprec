//! Encoder hardware (NVENC, AMF, QSV) via FFmpeg 9 con frame D3D11 senza copie; conversione
//! BGRA→NV12 su GPU.
//!
//! Ogni [`GpuFrame`](cliprec_core::GpuFrame) BGRA viene convertito in NV12 BT.709 a range limitato
//! da `ID3D11VideoProcessor`, scalato nella risoluzione d'uscita fissa con bande nere, dentro una
//! texture del pool hwframes D3D11 di FFmpeg, e passato all'encoder senza passare dalla CPU.
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
mod ffi;
mod nal;

use cliprec_core::config::{CodecChoice, OutputResolution, VideoConfig, default_max_bitrate_kbps};
use cliprec_core::{EncodeError, GpuDevice, VideoCodec, VideoEncoder};

/// Famiglia di encoder hardware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    Nvenc,
    Amf,
    Qsv,
}

impl Vendor {
    const ALL: [Vendor; 3] = [Vendor::Nvenc, Vendor::Amf, Vendor::Qsv];

    fn suffix(self) -> &'static str {
        match self {
            Vendor::Nvenc => "nvenc",
            Vendor::Amf => "amf",
            Vendor::Qsv => "qsv",
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
}

impl EncoderSettings {
    /// Impostazioni dalla configurazione e dalla dimensione della prima texture della sorgente.
    pub fn from_config(cfg: &VideoConfig, source_width: u32, source_height: u32) -> Self {
        let (width, height) = output_size(cfg.resolution, source_width, source_height);
        Self { codec: cfg.codec, fps: cfg.fps, width, height, quality: cfg.quality, max_bitrate_kbps: cfg.max_bitrate_kbps }
    }

    /// Tetto effettivo in kbit/s per `codec`.
    pub fn max_bitrate_kbps_for(&self, codec: VideoCodec) -> u32 {
        self.max_bitrate_kbps.unwrap_or_else(|| default_max_bitrate_kbps(codec, self.height, self.fps))
    }
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
/// altri come ripiego; per ogni codec NVENC, AMF, QSV.
pub fn candidates(choice: CodecChoice) -> Vec<(VideoCodec, String)> {
    let codecs: &[VideoCodec] = match choice {
        CodecChoice::Auto | CodecChoice::Hevc => &[VideoCodec::Hevc, VideoCodec::H264],
        CodecChoice::H264 => &[VideoCodec::H264, VideoCodec::Hevc],
        CodecChoice::Av1 => &[VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264],
    };
    codecs
        .iter()
        .flat_map(|&c| Vendor::ALL.iter().map(move |v| (c, format!("{}_{}", codec_prefix(c), v.suffix()))))
        .collect()
}

/// Apre l'encoder FFmpeg `encoder_name` (es. `hevc_nvenc`, `h264_qsv`) sul device di sessione.
///
/// Errori: [`EncodeError::Unsupported`] se l'encoder non esiste o non può funzionare su questo
/// device con queste opzioni; [`EncodeError::SessionRejected`] se la sessione è rifiutata (es.
/// sessioni esaurite); [`EncodeError::Failed`] per argomenti non validi.
pub fn open(device: &GpuDevice, settings: &EncoderSettings, encoder_name: &str) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    let unsupported = || EncodeError::Unsupported(format!("nome di encoder sconosciuto: {encoder_name}"));
    let (prefix, suffix) = encoder_name.split_once('_').ok_or_else(unsupported)?;
    let codec = [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1]
        .into_iter()
        .find(|&c| codec_prefix(c) == prefix)
        .ok_or_else(unsupported)?;
    let vendor = Vendor::ALL.into_iter().find(|v| v.suffix() == suffix).ok_or_else(unsupported)?;
    let enc = ffi::HwEncoder::open(device, settings, encoder_name, codec, vendor)?;
    tracing::info!(encoder = encoder_name, width = settings.width, height = settings.height, fps = settings.fps, "encoder aperto");
    Ok(Box::new(enc))
}

/// Apre il primo encoder di [`candidates`]`(settings.codec)` che funziona davvero su `device`.
/// Se nessuno si apre restituisce l'errore dell'ultimo tentativo.
pub fn open_best(device: &GpuDevice, settings: &EncoderSettings) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    open_after(device, settings, &[])
}

/// Come [`open_best`], ma salta gli encoder in `failed` (nomi FFmpeg, es. `hevc_nvenc`): per il
/// ripiego a runtime quando un encoder fallisce in modo ripetibile.
pub fn open_after(device: &GpuDevice, settings: &EncoderSettings, failed: &[String]) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    let mut last = EncodeError::Unsupported("nessun encoder hardware".into());
    for (_, name) in candidates(settings.codec).into_iter().filter(|(_, n)| !failed.contains(n)) {
        match open(device, settings, &name) {
            Ok(enc) => return Ok(enc),
            Err(e) => {
                tracing::debug!(encoder = %name, error = %e, "encoder scartato");
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

/// Combinazioni codec/encoder che si aprono davvero su `device` (1280×720 a 60 fps, CQ 23).
pub fn probe(device: &GpuDevice) -> Vec<(VideoCodec, String)> {
    let settings = EncoderSettings { codec: CodecChoice::Auto, fps: 60, width: 1280, height: 720, quality: 23, max_bitrate_kbps: None };
    candidates(CodecChoice::Av1)
        .into_iter()
        .filter(|(_, name)| open(device, &settings, name).is_ok())
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
    fn candidates_order() {
        let names: Vec<String> = candidates(CodecChoice::Auto).into_iter().map(|(_, n)| n).collect();
        assert_eq!(names[..3], ["hevc_nvenc", "hevc_amf", "hevc_qsv"]);
        assert_eq!(names[3], "h264_nvenc");
        assert_eq!(candidates(CodecChoice::H264)[0].1, "h264_nvenc");
        assert_eq!(candidates(CodecChoice::Av1).len(), 9);
    }
}
