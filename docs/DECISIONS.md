# Decisioni

Scelte prese durante la notte del 20 ottobre 2026 senza conferma di Luca (AVVIO.md, regola 1).
Le voci [DEFAULT] e [PROPOSTA] di SPEC.md valgono come decise; qui ci sono quelle e le altre.

## Adottate da SPEC.md così come sono

- [DEFAULT] FFmpeg da build LGPL condivisa già pronta per partire (BtbN `n9.0-latest-win64-lgpl-shared`,
  FFmpeg 9.0.2), scaricata da `cargo xtask ffmpeg` in `third_party/ffmpeg` (fuori da git).
- [DEFAULT] Impostazioni in processo separato Slint 1.18 con `AboutSlint`.
- [DEFAULT] Sorgente video automatica; HEVC se disponibile; VBR a qualità costante con tetto;
  NVENC P5, lookahead spento, 2 B-frame, keyframe ogni 1 s; MP4 con indice in testa.
- [DEFAULT] Clip video 60 s, clip audio 120 s; hotkey Ctrl+Alt+F10 (video) e Ctrl+Alt+F9 (audio).
- [DEFAULT] Fino a 8 sorgenti audio; modalità tracce separate; AAC-LC 48 kHz 192 kbit/s nelle clip video;
  clip audio un FLAC per traccia in una cartella per clip.
- [PROPOSTA] Opzione «aggiungi in testa una traccia Mix» in modalità separata, spenta per default
  (`audio.add_mix_track`).
- [DEFAULT] Soglie di prestazione della tabella di SPEC.md come criteri di accettazione.

## Prese stanotte

1. **Toolchain Rust 1.93.0 invece di 1.99.1.** Sul PC c'è la 1.93.0; aggiornarla avrebbe toccato
   `~/.rustup`, condiviso con le sessioni di pokewilds attive. Il bug FFI corretto dalla 1.99.1 è
   nato nella 1.99.0, quindi non riguarda la 1.93. Tutte le dipendenze scelte compilano con la 1.93
   (`rust-version = "1.93"`). Da rivedere: aggiornare con `rustup update` quando non girano altre sessioni.
2. **Contratti senza conferma.** SPEC.md chiede di fermarsi dopo la Fase 0 per approvare i contratti;
   AVVIO.md prevale: i contratti in `cliprec-core` sono committati e usati subito dai subagenti.
3. **Binding FFmpeg: `ffmpeg-the-third` 6.0.0+ffmpeg-9.0** (come da SPEC.md), con `bindgen` che usa la
   `libclang` dei Build Tools di Visual Studio 2022 (`LIBCLANG_PATH` in `.cargo/config.toml`).
4. **DLL di FFmpeg accanto agli eseguibili** copiate dai `build.rs` di `cliprec-encode` e `cliprec-mux`,
   invece di modificare `PATH`.
5. **Percorsi assoluti in `.cargo/config.toml`** (`FFMPEG_DIR`, `LIBCLANG_PATH`): i worktree dei
   subagenti stanno dentro il repository e ne ereditano la configurazione. Su un altro PC vanno adattati.
6. **Niente `sccache`.** Installarlo richiede di scrivere in `~/.cargo/bin`, fuori dal repository.
   Ogni worktree ha la sua cartella `target`. Da valutare di giorno.
7. **FLAC nel buffer dentro `cliprec-buffer`.** Il motore audio consegna PCM continuo per traccia
   (`AudioTrackSink`); il buffer comprime in FLAC al `push` (sul thread audio) e decomprime allo
   snapshot. Così il formato del buffer resta interno a un solo crate.
8. **Conversione BGRA→NV12 e scala nell'encoder.** Il trait `VideoEncoder` riceve texture BGRA della
   dimensione della sorgente; il pacer a frequenza costante sta nella sorgente (`VideoSource::start`).
9. **Subagenti avviati come `general-purpose`** con modello, effort e worktree passati alla chiamata:
   i file in `.claude/agents/` creati stanotte non sono caricati dalla sessione già avviata. I file
   restano per le sessioni future, con lo stesso modello ed effort di SPEC.md.
10. **Hook.** `PostToolUse` su `Edit|Write` esegue `cargo check -p <crate>` (`scripts/check-crate.ps1`);
    `SubagentStop` esegue clippy e test dei crate modificati nel worktree (`scripts/gate.ps1 -Hook`) e
    blocca al massimo tre volte, per non creare cicli senza fine di notte.
11. **Nome della named pipe** `\\.\pipe\cliprec-<utente>`, messaggi JSON una riga per messaggio.
12. **Spegnimento del PC: non lo esegue ClipRec.** AVVIO.md, regola 6, vieta di spegnere o riavviare
    il PC. Alle 03:47 la sessione «Pokewilds Godot iOS build setup» ha comunicato che, su indicazione di
    Luca, lo spegnimento forzato lo fa lei: un suo watcher dalle 09:00 controlla
    `.coordinamento/` e lancia `shutdown /s /f /t 60` quando ci sono sia `pokewilds-finito.txt` sia
    `cliprec-finito.txt`, e comunque alle 09:30. ClipRec scrive `cliprec-finito.txt` solo se Luca lo
    conferma direttamente in questa chat; altrimenti vale la scadenza delle 09:30. `.coordinamento/`
    è ignorata da git.
13. **Contratto `AudioTrackSink`: contiguità e salti.** Lo `start` di un blocco può scostarsi di
    qualche ms dalla fine del precedente: il buffer tratta i campioni come contigui. Oltre 100 ms è una
    discontinuità (es. dopo una sospensione): nuovo segmento, silenzio allo snapshot.
14. **Una sola `WasapiSource`** con `AudioSourceKind` (app, ingresso, uscita) al posto di tre tipi
    distinti; `AudioEngine` resta in `cliprec-audio`, non in `cliprec-core`.
15. **Extradata e pacchetti Annex B** da NVENC e QSV; li converte il muxer MP4 di FFmpeg.
    `VideoStreamParams::bit_rate` è il tetto (maxrate).
16. **AMF non verificato**: su questo PC non c'è una GPU AMD (`amfrt64.dll` assente). Le opzioni AMF
    sono scritte ma da provare su una Radeon.
17. **`MinUpdateInterval` a 3/4 di periodo** (minimo 1 ms), non 1/fps: con 1/fps esatto, su questo
    schermo a 60 Hz arrivavano circa 50 frame nuovi al secondo invece di 60 (misura di video-capture).
18. **pts video = istante del tick, `CAPTURE_PTS_OFFSET = 0`**: WGC consegna il frame circa 10 ms prima
    della sua visualizzazione e circa mezzo periodo dopo la produzione; lo scarto è dentro ±20 ms in
    entrambi i casi. La costante in `cliprec-capture/src/source.rs` resta come manopola di taratura.
19. **Tick persi del pacer**: il motore li riempie (fino a 100 ms per tick) ripetendo l'ultimo frame
    codificato, tenuto in una copia GPU (`CopyResource` a ogni frame, nessuna copia su CPU), come i
    frame duplicati di OBS. Oltre, il buco resta nel file come salto di timestamp.
20. **RAM libera insufficiente all'avvio**: invece della proposta interattiva della specifica, il tetto
    del buffer scende a metà della RAM libera e un avviso chiede di ridurre durata o qualità.
21. **Arena video preallocata a 1/4 della stima** e cresciuta a blocchi da 4 MiB (bloccati e
    riciclati) fino al tetto: con HEVC 1080p il buffer scende da 82 a 19 MB.
22. **WAV a 24 bit interi** nelle clip audio (i campioni nel buffer sono già a 24 bit nel FLAC).
23. **Encoder aperto su un thread a parte** al primo frame; i parametri del buffer li imposta solo la
    sessione ancora viva; fallback con `open_after` e backoff da 1 a 30 s; video morto ritentato
    da 5 a 60 s; un device perso o un cambio di GPU ricreano la sessione mantenendo il buffer.
24. **`{game}` nel nome file** = nome dell'eseguibile catturato, ricavato dalla descrizione della
    sorgente; «Desktop» per i monitor, «Audio» in modalità solo audio.
25. **Lingue degli avvisi**: tray, toast, messaggi della shell e avvisi del motore sono in italiano e
    inglese (tabella in `cliprec-app/src/i18n.rs`). Restano in italiano i messaggi interni che
    arrivano dagli altri crate dentro un avviso (stato di una sorgente, errori del muxer).
26. **Test end-to-end del motore in fila** (mutex nei test): in parallelo, sotto carico, l'avvio di
    `ffprobe` falliva a volte con `0xc0000142` (risorse di sistema), non per un difetto di ClipRec.
27. **Iniezione DLL per il fullscreen esclusivo: valutata, non implementata** (20 ottobre, su richiesta
    di Luca). Rischio anti-cheat e costo di manutenzione alti, beneficio limitato perché WGC cattura già la
    maggior parte dei giochi DX11/DX12 «a schermo intero». Eventuale modulo futuro solo sperimentale,
    opzionale e per elenco di giochi. Dettagli in `docs/REVIEW-CODICE.md`, «Valutazione».
28. **Risoluzione d'uscita fissa** (C1): «Nativa» = risoluzione del monitor del bersaglio, non del primo
    frame; 1440p, 1080p e 720p scalano l'immagine (es. 1080p da uno schermo 1440p) con bande nere se le
    proporzioni differiscono. L'encoder non si apre mai sulla dimensione di un segnaposto.
29. **Encoder di riserva software**: SVT-AV1 (BSD, già nelle DLL di FFmpeg) dopo NVENC, AMF e QSV, con
    lettura del frame su CPU solo in quel caso.
30. **VC++ Redistributable nell'installer** (M1, scelta di Luca) invece del CRT statico.
