//! Il device D3D11 unico condiviso da cattura, conversione ed encoder.

use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1};
use windows::core::Interface;

#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    #[error("nessun adattatore con LUID {0:#x}")]
    AdapterNotFound(i64),
    #[error("creazione del device D3D11 fallita: {0}")]
    Create(#[from] windows::core::Error),
}

/// Device D3D11 hardware con supporto BGRA e video, protetto per l'uso da più thread.
///
/// Un solo `GpuDevice` per sessione di cattura: la sorgente video, la conversione BGRA→NV12
/// e l'encoder lavorano tutti su questo device, così i frame restano texture sulla GPU.
/// Clonarlo copia solo i riferimenti COM.
#[derive(Clone)]
pub struct GpuDevice {
    pub device: ID3D11Device,
    /// Contesto immediato. Protetto da `ID3D11Multithread::SetMultithreadProtected(true)`.
    pub context: ID3D11DeviceContext,
    /// LUID dell'adattatore (`HighPart << 32 | LowPart`).
    pub adapter_luid: i64,
    pub adapter_name: String,
}

// SAFETY: ID3D11Device è free-threaded per specifica D3D11; il contesto immediato è reso sicuro
// tra thread da SetMultithreadProtected(true) impostato in `create`, che serializza ogni chiamata.
unsafe impl Send for GpuDevice {}
// SAFETY: vedi sopra.
unsafe impl Sync for GpuDevice {}

impl std::fmt::Debug for GpuDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuDevice")
            .field("adapter_luid", &format_args!("{:#x}", self.adapter_luid))
            .field("adapter_name", &self.adapter_name)
            .finish()
    }
}

pub fn luid_to_i64(luid: LUID) -> i64 {
    (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart)
}

impl GpuDevice {
    /// Crea il device sull'adattatore con questo LUID, o su quello predefinito se `None`.
    pub fn create(adapter_luid: Option<i64>) -> Result<Self, GpuError> {
        let adapter = match adapter_luid {
            Some(luid) => Some(find_adapter(luid)?),
            None => None,
        };
        let levels: [D3D_FEATURE_LEVEL; 2] = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let driver = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
        let adapter_param: Option<IDXGIAdapter> = match &adapter {
            Some(a) => Some(a.cast()?),
            None => None,
        };
        // SAFETY: tutti i puntatori di uscita sono variabili locali valide; l'elenco dei livelli vive
        // per tutta la chiamata.
        unsafe {
            D3D11CreateDevice(
                adapter_param.as_ref(),
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))?;
        let context = context.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))?;
        let mt: ID3D11Multithread = device.cast()?;
        // SAFETY: chiamata COM su un'interfaccia valida appena ottenuta.
        let _ = unsafe { mt.SetMultithreadProtected(true) };

        let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice = device.cast()?;
        // SAFETY: chiamate COM su interfacce valide.
        let desc = unsafe { dxgi.GetAdapter()?.GetDesc()? };
        let name_len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
        Ok(Self {
            device,
            context,
            adapter_luid: luid_to_i64(desc.AdapterLuid),
            adapter_name: String::from_utf16_lossy(&desc.Description[..name_len]),
        })
    }
}

fn find_adapter(luid: i64) -> Result<IDXGIAdapter1, GpuError> {
    // SAFETY: CreateDXGIFactory1 non ha precondizioni.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    for i in 0.. {
        // SAFETY: chiamate COM su una factory valida; EnumAdapters1 restituisce errore oltre l'ultimo.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else { break };
        // SAFETY: interfaccia valida.
        let desc = unsafe { adapter.GetDesc1()? };
        if luid_to_i64(desc.AdapterLuid) == luid {
            return Ok(adapter);
        }
    }
    Err(GpuError::AdapterNotFound(luid))
}
