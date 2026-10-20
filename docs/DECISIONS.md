# Decisioni

Scelte prese durante la notte del 20 ottobre 2026 senza conferma di Luca (AVVIO.md, regola 1).
Le voci [DEFAULT] e [PROPOSTA] di SPEC.md valgono come decise; qui ci sono quelle e le altre.

## Adottate da SPEC.md così come sono

- [DEFAULT] FFmpeg da build LGPL condivisa già pronta per partire (BtbN `n9.0-latest-win64-lgpl-shared`,
  FFmpeg 9.0.2), scaricata da `cargo xtask ffmpeg` in `third_party/ffmpeg` (fuori da git).
- [DEFAULT] Impostazioni in processo separato Slint 1.18 con `AboutSlint`.
- [DEFAULT] Sorgente video automatica; HEVC se disponibile; VBR a qualità costante con tetto;
  NVENC P5, lookahead spento, keyframe ogni 1 s; MP4 con indice in testa. I 2 B-frame valgono per
  QSV e AMF H.264, non più per NVENC (voce 31).
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
31. **NVENC senza B-frame, SVT-AV1 con `lp=4`** (m4, 20 ottobre). La scomposizione della memoria
    (`cargo xtask mem`, `cliprec-encode/tests/memory.rs`, PROGRESS «Scomposizione della RAM») mostra
    che oltre ai buffer la RAM era quasi tutta dell'encoder: FFmpeg 9 tiene almeno 4 × (B + 1)
    superfici NVENC (un valore di `surfaces` più basso viene rialzato) e per ognuna il driver tiene
    residente in RAM un buffer di bitstream di circa 2,5 byte per pixel. Con 2 B-frame erano 12 × 5,2 MB
    a 1080p e 12 × 9,2 MB a 1440p. Senza B-frame le superfici sono 4: `hevc_nvenc` a 1080p passa da
    84 a 30 MB residenti e l'app da 96 a 29 MB esclusi i buffer (soglia 50). Il prezzo, misurato su una
    registrazione di gioco 1080p con le opzioni e il tetto di ClipRec (CQ 23): +15% di bitrate in HEVC
    (10,5 contro 9,1 Mbit/s, SSIM 0,9898 contro 0,9907) e +7% in H.264, quindi un buffer un po' più
    grande a pari durata (a 60 s circa +10 MB, contro −54 MB dell'encoder). Con 1 B-frame l'encoder
    tiene 62 MB e la soglia non si rispetta. Per tornare indietro basta una riga in
    `cliprec-encode/src/lib.rs`. Due test NVENC cambiano soglia, con i valori misurati nel codice: sul
    rumore puro la media su 3 s arriva a 1,36 volte il tetto (1,30 con i B-frame; limite teorico del
    VBV 1,33), tollerato fino a 1,40; sul campo piatto `h264_nvenc` a 720p sbaglia fino a 7 livelli
    (tolleranza da 6 a 8). Le superfici restano 4 (default di FFmpeg senza B-frame): con 3 o 2 si
    risparmiano altri 5,5 MB ciascuna, ma con meno margine quando la GPU è carica, e non l'ho potuto
    provare con un gioco. SVT-AV1, la riserva software, con il parallelismo automatico (che su questo
    PC a 28 thread vale quanto `lp=6`) teneva circa 1 GB residente a 1080p; `lp=4` lo porta a 660 MB con 90 fps a
    1440p sul rumore (servono 60). Scartati `pred-struct=1` e `rtc=1` (sotto 100 MB, ma il tetto di
    bitrate salta a 416 Mbit/s e sul rumore si scende sotto 20 fps) e `lp=3` (57 fps a 1440p).
32. **Sorgente «Tutto tranne un'app»** (`kind = "except"`, 20 ottobre, richiesta di Luca). Process
    loopback in modalità EXCLUDE sull'albero dell'app: tutto l'audio del PC senza quel programma,
    senza passare da un dispositivo. Serve a separare Discord dal resto senza il Loopback della
    Focusrite, che riceveva anche la parte di Discord che suona sull'uscita predefinita (notifiche,
    soundboard, video) e la restituiva in ritardo dopo il giro nell'hardware, con l'eco tra le tracce.
    Limite di Windows: si esclude un solo albero di processi; finché l'app non è aperta si esclude
    ClipRec, quindi in quella sorgente il «ding» di salvataggio entra quando l'app è aperta.
