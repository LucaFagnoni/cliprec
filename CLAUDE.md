# ClipRec

Registratore sempre attivo per Windows: buffer in RAM degli ultimi minuti, due hotkey
(«Clip video» e «Clip audio»), tracce audio separate per sorgente. Specifica completa in
`SPEC.md`, ricerca sullo stack in `docs/RESEARCH.md`, decisioni in `docs/DECISIONS.md`,
stato in `docs/PROGRESS.md`.

## Comandi

```powershell
cargo xtask ffmpeg-ridotta                           # una volta: FFmpeg 9 LGPL ridotta (MSYS2) in third_party/ffmpeg-ridotta
cargo xtask ffmpeg                                   # una volta: FFmpeg completa (ffmpeg/ffprobe per i test) in third_party/ffmpeg
cargo build --workspace                              # debug
cargo build --workspace --release                    # release (lto fat)
cargo test --workspace                               # test
cargo clippy --workspace --all-targets -- -D warnings
scripts\gate.ps1 -Crate cliprec-xxx                  # clippy + test di un crate
cargo xtask probe <clip> [--duration 60 --tracks 2 --names "A,B" --fps 60]   # validazione ffprobe
cargo xtask sync <clip>                              # sincronia lampo/bip (sorgenti sintetiche)
cargo xtask dist                                     # cartella di distribuzione in target/dist
& "$env:LOCALAPPDATA\Programs\Inno Setup 7\ISCC.exe" /Qp installer\cliprec.iss   # installer in target/dist
python -I scripts/icone.py                           # icone (PNG, ICO, .res, tray) da assets/cliprec.png
target\release\cliprec-app.exe --prova 10 --sintetica --cartella <dir>   # clip video+audio di prova
target\release\cliprec-app.exe --sessione-lunga <minuti> <dir>          # CSV di memoria e frame (config da CLIPREC_CONFIG)
cargo xtask mem <pid|cliprec-app.exe>                # memoria per categoria (immagini, heap, stack, privata, bloccata)
cargo test --release -p cliprec-encode --test memory -- --nocapture   # RAM residente per encoder e fase
scripts\pipe.ps1 Status TestClip Quit                # comandi al core sulla named pipe
```

Variabili per prove e notte: `CLIPREC_CONFIG` (percorso di config.toml), `CLIPREC_LOG_DIR`,
`CLIPREC_HEADLESS=1` (niente toast, suoni, registro, menu Start).

`.cargo/config.toml` imposta `FFMPEG_DIR` (build ridotta, linkata e distribuita), `FFMPEG_TOOLS_DIR`
(build completa, solo ffmpeg/ffprobe per test e `xtask probe`/`sync`) e `LIBCLANG_PATH` (percorsi assoluti su questo PC).
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

- Fase 0 · Fondamenta: fatta.
- Fase 1 · Componenti: fatta, tutti i crate uniti su `main`.
- Fase 2 · Integrazione: catena verticale fatta (`crates/cliprec-app/src/engine.rs`, test end-to-end).
- Fase 3 · Robustezza: tre revisioni avversarie corrette; test di guasto; sessione di 3,5 h senza
  crescita di memoria. Da fare di giorno: misura A/V con cattura reale, hotkey dal vivo, toast,
  controller, UAC, AMF, monitor oltre 60 Hz, gioco vero con PresentMon.
- Fase 4 · Distribuzione: `xtask dist`, installer Inno Setup 7 compilato e provato (installa, clip, disinstalla), zip portable, README, build
  FFmpeg ridotta (`installer/FFMPEG-RIDOTTA.md`): cartella 38,1 MiB, installer 12,6 MiB. Da fare: firma del codice.
- Prove ripetibili senza schermo dell'utente: `scripts\prova-completa.ps1`.
- Dettagli e misure in `docs/PROGRESS.md`.
