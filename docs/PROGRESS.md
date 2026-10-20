# Stato di ClipRec

Ultimo aggiornamento: 20 ottobre 2026, 05:10 (bozza del riepilogo; versione finale alle 08:40).

## Riepilogo per Luca

### Cosa funziona

- **Catena completa su `main`**: cattura (WGC o Desktop Duplication) → encoder hardware (NVENC, QSV)
  → buffer solo in RAM → hotkey → clip MP4 multitraccia, più la clip solo audio (un FLAC per traccia).
  Verificata da test end-to-end con sorgenti sintetiche (ffprobe e sincronia lampo/bip) e dal vivo
  sull'app completa con la finestra di `cargo xtask testwindow`.
- **Audio**: tracce separate con i nomi, traccia unica con mix e limiter, opzione «Mix» in testa;
  app con process loopback, microfono, loopback d'uscita; silenzio continuo; deriva corretta.
- **App**: tray in italiano e inglese con stato, ultime clip, avvia/ferma; hotkey Raw Input
  (Ctrl+Alt+F10 video, Ctrl+Alt+F9 audio, anche tasti del mouse); istanza unica; named pipe per la
  finestra impostazioni; primo avvio con i predefiniti; robustezza (video che non parte → solo audio,
  encoder che fallisce → successivo, device perso → riavvio con il buffer intatto).
- **Impostazioni**: finestra Slint a sei schede, stima della RAM dal vivo, pulsante «Prova».
- **Distribuzione**: `cargo xtask dist`, installer Inno Setup (da compilare), README, avvisi di licenza.

### Come provarlo

```powershell
cargo xtask ffmpeg                       # solo la prima volta
cargo build --release
# 1. Prova senza toccare nulla: clip video+audio da sorgenti sintetiche, poi validazione
target\release\cliprec-app.exe --prova 10 --sintetica --cartella D:\clip-prova
cargo xtask probe "D:\clip-prova\Desktop_<data>.mp4" --duration 9 --fps 60
cargo xtask sync  "D:\clip-prova\Desktop_<data>.mp4"
# 2. Uso vero: avvia l'app (icona nella tray), gioca in finestra senza bordi, premi Ctrl+Alt+F10
target\release\cliprec-app.exe
# 3. Impostazioni (anche dal menu della tray)
target\release\cliprec-settings.exe
# 4. Comandi sulla pipe
scripts\pipe.ps1 Status TestClip
```

Le clip vanno in `%USERPROFILE%\Videos\ClipRec`; config in `%APPDATA%\ClipRec\config.toml`; log in
`%LOCALAPPDATA%\ClipRec\logs` (con i tempi di snapshot e scrittura di ogni clip).

### Cosa provare di giorno (non si poteva di notte)

1. Hotkey vere con tastiera e mouse, dentro un gioco in finestra senza bordi.
2. Toast «Clip salvata» con i pulsanti Apri e Cartella, e il suono.
3. Sincronia A/V con cattura reale: `cargo xtask testwindow` più un bip che suona dagli altoparlanti,
   catturato in loopback; `cargo xtask sync` sulla clip. Se lo scarto è sistematico, regolare
   `CAPTURE_PTS_OFFSET` in `crates/cliprec-capture/src/source.rs`.
4. Monitor a 144 Hz o più (stanotte lo schermo era a 60 Hz) e un gioco vero con PresentMon
   (`scripts\bench\presentmon.ps1`) per il calo di fps.
5. Microfono, Discord e un gioco come sorgenti audio; scollegare le cuffie durante la registrazione.
6. Un controller con `video_pad = "Back+A"` in config.toml, e le lingue della tray (`language = "en"`).
7. RAM con VMMap (la memoria privata alta sembra del driver NVENC).

### Cosa manca

- Build FFmpeg ridotta (oggi la distribuzione pesa 130 MB contro i 40 della specifica; piano in
  `installer/FFMPEG-RIDOTTA.md`), firma del codice, installer compilato.
- AMF (GPU AMD) mai provato; «riavvia come amministratore» per i giochi elevati solo segnalato.
- Controller XInput (facoltativo): fatto (`hotkeys.video_pad`/`audio_pad`, es. «Back+A»), mai provato
  con un controller vero.

### Decisioni da confermare

Tutte in `docs/DECISIONS.md`. Le più importanti: Rust 1.93 invece di 1.99.1 (voce 1); contratti
approvati senza di te (voce 2); `MinUpdateInterval` a 3/4 di periodo (17); pts video = tick, senza
compensazione (18); tick persi riempiti con l'ultimo frame (19); RAM insufficiente → tetto ridotto e
avviso invece della proposta interattiva (20); spegnimento del PC lasciato alla sessione di pokewilds (12).

## Misure (release, RTX 3080, sorgenti sintetiche 1080p60 HEVC)

| Metrica | Soglia | Misurato |
| --- | --- | --- |
| Avvio a freddo → encoder pronto | < 1,5 s | 0,32 s |
| Hotkey → snapshot | < 20 ms | 0,7 ms (video), 0,14 ms (audio) |
| Hotkey → file, clip di 60 s | < 1 s | 0,69 s (con la clip audio scritta in parallelo) |
| Durata della clip | ≥ N, ≤ N + 1 GOP | 60,77 s per N = 60 |
| Sincronia tra tracce | ±20 ms | 0 ms |
| Sincronia A/V (sintetica) | ±20 ms | da −3 a −16 ms (al più un frame, artefatto della misura); −3 ms con 21 tick persi prima dei lampi |
| CPU del processo (sessione lunga) | < 2% | 4,4% di un core = 0,16% della CPU |
| RAM del processo esclusi i buffer | < 50 MB | working set circa 16 MB (98 MB totali con 82 MB di buffer); memoria privata impegnata 336 MB con buffer di 19 MB, quasi tutta del driver NVENC/CUDA (da confermare con VMMap) |
| Distribuzione | < 40 MB | 129,9 MB oggi (FFmpeg completa); 26-30 MB stimati con la build ridotta |

## Fatto, in ordine

- **Fase 0** (03:30-03:40): workspace Cargo (9 crate + `xtask` + `tests`), contratti in `cliprec-core`,
  FFmpeg 9.0.2 LGPL in `third_party/ffmpeg`, `CLAUDE.md`, agenti, regole, hook.
- **Fase 1** (03:41-04:06): sette subagenti in worktree separati, tutti uniti su `main`.
  - `cliprec-capture`: WGC su monitor e finestre, Desktop Duplication di ripiego, pacer CFR a triplo
    buffer, modalità automatica, frame neri o fermi segnalati, `SyntheticSource` con lampo a ogni secondo.
  - `cliprec-encode`: `hevc_nvenc`/`h264_nvenc` zero-copy da texture D3D11, `hevc_qsv`/`h264_qsv` sulla
    UHD 770, BGRA→NV12 BT.709 con bande nere, `open_best`/`open_after`/`probe`. AMF scritto, non provato.
  - `cliprec-audio`: `WasapiSource` (app con process loopback, ingresso, uscita), silenzio continuo,
    mix con deriva corretta (0,03 ms su un'ora simulata), limiter, MMCSS, riconnessione, `BeepSource`.
  - `cliprec-buffer`: arena di blocchi riciclati e bloccati in RAM, scarto a GOP interi, FLAC per
    traccia, snapshot senza copie, `AudioRefs` (decompressione fuori dal lock).
  - `cliprec-mux`: MP4/MKV con AAC per traccia e nomi, prima traccia predefinita, `+faststart`,
    `.part` univoco rinominato, nomi mai sovrascritti, controllo spazio; clip audio FLAC/WAV o MKA.
  - `cliprec-app`: tray, hotkey Raw Input, istanza unica, supervisore, named pipe, toast, log rotante.
  - `cliprec-settings`: finestra Slint a sei schede con `AboutSlint`, stima RAM, client pipe, it/en.
  - `xtask`/`tests`: `probe`, `sync`, `gen-test-clip`, `bench-report`, `testwindow`, script PresentMon/WPR.
- **Fase 2** (04:10): motore collegato; catena verticale verificata da un test end-to-end.
- **Fase 3** (04:20-05:10): due revisioni avversarie (25 e 16 voci) corrette dai proprietari dei crate;
  test di iniezione dei guasti (video che non parte, cartella non scrivibile, due clip nello stesso
  secondo, tick persi prima del lampo); modalità traccia unica e Mix provate end-to-end; primo avvio;
  lingue italiano e inglese nella tray; avviso della seconda istanza.
- **Fase 4, parte fattibile** (04:40): `cargo xtask dist` e `notices`, installer Inno Setup (non
  compilato), `README.md`, `THIRD_PARTY_NOTICES.md`, piano per la build FFmpeg ridotta.

## Provato dal vivo stanotte (senza tastiera né schermo dell'utente)

- App completa (tray, motore, pipe) sulla finestra di `cargo xtask testwindow` con WGC: `Status`,
  `TestClip` (clip di prova da 5 s, nome `xtask_…`), `ReloadConfig` (fps 60 → 30: encoder riaperto,
  clip a 30 fps, risoluzione nativa della finestra 642×392), `Quit` (uscita con codice 0).
- Un cambio di fps, codec o risoluzione svuota il buffer video (pacchetti con parametri diversi non
  stanno nella stessa clip); l'audio resta.

## In corso

- Sessione lunga dalle 04:18 alle 07:48 circa (`target/sessione-lunga/sessione-lunga.csv`): al minuto
  36 memoria privata 408,7 MB (+2 MB dal minuto 10), 2 frame persi (all'avvio), una clip ogni 10 minuti.
- Revisione finale della notte (sola lettura) sulle modifiche dopo la seconda revisione.

## Problemi aperti

- Coordinamento con pokewilds (03:47): le sessioni di pokewilds finiscono entro le 09:00 e nessuna deve
  restare attiva dopo. La sessione «Pokewilds Godot iOS build setup» dice che Luca le ha chiesto lo
  spegnimento forzato e se ne occupa lei (watcher su `.coordinamento/`, `shutdown /s /f /t 60` quando
  entrambe le sessioni hanno scritto il file di fine, o comunque alle 09:30). ClipRec non spegne il PC
  (AVVIO.md, regola 6) e scrive `.coordinamento/cliprec-finito.txt` solo con la conferma diretta di
  Luca. Vedi DECISIONS.md, voce 12.
- Worktree dei subagenti in `.claude/worktrees/` (circa 18 GB di `target`): i rami sono tutti uniti su
  `main`; si possono rimuovere con `git worktree remove --force --force <percorso>`.
