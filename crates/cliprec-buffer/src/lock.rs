//! FFI: blocco delle pagine del buffer in RAM (`SetProcessWorkingSetSizeEx` + `VirtualLock`).
//!
//! Ogni blocco alza il working set minimo e massimo della sua dimensione più un margine;
//! `unlock` sblocca le pagine e toglie esattamente lo stesso aumento, così dopo molte ricariche
//! del buffer il working set non cresce e il blocco continua a riuscire.

use std::sync::Mutex;

use windows::Win32::System::Memory::{
    QUOTA_LIMITS_HARDWS_MAX_DISABLE, QUOTA_LIMITS_HARDWS_MIN_DISABLE, SetProcessWorkingSetSizeEx, VirtualLock,
    VirtualUnlock,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessWorkingSetSize};

/// Serializza lettura e modifica del working set tra arene diverse.
static WORKING_SET: Mutex<()> = Mutex::new(());

/// Margine per le pagine parziali agli estremi del blocco.
const MARGIN: usize = 2 * 4096;

/// Working set (minimo, massimo) del processo.
pub(crate) fn working_set() -> Option<(usize, usize)> {
    let (mut min, mut max) = (0usize, 0usize);
    // SAFETY: pseudo-handle del processo corrente; i puntatori sono variabili locali valide.
    unsafe { GetProcessWorkingSetSize(GetCurrentProcess(), &mut min, &mut max) }.ok()?;
    Some((min, max))
}

/// Sposta minimo e massimo del working set di `delta` byte (negativo = riduce).
fn shift_working_set(delta: isize) -> bool {
    let Some((min, max)) = working_set() else { return false };
    let (new_min, new_max) = (min.saturating_add_signed(delta), max.saturating_add_signed(delta));
    // SAFETY: pseudo-handle valido; limiti non rigidi, Windows può comunque scendere sotto il massimo.
    unsafe {
        SetProcessWorkingSetSizeEx(
            GetCurrentProcess(),
            new_min,
            new_max.max(new_min),
            QUOTA_LIMITS_HARDWS_MIN_DISABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE,
        )
    }
    .is_ok()
}

fn extra(buf: &[u8]) -> isize {
    isize::try_from(buf.len() + MARGIN).unwrap_or(isize::MAX)
}

/// Alza il working set e blocca le pagine di `buf`. `false` (senza effetti) se non riesce.
pub(crate) fn lock(buf: &[u8]) -> bool {
    if buf.is_empty() {
        return false;
    }
    let _guard = WORKING_SET.lock().unwrap_or_else(|e| e.into_inner());
    if !shift_working_set(extra(buf)) {
        return false;
    }
    // SAFETY: `buf` è memoria allocata e valida per tutta la chiamata; VirtualLock non la modifica.
    let locked = unsafe { VirtualLock(buf.as_ptr().cast(), buf.len()) }.is_ok();
    if !locked {
        shift_working_set(-extra(buf));
    }
    locked
}

/// Sblocca le pagine di `buf` (bloccate con [`lock`]) e restituisce l'aumento del working set.
pub(crate) fn unlock(buf: &[u8]) {
    let _guard = WORKING_SET.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: `buf` è ancora allocata (chiamata dal Drop del proprietario, prima del rilascio).
    let _ = unsafe { VirtualUnlock(buf.as_ptr().cast(), buf.len()) };
    shift_working_set(-extra(buf));
}
