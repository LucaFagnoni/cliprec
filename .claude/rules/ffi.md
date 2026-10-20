---
paths:
  - "crates/cliprec-capture/**"
  - "crates/cliprec-encode/**"
  - "crates/cliprec-audio/**"
  - "crates/cliprec-core/src/gpu.rs"
---
# FFI e unsafe
- `unsafe` solo in moduli FFI; ogni blocco preceduto da `// SAFETY:` che spiega perché è corretto.
- `windows` 0.62.2 soltanto. Nessun tipo da `windows-core` 0.100.
- Un solo `GpuDevice` per sessione; nessun readback su CPU nel percorso normale.
- Niente allocazioni per frame o pacchetto nel percorso caldo: riusa buffer preallocati.
