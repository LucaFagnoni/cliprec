//! Puntatore del mouse per Desktop Duplication (che non lo include nell'immagine).
//!
//! La forma da `GetFramePointerShape` (colore, colore con maschera, monocromatica) viene convertita
//! una volta per forma in due texture: «normale» (BGRA con alfa) e «inversione» (bianco dove il
//! puntatore inverte lo schermo, nero altrove). Sul frame si disegnano due quad: il primo con alfa,
//! il secondo con `INV_DEST_COLOR / INV_SRC_COLOR`, che dà `1 − dst` sotto il bianco e lascia
//! intatto il resto. Lo XOR con un colore qualsiasi è approssimato con l'inversione (i puntatori di
//! sistema usano solo nero, bianco e inversione).
//!
//! Lo stato della pipeline è isolato: `SwapDeviceContextState` sotto il lock di `ID3D11Multithread`,
//! così chi usa il contesto immediato (encoder) ritrova il suo stato e nessuno si infila a metà.

use cliprec_core::GpuDevice;
use windows::Win32::Foundation::{E_POINTER, POINT};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_BLEND_DESC, D3D11_BLEND_INV_DEST_COLOR, D3D11_BLEND_INV_SRC_ALPHA,
    D3D11_BLEND_INV_SRC_COLOR, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA, D3D11_BLEND_ZERO,
    D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_CULL_NONE, D3D11_FILL_SOLID, D3D11_FILTER_MIN_MAG_MIP_POINT,
    D3D11_RASTERIZER_DESC, D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_IMMUTABLE,
    D3D11_VIEWPORT, ID3D11BlendState, ID3D11Device1, ID3D11DeviceContext1, ID3D11Multithread, ID3D11PixelShader,
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, ID3DDeviceContextState,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, IDXGIOutputDuplication,
};
use windows::core::{Interface, s};

use crate::pacer::create_rtv;

const SHADERS: &str = r"
struct O { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
O vs(uint id : SV_VertexID) {
    float2 t = float2(id & 1, id >> 1);
    O o;
    o.pos = float4(t.x * 2 - 1, 1 - t.y * 2, 0, 1);
    o.uv = t;
    return o;
}
Texture2D tex : register(t0);
SamplerState smp : register(s0);
float4 ps(O i) : SV_Target { return tex.Sample(smp, i.uv); }
";

/// Forma del puntatore convertita in due immagini BGRA della stessa dimensione.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct ShapePixels {
    pub width: u32,
    pub height: u32,
    /// BGRA con alfa non premoltiplicato.
    pub normal: Vec<u8>,
    /// BGRA: bianco opaco dove lo schermo va invertito, nero altrove.
    pub invert: Vec<u8>,
    pub has_invert: bool,
}

/// Converte la forma restituita da `GetFramePointerShape`. `false` se i dati non bastano.
pub(crate) fn convert_shape(info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, data: &[u8], out: &mut ShapePixels) -> bool {
    let (w, pitch) = (info.Width as usize, info.Pitch as usize);
    let h = if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
        info.Height as usize / 2
    } else {
        info.Height as usize
    };
    let needed = if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 { 2 * h * pitch } else { h * pitch };
    if w == 0 || h == 0 || data.len() < needed {
        return false;
    }
    out.width = w as u32;
    out.height = h as u32;
    out.normal.clear();
    out.normal.resize(w * h * 4, 0);
    out.invert.clear();
    out.invert.resize(w * h * 4, 0);
    out.has_invert = false;
    for y in 0..h {
        for x in 0..w {
            let o = (y * w + x) * 4;
            let (normal, invert): ([u8; 4], bool) = match info.Type {
                t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 => {
                    let bit = |row: usize| data[row * pitch + x / 8] & (0x80 >> (x % 8)) != 0;
                    match (bit(y), bit(y + h)) {
                        (false, false) => ([0, 0, 0, 255], false),
                        (false, true) => ([255, 255, 255, 255], false),
                        (true, false) => ([0, 0, 0, 0], false),
                        (true, true) => ([0, 0, 0, 0], true),
                    }
                }
                t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 => {
                    let p = &data[y * pitch + x * 4..y * pitch + x * 4 + 4];
                    if p[3] == 0 {
                        ([p[0], p[1], p[2], 255], false) // colore sostituito
                    } else {
                        ([0, 0, 0, 0], p[..3] != [0, 0, 0]) // XOR: nero = invariato
                    }
                }
                t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 => {
                    let p = &data[y * pitch + x * 4..y * pitch + x * 4 + 4];
                    ([p[0], p[1], p[2], p[3]], false)
                }
                _ => return false,
            };
            out.normal[o..o + 4].copy_from_slice(&normal);
            if invert {
                out.invert[o..o + 4].copy_from_slice(&[255, 255, 255, 255]);
                out.has_invert = true;
            }
        }
    }
    true
}

struct ShapeTextures {
    width: u32,
    height: u32,
    normal: ID3D11ShaderResourceView,
    invert: Option<ID3D11ShaderResourceView>,
}

/// Disegna il puntatore sui frame di una duplicazione.
pub(crate) struct CursorOverlay {
    device: GpuDevice,
    ctx: ID3D11DeviceContext1,
    lock: ID3D11Multithread,
    /// Stato della pipeline del puntatore (shader, sampler, rasterizer già impostati).
    state: ID3DDeviceContextState,
    alpha: ID3D11BlendState,
    inverse: ID3D11BlendState,
    shape: Option<ShapeTextures>,
    pixels: ShapePixels,
    raw: Vec<u8>,
    /// Viste sulle texture degli slot (al più 4): niente oggetti nuovi a ogni frame.
    rtvs: Vec<(ID3D11Texture2D, ID3D11RenderTargetView)>,
    position: POINT,
    visible: bool,
}

// SAFETY: oggetti D3D11 free-threaded; il contesto immediato è protetto e le sequenze di chiamate
// sono racchiuse in ID3D11Multithread::Enter/Leave. Usato da un solo thread alla volta (DDA).
unsafe impl Send for CursorOverlay {}

fn compile(entry: windows::core::PCSTR, target: windows::core::PCSTR) -> windows::core::Result<ID3DBlob> {
    let mut code = None;
    // SAFETY: sorgente e nomi validi per tutta la chiamata; puntatore d'uscita locale.
    unsafe {
        D3DCompile(
            SHADERS.as_ptr().cast(),
            SHADERS.len(),
            s!("cursor"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            None,
        )?
    };
    code.ok_or_else(|| E_POINTER.into())
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: il blob possiede il buffer per tutta la sua vita; dimensione dichiarata dal blob.
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()) }
}

fn blend(src: windows::Win32::Graphics::Direct3D11::D3D11_BLEND, dst: windows::Win32::Graphics::Direct3D11::D3D11_BLEND) -> D3D11_BLEND_DESC {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: src,
        DestBlend: dst,
        BlendOp: D3D11_BLEND_OP_ADD,
        // L'alfa del frame resta quello del desktop.
        SrcBlendAlpha: D3D11_BLEND_ZERO,
        DestBlendAlpha: D3D11_BLEND_ONE,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    desc
}

impl CursorOverlay {
    pub fn new(device: &GpuDevice) -> windows::core::Result<Self> {
        let d1: ID3D11Device1 = device.device.cast()?;
        let ctx: ID3D11DeviceContext1 = device.context.cast()?;
        let lock: ID3D11Multithread = device.context.cast()?;
        let (vs_code, ps_code) = (compile(s!("vs"), s!("vs_4_0"))?, compile(s!("ps"), s!("ps_4_0"))?);
        let (mut vs, mut ps) = (None::<ID3D11VertexShader>, None::<ID3D11PixelShader>);
        let (mut alpha, mut inverse, mut sampler, mut raster, mut state) = (None, None, None, None, None);
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let raster_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: true.into(),
            ..Default::default()
        };
        // SAFETY: descrittori e bytecode locali validi per tutta la chiamata; puntatori d'uscita locali.
        unsafe {
            device.device.CreateVertexShader(blob_bytes(&vs_code), None, Some(&mut vs))?;
            device.device.CreatePixelShader(blob_bytes(&ps_code), None, Some(&mut ps))?;
            device.device.CreateBlendState(&blend(D3D11_BLEND_SRC_ALPHA, D3D11_BLEND_INV_SRC_ALPHA), Some(&mut alpha))?;
            device.device.CreateBlendState(&blend(D3D11_BLEND_INV_DEST_COLOR, D3D11_BLEND_INV_SRC_COLOR), Some(&mut inverse))?;
            device.device.CreateSamplerState(&sampler_desc, Some(&mut sampler))?;
            device.device.CreateRasterizerState(&raster_desc, Some(&mut raster))?;
            d1.CreateDeviceContextState(
                0,
                &[device.device.GetFeatureLevel()],
                D3D11_SDK_VERSION,
                &ID3D11Device1::IID,
                None,
                Some(&mut state),
            )?;
        }
        let state: ID3DDeviceContextState = state.ok_or(windows::core::Error::from(E_POINTER))?;
        // SAFETY: sequenza sotto il lock del device; lo stato precedente viene rimesso subito.
        unsafe {
            lock.Enter();
            let mut previous = None;
            ctx.SwapDeviceContextState(&state, Some(&mut previous));
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.VSSetShader(vs.as_ref(), None);
            ctx.PSSetShader(ps.as_ref(), None);
            ctx.PSSetSamplers(0, Some(&[sampler]));
            ctx.RSSetState(raster.as_ref());
            if let Some(p) = &previous {
                ctx.SwapDeviceContextState(p, None);
            }
            lock.Leave();
        }
        Ok(Self {
            device: device.clone(),
            ctx,
            lock,
            state,
            alpha: alpha.ok_or(windows::core::Error::from(E_POINTER))?,
            inverse: inverse.ok_or(windows::core::Error::from(E_POINTER))?,
            shape: None,
            pixels: ShapePixels::default(),
            raw: Vec::new(),
            rtvs: Vec::new(),
            position: POINT::default(),
            visible: false,
        })
    }

    /// Aggiorna posizione e forma dalle informazioni del frame appena acquisito.
    pub fn update(&mut self, dup: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) -> windows::core::Result<()> {
        if info.LastMouseUpdateTime != 0 {
            self.visible = info.PointerPosition.Visible.as_bool();
            self.position = info.PointerPosition.Position;
        }
        if info.PointerShapeBufferSize > 0 {
            // Alloca solo quando la forma cresce (cambio di puntatore), mai a ogni frame.
            self.raw.resize(info.PointerShapeBufferSize as usize, 0);
            let (mut required, mut shape) = (0u32, DXGI_OUTDUPL_POINTER_SHAPE_INFO::default());
            // SAFETY: buffer di `raw.len()` byte; puntatori d'uscita locali.
            unsafe {
                dup.GetFramePointerShape(
                    self.raw.len() as u32,
                    self.raw.as_mut_ptr().cast(),
                    &mut required,
                    &mut shape,
                )?
            };
            let len = (required as usize).min(self.raw.len());
            self.set_shape(&shape, len)?;
        }
        Ok(())
    }

    fn set_shape(&mut self, info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, len: usize) -> windows::core::Result<()> {
        if !convert_shape(info, &self.raw[..len], &mut self.pixels) {
            self.shape = None;
            return Ok(());
        }
        let p = &self.pixels;
        let normal = self.texture(&p.normal, p.width, p.height)?;
        let invert = if p.has_invert { Some(self.texture(&p.invert, p.width, p.height)?) } else { None };
        self.shape = Some(ShapeTextures { width: p.width, height: p.height, normal, invert });
        Ok(())
    }

    fn texture(&self, bgra: &[u8], width: u32, height: u32) -> windows::core::Result<ID3D11ShaderResourceView> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let data = D3D11_SUBRESOURCE_DATA { pSysMem: bgra.as_ptr().cast(), SysMemPitch: width * 4, SysMemSlicePitch: 0 };
        let (mut tex, mut srv) = (None, None);
        // SAFETY: `bgra` contiene width×height pixel da 4 byte e vive per tutta la chiamata.
        unsafe {
            self.device.device.CreateTexture2D(&desc, Some(&data), Some(&mut tex))?;
            let tex: ID3D11Texture2D = tex.ok_or(windows::core::Error::from(E_POINTER))?;
            self.device.device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
        }
        srv.ok_or_else(|| E_POINTER.into())
    }

    fn rtv(&mut self, target: &ID3D11Texture2D) -> windows::core::Result<ID3D11RenderTargetView> {
        if let Some((_, v)) = self.rtvs.iter().find(|(t, _)| t == target) {
            return Ok(v.clone());
        }
        let v = create_rtv(&self.device, target)?;
        if self.rtvs.len() >= 4 {
            self.rtvs.remove(0);
        }
        self.rtvs.push((target.clone(), v.clone()));
        Ok(v)
    }

    /// Disegna il puntatore su `target` (`width`×`height`), se visibile.
    pub fn draw(&mut self, target: &ID3D11Texture2D, width: u32, height: u32) -> windows::core::Result<()> {
        let Some(shape) = &self.shape else { return Ok(()) };
        if !self.visible {
            return Ok(());
        }
        let (x, y) = (self.position.x, self.position.y);
        let (sw, sh) = (shape.width as i32, shape.height as i32);
        if x >= width as i32 || y >= height as i32 || x + sw <= 0 || y + sh <= 0 {
            return Ok(());
        }
        let (normal, invert) = (shape.normal.clone(), shape.invert.clone());
        let viewport = D3D11_VIEWPORT {
            TopLeftX: x as f32,
            TopLeftY: y as f32,
            Width: sw as f32,
            Height: sh as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let rtv = self.rtv(target)?;
        // SAFETY: sequenza sotto il lock del device; lo stato di chi usa il contesto immediato viene
        // salvato e rimesso da SwapDeviceContextState; viste e stati sono vivi.
        unsafe {
            self.lock.Enter();
            let mut previous = None;
            self.ctx.SwapDeviceContextState(&self.state, Some(&mut previous));
            self.ctx.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            self.ctx.RSSetViewports(Some(&[viewport]));
            self.ctx.PSSetShaderResources(0, Some(&[Some(normal)]));
            self.ctx.OMSetBlendState(&self.alpha, None, u32::MAX);
            self.ctx.Draw(4, 0);
            if let Some(inv) = invert {
                self.ctx.PSSetShaderResources(0, Some(&[Some(inv)]));
                self.ctx.OMSetBlendState(&self.inverse, None, u32::MAX);
                self.ctx.Draw(4, 0);
            }
            // Niente riferimenti al frame nello stato del puntatore.
            self.ctx.OMSetRenderTargets(None, None);
            self.ctx.PSSetShaderResources(0, Some(&[None]));
            if let Some(p) = &previous {
                self.ctx.SwapDeviceContextState(p, None);
            }
            self.lock.Leave();
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn set_test_shape(&mut self, info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, data: &[u8], at: POINT) {
        self.raw = data.to_vec();
        self.set_shape(info, data.len()).unwrap();
        self.position = at;
        self.visible = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pacer::{clear_black, create_bgra};
    use crate::testutil::Readback;

    fn mono_info(w: u32, h: u32) -> DXGI_OUTDUPL_POINTER_SHAPE_INFO {
        DXGI_OUTDUPL_POINTER_SHAPE_INFO {
            Type: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32,
            Width: w,
            Height: 2 * h,
            Pitch: 1,
            ..Default::default()
        }
    }

    /// Monocromatico 4×1: (AND, XOR) = (0,0) nero, (0,1) bianco, (1,0) trasparente, (1,1) inverti.
    const MONO: [u8; 2] = [0b0011_0000, 0b0101_0000];

    #[test]
    fn monochrome_and_masked_shapes_convert() {
        let mut out = ShapePixels::default();
        assert!(convert_shape(&mono_info(4, 1), &MONO, &mut out));
        assert_eq!((out.width, out.height, out.has_invert), (4, 1, true));
        assert_eq!(&out.normal[..], &[0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&out.invert[12..16], &[255, 255, 255, 255]);
        assert_eq!(&out.invert[..12], &[0; 12]);

        let masked = DXGI_OUTDUPL_POINTER_SHAPE_INFO {
            Type: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32,
            Width: 2,
            Height: 1,
            Pitch: 8,
            ..Default::default()
        };
        // Pixel 0: colore rosso sostituito (alfa 0); pixel 1: XOR con bianco (alfa 0xFF).
        assert!(convert_shape(&masked, &[0, 0, 255, 0, 255, 255, 255, 255], &mut out));
        assert_eq!(&out.normal[..], &[0, 0, 255, 255, 0, 0, 0, 0]);
        assert_eq!(&out.invert[4..], &[255, 255, 255, 255]);

        assert!(!convert_shape(&mono_info(4, 1), &MONO[..1], &mut out), "dati troppo corti");
    }

    /// m17: i tre tipi di forma disegnati su uno sfondo grigio sintetico (mai lo schermo).
    #[test]
    fn cursor_is_blended_on_the_frame() {
        let device = GpuDevice::create(None).unwrap();
        let frame = create_bgra(&device, 32, 32).unwrap();
        let rtv = create_rtv(&device, &frame).unwrap();
        let mut overlay = CursorOverlay::new(&device).unwrap();
        let mut rb = Readback::new(&device, 32, 32);
        let gray = |o: &mut CursorOverlay, f: &ID3D11Texture2D| {
            // SAFETY: vista valida; contesto protetto.
            unsafe { device.context.ClearRenderTargetView(&rtv, &[0.5, 0.5, 0.5, 1.0]) };
            o.draw(f, 32, 32).unwrap();
        };

        // Monocromatico in (4, 4).
        overlay.set_test_shape(&mono_info(4, 1), &MONO, POINT { x: 4, y: 4 });
        gray(&mut overlay, &frame);
        let px = rb.read(&frame);
        let g = px.pixel(0, 0)[0];
        assert!((127..=128).contains(&g), "sfondo {g}");
        assert_eq!(px.pixel(4, 4)[..3], [0, 0, 0], "nero");
        assert_eq!(px.pixel(5, 4)[..3], [255, 255, 255], "bianco");
        assert_eq!(px.pixel(6, 4)[..3], [g, g, g], "trasparente");
        let inv = px.pixel(7, 4)[0];
        assert!(inv.abs_diff(255 - g) <= 1, "inversione {inv} su {g}");
        assert_eq!(px.pixel(4, 5)[..3], [g, g, g], "fuori dal puntatore");

        // Colore con alfa al 50% in (30, 30): metà fuori dal frame, nessun errore.
        let color = DXGI_OUTDUPL_POINTER_SHAPE_INFO {
            Type: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32,
            Width: 4,
            Height: 1,
            Pitch: 16,
            ..Default::default()
        };
        let red_half = [0, 0, 255, 128].repeat(4);
        overlay.set_test_shape(&color, &red_half, POINT { x: 30, y: 30 });
        gray(&mut overlay, &frame);
        let px = rb.read(&frame);
        let [b, _, r, _] = px.pixel(31, 30);
        assert!(r > 180 && b < 80, "rosso al 50% su grigio: r {r} b {b}");

        // Stato dell'encoder intatto: lo sfondo si pulisce ancora con la sua vista.
        clear_black(&device, &frame).unwrap();
        assert_eq!(rb.read(&frame).pixel(31, 30)[..3], [0, 0, 0]);
    }
}
