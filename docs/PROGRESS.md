# Stato di ClipRec

Ultimo aggiornamento: 20 ottobre 2026, mattina (correzioni dopo la revisione del codice, verifica
indipendente, analisi della RAM). Il riepilogo della notte segue più sotto, aggiornato dove le correzioni lo cambiano.

## Riepilogo per Luca

### Cattura ottimizzata (20 ottobre, sera, DECISIONS 44)

- WGC senza copie: la superficie catturata va direttamente alla conversione (test
  `wgc_frames_are_delivered_without_copy`); una copia BGRA piena in meno per ogni frame arrivato.
- Encoder su un thread proprio con una coda di 6 frame: i picchi di lentezza non fanno più
  perdere tick al pacer.
- Da provare: frame persi in un gioco pesante rispetto alla 0.1.8.

### Limiti noti ridotti (20 ottobre, pomeriggio, DECISIONS 43)

- Frame ripetuti e riempitivi senza conversione né copie sulla GPU (meno frame persi sotto carico).
- Fullscreen esclusivo anche col bersaglio monitor; col segnale di Windows il cambio scatta dopo 1 s
  di frame neri o fermi invece di 3-5.
- Hotkey con un gioco elevato tramite `RegisterHotKey` (solo tastiera).
- «Blocca la modalità esclusiva» sui dispositivi registrati; «Aggiungi i canali del router».
- Da provare dal vivo: un gioco a schermo intero esclusivo (DX9 o con le ottimizzazioni a schermo
  intero disattivate), la hotkey dentro un gioco avviato come amministratore, osu! in modalità
  esclusiva (avviso e poi audio registrato dopo il riavvio del gioco), Sonar con i canali, e i
  frame persi in un gioco pesante rispetto alla 0.1.7.

### Dopo la revisione del codice (20 ottobre, mattina)

La revisione indipendente (`docs/REVIEW-CODICE.md`, 38 falle) è stata corretta da sette subagenti, uno
per crate, in worktree separati, più le correzioni del motore fatte da me. Ogni ramo è stato unito da
solo, con build release, tutti i test e clippy dopo ogni unione. Lo stato di ogni falla, con i commit e
i test di regressione, è nella tabella di `docs/REVIEW-CODICE.md`.

- **Corrette 34 falle su 38**, tutte con un test che falliva prima e passa dopo, tranne m20 e m22
  (motivo nella tabella). Rimandate: m9, m16, m18, m28.
- **Cosa cambia per te:**
  - Risoluzione d'uscita fissa e scalabile: «Nativa» è quella del monitor bersaglio; con uno schermo
    1440p puoi scegliere 1080p o 720p (DECISIONS 28). Una finestra minimizzata all'avvio non produce
    più clip minuscole (C1).
  - Encoder di riserva SVT-AV1 software quando nessun encoder hardware si apre (DECISIONS 29); senza
    video pronto la hotkey video salva almeno la clip audio.
  - Il Redistributable di Visual C++ è nell'installer, e il runtime nella cartella portable (DECISIONS 30).
  - Una config illeggibile non fa più partire la cattura con i predefiniti: buffer fermo, avviso,
    impostazioni aperte; quando salvi una config valida il buffer parte.
  - Salvataggi in coda su un solo thread a bassa priorità, audio scritto in streaming: picco di memoria
    da 44 MB a 131 kB per una clip di 60 s; FLAC fuori dal thread di cattura (0 allocazioni).
  - Nessun campione audio vero scartato con latenze alte; l'audio copre tutta la clip video; il suono
    di salvataggio non finisce più nella traccia «Sistema».
  - Lo stato riporta gli fps effettivi e i codec disponibili; «Prova» dà un esito per il video e per
    ogni traccia. Nuovi formati audio: WAV polifonico, Opus, AAC. Ritardo per sorgente (±1 s).
  - FFmpeg fissata a una build precisa con SHA-256 e licenze complete nella distribuzione.
- **Valutata l'iniezione DLL** per il fullscreen esclusivo: non implementata (anti-cheat, firma,
  manutenzione), dettagli in DECISIONS 27 e in REVIEW-CODICE.md.
- **Verifica indipendente** (subagente opus, contesto pulito): 31 correzioni su 34 confermate. Sui tre
  punti incompleti (M2, m3, m4) e sulle cinque regressioni minori trovate ho fatto un secondo giro con
  test di regressione, descritto in REVIEW-CODICE.md, sezione «Verifica indipendente».
  Commit `d45a591`: salvare una config valida avvia il buffer fermo e l'avvio con Windows resta
  com'era (M2); la clip video non finisce con l'audio muto quando il mixer è in ritardo (m3); la coda
  dei salvataggi sopravvive a un panic; un ritardo negativo non lascia silenzio in coda.
  Clip di prova dopo il secondo giro: `probe` ok, A/V −3,75 ms, 0 ms tra le tracce.
- **RAM esclusi i buffer (m4): ora rispettata.** Prima 96 MB esclusi i buffer (working set privato
  115 MB con 19 MB di buffer) contro una soglia di 50. La scomposizione (sotto, «Scomposizione della
  RAM») mostra che erano i buffer di bitstream di NVENC: 12 superfici da 5,2 MB a 1080p, imposte da
  FFmpeg con 2 B-frame. Senza B-frame su NVENC (DECISIONS 31) sono 4: **29 MB esclusi i buffer** in HEVC
  1080p60, da 24 a 34 MB fra 720p e 1440p e fra 30 e 144 fps. Il prezzo è circa il 15% di bitrate in
  più a qualità costante. SVT-AV1 di riserva da circa 1 GB a 660 MB (`lp=4`). La sessione di 3,5 h è
  stata fatta col binario di prima delle correzioni.

### Cosa funziona

- **Catena completa su `main`**: cattura (WGC o Desktop Duplication) → encoder hardware (NVENC, QSV;
  SVT-AV1 software di riserva) → buffer solo in RAM → hotkey → clip MP4 multitraccia, più la clip solo audio (un FLAC per traccia).
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
- **Distribuzione**: `cargo xtask dist` (con il runtime VC++), installer Inno Setup con il
  Redistributable (da compilare), README, avvisi di licenza completi.
- **Qualità**: 227 test verdi (1 ignorato: la cattura vera del microfono), `cargo clippy --workspace
  --all-targets -- -D warnings` pulito, tre revisioni avversarie in fase 3, poi la revisione
  indipendente con le correzioni e una verifica a contesto pulito.

### Come provarlo

```powershell
cargo xtask ffmpeg-ridotta               # solo la prima volta (~10 min)
cargo xtask ffmpeg                       # solo la prima volta (strumenti di test)
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
7. RAM con cattura vera e un gioco: `cargo xtask mem cliprec-app.exe` con l'app in tray (oggi solo
   sorgenti sintetiche); qualità e bitrate senza B-frame su NVENC con un gioco vero (DECISIONS 31).
8. Dopo le correzioni della revisione:
   - finestra minimizzata come bersaglio all'avvio: la clip deve avere la risoluzione della finestra;
   - uscita a 1080p su uno schermo 1440p (impostazioni → Video → Risoluzione);
   - il «ding» di salvataggio non deve sentirsi nella traccia «Sistema» della clip successiva;
   - un microfono Bluetooth: la clip video non deve finire con l'audio muto;
   - config.toml rotta a mano: avviso, impostazioni aperte, nessuna cattura; salvando dalle
     impostazioni il buffer deve partire, e l'avvio con Windows deve restare com'era;
   - «Prova» nelle impostazioni: esito per video e tracce;
   - installer compilato su un PC senza Visual C++ (deve installare il Redistributable);
   - una GPU senza encoder hardware (o con i driver tolti): ripiego su SVT-AV1.
9. Sessione lunga di nuovo, col binario attuale (`--sessione-lunga 480`).

### Cosa manca

- Firma del codice.
- AMF (GPU AMD) mai provato; UAC, toast e controller scritti ma mai eseguiti dal vivo.
- I messaggi interni dei crate dentro gli avvisi restano in italiano anche con l'interfaccia inglese.
- Rimandate dalla revisione: m9 e R3 (audio preso in esclusiva da un'altra app: nessun segnale
  affidabile, e la sorgente «Sistema» in process loopback, scelta per m7, non lo segnala più),
  m16 (consenso alla cattura di Windows), m18 (HDR, serve uno schermo HDR), m28 (`unsafe` in moduli
  FFI dedicati), icona delle finestre nelle impostazioni (m25). Restano sul thread del motore il nuovo
  tentativo del video e il riavvio dopo un device perso (m1).
- Iniezione DLL per il fullscreen esclusivo: valutata e non fatta (DECISIONS 27).

### Decisioni da confermare

Tutte in `docs/DECISIONS.md` (31 voci; 27-31 dopo la revisione: iniezione DLL, risoluzione d'uscita, SVT-AV1, Redistributable, NVENC senza B-frame per la RAM). Le più importanti: Rust 1.93 invece di 1.99.1 (1); contratti
approvati senza di te (2); `MinUpdateInterval` a 3/4 di periodo (17); pts video = tick, senza
compensazione (18); tick persi riempiti con l'ultimo frame (19); RAM insufficiente → tetto ridotto e
avviso invece della proposta interattiva (20); spegnimento del PC lasciato alla sessione di pokewilds (12);
NVENC senza B-frame, +15% di bitrate per 54 MB di RAM in meno (31).

### Spegnimento del PC: annullato da Luca, il PC resta acceso

- **08:50: Luca ha scritto in questa chat «annullate lo spegnimento, comunicalo anche all'altro
  agente».** Ho inoltrato la richiesta alla sessione «Pokewilds Godot iOS build setup» chiedendo di
  fermare il watcher (niente `shutdown /s` né alla scadenza delle 09:30) e, se partito, di eseguire
  `shutdown /a`. Sul PC non risultano operazioni pianificate di spegnimento.
- **08:51: pokewilds conferma** che il suo watcher è terminato e verificato (nessuna copia rimasta) e
  che nessun conto alla rovescia era partito (`shutdown /a`: «nessun arresto in corso»). Quindi **il PC
  resta acceso**: nessuna delle due sessioni lo spegnerà. In `.coordinamento/` resta solo
  `pokewilds-finito.txt`, un promemoria innocuo (la cartella è ignorata da git).

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

Le righe della sessione continua vengono dal binario di prima della revisione.

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
| RAM del processo esclusi i buffer | < 50 MB | **29 MB** (working set privato 47,8 MB con 18,9 MB di buffer; prima 114,8, cioè 96 esclusi i buffer). Da 24 a 34 MB fra 720p e 1440p e fra 30 e 144 fps; 5 MB in solo audio. Scomposizione sotto |
| Picco di memoria al salvataggio | (nessuna) | 131 kB per una clip di 60 s su 2 tracce (prima 44 MB) |
| Distribuzione | < 40 MB | **38,1 MiB** con la build FFmpeg ridotta (prima 136,9 MiB): DLL FFmpeg 13,5 MiB, exe 21,0 MiB, licenze 3,5 MiB, runtime VC++ 0,1 MiB. Installer 12,6 MiB (prima 45), zip portable 16,2 MiB (prima 61) |

### Scomposizione della RAM (m4)

Working set privato di `cliprec-app.exe --sessione-lunga` (sorgenti sintetiche, RTX 3080, driver
616.56), misurato con `cargo xtask mem` dopo 75 s, a buffer pieno; le pagine bloccate con
`VirtualLock` sono il buffer. «Prima» è il binario di `f568a23`, «dopo» quello con NVENC senza B-frame
(DECISIONS 31).

| Categoria, HEVC 1080p60 (MB) | Prima | Dopo |
| --- | --- | --- |
| Immagini (DLL ed exe, solo le pagine scritte) | 1,0 | 1,0 |
| Heap del processo (primo segmento) | 0,4 | 0,5 |
| Stack dei thread (30) | 0,1 | 0,1 |
| Privata: buffer (bloccata) | 18,9 | 18,9 |
| Privata: bitstream NVENC | 57,5 (12 × 5,2 MB) | 5,2 (4 × 5,2 MB, uno residente) |
| Privata: resto (device D3D11 e driver NVIDIA, sessione NVENC, blocchi grandi degli heap) | 36,7 | 22,1 |
| **Totale** | 114,8 | 47,8 |
| **Esclusi i buffer** | 95,9 | 28,9 |

- Ciò che è di ClipRec (exe, FFmpeg, audio, FLAC, stack, heap) vale circa 5 MB: in solo audio il
  processo sta a 7,4 MB con 2,1 MB di buffer. Il resto è del driver e cresce con la risoluzione.
- Il buffer di bitstream di NVENC vale circa 2,5 byte per pixel (2,4 MB a 720p, 5,2 MB a 1080p,
  9,2 MB a 1440p), uno per superficie. FFmpeg 9 ne impone almeno 4 × (B-frame + 1): l'opzione
  `surfaces` più bassa viene rialzata (l'avviso è sotto il livello di log che teniamo).
- Il commit privato (230 MB dopo, 376 prima) non è RAM occupata: è soprattutto memoria del driver
  allocata e mai toccata (per esempio 3 × 8,9 MB senza pagine residenti).
- Per fase (`cargo test --release -p cliprec-encode --test memory -- --nocapture`, 1080p60, MB
  residenti dopo 120 frame): device D3D11 NVIDIA 8; `hevc_nvenc` 84 con 2 B-frame, 62 con 1, 30 senza;
  `h264_nvenc` 33; `hevc_qsv` sulla UHD 770 170 alla prima apertura, poi il driver Intel tiene una
  cache che si stabilizza (circa 250 MB nel processo, solo quando si cattura sull'Intel); `libsvtav1`
  circa 1 GB, 660 MB con `lp=4`. Dopo la chiusura ogni encoder restituisce tutto (nessuna perdita in 4
  aperture).

| Variante (MB esclusi i buffer) | Prima | Dopo |
| --- | --- | --- |
| HEVC 1080p60 | 95,9 | 28,9 |
| H.264 1080p60 | 70,5 | 26,0 |
| HEVC 1440p60 | 138,4 | 33,5 |
| HEVC 720p60 | 64,9 | 24,4 |
| HEVC 1080p30 | 93,8 | 34,3 |
| HEVC 1080p144 | 81,8 | 27,6 |
| Solo audio | 5,4 | 5,3 |

Nessun frame perso in nessuna variante, prima e dopo. Non misurati: la cattura WGC vera (il suo
frame pool è in VRAM) e AMF (nessuna GPU AMD qui).

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
- **FFmpeg ridotta** (20 ottobre, pomeriggio): `cargo xtask ffmpeg-ridotta` compila con MSYS2
  (scaricata e fissata in `third_party/toolchain`) una FFmpeg 9 LGPL con i soli encoder, muxer e
  hwcontext usati (`installer/FFMPEG-RIDOTTA.md`, DECISIONS 36). DLL da 112 a 13,5 MiB, cartella di
  distribuzione 38,1 MiB. La build BtbN completa resta solo per ffmpeg/ffprobe nei test
  (`FFMPEG_TOOLS_DIR`); i test di mux generano il video software con `ffmpeg.exe` invece che con
  la libreria linkata. Test, clippy, dist, installer e clip di prova (`hevc_nvenc`, probe e sync
  ok, A/V −7 ms) verdi.

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
- `.cargo/config.toml` contiene percorsi assoluti di questo PC (`FFMPEG_DIR`, `FFMPEG_TOOLS_DIR`, `LIBCLANG_PATH`).
