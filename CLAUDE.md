# ClipRec

Registratore sempre attivo per Windows: buffer in RAM degli ultimi minuti, due hotkey
(«Clip video» e «Clip audio»), tracce audio separate per sorgente. Specifica completa in
`SPEC.md`, ricerca sullo stack in `docs/RESEARCH.md`, decisioni in `docs/DECISIONS.md`,
stato in `docs/PROGRESS.md`.

## Comandi

```powershell
cargo xtask ffmpeg                                   # una volta: FFmpeg 9 LGPL in third_party/ffmpeg
cargo build --workspace                              # debug
cargo build --workspace --release                    # release (lto fat)
cargo test --workspace                               # test
cargo clippy --workspace --all-targets -- -D warnings
scripts\gate.ps1 -Crate cliprec-xxx                  # clippy + test di un crate
```

`.cargo/config.toml` imposta `FFMPEG_DIR` e `LIBCLANG_PATH` (percorsi assoluti su questo PC).
I `build.rs` di `cliprec-encode` e `cliprec-mux` copiano le DLL di FFmpeg accanto agli eseguibili.

## Crate

| Crate | Contenuto | Proprietario |
| --- | --- | --- |
| `cliprec-core` | contratti: `QpcTime`, `GpuDevice`, `GpuFrame`, `PacketRef`/`EncodedPacket`, trait `VideoSource`/`VideoEncoder`/`AudioSource`/`AudioTrackSink`, snapshot, `Config`, IPC | agente principale |
| `cliprec-capture` | WGC, Desktop Duplication, pacer CFR, sorgente sintetica | video-capture |
| `cliprec-encode` | FFmpeg hw encoder, BGRA→NV12 su GPU | video-encoder |
| `cliprec-audio` | WASAPI (app, ingresso, uscita), silenzio, mix, deriva | audio-engine |
| `cliprec-buffer` | ring video a GOP, ring audio FLAC, VirtualLock, snapshot | replay-buffer |
| `cliprec-mux` | MP4/MKV + AAC, clip audio FLAC/WAV/MKA, `.part` → rinomina | replay-buffer |
| `cliprec-app` | tray, Raw Input, toast, istanza unica, supervisore, motore | app-shell + agente principale |
| `cliprec-settings` | finestra Slint, client named pipe | settings-ui |
| `xtask`, `tests` | FFmpeg, ffprobe, misura sincronia, benchmark, test e2e | test-harness |

## Regole

- `cliprec-core` lo modifica solo l'agente principale: un subagente che ha bisogno di cambiare un
  contratto lo chiede nel rapporto e si ferma.
- Niente `unwrap()`/`expect()` nei percorsi di runtime (clippy li segnala; ammessi nei test).
  Errori con `thiserror` nelle librerie, `anyhow` solo nei binari. L'app degrada e notifica.
- `unsafe` solo nei moduli FFI, ogni blocco con `// SAFETY:`.
- Percorso caldo (cattura → encoder → buffer) senza allocazioni per frame/pacchetto e senza disco.
- Versioni fissate in `Cargo.toml` del workspace e in `Cargo.lock`; nessun aggiornamento senza un
  motivo scritto in `docs/DECISIONS.md`. `windows` resta 0.62.x (non mescolare con 0.100).
- Tutti i timestamp sono QPC in 100 ns (`QpcTime`). Audio interno: f32 stereo 48 kHz.
- Di notte e nei test automatici: niente audio dagli altoparlanti, niente finestre a schermo
  intero, niente toast, niente scritture fuori dal repository (registro, menu Start, avvio
  automatico). Usare sorgenti sintetiche.
- Commit frequenti su `main`, nessun push. I subagenti lavorano in worktree separati
  (`.claude/worktrees/`); l'agente principale unisce i rami uno alla volta e rilancia tutti i test.

## Stato delle fasi

- Fase 0 · Fondamenta: fatta (contratti, workspace, FFmpeg, hook, agenti).
- Fase 1 · Componenti in parallelo: vedi `docs/PROGRESS.md`.
- Fase 2 · Integrazione: vedi `docs/PROGRESS.md`.
- Fase 3 · Robustezza, Fase 4 · Distribuzione: da fare.
