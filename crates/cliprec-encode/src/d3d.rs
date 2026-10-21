//! Modulo FFI D3D11: conversione BGRA→NV12 con scala e bande nere su `ID3D11VideoProcessor`.
//!
//! Tutto resta sulla GPU: il Blt è accodato sul contesto immediato del [`GpuDevice`], che
//! `cliprec-core` rende sicuro tra thread con `SetMultithreadProtected(true)`.

use std::ffi::c_void;
use std::mem::ManuallyDrop;

use cliprec_core::GpuDevice;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEX2D_ARRAY_VPOV, D3D11_TEX2D_VPIV, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0,
    D3D11_VIDEO_COLOR_RGBA, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BOX, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_USAGE_STAGING,
    D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2DARRAY, ID3D11Device, ID3D11DeviceContext,
    ID3D11Texture2D, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Wdk::Graphics::Direct3D::{
    D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH, D3DKMT_SCHEDULINGPRIORITYCLASS_NORMAL, D3DKMTSetProcessSchedulingPriorityClass,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::core::Interface;

/// Viste d'ingresso tenute in cache: il pool di cattura ruota poche texture.
// ponytail: cache a rotazione di 4 viste; una texture fuori cache costa una vista nuova per frame,
// alzare il numero se la sorgente usa pool più grandi.
const INPUT_VIEWS: usize = 8;

/// Rettangolo in cui disegnare una sorgente `src_w`×`src_h` dentro `out_w`×`out_h` mantenendo
/// le proporzioni, centrato e con coordinate pari (crominanza 4:2:0 allineata).
pub(crate) fn letterbox(src_w: u32, src_h: u32, out_w: u32, out_h: u32) -> RECT {
    let (sw, sh, ow, oh) = (u64::from(src_w.max(1)), u64::from(src_h.max(1)), u64::from(out_w), u64::from(out_h));
    // Confronto in interi: sw/sh > ow/oh ⇔ la sorgente è più larga dell'uscita.
    let (w, h) = if sw * oh > ow * sh { (ow, (ow * sh + sw / 2) / sw) } else { ((oh * sw + sh / 2) / sh, oh) };
    let (w, h) = ((w.min(ow) & !1) as i32, (h.min(oh) & !1) as i32);
    let x = ((out_w as i32 - w) / 2) & !1;
    let y = ((out_h as i32 - h) / 2) & !1;
    RECT { left: x, top: y, right: x + w, bottom: y + h }
}

/// Priorità GPU del processo: classe HIGH (mai REALTIME, che può bloccare NVENC) o NORMAL, più la
/// priorità dei thread GPU del device (7 con HIGH, 0 con NORMAL; non fatale se rifiutata).
pub(crate) fn set_gpu_priority(gpu: &GpuDevice, high: bool) -> windows::core::Result<()> {
    let class = if high { D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH } else { D3DKMT_SCHEDULINGPRIORITYCLASS_NORMAL };
    // SAFETY: GetCurrentProcess restituisce la pseudo-handle del processo corrente, sempre valida.
    unsafe { D3DKMTSetProcessSchedulingPriorityClass(GetCurrentProcess(), class) }.ok()?;
    let dxgi: IDXGIDevice = gpu.device.cast()?;
    // SAFETY: chiamata COM su un'interfaccia valida.
    if let Err(e) = unsafe { dxgi.SetGPUThreadPriority(if high { 7 } else { 0 }) } {
        tracing::warn!(error = %e, "SetGPUThreadPriority rifiutata");
    }
    Ok(())
}

/// Video processor costruito per una dimensione d'ingresso, con le sue viste.
struct Processor {
    src_w: u32,
    src_h: u32,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// (puntatore della texture, vista). La vista tiene un riferimento alla texture, quindi il
    /// puntatore non può essere riusato da un'altra texture finché è in cache.
    /// `None` come vista = la texture non accetta viste d'ingresso, va copiata in `copy`.
    inputs: [Option<(usize, Option<ID3D11VideoProcessorInputView>)>; INPUT_VIEWS],
    next_input: usize,
    /// (puntatore della texture del pool, indice nell'array, vista).
    outputs: Vec<(usize, usize, ID3D11VideoProcessorOutputView)>,
    /// Copia BGRA render target per le sorgenti che non accettano una vista d'ingresso.
    copy: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
}

/// Texture `format` render target della dimensione data.
fn texture(device: &ID3D11Device, w: u32, h: u32, format: DXGI_FORMAT) -> windows::core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..Default::default()
    };
    let mut tex = None;
    // SAFETY: descrittore locale valido per la chiamata; puntatore d'uscita locale.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))
}

/// Vista d'ingresso del video processor su una texture 2D (non array).
fn input_view(
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    tex: &ID3D11Texture2D,
) -> windows::core::Result<ID3D11VideoProcessorInputView> {
    let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
        FourCC: 0,
        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
    };
    let mut view = None;
    // SAFETY: texture ed enumeratore validi sullo stesso device; desc vive per la chiamata.
    unsafe { video_device.CreateVideoProcessorInputView(tex, enumerator, &desc, Some(&mut view))? };
    view.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))
}

/// Dove finisce il frame NV12 convertito.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    /// Direttamente nella texture del pool hwframes (render target): NVENC, AMF.
    Pool,
    /// In una texture render target intermedia, poi copiata su GPU nella fetta del pool: QSV.
    PoolViaCopy,
    /// In una texture render target intermedia, poi letta dalla CPU (encoder software).
    Cpu,
}

pub(crate) struct Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// QSV e software: texture NV12 render target intermedia.
    staging: Option<ID3D11Texture2D>,
    /// Solo software: texture NV12 `USAGE_STAGING` leggibile dalla CPU.
    readback: Option<ID3D11Texture2D>,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    out_w: u32,
    out_h: u32,
    fps: u32,
    /// Uscita BGRA a colori pieni (4:4:4, convertito dall'encoder) invece di NV12.
    rgb: bool,
    proc: Option<Processor>,
}

impl Converter {
    pub(crate) fn new(gpu: &GpuDevice, out_w: u32, out_h: u32, fps: u32, output: Output, rgb: bool) -> windows::core::Result<Self> {
        let staging = match output {
            Output::Pool => None,
            Output::PoolViaCopy | Output::Cpu => Some(texture(&gpu.device, out_w, out_h, DXGI_FORMAT_NV12)?),
        };
        let readback = match output {
            Output::Cpu => {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: out_w,
                    Height: out_h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    ..Default::default()
                };
                let mut tex = None;
                // SAFETY: descrittore locale valido per la chiamata; puntatore d'uscita locale.
                unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
                Some(tex.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))?)
            }
            _ => None,
        };
        Ok(Self {
            device: gpu.device.clone(),
            context: gpu.context.clone(),
            staging,
            readback,
            video_device: gpu.device.cast()?,
            video_context: gpu.context.cast()?,
            out_w,
            out_h,
            fps,
            rgb,
            proc: None,
        })
    }

    /// `true` se il device è stato rimosso (driver aggiornato o reset, TDR).
    pub(crate) fn device_removed(&self) -> bool {
        device_removed(&self.device)
    }

    /// Copia l'ultimo frame convertito (con [`Output::Cpu`]) nella texture leggibile, la mappa e
    /// passa a `f` i piani Y e UV interlacciato con i rispettivi passi in byte. Attende la GPU.
    pub(crate) fn read_nv12(&mut self, f: impl FnOnce(&[u8], usize, &[u8], usize)) -> windows::core::Result<()> {
        let (Some(staging), Some(readback)) = (&self.staging, &self.readback) else {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_UNEXPECTED));
        };
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: due texture NV12 della stessa dimensione sullo stesso device; `readback` è
        // USAGE_STAGING con CPU_ACCESS_READ, quindi mappabile in lettura.
        unsafe {
            self.context.CopyResource(readback, staging);
            self.context.Map(readback, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
        }
        let pitch = map.RowPitch as usize;
        let h = self.out_h as usize;
        if !map.pData.is_null() && pitch >= self.out_w as usize {
            // SAFETY: per NV12 la mappatura contiene `h` righe di Y da `pitch` byte seguite da `h/2`
            // righe di UV interlacciato con lo stesso passo; i dati restano validi fino a Unmap.
            let (y, uv) = unsafe {
                let base = map.pData as *const u8;
                (std::slice::from_raw_parts(base, pitch * h), std::slice::from_raw_parts(base.add(pitch * h), pitch * (h / 2)))
            };
            f(y, pitch, uv, pitch);
        }
        // SAFETY: risorsa mappata qui sopra.
        unsafe { self.context.Unmap(readback, 0) };
        Ok(())
    }

    /// Accoda la conversione di `src` (BGRA, `src_w`×`src_h`) nella fetta `slice` della texture
    /// NV12 `dst` (puntatore `ID3D11Texture2D*` preso da `AVFrame.data[0]`). Con [`Output::Cpu`]
    /// `dst` è nullo e il risultato resta nella texture intermedia per [`Converter::read_nv12`].
    pub(crate) fn blit(
        &mut self,
        src: &ID3D11Texture2D,
        src_w: u32,
        src_h: u32,
        dst: *mut c_void,
        slice: usize,
    ) -> windows::core::Result<()> {
        if self.proc.as_ref().is_none_or(|p| p.src_w != src_w || p.src_h != src_h) {
            self.proc = None;
            self.proc = Some(self.build(src_w, src_h)?);
        }
        let Some(p) = self.proc.as_mut() else { return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_UNEXPECTED)) };

        let src_key = src.as_raw() as usize;
        let cached = p.inputs.iter().flatten().find(|(k, _)| *k == src_key).map(|(_, v)| v.clone());
        let direct = match cached {
            Some(v) => v,
            None => {
                // Le texture senza BIND_RENDER_TARGET (es. solo SHADER_RESOURCE) non accettano una
                // vista d'ingresso (E_INVALIDARG su RTX 3080): per quelle si copia su GPU.
                let v = input_view(&self.video_device, &p.enumerator, src).ok();
                p.inputs[p.next_input] = Some((src_key, v.clone()));
                p.next_input = (p.next_input + 1) % INPUT_VIEWS;
                v
            }
        };
        let input = match direct {
            Some(v) => v,
            None => {
                if p.copy.is_none() {
                    let tex = texture(&self.device, src_w, src_h, DXGI_FORMAT_B8G8R8A8_UNORM)?;
                    let view = input_view(&self.video_device, &p.enumerator, &tex)?;
                    p.copy = Some((tex, view));
                }
                let Some((tex, view)) = &p.copy else { return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_UNEXPECTED)) };
                let area = D3D11_BOX { left: 0, top: 0, front: 0, right: src_w, bottom: src_h, back: 1 };
                // SAFETY: entrambe le texture sono BGRA sullo stesso device e coprono `area`
                // (contratto di GpuFrame: la texture è grande almeno quanto la dimensione dichiarata).
                unsafe { self.context.CopySubresourceRegion(tex, 0, 0, 0, 0, src, 0, Some(&area)) };
                view.clone()
            }
        };

        let (target, target_slice) = match &self.staging {
            Some(t) => (t.as_raw(), 0),
            None => (dst, slice),
        };
        let dst_key = target as usize;
        let output = match p.outputs.iter().find(|(k, s, _)| *k == dst_key && *s == target_slice) {
            Some((_, _, v)) => v.clone(),
            None => {
                // SAFETY: `dst` è il puntatore ID3D11Texture2D di un AVFrame D3D11 vivo per tutta la
                // chiamata; lo prendiamo in prestito senza toccarne il conteggio dei riferimenti.
                let Some(tex) = (unsafe { ID3D11Texture2D::from_raw_borrowed(&target) }) else {
                    return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER));
                };
                let mut tex_desc = D3D11_TEXTURE2D_DESC::default();
                // SAFETY: interfaccia valida, puntatore d'uscita locale.
                unsafe { tex.GetDesc(&mut tex_desc) };
                let desc = if tex_desc.ArraySize > 1 {
                    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
                        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                            Texture2DArray: D3D11_TEX2D_ARRAY_VPOV { MipSlice: 0, FirstArraySlice: target_slice as u32, ArraySize: 1 },
                        },
                    }
                } else {
                    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
                    }
                };
                let mut view = None;
                // SAFETY: texture del pool ed enumeratore validi sullo stesso device.
                unsafe { self.video_device.CreateVideoProcessorOutputView(tex, &p.enumerator, &desc, Some(&mut view))? };
                let view = view.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))?;
                p.outputs.push((dst_key, target_slice, view.clone()));
                view
            }
        };

        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(input)),
            ..Default::default()
        };
        // SAFETY: processor, vista d'uscita e vista d'ingresso sono valide; `stream` vive per la chiamata.
        let res = unsafe { self.video_context.VideoProcessorBlt(&p.processor, &output, 0, std::slice::from_ref(&stream)) };
        drop(ManuallyDrop::into_inner(stream.pInputSurface));
        res?;
        if let Some(staging) = self.staging.as_ref().filter(|_| !dst.is_null()) {
            // SAFETY: `dst` è il puntatore ID3D11Texture2D di un AVFrame vivo per la chiamata;
            // sorgente e destinazione sono NV12 della stessa dimensione sullo stesso device.
            unsafe {
                let Some(dst_tex) = ID3D11Texture2D::from_raw_borrowed(&dst) else {
                    return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER));
                };
                self.context.CopySubresourceRegion(dst_tex, slice as u32, 0, 0, 0, staging, 0, None);
            }
        }
        Ok(())
    }

    fn build(&self, src_w: u32, src_h: u32) -> windows::core::Result<Processor> {
        let rate = DXGI_RATIONAL { Numerator: self.fps, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: src_w,
            InputHeight: src_h,
            OutputFrameRate: rate,
            OutputWidth: self.out_w,
            OutputHeight: self.out_h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let vc = &self.video_context;
        let dest = letterbox(src_w, src_h, self.out_w, self.out_h);
        let full_src = RECT { left: 0, top: 0, right: src_w as i32, bottom: src_h as i32 };
        let full_out = RECT { left: 0, top: 0, right: self.out_w as i32, bottom: self.out_h as i32 };
        let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
        // SAFETY: chiamate COM su interfacce valide; descrittori e rettangoli sono locali che
        // vivono per tutta la durata di ogni chiamata.
        unsafe {
            let enumerator = self.video_device.CreateVideoProcessorEnumerator(&desc)?;
            let processor = self.video_device.CreateVideoProcessor(&enumerator, 0)?;
            vc.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vc.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            vc.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            let out_space = if self.rgb { DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 } else { DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709 };
            vc.VideoProcessorSetOutputColorSpace1(&processor, out_space);
            vc.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&full_src));
            vc.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dest));
            vc.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full_out));
            vc.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            Ok(Processor {
                src_w,
                src_h,
                enumerator,
                processor,
                inputs: Default::default(),
                next_input: 0,
                outputs: Vec::new(),
                copy: None,
            })
        }
    }
}

/// D3D11 dice che il device è perso.
pub(crate) fn device_removed(device: &ID3D11Device) -> bool {
    // SAFETY: chiamata COM senza parametri su un'interfaccia valida.
    unsafe { device.GetDeviceRemovedReason() }.is_err()
}

#[cfg(test)]
mod tests {
    use super::letterbox;

    #[test]
    fn letterbox_keeps_aspect_and_even_coords() {
        let r = letterbox(1920, 1080, 1920, 1080);
        assert_eq!((r.left, r.top, r.right, r.bottom), (0, 0, 1920, 1080));
        // 4:3 in 16:9: bande laterali.
        let r = letterbox(1600, 1200, 1280, 720);
        assert_eq!((r.left, r.top, r.right, r.bottom), (160, 0, 1120, 720));
        // 21:9 in 16:9: bande sopra e sotto.
        let r = letterbox(2560, 1080, 1920, 1080);
        assert_eq!((r.right - r.left, r.bottom - r.top), (1920, 810));
        assert_eq!(r.top % 2, 0);
        assert_eq!(r.top, 134);
    }
}
