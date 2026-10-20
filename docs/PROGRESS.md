# Stato di ClipRec

Ultimo aggiornamento: 20 ottobre 2026, 08:40 (riepilogo finale della notte).

## Riepilogo per Luca

### Cosa funziona

- **Catena completa su `main`**: cattura (WGC o Desktop Duplication) → encoder hardware (NVENC, QSV)
  → buffer solo in RAM → hotkey → clip MP4 multitraccia, più la clip solo audio (un FLAC per traccia).
  Verificata da test end-to-end con sorgenti sintetiche (ffprobe e sincronia lampo/bip), dal vivo
  sull'app completa con la finestra di `cargo xtask testwindow`, e da una sessione di 3,5 ore.
- **Audio**: tracce separate con i nomi, traccia unica con mix e limiter, opzione «Mix» in testa;
  app con process loopback, microfono, loopback d'uscita; silenzio continuo; deriva corretta.
- **App**: tray in italiano e inglese con stato, ultime clip, avvia/ferma, riavvio come
  amministratore; hotkey Raw Input (Ctrl+Alt+F10 video, Ctrl+Alt+F9 audio, anche tasti del mouse) e,
  facoltativo, controller XInput; istanza unica; named pipe per le impostazioni; primo avvio con i
  predefiniti; robustezza (video che non parte → solo audio e nuovi tentativi, encoder che fallisce →
  successivo, device perso o cambio di GPU → riavvio con il buffer intatto, `.part` mai scambiati per
  clip valide).
- **Impostazioni**: finestra Slint a sei schede, stima della RAM dal vivo, pulsante «Prova».
- **Distribuzione**: `cargo xtask dist`, installer Inno Setup (da compilare), README, avvisi di licenza.
- **Qualità**: 155 test verdi (1 ignorato: la cattura vera del microfono), `cargo clippy --workspace
  --all-targets -- -D warnings` pulito, tre revisioni avversarie con le correzioni.

### Come provarlo

```powershell
cargo xtask ffmpeg                       # solo la prima volta
cargo build --release
# 1. Tutte le prove della notte in un colpo (clip sintetica + app completa sulla finestra di test)
scripts\prova-completa.ps1
# 2. Prova manuale con sorgenti sintetiche, poi validazione
target\release\cliprec-app.exe --prova 10 --sintetica --cartella D:\clip-prova
cargo xtask probe "D:\clip-prova\Desktop_<data>.mp4" --duration 9 --fps 60
cargo xtask sync  "D:\clip-prova\Desktop_<data>.mp4"
# 3. Uso vero: avvia l'app (icona nella tray), gioca in finestra senza bordi, premi Ctrl+Alt+F10
target\release\cliprec-app.exe
# 4. Impostazioni (anche dal menu della tray) e comandi sulla pipe
target\release\cliprec-settings.exe
scripts\pipe.ps1 Status TestClip
```

Le clip vanno in `%USERPROFILE%\Videos\ClipRec`; config in `%APPDATA%\ClipRec\config.toml`; log in
`%LOCALAPPDATA%\ClipRec\logs` (con i tempi di snapshot e scrittura di ogni clip). Al primo avvio
l'app scrive la config predefinita e apre le impostazioni.

### Cosa provare di giorno (non si poteva di notte)

1. Hotkey vere con tastiera e mouse, dentro un gioco in finestra senza bordi.
2. Toast «Clip salvata» con i pulsanti Apri e Cartella, e il suono.
3. Sincronia A/V con cattura reale: `cargo xtask testwindow` più un bip che suona dagli altoparlanti,
   catturato in loopback; `cargo xtask sync` sulla clip. Se lo scarto è sistematico, regolare
   `CAPTURE_PTS_OFFSET` in `crates/cliprec-capture/src/source.rs`.
4. Monitor a 144 Hz o più (stanotte lo schermo era a 60 Hz) e un gioco vero con PresentMon
   (`scripts\bench\presentmon.ps1`) per il calo di fps.
5. Microfono, Discord e un gioco come sorgenti audio; scollegare le cuffie durante la registrazione.
6. Un controller con `video_pad = "Back+A"` in config.toml, le lingue (`language = "en"`) e
   «Riavvia come amministratore» con un gioco elevato (apre il dialogo UAC).
7. RAM con VMMap (la memoria privata alta sembra del driver NVENC).

### Cosa manca

- Build FFmpeg ridotta (oggi la distribuzione pesa 130 MB contro i 40 della specifica; piano in
  `installer/FFMPEG-RIDOTTA.md`), firma del codice, installer compilato.
- AMF (GPU AMD) mai provato; UAC, toast e controller scritti ma mai eseguiti dal vivo.
- I messaggi interni dei crate dentro gli avvisi restano in italiano anche con l'interfaccia inglese.

### Decisioni da confermare

Tutte in `docs/DECISIONS.md` (26 voci). Le più importanti: Rust 1.93 invece di 1.99.1 (1); contratti
approvati senza di te (2); `MinUpdateInterval` a 3/4 di periodo (17); pts video = tick, senza
compensazione (18); tick persi riempiti con l'ultimo frame (19); RAM insufficiente → tetto ridotto e
avviso invece della proposta interattiva (20); spegnimento del PC lasciato alla sessione di pokewilds (12).

### Spegnimento del PC: annullato da Luca, il PC resta acceso

- **08:50: Luca ha scritto in questa chat «annullate lo spegnimento, comunicalo anche all'altro
  agente».** Ho inoltrato la richiesta alla sessione «Pokewilds Godot iOS build setup» chiedendo di
  fermare il watcher (niente `shutdown /s` né alla scadenza delle 09:30) e, se partito, di eseguire
  `shutdown /a`. Sul PC non risultano operazioni pianificate di spegnimento.

- ClipRec non spegne e non riavvia il PC: lo vieta AVVIO.md, regola 6, e la decisione spetta a te.
- La sessione «Pokewilds Godot iOS build setup» ha scritto (03:47) che le hai chiesto lo spegnimento
  forzato e che se ne occupa lei: un suo watcher su `.coordinamento/` lancia `shutdown /s /f /t 60`
  quando trova sia `pokewilds-finito.txt` sia `cliprec-finito.txt`, e comunque alle 09:30.
- `cliprec-finito.txt` **non l'ho scritto**: lo avrei fatto solo con una tua conferma diretta in questa
  chat, che non è arrivata. Da parte di ClipRec quindi il PC resta acceso; se il watcher di pokewilds
  è attivo, lo spegnerà alle 09:30. Alle 08:40 in `.coordinamento/` non c'era ancora nessun file.
- Alle 08:40 ClipRec non ha processi in esecuzione e tutto il lavoro è committato su `main`; nessun
  lavoro nuovo dopo quell'ora, stop alle 09:00.

## Misure (release, RTX 3080, sorgenti sintetiche 1080p60 HEVC)

| Metrica | Soglia | Misurato |
| --- | --- | --- |
| Avvio a freddo → encoder pronto | < 1,5 s | 0,32 s |
| Hotkey → snapshot | < 20 ms | 0,7 ms (video), 0,14 ms (audio) |
| Hotkey → file, clip di 60 s | < 1 s | 0,69 s (con la clip audio scritta in parallelo) |
| Durata della clip | ≥ N, ≤ N + 1 GOP | 60,77 s per N = 60; 21 clip su 21 valide nella sessione lunga |
| Sincronia tra tracce | ±20 ms | 0 ms (anche dopo 3,5 ore) |
| Sincronia A/V (sintetica) | ±20 ms | da −3 a −16 ms (al più un frame, artefatto della misura); −11 ms dopo 3,5 ore |
| Sessione continua | 8 h senza crescita né frame persi sistematici | 3,5 h: memoria privata 400 → 411 MB, piatta dal minuto 90; buffer costante 84,9 MB; 2 frame persi, entrambi all'avvio |
| CPU del processo | < 2% | 4,4% di un core = 0,16% della CPU |
| RAM del processo esclusi i buffer | < 50 MB | working set circa 16 MB; memoria privata impegnata alta, quasi tutta del driver NVENC (da confermare con VMMap) |
| Distribuzione | < 40 MB | 130 MB oggi (FFmpeg completa, 4 DLL su 7); 26-30 MB stimati con la build ridotta |

## Fatto, in ordine

- **Fase 0** (03:30-03:40): workspace Cargo (9 crate + `xtask` + `tests`), contratti in `cliprec-core`,
  FFmpeg 9.0.2 LGPL in `third_party/ffmpeg`, `CLAUDE.md`, agenti, regole, hook.
- **Fase 1** (03:41-04:06): sette subagenti in worktree separati, tutti uniti su `main`.
  - `cliprec-capture`: WGC su monitor e finestre, Desktop Duplication di ripiego, pacer CFR a triplo
    buffer, modalità automatica, frame neri o fermi segnalati, `SyntheticSource` con lampo a ogni secondo.
  - `cliprec-encode`: `hevc_nvenc`/`h264_nvenc` zero-copy da texture D3D11, `hevc_qsv`/`h264_qsv` sulla
    UHD 770, BGRA→NV12 BT.709 con bande nere, keyframe solo IDR, `open_best`/`open_after`/`probe`.
  - `cliprec-audio`: `WasapiSource` (app con process loopback, ingresso, uscita), silenzio continuo,
    mix con deriva corretta (0,03 ms su un'ora simulata), limiter, MMCSS, riconnessione, `BeepSource`.
  - `cliprec-buffer`: arena di blocchi riciclati e bloccati in RAM, scarto a GOP interi, FLAC per
    traccia, snapshot senza copie, `AudioRefs` (decompressione fuori dal lock).
  - `cliprec-mux`: MP4/MKV con AAC per traccia e nomi, prima traccia predefinita, `+faststart`,
    `.part` univoco rinominato senza sovrascrivere, percorsi lunghi, pulizia dei soli file di ClipRec.
  - `cliprec-app`: tray, hotkey Raw Input, controller XInput, istanza unica, supervisore, named pipe,
    toast, log rotante, lingue.
  - `cliprec-settings`: finestra Slint a sei schede con `AboutSlint`, stima RAM, client pipe, it/en.
  - `xtask`/`tests`: `probe`, `sync`, `gen-test-clip`, `bench-report`, `testwindow`, `dist`,
    `notices`, script PresentMon/WPR.
- **Fase 2** (04:10): motore collegato; catena verticale verificata da un test end-to-end.
- **Fase 3** (04:20-05:45): tre revisioni avversarie (25, 16 e 8 voci, l'ultima senza falle gravi)
  corrette dai proprietari dei crate; test di iniezione dei guasti (video che non parte, cartella non
  scrivibile, due clip nello stesso secondo, tick persi prima del lampo); modalità traccia unica e Mix
  end-to-end; primo avvio; lingue; seconda istanza; riavvio come amministratore; controller XInput.
- **Fase 4, parte fattibile** (04:40): `cargo xtask dist` e `notices`, installer Inno Setup (non
  compilato), `README.md`, `THIRD_PARTY_NOTICES.md`, piano per la build FFmpeg ridotta. La cartella
  di distribuzione è stata provata da sola (`--prova` con le sole 4 DLL incluse).
- **Sessione lunga** (04:18-07:48): vedi la tabella delle misure; CSV in
  `target/sessione-lunga/sessione-lunga.csv`.

## Provato dal vivo stanotte (senza tastiera né schermo dell'utente)

- App completa (tray, motore, pipe) sulla finestra di `cargo xtask testwindow` con WGC: `Status`,
  `TestClip` (clip di prova da 5 s, nome `xtask_…`), `ReloadConfig` (fps 60 → 30: encoder riaperto,
  clip a 30 fps, risoluzione nativa della finestra 642×392), `Quit` (uscita con codice 0).
- Un cambio di fps, codec o risoluzione svuota il buffer video (pacchetti con parametri diversi non
  stanno nella stessa clip); l'audio resta.

## Problemi aperti

- **Incidente alle 05:41 (colpa dello script di prova, non di ClipRec).** La prima versione di
  `scripts\prova-completa.ps1` scriveva la config di prova con `Set-Content -Encoding utf8NoBOM`, che
  in Windows PowerShell 5.1 non esiste: l'app è partita con i predefiniti, ha catturato **il monitor
  principale** per circa 10 s e ha salvato una clip in `C:\Users\Luca\Videos\ClipRec`. Ho spostato
  quel file (non cancellato) in `target\catturato-per-errore\Desktop_2026-10-20_05-41-28.mp4` e
  rimosso la cartella `Videos\ClipRec`, vuota e creata dal test. Cancellalo tu se non ti serve. Lo
  script ora scrive la config con .NET, si ferma se non la trova e chiude l'app senza salvare se la
  sorgente non è la finestra di test.
- Worktree dei subagenti in `.claude/worktrees/` (circa 18 GB di `target`): i rami sono tutti uniti su
  `main`; si possono rimuovere con `git worktree remove --force --force <percorso>`.
- `.cargo/config.toml` contiene percorsi assoluti di questo PC (`FFMPEG_DIR`, `LIBCLANG_PATH`).
