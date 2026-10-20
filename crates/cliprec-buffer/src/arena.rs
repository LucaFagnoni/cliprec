//! Arena di blocchi preallocati e riutilizzati, con una coda di voci (pacchetti o blocchi FLAC).
//!
//! I byte si scrivono in coda all'ultimo blocco; quando non c'è posto si passa a un blocco
//! libero (riciclato) o, solo se non ce ne sono, a uno nuovo. Le voci si scartano dalla testa e i
//! blocchi rimasti senza voci tornano liberi. Nessuna allocazione per voce a regime.
//!
//! Gli snapshot condividono i blocchi con `Arc`, senza copiare: prima di condividere, `seal`
//! chiude il blocco corrente (la scrittura riprende da un blocco nuovo), così il blocco in
//! scrittura resta sempre di proprietà esclusiva e tutto il modulo è senza `unsafe`. Un blocco
//! libero si riusa solo quando nessuno snapshot lo tiene più.

use std::collections::VecDeque;
use std::sync::Arc;

use cliprec_core::PacketData;

struct Chunk(Box<[u8]>);

/// Una fetta di un blocco condivisa con uno snapshot.
struct Slice {
    chunk: Arc<Chunk>,
    start: usize,
    end: usize,
}

impl AsRef<[u8]> for Slice {
    fn as_ref(&self) -> &[u8] {
        self.chunk.0.get(self.start..self.end).unwrap_or_default()
    }
}

pub(crate) struct Entry<M> {
    chunk: u64,
    off: usize,
    len: usize,
    pub meta: M,
}

pub(crate) struct ChunkRing<M> {
    chunk_size: usize,
    lock_pages: bool,
    lock_failed: bool,
    /// Blocchi in uso, dal più vecchio; l'ultimo è quello in scrittura.
    chunks: VecDeque<Arc<Chunk>>,
    /// Numero progressivo di `chunks[0]`.
    first_chunk: u64,
    /// Byte già scritti nell'ultimo blocco.
    fill: usize,
    free: Vec<Arc<Chunk>>,
    entries: VecDeque<Entry<M>>,
    /// Numero progressivo di `entries[0]`.
    first_seq: u64,
    in_use: usize,
    allocated: usize,
    /// Oltre questa memoria totale i blocchi liberati vengono restituiti al sistema.
    pub max_allocated: usize,
}

impl<M> ChunkRing<M> {
    pub fn new(chunk_size: usize, lock_pages: bool, max_allocated: usize) -> Self {
        Self {
            chunk_size,
            lock_pages,
            lock_failed: false,
            chunks: VecDeque::new(),
            first_chunk: 0,
            fill: 0,
            free: Vec::new(),
            entries: VecDeque::new(),
            first_seq: 0,
            in_use: 0,
            allocated: 0,
            max_allocated,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn first_seq(&self) -> u64 {
        self.first_seq
    }

    /// Numero progressivo che avrà la prossima voce.
    pub fn next_seq(&self) -> u64 {
        self.first_seq + self.entries.len() as u64
    }

    pub fn get(&self, seq: u64) -> Option<&Entry<M>> {
        let i = usize::try_from(seq.checked_sub(self.first_seq)?).ok()?;
        self.entries.get(i)
    }

    pub fn front(&self) -> Option<&Entry<M>> {
        self.entries.front()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, &Entry<M>)> {
        (self.first_seq..).zip(self.entries.iter())
    }

    /// Byte dei blocchi in uso (contengono voci vive).
    pub fn in_use_bytes(&self) -> usize {
        self.in_use
    }

    /// Memoria totale dei blocchi, in uso e liberi.
    pub fn allocated_bytes(&self) -> usize {
        self.allocated
    }

    /// `true` se tutti i blocchi allocati sono bloccati in RAM (o il blocco non è richiesto).
    pub fn locked(&self) -> bool {
        self.lock_pages && !self.lock_failed
    }

    /// Byte di blocchi in uso in più che servirebbero per scrivere `len` byte.
    pub fn extra_needed(&self, len: usize) -> usize {
        match self.chunks.back() {
            Some(c) if self.fill + len <= c.0.len() => 0,
            _ => len.max(self.chunk_size),
        }
    }

    /// Prealloca blocchi liberi finché la memoria totale arriva a `bytes`.
    pub fn reserve(&mut self, bytes: usize) {
        while self.allocated + self.chunk_size <= bytes {
            let c = self.alloc(self.chunk_size);
            self.free.push(c);
        }
    }

    fn alloc(&mut self, size: usize) -> Arc<Chunk> {
        let buf = vec![0u8; size].into_boxed_slice();
        if self.lock_pages && !self.lock_failed && !crate::lock::lock(&buf) {
            self.lock_failed = true;
            tracing::warn!("VirtualLock non riuscito: il buffer può finire nel file di paging");
        }
        self.allocated += size;
        Arc::new(Chunk(buf))
    }

    /// Accoda `data` come nuova voce.
    pub fn push(&mut self, data: &[u8], meta: M) {
        let len = data.len();
        if self.extra_needed(len) > 0 || self.chunks.back_mut().and_then(Arc::get_mut).is_none() {
            // Un blocco libero abbastanza grande che nessuno snapshot tiene più, o uno nuovo.
            let reusable = self
                .free
                .iter()
                .position(|c| c.0.len() >= len && Arc::strong_count(c) == 1 && Arc::weak_count(c) == 0);
            let chunk = match reusable {
                Some(i) => self.free.swap_remove(i),
                None => self.alloc(len.max(self.chunk_size)),
            };
            self.in_use += chunk.0.len();
            self.chunks.push_back(chunk);
            self.fill = 0;
        }
        let id = self.first_chunk + self.chunks.len() as u64 - 1;
        let off = self.fill;
        if let Some(dst) = self.chunks.back_mut().and_then(Arc::get_mut).and_then(|c| c.0.get_mut(off..off + len)) {
            dst.copy_from_slice(data);
        }
        self.fill += len;
        self.entries.push_back(Entry { chunk: id, off, len, meta });
    }

    /// Scarta la voce più vecchia e libera i blocchi rimasti vuoti.
    pub fn pop_front(&mut self) -> Option<Entry<M>> {
        let e = self.entries.pop_front()?;
        self.first_seq += 1;
        // Con la coda vuota si libera anche il blocco corrente.
        let keep_from = self.entries.front().map_or(self.first_chunk + self.chunks.len() as u64, |f| f.chunk);
        while self.first_chunk < keep_from {
            let Some(c) = self.chunks.pop_front() else { break };
            self.first_chunk += 1;
            self.in_use -= c.0.len();
            if self.allocated > self.max_allocated {
                self.allocated -= c.0.len();
            } else {
                self.free.push(c);
            }
        }
        if self.chunks.is_empty() {
            self.fill = 0;
        }
        Some(e)
    }

    pub fn clear(&mut self) {
        while self.pop_front().is_some() {}
    }

    pub fn bytes(&self, e: &Entry<M>) -> &[u8] {
        let i = (e.chunk - self.first_chunk) as usize;
        self.chunks.get(i).and_then(|c| c.0.get(e.off..e.off + e.len)).unwrap_or_default()
    }

    /// Chiude il blocco corrente: la prossima voce andrà in un altro blocco. Va chiamata prima
    /// di `share`; lo spazio rimasto nel blocco torna disponibile quando il blocco viene liberato.
    pub fn seal(&mut self) {
        if let Some(c) = self.chunks.back() {
            self.fill = c.0.len();
        }
    }

    /// Riferimento condiviso ai byte di una voce (dopo `seal`), senza copia.
    pub fn share(&self, e: &Entry<M>) -> PacketData {
        let i = (e.chunk - self.first_chunk) as usize;
        match self.chunks.get(i) {
            Some(c) => PacketData(Arc::new(Slice { chunk: Arc::clone(c), start: e.off, end: e.off + e.len })),
            None => PacketData::from(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_chunks_and_shares_closed_ones() {
        let mut r = ChunkRing::new(64, false, usize::MAX);
        for i in 0..100u8 {
            r.push(&[i; 20], i);
            if r.len() > 6 {
                r.pop_front();
            }
        }
        // 6 voci da 20 B in blocchi da 64 B: al massimo 3 in uso + qualcuno libero, mai 100.
        assert!(r.allocated_bytes() <= 5 * 64, "{}", r.allocated_bytes());
        let first = r.front().map(|e| r.bytes(e).to_vec()).unwrap();
        assert_eq!(first, vec![94; 20]);
        // Uno snapshot che tiene un blocco impedisce di riciclarlo.
        r.seal();
        let held = r.share(r.front().unwrap());
        for i in 0..50u8 {
            r.push(&[i; 20], i);
            r.pop_front();
        }
        assert_eq!(&*held, &[94; 20][..]);
    }
}
