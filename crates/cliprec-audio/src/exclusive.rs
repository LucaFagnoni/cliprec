//! Modalità esclusiva (modulo FFI). Un'app che prende un dispositivo in esclusiva (WASAPI exclusive)
//! scavalca il mixer di Windows: nessun loopback, process loopback compreso, la sente. L'unico
//! rimedio senza entrare nel processo è non permetterla: ClipRec toglie dai dispositivi che registra
//! il permesso «Consenti alle applicazioni di assumere il controllo esclusivo» (proprietà audio di
//! Windows → Avanzate), che gli utenti possono cambiare senza essere amministratori. L'app allora
//! apre il dispositivo in modalità condivisa, o, se insiste sull'esclusiva, non suona e lo dice.

use cliprec_core::config::{AudioSourceConfig, AudioSourceKind};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole, eRender};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear, PropVariantToStringAlloc, PropVariantToUInt32};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, STGM_READ,
    STGM_READWRITE,
};
use windows::Win32::System::Variant::{VT_EMPTY, VT_UI4};
use windows::core::{GUID, HSTRING};

/// `{b3f8fa53-0004-438e-9003-51a46e139bfc},3`: 1 o assente = l'esclusiva è permessa.
const PKEY_ALLOW_EXCLUSIVE: PROPERTYKEY =
    PROPERTYKEY { fmtid: GUID::from_u128(0xb3f8fa53_0004_438e_9003_51a46e139bfc), pid: 3 };

/// Toglie il permesso di esclusiva dai dispositivi che `sources` registrano: quelli scelti, i
/// predefiniti di «Uscita» e «Ingresso» senza scelta, e l'uscita predefinita per le sorgenti delle
/// app (il process loopback resta muto se l'app la prende in esclusiva). Restituisce i nomi dei
/// dispositivi cambiati; gli errori finiscono nel log.
pub fn forbid_exclusive_mode(sources: &[AudioSourceConfig]) -> Vec<String> {
    // SAFETY: inizializzazione COM del thread corrente, bilanciata sotto se riesce (anche S_FALSE).
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let changed = forbid(sources).unwrap_or_else(|e| {
        tracing::warn!("modalità esclusiva: dispositivi non raggiungibili: {e}");
        Vec::new()
    });
    if init.is_ok() {
        // SAFETY: bilancia il CoInitializeEx riuscito sopra, sullo stesso thread.
        unsafe { CoUninitialize() };
    }
    changed
}

fn forbid(sources: &[AudioSourceConfig]) -> windows::core::Result<Vec<String>> {
    // SAFETY: COM inizializzato dal chiamante; interfaccia creata qui.
    let en: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let mut seen = Vec::new();
    let mut changed = Vec::new();
    for s in sources {
        // SAFETY: chiamate COM su un enumeratore valido; l'ID è una stringa wide valida per la chiamata.
        let device = unsafe {
            match &s.kind {
                AudioSourceKind::Output { device: Some(id) } | AudioSourceKind::Input { device: Some(id) } => {
                    en.GetDevice(&HSTRING::from(id.as_str()))
                }
                AudioSourceKind::Input { device: None } => en.GetDefaultAudioEndpoint(eCapture, eConsole),
                AudioSourceKind::Output { device: None } | AudioSourceKind::App { .. } | AudioSourceKind::Except { .. } => {
                    en.GetDefaultAudioEndpoint(eRender, eConsole)
                }
            }
        };
        let Ok(device) = device else { continue }; // dispositivo assente: la sorgente lo aspetta
        let Some(id) = device_id(&device) else { continue };
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        match forbid_on(&device) {
            Ok(true) => {
                let name = friendly_name(&device).unwrap_or_default();
                tracing::info!(device = %name, "modalità esclusiva disattivata");
                changed.push(name);
            }
            Ok(false) => {}
            Err(e) => tracing::warn!("modalità esclusiva non disattivata su {}: {e}", friendly_name(&device).unwrap_or_default()),
        }
    }
    Ok(changed)
}

/// Esclusiva permessa sul dispositivo (valore assente = permessa, come nelle proprietà di Windows).
fn allowed(device: &IMMDevice) -> windows::core::Result<bool> {
    // SAFETY: dispositivo valido; un VT_UI4 o un VT_EMPTY non hanno memoria da liberare.
    let v = unsafe { device.OpenPropertyStore(STGM_READ)?.GetValue(&PKEY_ALLOW_EXCLUSIVE)? };
    // SAFETY: `v` è una PROPVARIANT valida appena letta; VT_EMPTY (valore assente) varrebbe 0.
    Ok(unsafe { v.Anonymous.Anonymous.vt == VT_EMPTY || !matches!(PropVariantToUInt32(&raw const v), Ok(0)) })
}

/// `Ok(true)` se l'esclusiva era permessa e ora non lo è più.
fn forbid_on(device: &IMMDevice) -> windows::core::Result<bool> {
    if !allowed(device)? {
        return Ok(false);
    }
    // SAFETY: dispositivo valido; chiave e valore vivono per tutta la chiamata. Una PROPVARIANT
    // azzerata con `vt = VT_UI4` vale 0 (REG_DWORD 0, come la casella tolta).
    unsafe {
        let store = device.OpenPropertyStore(STGM_READWRITE)?;
        let mut off = PROPVARIANT::default();
        (*off.Anonymous.Anonymous).vt = VT_UI4;
        store.SetValue(&PKEY_ALLOW_EXCLUSIVE, &off)?;
        store.Commit()?;
    }
    Ok(true)
}

fn device_id(device: &IMMDevice) -> Option<String> {
    // SAFETY: dispositivo valido; la stringa restituita va liberata con CoTaskMemFree.
    unsafe {
        let p = device.GetId().ok()?;
        let id = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        id
    }
}

fn friendly_name(device: &IMMDevice) -> Option<String> {
    // SAFETY: dispositivo valido; la stringa allocata da PropVariantToStringAlloc si libera con CoTaskMemFree.
    unsafe {
        let mut v = device.OpenPropertyStore(STGM_READ).ok()?.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let p = PropVariantToStringAlloc(&raw const v);
        let _ = PropVariantClear(&raw mut v);
        let p = p.ok()?;
        let name = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Solo lettura (nessuna scrittura fuori dal repository nei test): ogni dispositivo attivo si
    /// apre in lettura e scrittura da utente normale e il permesso di esclusiva si legge.
    #[test]
    fn exclusive_permission_is_readable_and_store_is_writable() {
        use windows::Win32::Media::Audio::{DEVICE_STATE_ACTIVE, eAll};
        // SAFETY: test; COM inizializzato e bilanciato qui.
        unsafe {
            let init = CoInitializeEx(None, COINIT_MULTITHREADED);
            {
                let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
                let all = en.EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE).unwrap();
                for i in 0..all.GetCount().unwrap() {
                    let d = all.Item(i).unwrap();
                    let name = friendly_name(&d).unwrap_or_default();
                    let allowed = allowed(&d).unwrap();
                    let writable = d.OpenPropertyStore(STGM_READWRITE).is_ok();
                    println!("{name}: esclusiva {}, scrivibile {writable}", if allowed { "permessa" } else { "vietata" });
                    assert!(writable, "{name}: proprietà non scrivibili da utente normale");
                }
            }
            if init.is_ok() {
                CoUninitialize();
            }
        }
    }
}
