//! Aiuti per i test: lettura su CPU di texture sintetiche (mai dello schermo dell'utente).

use cliprec_core::GpuDevice;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

pub struct Readback {
    device: GpuDevice,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
}

// SAFETY: solo test; il contesto immediato è protetto per l'uso da più thread.
unsafe impl Send for Readback {}

pub struct Pixels {
    data: Vec<u8>,
    pitch: usize,
}

impl Pixels {
    /// Byte BGRA del pixel (x, y).
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let o = y as usize * self.pitch + x as usize * 4;
        [self.data[o], self.data[o + 1], self.data[o + 2], self.data[o + 3]]
    }
}

impl Readback {
    pub fn new(device: &GpuDevice, width: u32, height: u32) -> Self {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        // SAFETY: descrittore e puntatore d'uscita locali.
        unsafe { device.device.CreateTexture2D(&desc, None, Some(&mut tex)).unwrap() };
        Self { device: device.clone(), staging: tex.unwrap(), width, height }
    }

    /// Copia `tex` (stessa dimensione) e ne legge i pixel.
    pub fn read(&mut self, tex: &ID3D11Texture2D) -> Pixels {
        let ctx = &self.device.context;
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: texture valide della stessa dimensione e formato; la mappatura è letta entro i
        // limiti RowPitch × altezza e chiusa con Unmap prima di tornare.
        unsafe {
            ctx.CopyResource(&self.staging, tex);
            ctx.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut map)).unwrap();
            let pitch = map.RowPitch as usize;
            let len = pitch * self.height as usize;
            let data = std::slice::from_raw_parts(map.pData as *const u8, len).to_vec();
            ctx.Unmap(&self.staging, 0);
            assert!(pitch >= self.width as usize * 4);
            Pixels { data, pitch }
        }
    }
}
