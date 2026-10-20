//! Avvio e chiusura dei processi, per agganciare subito le sorgenti App e «tutto tranne un'app».
//! Sempre: ricerca periodica ogni [`POLL`]. Facoltativo ([`listen`]): eventi di Windows via WMI
//! (`Win32_ProcessTrace`), che Windows concede solo a un processo avviato come amministratore.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, EOAC_NONE,
    RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Wmi::{
    IEnumWbemClassObject, IWbemLocator, WBEM_E_ACCESS_DENIED, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY,
    WbemLocator,
};
use windows::core::{BSTR, Interface, PCWSTR};

/// Intervallo della ricerca periodica del processo di un'app.
pub const POLL: Duration = Duration::from_millis(500);

/// Testo dell'errore quando Windows nega gli eventi (ClipRec non è amministratore).
pub const NEEDS_ADMIN: &str = "gli eventi di avvio e chiusura delle app richiedono ClipRec avviato come amministratore; \
     resta la ricerca ogni mezzo secondo";

/// Cresce a ogni avvio o chiusura di un processo segnalato da Windows.
static EVENTS: AtomicU64 = AtomicU64::new(0);
static LISTENING: OnceLock<Result<(), String>> = OnceLock::new();

/// Numero di eventi ricevuti finora: chi lo vede cambiare ricontrolla subito i processi.
pub(crate) fn generation() -> u64 {
    EVENTS.load(Ordering::Acquire)
}

/// Inizia, una volta per processo, ad ascoltare gli eventi di Windows. `Err` con un testo per
/// l'utente se Windows li nega: si resta sulla ricerca periodica.
pub fn listen() -> Result<(), String> {
    LISTENING
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("eventi processi".into())
                .spawn(move || run(&tx))
                .map_err(|e| format!("thread degli eventi dei processi: {e}"))?;
            rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| Err("WMI non risponde".into()))
        })
        .clone()
}

fn run(ready: &std::sync::mpsc::Sender<Result<(), String>>) {
    let events = match subscribe() {
        Ok(e) => e,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    loop {
        let mut obj = [None];
        let mut n = 0u32;
        // SAFETY: `obj` ha un posto e `n` è un'uscita valida; timeout di 1 s, il thread vive con il processo.
        let hr = unsafe { events.Next(1000, &mut obj, &mut n) };
        if hr.is_err() {
            tracing::warn!("eventi dei processi interrotti: {hr:?}");
            return;
        }
        if n > 0 {
            EVENTS.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// Iscrizione a `Win32_ProcessTrace` (avvii e chiusure). La negazione arriva alla prima lettura.
fn subscribe() -> Result<IEnumWbemClassObject, String> {
    let fail = |e: windows::core::Error| {
        if e.code() == windows::core::HRESULT(WBEM_E_ACCESS_DENIED.0) || e.code() == windows::Win32::Foundation::E_ACCESSDENIED {
            NEEDS_ADMIN.to_string()
        } else {
            format!("eventi dei processi non disponibili: {e}")
        }
    };
    // SAFETY: COM inizializzato su questo thread (MTA) prima di ogni chiamata; interfacce create qui
    // e usate solo da questo thread; le BSTR vivono per tutta la chiamata.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).map_err(fail)?;
        let services = locator
            .ConnectServer(&BSTR::from("ROOT\\CIMV2"), &BSTR::new(), &BSTR::new(), &BSTR::new(), 0, &BSTR::new(), None)
            .map_err(fail)?;
        let blanket = |p: &windows::core::IUnknown| {
            CoSetProxyBlanket(p, RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE, PCWSTR::null(), RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE, None, EOAC_NONE)
        };
        blanket(&services.cast().map_err(fail)?).map_err(fail)?;
        let events = services
            .ExecNotificationQuery(
                &BSTR::from("WQL"),
                &BSTR::from("SELECT * FROM Win32_ProcessTrace"),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            )
            .map_err(fail)?;
        blanket(&events.cast().map_err(fail)?).map_err(fail)?;
        // La prima lettura (timeout 0) dice se Windows concede davvero gli eventi.
        let mut obj = [None];
        let mut n = 0u32;
        let hr = events.Next(0, &mut obj, &mut n);
        if hr.is_err() {
            return Err(fail(hr.into()));
        }
        if n > 0 {
            EVENTS.fetch_add(1, Ordering::AcqRel);
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::time::Instant;

    /// Da amministratore gli eventi arrivano; altrimenti l'errore lo dice e non blocca nulla.
    #[test]
    fn listen_reports_admin_or_sees_new_processes() {
        match listen() {
            Err(e) => {
                println!("eventi negati: {e}");
                assert!(!e.is_empty());
            }
            Ok(()) => {
                let before = generation();
                let mut child = std::process::Command::new(r"C:\Windows\System32\PING.EXE")
                    .args(["-n", "2", "127.0.0.1"])
                    .stdout(std::process::Stdio::null())
                    .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                    .spawn()
                    .unwrap();
                let t0 = Instant::now();
                while generation() == before && t0.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let _ = child.kill();
                let _ = child.wait();
                assert!(generation() > before, "nessun evento per un processo nuovo");
            }
        }
    }
}
