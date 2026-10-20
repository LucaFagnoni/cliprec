//! `cargo xtask mem <pid|nome.exe> [--top N]`: scomposizione della memoria di un processo per
//! categoria, come VMMap, con `VirtualQueryEx` e `QueryWorkingSetEx`.
//!
//! Categorie: immagini (per DLL), file mappati, memoria condivisibile senza file (sezioni del
//! driver), heap (allocazioni che partono da un heap di Toolhelp: solo il primo segmento), stack
//! (allocazioni private con una pagina di guardia), privata (il resto, compresi i blocchi grandi
//! degli heap e il buffer). Per ognuna: commit, working set, working set privato (pagine non
//! condivisibili) e pagine bloccate in RAM (`VirtualLock`: il buffer di ClipRec).

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, HEAPLIST32, Heap32ListFirst, Heap32ListNext, PROCESSENTRY32W, Process32FirstW, Process32NextW,
    TH32CS_SNAPHEAPLIST, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_IMAGE, MEM_MAPPED, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD,
    PAGE_READWRITE, PAGE_WRITECOPY, VirtualQueryEx,
};
use windows::Win32::System::ProcessStatus::{
    GetMappedFileNameW, GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2, PSAPI_WORKING_SET_EX_INFORMATION,
    QueryWorkingSetEx,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};

const PAGE: usize = 4096;

/// Byte per categoria.
#[derive(Clone, Copy, Default)]
struct Usage {
    committed: usize,
    /// Commit a carico del processo: memoria privata e pagine scrivibili delle immagini.
    charged: usize,
    ws: usize,
    private_ws: usize,
    locked: usize,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Self) {
        self.committed += o.committed;
        self.charged += o.charged;
        self.ws += o.ws;
        self.private_ws += o.private_ws;
        self.locked += o.locked;
    }
}

/// Handle chiuso alla fine.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: handle aperto da noi e chiuso una volta sola.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub fn run(args: &[String]) -> Result<()> {
    let target = args.first().context("uso: cargo xtask mem <pid|nome.exe> [--top N]")?;
    let top: usize = crate::opt(args, "--top")?.unwrap_or(12);
    let pid = match target.parse::<u32>() {
        Ok(pid) => pid,
        Err(_) => find_pid(target)?,
    };
    // SAFETY: chiamata senza puntatori; l'handle finisce subito in `Handle`.
    let process = Handle(unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) }.context("OpenProcess")?);
    let h = process.0;
    let heaps = heap_ids(pid);

    // Regioni raggruppate per allocazione.
    let mut allocations: BTreeMap<usize, Vec<MEMORY_BASIC_INFORMATION>> = BTreeMap::new();
    let mut addr = 0usize;
    loop {
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: `mbi` è una struttura locale della dimensione passata; l'indirizzo è solo letto.
        let n = unsafe { VirtualQueryEx(h, Some(addr as *const _), &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
        if n == 0 || mbi.RegionSize == 0 {
            break;
        }
        if mbi.State == MEM_COMMIT {
            allocations.entry(mbi.AllocationBase as usize).or_default().push(mbi);
        }
        addr = mbi.BaseAddress as usize + mbi.RegionSize;
    }

    let mut by_category: BTreeMap<&str, Usage> = BTreeMap::new();
    let mut images: BTreeMap<String, Usage> = BTreeMap::new();
    let mut privates: Vec<(usize, Usage, &str)> = Vec::new();
    let (mut stacks, mut heap_count) = (0, 0);
    for (&base, regions) in &allocations {
        let mut u = Usage::default();
        for r in regions {
            let (ws, private_ws, locked) = working_set(h, r.BaseAddress as usize, r.RegionSize)?;
            let writable = r.Protect.0 & (PAGE_READWRITE.0 | PAGE_WRITECOPY.0 | PAGE_EXECUTE_READWRITE.0 | PAGE_EXECUTE_WRITECOPY.0) != 0;
            let charged = if r.Type == MEM_IMAGE && !writable || r.Type == MEM_MAPPED { 0 } else { r.RegionSize };
            u += Usage { committed: r.RegionSize, charged, ws, private_ws, locked };
        }
        let first = regions[0];
        let category = if first.Type == MEM_IMAGE {
            *images.entry(mapped_name(h, base).unwrap_or_else(|| format!("{base:#x}"))).or_default() += u;
            "immagini (DLL, exe)"
        } else if first.Type == MEM_MAPPED {
            if mapped_name(h, base).is_some() { "file mappati" } else { "condivisibile (sezioni)" }
        } else if let Some(&(_, default)) = heaps.iter().find(|(id, _)| *id == base) {
            heap_count += 1;
            if default { "heap del processo" } else { "altri heap" }
        } else if regions.iter().any(|r| r.Protect.0 & PAGE_GUARD.0 != 0) {
            stacks += 1;
            "stack dei thread"
        } else {
            privates.push((base, u, "privata"));
            "privata (VirtualAlloc, blocchi grandi)"
        };
        *by_category.entry(category).or_default() += u;
    }

    let mut c = PROCESS_MEMORY_COUNTERS_EX2 { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32, ..Default::default() };
    // SAFETY: struttura locale della dimensione dichiarata in `cb`; handle valido.
    unsafe { GetProcessMemoryInfo(h, std::ptr::from_mut(&mut c).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb) }.context("GetProcessMemoryInfo")?;
    let mb = |b: usize| b as f64 / 1e6;
    println!(
        "pid {pid}: commit privato {:.1} MB, working set {:.1} MB, working set privato {:.1} MB",
        mb(c.PrivateUsage),
        mb(c.WorkingSetSize),
        mb(c.PrivateWorkingSetSize)
    );
    println!("{heap_count} heap (primo segmento), {stacks} stack");
    println!();
    let row = |name: &str, u: &Usage| {
        println!("{name:<42} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1}", mb(u.committed), mb(u.charged), mb(u.ws), mb(u.private_ws), mb(u.locked));
    };
    println!("{:<42} {:>9} {:>9} {:>9} {:>9} {:>9}", "categoria (MB)", "commit", "a carico", "WS", "WS priv.", "bloccata");
    let mut total = Usage::default();
    for (name, u) in &by_category {
        row(name, u);
        total += *u;
    }
    row("totale", &total);

    println!("\nimmagini per working set privato:");
    let mut images: Vec<_> = images.into_iter().collect();
    images.sort_by_key(|(_, u)| std::cmp::Reverse(u.private_ws));
    for (name, u) in images.iter().take(top) {
        row(name, u);
    }
    println!("\nallocazioni private per working set privato:");
    privates.sort_by_key(|(_, u, _)| std::cmp::Reverse(u.private_ws));
    for (base, u, _) in privates.iter().take(top) {
        row(&format!("{base:#014x}"), u);
    }
    // Allocazioni private della stessa dimensione (a 64 kB): i pool di buffer uguali si vedono qui.
    let mut sizes: BTreeMap<usize, (usize, Usage)> = BTreeMap::new();
    for (_, u, _) in &privates {
        let e = sizes.entry(u.committed.div_ceil(65536) * 65536).or_default();
        e.0 += 1;
        e.1 += *u;
    }
    let mut sizes: Vec<_> = sizes.into_iter().filter(|(_, (n, _))| *n > 1).collect();
    sizes.sort_by_key(|(_, (_, u))| std::cmp::Reverse(u.private_ws));
    println!("\nallocazioni private ripetute (numero × commit):");
    for (size, (n, u)) in sizes.iter().take(top) {
        row(&format!("{n} × {:.2} MB", mb(*size)), u);
    }
    Ok(())
}

/// Pagine residenti, residenti non condivisibili e bloccate nella regione.
fn working_set(h: HANDLE, base: usize, size: usize) -> Result<(usize, usize, usize)> {
    let mut info: Vec<PSAPI_WORKING_SET_EX_INFORMATION> = (0..size / PAGE)
        .map(|i| PSAPI_WORKING_SET_EX_INFORMATION { VirtualAddress: (base + i * PAGE) as *mut _, ..Default::default() })
        .collect();
    let bytes = u32::try_from(std::mem::size_of_val(info.as_slice())).context("regione troppo grande")?;
    // SAFETY: il vettore ha `bytes` byte con gli indirizzi impostati; la funzione scrive gli attributi.
    unsafe { QueryWorkingSetEx(h, info.as_mut_ptr().cast(), bytes) }.context("QueryWorkingSetEx")?;
    let (mut ws, mut private, mut locked) = (0, 0, 0);
    for i in &info {
        // SAFETY: `Flags` copre tutta l'unione; i bit sono quelli di PSAPI_WORKING_SET_EX_BLOCK.
        let flags = unsafe { i.VirtualAttributes.Flags };
        if flags & 1 == 0 {
            continue;
        }
        ws += PAGE;
        if flags & (1 << 15) == 0 {
            private += PAGE;
        }
        if flags & (1 << 22) != 0 {
            locked += PAGE;
        }
    }
    Ok((ws, private, locked))
}

/// Nome del file mappato all'indirizzo (solo il nome, senza percorso del device).
fn mapped_name(h: HANDLE, addr: usize) -> Option<String> {
    let mut buf = [0u16; 1024];
    // SAFETY: buffer locale; la funzione scrive al più `buf.len()` caratteri.
    let n = unsafe { GetMappedFileNameW(h, addr as *const _, &mut buf) } as usize;
    let path = String::from_utf16_lossy(buf.get(..n)?);
    (n > 0).then(|| path.rsplit('\\').next().unwrap_or(&path).to_owned())
}

/// Heap del processo da Toolhelp: (indirizzo, è l'heap predefinito).
fn heap_ids(pid: u32) -> Vec<(usize, bool)> {
    let mut out = Vec::new();
    // SAFETY: snapshot chiuso da `Handle`; la struttura ha `dwSize` impostato come richiesto.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPHEAPLIST, pid) else { return out };
        let snap = Handle(snap);
        let mut hl = HEAPLIST32 { dwSize: std::mem::size_of::<HEAPLIST32>(), ..Default::default() };
        let mut ok = Heap32ListFirst(snap.0, &mut hl).is_ok();
        while ok {
            out.push((hl.th32HeapID, hl.dwFlags & 1 != 0));
            ok = Heap32ListNext(snap.0, &mut hl).is_ok();
        }
    }
    out
}

/// Primo processo con quel nome di eseguibile.
fn find_pid(name: &str) -> Result<u32> {
    // SAFETY: snapshot chiuso da `Handle`; la struttura ha `dwSize` impostato come richiesto.
    unsafe {
        let snap = Handle(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?);
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap.0, &mut e).is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            if String::from_utf16_lossy(&e.szExeFile[..len]).eq_ignore_ascii_case(name) {
                return Ok(e.th32ProcessID);
            }
            ok = Process32NextW(snap.0, &mut e).is_ok();
        }
    }
    bail!("nessun processo {name}")
}
