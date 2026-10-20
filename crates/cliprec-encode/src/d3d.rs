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
    D3D11_BIND_RENDER_TARGET, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2DARRAY, ID3D11Device, ID3D11DeviceContext,
    ID3D11Texture2D, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_NV12,
    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

/// Viste d'ingresso tenute in cache: il pool di cattura ruota poche texture.
// ponytail: cache a rotazione di 4 viste; una texture fuori cache costa una vista nuova per frame,
// alzare il numero se la sorgente usa pool più grandi.
const INPUT_VIEWS: usize = 4;

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

/// Video processor costruito per una dimensione d'ingresso, con le sue viste.
struct Processor {
    src_w: u32,
    src_h: u32,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// (puntatore della texture, vista). La vista tiene un riferimento alla texture, quindi il
    /// puntatore non può essere riusato da un'altra texture finché è in cache.
    inputs: [Option<(usize, ID3D11VideoProcessorInputView)>; INPUT_VIEWS],
    next_input: usize,
    /// (puntatore della texture del pool, indice nell'array, vista).
    outputs: Vec<(usize, usize, ID3D11VideoProcessorOutputView)>,
}

pub(crate) struct Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Solo QSV: texture NV12 render target intermedia, copiata poi nella fetta del pool.
    staging: Option<ID3D11Texture2D>,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    out_w: u32,
    out_h: u32,
    fps: u32,
    proc: Option<Processor>,
}

impl Converter {
    pub(crate) fn new(gpu: &GpuDevice, out_w: u32, out_h: u32, fps: u32, via_staging: bool) -> windows::core::Result<Self> {
        let staging = if via_staging {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: out_w,
                Height: out_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            };
            let mut tex = None;
            // SAFETY: descrittore locale valido per la chiamata; puntatore d'uscita locale.
            unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
            Some(tex.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))?)
        } else {
            None
        };
        Ok(Self {
            device: gpu.device.clone(),
            context: gpu.context.clone(),
            staging,
            video_device: gpu.device.cast()?,
            video_context: gpu.context.cast()?,
            out_w,
            out_h,
            fps,
            proc: None,
        })
    }

    /// `true` se il device è stato rimosso (driver aggiornato o reset, TDR).
    pub(crate) fn device_removed(&self) -> bool {
        // SAFETY: chiamata COM senza parametri su un'interfaccia valida.
        unsafe { self.device.GetDeviceRemovedReason() }.is_err()
    }

    /// Accoda la conversione di `src` (BGRA, `src_w`×`src_h`) nella fetta `slice` della texture
    /// NV12 `dst` (puntatore `ID3D11Texture2D*` preso da `AVFrame.data[0]`).
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
        let input = match p.inputs.iter().flatten().find(|(k, _)| *k == src_key) {
            Some((_, v)) => v.clone(),
            None => {
                let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                    },
                };
                let mut view = None;
                // SAFETY: texture ed enumeratore validi sullo stesso device; desc vive per la chiamata.
                unsafe { self.video_device.CreateVideoProcessorInputView(src, &p.enumerator, &desc, Some(&mut view))? };
                let view = view.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))?;
                p.inputs[p.next_input] = Some((src_key, view.clone()));
                p.next_input = (p.next_input + 1) % INPUT_VIEWS;
                view
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
        if let Some(staging) = &self.staging {
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
            vc.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
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
            })
        }
    }
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
