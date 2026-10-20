//! FFI: blocco delle pagine del buffer in RAM (`SetProcessWorkingSetSizeEx` + `VirtualLock`).

use std::sync::Mutex;

use windows::Win32::System::Memory::{
    QUOTA_LIMITS_HARDWS_MAX_DISABLE, QUOTA_LIMITS_HARDWS_MIN_DISABLE, SetProcessWorkingSetSizeEx, VirtualLock,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessWorkingSetSize};

/// Serializza lettura e aumento del working set tra arene diverse.
static WORKING_SET: Mutex<()> = Mutex::new(());

/// Alza il working set minimo di `buf.len()` e blocca le pagine di `buf`. `false` se non riesce.
pub(crate) fn lock(buf: &[u8]) -> bool {
    if buf.is_empty() {
        return true;
    }
    let _guard = WORKING_SET.lock().unwrap_or_else(|e| e.into_inner());
    // Margine per le pagine parziali agli estremi e le strutture del processo.
    let extra = buf.len() + 256 * 1024;
    // SAFETY: pseudo-handle del processo corrente, sempre valido e da non chiudere.
    let process = unsafe { GetCurrentProcess() };
    let (mut min, mut max) = (0usize, 0usize);
    // SAFETY: i puntatori sono variabili locali valide per tutta la chiamata.
    if unsafe { GetProcessWorkingSetSize(process, &mut min, &mut max) }.is_err() {
        return false;
    }
    let (new_min, new_max) = (min.saturating_add(extra), max.saturating_add(extra));
    // SAFETY: handle valido; limiti non rigidi, quindi Windows può comunque scendere sotto il massimo.
    let raised = unsafe {
        SetProcessWorkingSetSizeEx(
            process,
            new_min,
            new_max.max(new_min),
            QUOTA_LIMITS_HARDWS_MIN_DISABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE,
        )
    };
    if raised.is_err() {
        return false;
    }
    // SAFETY: `buf` è memoria allocata e valida per tutta la chiamata; VirtualLock la legge soltanto.
    unsafe { VirtualLock(buf.as_ptr().cast(), buf.len()) }.is_ok()
}
