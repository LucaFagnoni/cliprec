//! Modulo FFI FFmpeg: hwdevice/hwframes D3D11, encoder hardware e conversione dei pacchetti.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

use cliprec_core::{EncodeError, GpuDevice, GpuFrame, HNS_PER_SEC, PacketRef, QpcTime, VideoEncoder, VideoStreamParams};
use ffmpeg_the_third::ffi;
use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_DECODER, D3D11_BIND_RENDER_TARGET};
use windows::core::Interface;

use crate::d3d::Converter;
use crate::{EncoderSettings, Vendor};

/// Texture del pool fisso per QSV (`async_depth` 4 + B-frame + margine). D3D11 rifiuta array NV12
/// con `BIND_RENDER_TARGET` oltre 2 elementi (misurato su RTX 3080 e UHD 770), e FFmpeg con
/// `initial_pool_size > 0` crea un array: per NVENC/AMF il pool è quindi dinamico (texture singole
/// render target), per QSV, che richiede un pool fisso da derivare, è un array `BIND_DECODER`
/// riempito con una copia GPU da una texture render target intermedia.
const QSV_POOL_SIZE: c_int = 16;

const EAGAIN: c_int = 11;
const ENOENT: c_int = 2;
const EINVAL: c_int = 22;
const ENOSYS: c_int = 40;

/// `AVD3D11VADeviceContext` di `libavutil/hwcontext_d3d11va.h` (non coperto dai binding).
#[repr(C)]
struct AVD3D11VADeviceContext {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
}

/// `AVD3D11VAFramesContext` di `libavutil/hwcontext_d3d11va.h`.
#[repr(C)]
struct AVD3D11VAFramesContext {
    texture: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
    texture_infos: *mut c_void,
}

fn av_error_string(code: c_int) -> String {
    let mut buf = [0 as c_char; 128];
    // SAFETY: il buffer è locale e la sua lunghezza è passata; av_strerror lo termina con NUL.
    unsafe {
        ffi::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

/// Errore all'apertura: «non supportato» se l'encoder o il device non possono funzionare con
/// queste opzioni, altrimenti «sessione rifiutata» (es. sessioni NVENC esaurite, memoria).
fn open_error(name: &str, step: &str, code: c_int) -> EncodeError {
    let msg = format!("{name}: {step}: {}", av_error_string(code));
    if [ffi::AVERROR(ENOSYS), ffi::AVERROR(ENOENT), ffi::AVERROR(EINVAL)].contains(&code) {
        EncodeError::Unsupported(msg)
    } else {
        EncodeError::SessionRejected(msg)
    }
}

/// Encoder FFmpeg su frame D3D11 (NV12) del [`GpuDevice`] di sessione.
pub(crate) struct HwEncoder {
    ctx: *mut ffi::AVCodecContext,
    device_ref: *mut ffi::AVBufferRef,
    /// Pool NV12 D3D11 in cui il video processor scrive.
    frames_ref: *mut ffi::AVBufferRef,
    /// Solo QSV: device e pool QSV derivati da quelli D3D11 (stesse texture).
    qsv_device_ref: *mut ffi::AVBufferRef,
    qsv_frames_ref: *mut ffi::AVBufferRef,
    frame: *mut ffi::AVFrame,
    qsv_frame: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    converter: Converter,
    params: VideoStreamParams,
    /// Indice e pts QPC del primo frame: pts dell'encoder = indice − primo indice (time_base 1/fps).
    first: Option<(u64, QpcTime)>,
    last_pts: i64,
}

// SAFETY: i contesti FFmpeg non sono legati al thread che li ha creati; `HwEncoder` è usato da un
// thread alla volta (`&mut self`) e le interfacce COM del Converter vengono da un device
// free-threaded con contesto immediato protetto da SetMultithreadProtected.
unsafe impl Send for HwEncoder {}

impl Drop for HwEncoder {
    fn drop(&mut self) {
        // SAFETY: ogni puntatore è nullo o di proprietà esclusiva di `self`; le funzioni di
        // rilascio di FFmpeg accettano puntatori nulli e li azzerano.
        unsafe {
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_frame_free(&mut self.frame);
            ffi::av_frame_free(&mut self.qsv_frame);
            ffi::av_packet_free(&mut self.pkt);
            ffi::av_buffer_unref(&mut self.qsv_frames_ref);
            ffi::av_buffer_unref(&mut self.qsv_device_ref);
            ffi::av_buffer_unref(&mut self.frames_ref);
            ffi::av_buffer_unref(&mut self.device_ref);
        }
    }
}

struct Dict(*mut ffi::AVDictionary);

impl Dict {
    fn set(&mut self, key: &str, value: impl ToString) {
        let (Ok(k), Ok(v)) = (CString::new(key), CString::new(value.to_string())) else { return };
        // SAFETY: chiave e valore sono stringhe C valide; av_dict_set le copia.
        unsafe { ffi::av_dict_set(&mut self.0, k.as_ptr(), v.as_ptr(), 0) };
    }
}

impl Drop for Dict {
    fn drop(&mut self) {
        // SAFETY: il dizionario è nullo o allocato da av_dict_set.
        unsafe { ffi::av_dict_free(&mut self.0) };
    }
}

impl HwEncoder {
    pub(crate) fn open(gpu: &GpuDevice, s: &EncoderSettings, name: &str, codec: cliprec_core::VideoCodec, vendor: Vendor) -> Result<Self, EncodeError> {
        let cname = CString::new(name).map_err(|_| EncodeError::Unsupported(name.to_owned()))?;
        // SAFETY: stringa C valida; la funzione restituisce un puntatore statico o nullo.
        let av_codec = unsafe { ffi::avcodec_find_encoder_by_name(cname.as_ptr()) };
        if av_codec.is_null() {
            return Err(EncodeError::Unsupported(format!("{name}: assente in questa build di FFmpeg")));
        }
        if s.width == 0 || s.height == 0 || !s.width.is_multiple_of(2) || !s.height.is_multiple_of(2) || s.fps == 0 {
            return Err(EncodeError::Failed(format!("{name}: dimensione {}×{} @ {} fps non valida", s.width, s.height, s.fps)));
        }
        let converter = Converter::new(gpu, s.width, s.height, s.fps, vendor == Vendor::Qsv)
            .map_err(|e| EncodeError::Unsupported(format!("{name}: video processor D3D11: {e}")))?;

        let max_kbps = s.max_bitrate_kbps_for(codec);
        let mut enc = HwEncoder {
            ctx: ptr::null_mut(),
            device_ref: ptr::null_mut(),
            frames_ref: ptr::null_mut(),
            qsv_device_ref: ptr::null_mut(),
            qsv_frames_ref: ptr::null_mut(),
            frame: ptr::null_mut(),
            qsv_frame: ptr::null_mut(),
            pkt: ptr::null_mut(),
            converter,
            params: VideoStreamParams {
                codec,
                width: s.width,
                height: s.height,
                fps: s.fps,
                extradata: Vec::new(),
                bit_rate: u64::from(max_kbps) * 1000,
                encoder_name: name.to_owned(),
            },
            first: None,
            last_pts: -1,
        };

        // SAFETY: sequenza documentata di libavutil/libavcodec. Ogni puntatore allocato finisce
        // subito in `enc`, che lo libera in Drop anche se un passo successivo fallisce. I campi
        // scritti nei contesti D3D11 seguono hwcontext_d3d11va.h; i riferimenti COM passati a
        // FFmpeg sono AddRef-ati (clone + into_raw) perché FFmpeg li rilascia alla chiusura.
        unsafe {
            enc.device_ref = ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::D3D11VA);
            if enc.device_ref.is_null() {
                return Err(EncodeError::Failed(format!("{name}: av_hwdevice_ctx_alloc")));
            }
            let dev = (*enc.device_ref).data as *mut ffi::AVHWDeviceContext;
            let d3d = (*dev).hwctx as *mut AVD3D11VADeviceContext;
            (*d3d).device = gpu.device.clone().into_raw();
            (*d3d).device_context = gpu.context.clone().into_raw();
            let ret = ffi::av_hwdevice_ctx_init(enc.device_ref);
            if ret < 0 {
                return Err(open_error(name, "av_hwdevice_ctx_init", ret));
            }

            enc.frames_ref = ffi::av_hwframe_ctx_alloc(enc.device_ref);
            if enc.frames_ref.is_null() {
                return Err(EncodeError::Failed(format!("{name}: av_hwframe_ctx_alloc")));
            }
            let fc = (*enc.frames_ref).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::D3D11;
            (*fc).sw_format = ffi::AVPixelFormat::NV12;
            (*fc).width = s.width as c_int;
            (*fc).height = s.height as c_int;
            let fhw = (*fc).hwctx as *mut AVD3D11VAFramesContext;
            if vendor == Vendor::Qsv {
                (*fc).initial_pool_size = QSV_POOL_SIZE;
                (*fhw).bind_flags = D3D11_BIND_DECODER.0 as u32;
            } else {
                (*fhw).bind_flags = D3D11_BIND_RENDER_TARGET.0 as u32;
            }
            let ret = ffi::av_hwframe_ctx_init(enc.frames_ref);
            if ret < 0 {
                return Err(open_error(name, "av_hwframe_ctx_init", ret));
            }

            let (pix_fmt, enc_frames) = if vendor == Vendor::Qsv {
                let ret = ffi::av_hwdevice_ctx_create_derived(&mut enc.qsv_device_ref, ffi::AVHWDeviceType::QSV, enc.device_ref, 0);
                if ret < 0 {
                    return Err(EncodeError::Unsupported(format!("{name}: device QSV derivato: {}", av_error_string(ret))));
                }
                let ret = ffi::av_hwframe_ctx_create_derived(
                    &mut enc.qsv_frames_ref,
                    ffi::AVPixelFormat::QSV,
                    enc.qsv_device_ref,
                    enc.frames_ref,
                    0,
                );
                if ret < 0 {
                    return Err(EncodeError::Unsupported(format!("{name}: frame QSV derivati: {}", av_error_string(ret))));
                }
                enc.qsv_frame = ffi::av_frame_alloc();
                (ffi::AVPixelFormat::QSV, enc.qsv_frames_ref)
            } else {
                (ffi::AVPixelFormat::D3D11, enc.frames_ref)
            };

            enc.ctx = ffi::avcodec_alloc_context3(av_codec);
            enc.frame = ffi::av_frame_alloc();
            enc.pkt = ffi::av_packet_alloc();
            if enc.ctx.is_null() || enc.frame.is_null() || enc.pkt.is_null() || (vendor == Vendor::Qsv && enc.qsv_frame.is_null()) {
                return Err(EncodeError::Failed(format!("{name}: allocazione del contesto")));
            }
            let c = &mut *enc.ctx;
            c.width = s.width as c_int;
            c.height = s.height as c_int;
            c.time_base = ffi::AVRational { num: 1, den: s.fps as c_int };
            c.framerate = ffi::AVRational { num: s.fps as c_int, den: 1 };
            c.pix_fmt = pix_fmt;
            c.sw_pix_fmt = ffi::AVPixelFormat::NV12;
            c.hw_frames_ctx = ffi::av_buffer_ref(enc_frames);
            c.gop_size = s.fps as c_int;
            c.max_b_frames = if codec == cliprec_core::VideoCodec::Hevc && vendor == Vendor::Amf { 0 } else { 2 };
            c.rc_max_rate = i64::from(max_kbps) * 1000;
            c.rc_buffer_size = (i64::from(max_kbps) * 1000).min(i64::from(c_int::MAX)) as c_int;
            c.flags |= ffi::AV_CODEC_FLAG_GLOBAL_HEADER as c_int | ffi::AV_CODEC_FLAG_CLOSED_GOP as c_int;
            c.color_primaries = ffi::AVColorPrimaries::BT709;
            c.color_trc = ffi::AVColorTransferCharacteristic::BT709;
            c.colorspace = ffi::AVColorSpace::BT709;
            c.color_range = ffi::AVColorRange::MPEG;

            let mut opts = Dict(ptr::null_mut());
            let q = s.quality.clamp(1, 51);
            match vendor {
                Vendor::Nvenc => {
                    c.bit_rate = 0;
                    opts.set("preset", "p5");
                    opts.set("tune", "hq");
                    opts.set("rc", "vbr");
                    opts.set("cq", q);
                    opts.set("rc-lookahead", 0);
                    opts.set("forced-idr", 1);
                }
                Vendor::Amf => {
                    // ponytail: non verificato (niente GPU AMD qui); livello QVBR = quality.
                    c.bit_rate = i64::from(max_kbps) * 1000 * 2 / 3;
                    opts.set("usage", "transcoding");
                    opts.set("quality", "quality");
                    opts.set("rc", "qvbr");
                    opts.set("qvbr_quality_level", q);
                    opts.set("forced_idr", 1);
                }
                Vendor::Qsv => {
                    // global_quality + maxrate + bitrate ⇒ QVBR in qsvenc.
                    c.bit_rate = i64::from(max_kbps) * 1000 * 2 / 3;
                    c.global_quality = c_int::from(q);
                    opts.set("preset", "faster");
                    opts.set("look_ahead", 0);
                    opts.set("forced_idr", 1);
                    opts.set("idr_interval", 0);
                }
            }

            let ret = ffi::avcodec_open2(enc.ctx, av_codec, &mut opts.0);
            if ret < 0 {
                return Err(open_error(name, "avcodec_open2", ret));
            }
            let c = &*enc.ctx;
            if !c.extradata.is_null() && c.extradata_size > 0 {
                enc.params.extradata = std::slice::from_raw_parts(c.extradata, c.extradata_size as usize).to_vec();
            }
        }
        Ok(enc)
    }

    /// Errore a runtime: device perso se D3D11 lo dice, altrimenti errore dell'encoder.
    fn runtime_error(&self, step: &str, detail: String) -> EncodeError {
        if self.converter.device_removed() {
            EncodeError::DeviceLost
        } else {
            EncodeError::Failed(format!("{}: {step}: {detail}", self.params.encoder_name))
        }
    }

    fn to_qpc(&self, ts: i64) -> QpcTime {
        let (_, base) = self.first.unwrap_or_default();
        let hns = i128::from(ts) * i128::from(HNS_PER_SEC) / i128::from(self.params.fps.max(1));
        base.add_hns(hns as i64)
    }

    /// Consegna a `sink` tutti i pacchetti pronti.
    fn drain(&mut self, sink: &mut dyn FnMut(PacketRef<'_>)) -> Result<(), EncodeError> {
        let duration = HNS_PER_SEC / i64::from(self.params.fps.max(1));
        loop {
            // SAFETY: contesto aperto e pacchetto allocato, entrambi di proprietà di `self`.
            let ret = unsafe { ffi::avcodec_receive_packet(self.ctx, self.pkt) };
            if ret == ffi::AVERROR(EAGAIN) || ret == ffi::AVERROR_EOF {
                return Ok(());
            }
            if ret < 0 {
                return Err(self.runtime_error("avcodec_receive_packet", av_error_string(ret)));
            }
            // SAFETY: dopo un receive riuscito `pkt` contiene dati validi per `size` byte fino a
            // av_packet_unref; la fetta non sopravvive alla chiamata di `sink`.
            unsafe {
                let p = &*self.pkt;
                let data = if p.data.is_null() || p.size <= 0 { &[][..] } else { std::slice::from_raw_parts(p.data, p.size as usize) };
                let dts = if p.dts == ffi::AV_NOPTS_VALUE { p.pts } else { p.dts };
                sink(PacketRef {
                    data,
                    pts: self.to_qpc(p.pts),
                    dts: self.to_qpc(dts),
                    duration,
                    keyframe: p.flags & ffi::AV_PKT_FLAG_KEY != 0,
                });
                ffi::av_packet_unref(self.pkt);
            }
        }
    }
}

impl VideoEncoder for HwEncoder {
    fn params(&self) -> &VideoStreamParams {
        &self.params
    }

    fn encode(&mut self, frame: &GpuFrame, sink: &mut dyn FnMut(PacketRef<'_>)) -> Result<(), EncodeError> {
        let (first_index, _) = *self.first.get_or_insert((frame.index, frame.pts));
        let pts = (frame.index.saturating_sub(first_index) as i64).max(self.last_pts + 1);
        self.last_pts = pts;

        // SAFETY: frame e pool sono di proprietà di `self`; av_hwframe_get_buffer riempie
        // data[0] (ID3D11Texture2D*) e data[1] (indice nell'array) secondo hwcontext_d3d11va.h.
        let (tex, slice) = unsafe {
            ffi::av_frame_unref(self.frame);
            let ret = ffi::av_hwframe_get_buffer(self.frames_ref, self.frame, 0);
            if ret < 0 {
                return Err(self.runtime_error("av_hwframe_get_buffer", av_error_string(ret)));
            }
            ((*self.frame).data[0] as *mut c_void, (*self.frame).data[1] as usize)
        };
        if let Err(e) = self.converter.blit(&frame.texture, frame.width, frame.height, tex, slice) {
            return Err(self.runtime_error("VideoProcessorBlt", e.to_string()));
        }

        // SAFETY: frame (e frame QSV) allocati e di proprietà di `self`; il pool QSV è derivato
        // dal pool D3D11 da cui viene `self.frame`, come richiede av_hwframe_map.
        let ret = unsafe {
            let send = if self.qsv_frame.is_null() {
                self.frame
            } else {
                ffi::av_frame_unref(self.qsv_frame);
                (*self.qsv_frame).format = ffi::AVPixelFormat::QSV.0;
                (*self.qsv_frame).hw_frames_ctx = ffi::av_buffer_ref(self.qsv_frames_ref);
                let ret = ffi::av_hwframe_map(self.qsv_frame, self.frame, 0);
                if ret < 0 {
                    return Err(self.runtime_error("av_hwframe_map", av_error_string(ret)));
                }
                self.qsv_frame
            };
            let f = &mut *send;
            f.pts = pts;
            f.duration = 1;
            f.color_primaries = ffi::AVColorPrimaries::BT709;
            f.color_trc = ffi::AVColorTransferCharacteristic::BT709;
            f.colorspace = ffi::AVColorSpace::BT709;
            f.color_range = ffi::AVColorRange::MPEG;
            ffi::avcodec_send_frame(self.ctx, send)
        };
        if ret < 0 {
            return Err(self.runtime_error("avcodec_send_frame", av_error_string(ret)));
        }
        self.drain(sink)
    }

    fn flush(&mut self, sink: &mut dyn FnMut(PacketRef<'_>)) -> Result<(), EncodeError> {
        // SAFETY: contesto aperto; un frame nullo chiede lo svuotamento.
        let ret = unsafe { ffi::avcodec_send_frame(self.ctx, ptr::null()) };
        if ret < 0 && ret != ffi::AVERROR_EOF {
            return Err(self.runtime_error("flush", av_error_string(ret)));
        }
        self.drain(sink)
    }
}
