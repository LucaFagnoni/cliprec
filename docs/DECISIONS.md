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
33. **Avvio come amministratore con un'attività pianificata** (20 ottobre, richiesta di Luca). Il
    valore `Run` non può avviare un programma con privilegi elevati senza la conferma UAC a ogni
    accesso; un'attività «all'accesso» con `HighestAvailable` sì. ClipRec la crea con `schtasks /XML`
    da un processo elevato (una conferma UAC quando si salva l'opzione, una quando la si toglie) e
    toglie il valore `Run`. L'XML fissa durata illimitata, avvio anche a batteria e priorità normale:
    i valori predefiniti fermerebbero ClipRec dopo 3 giorni e lo farebbero girare a priorità bassa.
    Un segnaposto accanto a config.toml evita di richiedere il permesso a ogni avvio. La finestra
    delle impostazioni è stata ridisegnata sul modello delle Impostazioni di Windows 11.
34. **Colore 4:4:4 e codifica avanzata** (20 ottobre, richiesta di Luca). Gli aloni colorati sul
    testo vengono dal 4:2:0 (un campione di colore ogni 2×2 pixel), non dalla conversione, che già
    media correttamente. Con `video.advanced.color_format = "yuv444"` il processore video consegna
    BGRA a colori pieni e NVENC converte in 4:4:4 (`rgb_mode=yuv444`, profilo `rext` o `high444p`):
    su una linea rossa da 1 pixel l'alone passa da 100 a 2 livelli su 255, i colori pieni restano
    entro 8 livelli. Solo NVENC H.264/HEVC; con gli altri encoder si registra in 4:2:0 con un
    avviso. Le altre opzioni (controllo del bitrate, preset, fotogrammi chiave, B-frame, look-ahead,
    AQ, bitrate AAC) hanno predefiniti uguali al comportamento precedente.
35. **Runtime VC++ scaricato dall'installer solo se manca** (20 ottobre, richiesta di Luca; cambia
    la 30). Il redistributable non è più nel pacchetto né in `third_party`: su «Installa»
    l'installer controlla il registro (`VC\Runtimes\x64`, anche WOW6432Node) e, se il runtime
    manca, lo scarica da `aka.ms/vs/17/release/vc_redist.x64.exe` con la pagina di download di
    Inno Setup e lo installa. Senza rete avvisa e prosegue: ClipRec usa le DLL del runtime nella
    sua cartella. Tolto `cargo xtask redist`.
36. **Aggiornamenti senza reinstallare** (20 ottobre, richiesta di Luca). All'avvio (dopo 15 s) e
    ogni 24 ore il core legge l'ultima release di `LucaFagnoni/cliprec` su GitHub; se è più nuova
    apre la finestra di aggiornamento (`cliprec-settings --aggiornamento`, stesso tema delle
    impostazioni) che chiede se installarla. Installazione: zip portable scaricato con `curl.exe`,
    SHA-256 confrontato con il file `.sha256` della release (`certutil`), estratto con `tar.exe`;
    i file in uso si rinominano in `.old` (Windows lo permette anche per exe e DLL caricate), i
    nuovi prendono il loro posto, ClipRec si riavvia e toglie i `.old` all'avvio seguente. Al
    primo errore si ripristinano i file vecchi. Niente dipendenze nuove, niente amministratore.
    Limite: lo SHA-256 garantisce l'integrità dello zip pubblicato, non l'autore; con un
    certificato di firma si potrà verificare anche la firma degli exe prima di sostituirli.
37. **FFmpeg ridotta compilata in casa** (20 ottobre; cambia il [DEFAULT] sulla build BtbN). ClipRec
    linka e distribuisce una build LGPL 3 con i soli componenti usati, compilata da
    `cargo xtask ffmpeg-ridotta` in `third_party/ffmpeg-ridotta` (`FFMPEG_DIR`): encoder NVENC, QSV
    e AMF per H.264/HEVC/AV1, SVT-AV1, AAC, FLAC, PCM 24 bit e Opus; muxer MP4, Matroska, FLAC,
    WAV e Opus; hwcontext D3D11VA e QSV; swresample. Niente avfilter, avdevice, swscale, decoder,
    rete né programmi. Le DLL passano da 112 a 13,5 MiB. La build BtbN completa resta in
    `third_party/ffmpeg` (`FFMPEG_TOOLS_DIR`) solo per ffmpeg/ffprobe nei test e in `xtask probe`/`sync`:
    i suoi exe caricano le DLL della loro cartella, quindi le due build non si mescolano.
    Toolchain fissata: MSYS2 base `msys2-base-x86_64-20260927.tar.xz` da repo.msys2.org
    (SHA-256 `ea2f31a0b6ade63914ce441ffb022f0f6aa96982bfefa2326460a26d5fb01322`, firma GPG di
    MSYS2 verificata), pacchetti UCRT64 installati per URL e versione esatta (pacman ne verifica la
    firma): gcc 16.2.0-6, binutils 2.47-4, mingw-w64 crt/headers/winpthreads 14.0.0.r495, nasm
    3.02-1, pkgconf 3.0.10-1, svt-av1 4.2.0-1, opus 1.6.1-2, libvpl 2.17.0-1, amf-headers 1.5.3-1,
    ffnvcodec-headers 13.0.19.0-1 (elenco in `xtask/src/ridotta.rs`). Sorgente: lo stesso commit della
    build BtbN precedente, `669f5ab083c7732c10f1ce9e4f1915960dd88457` (n9.0.2-28, ramo release/9.0),
    archivio GitHub con SHA-256 `e5b24d5779f736b3ef8a985d52051d48fece48712f810af7037283ad707ecd67`.
    Le librerie esterne e il runtime di gcc sono linkati staticamente (`-static`): le quattro DLL
    importano solo DLL di Windows. Due ritocchi al configure: `-lstdc++` (il `.pc` di libvpl non lo
    dichiara) e il parser HEVC (in una build minima `hevc/parse.o` resta senza le SEI e il link
    fallisce). `FFmpeg-BUILD.txt` e `THIRD_PARTY_NOTICES.md` leggono `ffmpeg-version.txt`, scritto
    dallo script al posto di `ffmpeg -version`.
38. **MP3 con LAME, ripiego AMF, avanzamento degli aggiornamenti** (20 ottobre, richieste di Luca).
    Clip audio anche in MP3 (192 kbit/s, un file per traccia): FFmpeg non ha un encoder MP3 suo,
    quindi la build ridotta aggiunge `libmp3lame` (LAME 3.100, LGPL-2.0+, statica, pacchetto MSYS2
    `mingw-w64-ucrt-x86_64-lame-3.100-3`) e il muxer `mp3`. AMF: su una RX 6700 XT nessun encoder
    AMD risultava disponibile e qui non c'è una GPU AMD per capirne il motivo; ora ogni encoder
    scartato scrive il motivo nel log (livello info) e, se AMF rifiuta l'apertura, si riprova lo
    stesso encoder in VBR semplice senza B-frame, pre-analisi e VBAQ prima di scartarlo. Updater:
    download con avanzamento (dimensione dello zip dalla release, file misurato ogni 150 ms) e
    chiusura automatica della finestra quando il core riparte con un'altra versione.
39. **Limiti noti risolti dove Windows lo permette** (20 ottobre, richiesta di Luca).
    Fullscreen esclusivo: invece di un hook nel gioco (DECISIONS 27), la cattura di una finestra che
    dà frame neri o fermi mentre copre il monitor in primo piano passa a Desktop Duplication di quel
    monitor, e torna a WGC quando il gioco non è più a schermo intero in primo piano. «Uscita»
    predefinita: torna al loopback del dispositivo predefinito, come «Audio del desktop» di OBS. Il
    process loopback in esclusione (m7) registrava i flussi di tutti i processi, compreso il mix che
    i router audio come SteelSeries Sonar ricostruiscono, microfono incluso: un tester lo sentiva
    nella traccia «Sistema». Con il loopback dell'endpoint torna anche il segnale di modalità
    esclusiva (R3). Prezzo: il suono di conferma, che parte dopo lo snapshot, può finire nelle clip
    successive; chi non lo vuole lo spegne o usa «Tutto tranne un'app» su `cliprec-app.exe`. Bordo
    giallo di WGC: permesso «senza bordo» chiesto una volta con `GraphicsCaptureAccess` (m16).
40. **Una sola finestra di impostazioni, riaperta dopo gli aggiornamenti** (20 ottobre, richiesta di
    Luca). Impostazioni e finestra di aggiornamento hanno un mutex per sessione
    (`Local\ClipRec.Impostazioni`, `Local\ClipRec.Aggiornamento`): un secondo avvio porta in primo
    piano quella aperta ed esce. La finestra legge la versione del core: se è diversa dalla propria
    (core aggiornato mentre era aperta) si riapre dall'eseguibile nuovo sulla stessa pagina, una
    volta sola; con modifiche non salvate avvisa invece di chiudersi. Il menu dei codec scrive la
    scheda che codifica («HEVC · AMD»): sulle RX 6000 non c'è AV1, quindi H.264 e HEVC sono già AMF.
41. **Monitor con Desktop Duplication, tetto del buffer dalla RAM totale** (20 ottobre, prove di gioco
    di Luca e di un tester AMD). Il monitor si cattura con DDA e WGC resta il ripiego (portatili
    ibridi, DDA non disponibile), come «Cattura schermo» di OBS sui PC fissi: WGC sul monitor mostrava
    il bordo giallo durante il gioco e, con il fullscreen esclusivo, frame neri; DDA vede tutto e
    costa meno. Su DDA del monitor i frame neri sono contenuto vero (caricamenti): niente avviso. Il
    tetto del buffer era metà della RAM *libera* al momento della creazione: all'avvio di un gioco
    scendeva a poche decine di MB e la clip copriva 20-30 s. Ora è un quarto della RAM fisica (l'arena
    cresce solo quando serve) e l'avviso resta solo per un limite scelto dall'utente. Con priorità GPU
    alta il processo passa anche a «sopra il normale»: nei caricamenti la CPU satura faceva perdere
    tick al pacer e mettere in ritardo il compressore FLAC. La lingua scelta nell'installer va nel
    registro (`HKCU\Software\ClipRec\InstallerLanguage`) e ClipRec la adotta una volta all'avvio;
    «Esci» dalla tray chiude anche le finestre di impostazioni e aggiornamento. Il renderer software di
    Slint dopo riduci/ripristina ridisegna solo le zone cambiate su un buffer svuotato: al ripristino
    un velo invisibile cambia e la finestra si ridisegna intera.
42. **Monitor di nuovo con WGC** (20 ottobre, 0.1.7). Con la 0.1.6 (DDA per il monitor) Luca perdeva
    in gioco oltre 10 000 tick in pochi minuti. La causa non si riproduce nei test (schermo fermo,
    puntatore, contesto usato a ogni frame: zero tick persi), quindi si torna alla cattura provata in
    gioco con la 0.1.5 e DDA resta il ripiego e la via per il fullscreen esclusivo delle finestre.
    Corretto comunque il ciclo DDA: `AcquireNextFrame` senza attesa e pausa di 2 ms fuori dal lock
    del contesto, invece di attese fino a 100 ms che potevano tenere fermo l'encoder. La parte 41 sul
    monitor con DDA è superata; restano valide le altre.
43. **Frame persi, fullscreen esclusivo, amministratore, audio esclusivo, router** (20 ottobre,
    richiesta di Luca sui limiti noti). Frame persi: un frame `repeated` (pacer senza frame nuovi o
    riempitivo di un tick perso) rimanda all'encoder l'ultima superficie NV12 invece di rifare la
    conversione, come i frame duplicati di `ffmpeg -r`; il motore non copia più ogni frame nuovo in
    una texture propria per i riempitivi (una copia BGRA piena a ogni frame, 1,8 GB/s a 1440p60).
    Riempitivi fino a 250 ms. Il pacer resta sincrono con l'encoder: separarli con una coda di
    texture costerebbe di nuovo una copia per frame. Fullscreen esclusivo: il cambio a DDA (39) vale
    anche per il monitor; con `SHQueryUserNotificationState = QUNS_RUNNING_D3D_FULL_SCREEN` basta un
    secondo di frame neri o fermi invece di 3-5 (il segnale da solo no: c'è anche per i giochi con le
    ottimizzazioni a schermo intero, che WGC cattura bene, e DDA in gioco va ancora provato, 42); una finestra che WGC rifiuta e copre il monitor passa a DDA. Amministratore:
    `RegisterHotKey` mentre un programma elevato è in primo piano (Raw Input lì tace), tolta appena
    torna un programma normale perché sottrae il tasto al gioco; il mouse resta scoperto. Audio
    esclusivo: nessuna cattura lo vede, quindi lo si impedisce sui dispositivi registrati con la
    proprietà `{b3f8fa53-0004-438e-9003-51a46e139bfc},3` dell'endpoint (la casella «Consenti alle
    applicazioni di assumere il controllo esclusivo», scrivibile senza amministratore), con un avviso
    per dispositivo; opzione `audio.block_exclusive`, attiva di serie perché è ciò che serve per
    registrare. Router: una «Uscita» per canale virtuale (Sonar, Voicemeeter, Wave Link), senza i
    mix «Stream»/«Monitor»; un'unica sorgente che somma più dispositivi avrebbe chiesto il mixer per
    ogni traccia. Iniezione nel gioco sempre esclusa (27).
44. **Cattura senza copie ed encoder su un thread proprio** (20 ottobre, richiesta di Luca).
    Zero-copy WGC: il frame del pool (4 buffer) va negli slot del pacer così com'è e torna al pool
    quando ne arriva uno più nuovo e il pacer non lo sta consegnando, quindi al più due in mano;
    il video processor legge la superficie WGC direttamente (`SHADER_RESOURCE | RENDER_TARGET`,
    nessun ripiego sulla copia, misurato). Sparisce una copia BGRA piena per ogni frame arrivato,
    compresi quelli in più dovuti a `MinUpdateInterval` a 3/4 di periodo (17), che resta così: i
    frame in più non costano più nulla. Memoria: 4 superfici invece di 2 del pool più 3 slot.
    `GpuFrame.texture` può essere più grande del contenuto (in alto a sinistra). Desktop
    Duplication copia ancora: deve rilasciare il frame subito. La sonda dei frame neri ora
    interroga una query d'evento prima di leggere: con il contesto fermo un `Map` senza attesa
    restava «in corso» per un giro. Encoder: il pacer converte e accoda (6 frame, 100 ms a 60 fps),
    il thread `cliprec-encode` manda a FFmpeg e raccoglie i pacchetti, che tornano alla chiamata
    successiva; a coda piena il pacer aspetta come prima, dopo 2 s è un errore dell'encoder. Frame
    della coda e buffer dei pacchetti girano tra i due thread (niente allocazioni a regime).
45. **Tagli dell'audit sull'eccesso di codice** (20 ottobre, richiesta di Luca). Tolta la dipendenza
    `auto-launch` (e con lei 11 pacchetti nel lock, nessuna versione cambiata): il valore `Run` si
    legge, scrive e toglie con le API del registro del crate `windows`, con la stessa regola di
    Gestione attività (`StartupApproved\Run`). Scrive solo in HKCU, come l'installer: `auto-launch`
    da amministratore scriveva in HKLM, per tutti gli utenti; un vecchio valore in HKLM si toglie
    quando l'avvio con Windows viene spento. Tolto il trait `VideoEncoder`, che aveva una sola
    implementazione: l'encoder è il tipo concreto `cliprec_encode::Encoder`. Tolti
    `Mixer::lane_error_ms` e il campo che leggeva (nessun chiamante) e il worktree abbandonato
    `confident-ptolemy-67f42c`, già unito.
46. **Tracce separate per clip video e clip solo audio** (20 ottobre, richiesta di Luca). La scheda
    Audio ha due gruppi («Tracce delle clip video», «Tracce delle clip solo audio»), ciascuno con
    traccia per sorgente o unica e «Mix» in testa. Il buffer registra l'unione di ciò che serve ai
    due tipi (`AudioConfig::recorded`: «Mix» se uno dei due lo vuole, le sorgenti se uno dei due le
    vuole) e al salvataggio si tengono le tracce del tipo di clip (`keeps_track`). Prezzo: con i due
    tipi diversi il mixer gira anche se serve a uno solo, e il buffer audio cresce di una traccia.
    `audio.audio_clip` assente (configurazioni precedenti) = come le clip video; le impostazioni lo
    scrivono solo quando la clip audio è diversa, così salvare senza toccare nulla non cambia il file.
47. **Release da GitHub Actions** (20 ottobre, richiesta di Luca). `.github/workflows/release.yml`,
    avviato a mano (`gh workflow run release.yml -f notes="..."`), su `windows-2025`: Rust 1.93,
    FFmpeg ridotta con `cargo xtask ffmpeg-ridotta` (in cache finché `ridotta.rs` e lo script non
    cambiano), `cargo xtask dist`, Inno Setup 7.1.0 scaricato da GitHub con SHA-256 verificato, zip
    con `.sha256`, installer e release `v<versione di Cargo.toml>`; si ferma se la release esiste.
    Le variabili d'ambiente del workflow sostituiscono i percorsi di questo PC in `.cargo/config.toml`.
    Niente test (servono GPU ed encoder hardware): si lanciano in locale prima. Così la build non
    tocca `target\dist`, da cui gira il ClipRec di Luca. `xtask dist` cerca il runtime VC++ anche
    nelle edizioni di Visual Studio 2022 in `Program Files`.
48. **Slint senza le parti inutilizzate, cache della build nel workflow** (21 ottobre, richiesta di
    Luca). Slint con `default-features = false` e solo `std`, `compat-1-2`, `backend-winit`,
    `renderer-software` e `accessibility`: via il renderer OpenGL (le impostazioni usano sempre il
    software, `main.rs`) e la tray di Slint (la tray è di `cliprec-app`). 12 crate in meno,
    `cliprec-settings.exe` da 19,1 a 18,3 MB; l'accessibilità resta per i lettori di schermo. Il
    workflow di release tiene in cache registro e `target` per `Cargo.lock`.
49. **Sorgenti separate per tipo di clip, menu per tipo di clip** (21 ottobre, scelte di Luca: due
    elenchi indipendenti, menu «Clip video · Clip audio · Audio · Generale · Informazioni»).
    `audio.audio_clip_sources` è l'elenco delle clip solo audio (assente = quello delle clip video).
    `AudioConfig::plan` decide cosa catturare (una sorgente con le stesse impostazioni nei due
    elenchi si cattura una volta), cosa registrare (un «Mix» per tipo di clip con le sole sue
    sorgenti, uno solo se coincidono, più le tracce delle sorgenti) e quali tracce, in che ordine,
    vanno in ciascuna clip. Il motore audio registra il piano con più mixer sullo stesso thread; il
    salvataggio sceglie le tracce (`select_tracks`). Le impostazioni scrivono l'elenco audio solo se
    diverso da quello video, come per le tracce (46). Ogni pagina di clip ha durata e file o formato,
    tracce, sorgenti (componente `SourceList` riusato) e scorciatoie; «Audio» tiene le opzioni comuni
    alle sorgenti, «Generale» cartella, nome del file, avvio, notifiche, memoria e lingua.
50. **Frame persi: misurati, e il pacer fa solo conversione e coda** (21 ottobre, 61 tick persi
    nell'istanza di Luca, tutti in pochi episodi: a riposo il contatore resta fermo). In prova sul
    PC di Luca, sessione vera (monitor 1440p → 1080p, HEVC 4:4:4): 0 tick persi a riposo, salvando
    una clip, con la CPU satura (56 thread) e con la GPU satura da un altro processo (`gpu_hog`).
    La causa non si è riprodotta, quindi: (1) il pacer scrive nel log ogni 10 s i tick persi con il
    ritardo massimo del risveglio e la callback più lunga, per sapere la prossima volta se è lo
    scheduler o la conversione; (2) i pacchetti vanno nel buffer dal thread dell'encoder
    (`Encoder::set_sink`), non più dal pacer: allocazione e `VirtualLock` di un blocco nuovo
    dell'arena, il lock del buffer e gli snapshot non toccano più il thread dei tick (test: sink
    fermo 80 ms ogni 30 pacchetti, `encode` resta a 0,35 ms); (3) i frame WGC tornano al pool sul
    thread della cattura, fuori dal lock degli slot, mai sul pacer; (4) pacer in MMCSS «Capture»
    (THREAD_PRIORITY_HIGHEST se non disponibile); (5) la ricerca dei codec all'avvio usa un device
    suo. Gli fps effettivi nello stato sono fps scelti × tick consegnati ÷ tick scaduti: 60,0 se
    non se ne perde nessuno (prima 59,9 per l'arrotondamento su finestre di poco più di 1 s).
51. **Niente compilazione incrementale in debug** (21 ottobre, Luca vedeva calare lo spazio su D:).
    `target` era arrivato a 93,9 GB in pochi giorni di build e test: 44,9 GB di cache incrementale in
    `target/debug/incremental` (una copia nuova a ogni modifica, mai tolta) e 32,7 GB di librerie in
    `target/debug/deps`. ClipRec in esecuzione su D: scrive solo le clip salvate. `cargo clean --profile
    dev` e `cargo clean --release` (non `cargo clean`: cancellerebbe `target/dist`, da cui gira il
    ClipRec di Luca) e `[profile.dev] incremental = false`. Da ripetere ogni tanto le due pulizie.
52. **Coda dell'encoder piena: si salta il frame, non si aspetta, non si cambia encoder** (21
    ottobre, istanza di Luca in gioco). Il log della 0.1.12 ha mostrato episodi con la callback del
    pacer lunga quanto il ritardo (200-625 ms): il pacer aspettava un posto nella coda (6 frame)
    mentre l'encoder era fermo dietro alla GPU satura dal gioco. Dopo 2 s di coda ferma l'encoder
    veniva dichiarato guasto: alle 12:00:49 HEVC è passato a H.264, il buffer video si è svuotato e,
    con il bitrate più alto di H.264, il buffer è cresciuto a 437 MiB. Ora il pacer prende prima un
    posto in coda e solo se c'è converte il frame; con la coda piena il frame si salta subito
    (`Encoded::Skipped`, senza conversione), il buco si riempie ripetendo il precedente come per i
    tick persi, e i salti contano nei frame persi dello stato e negli fps; nel log ogni 10 s. Solo
    una coda ferma per 10 s è un guasto dell'encoder. Priorità GPU REALTIME scartata: blocca NVENC
    su NVIDIA (RESEARCH). Test: `full_queue_skips_frames_without_blocking_or_failing`.
53. **Riempitivi solo con la coda libera; il frame saltato fa avanzare l'indice** (21 ottobre,
    regressione della 0.1.13 in gioco: fps 0, circa 6900 «salti» ogni 10 s). Con la coda piena il
    frame saltato lasciava `next_index` indietro; al tick dopo i riempitivi (fino a 15) prendevano i
    posti che si liberavano e il frame nuovo veniva saltato di nuovo, per sempre: l'encoder ripeteva
    frame vecchi e i pts correvano avanti. Ora `fill_and_encode` smette di riempire al primo
    riempitivo saltato e salta anche il frame; un frame saltato fa comunque avanzare `next_index` (il
    buco resta nel file come pausa) e conta una volta sola nei frame persi. Test:
    `full_queue_never_starves_new_frames`.
54. **Monitor: cambio a Desktop Duplication solo con il segnale di Windows** (22 ottobre, dopo la
    0.1.14 in gioco). I frame neri e fermi sul monitor sono contenuto vero (schermate di
    caricamento): facevano passare a DDA e tornare indietro a ogni caricamento, con qualche frame
    perso a ogni cambio, e mostravano un avviso. Ora su un monitor si passa a DDA solo quando
    Windows segnala lo schermo intero esclusivo (`looks_exclusive`, QUNS); niente avviso dei frame
    neri. Test: `monitor_switches_only_with_the_windows_signal`.
55. **Coda dell'encoder da 6 a 12 frame (200 ms a 60 fps)** (22 ottobre). Assorbe i picchi della
    GPU in gioco che facevano saltare frame. Ogni frame in coda tiene una superficie dell'encoder
    (circa 3 MB a 1080p 4:2:0, 6 MB in 4:4:4); il pool fisso di QSV è dimensionato sulla coda
    (coda + 8).
56. **Conversione fuori dal pacer** (22 ottobre). Nel log in gioco restavano tick persi con la
    callback del pacer a 35-63 ms: `VideoProcessorBlt` (BGRA→NV12) blocca nel driver quando la GPU
    è satura. Ora `Encoder` è un front sul thread del pacer (pts, contatori, decisione di saltare) e
    la conversione la fa il thread `cliprec-convert`, proprietario di `HwEncoder`, che poi accoda
    all'encoder. Il frame viaggia con un prestito della superficie (`GpuFrame::hold`, nuovo in
    `cliprec-core`): la cattura non riscrive né restituisce al pool di WGC uno slot prestato. Il
    triplo buffer regge un prestito oltre a quello del pacer, quindi si converte un frame alla
    volta: con una conversione in corso un frame nuovo si salta (come con la coda piena, DECISIONS
    52), uno ripetuto passa e rimanda la superficie già convertita. Un frame senza `hold` (test,
    sorgenti di prova) si converte prima che `encode` torni, come prima. Test:
    `frames_with_hold_are_converted_off_the_caller_thread`, `slot_in_use_is_never_written`.
57. **Il buffer restituisce i blocchi liberi da 2 minuti** (22 ottobre, Luca vedeva 437 MiB dopo un
    picco di bitrate). I blocchi dell'arena liberi e mai riusati da 2 minuti si sbloccano
    (VirtualUnlock) e si liberano, tranne due tenuti pronti; se il bitrate risale si riallocano sul
    thread dell'encoder. Test: `idle_free_chunks_are_given_back`.
58. **L'attività di avvio si legge dall'Utilità di pianificazione e si sostituisce se avvia un altro
    programma** (27 ottobre, Luca: ClipRec non partiva più all'accesso). L'attività «ClipRec» puntava
    a `target\dist`, cancellato: il segnaposto `avvio-amministratore` diceva che c'era, quindi non
    veniva mai ricreata (risultato 0x80070002, file non trovato). Ora a ogni avvio ClipRec legge con
    l'API COM dell'Utilità di pianificazione (senza privilegi; `schtasks` scrive nella code page della
    console e storpierebbe i percorsi accentati) quale programma l'attività avvia: se è un altro, la
    ricrea (una conferma UAC); se l'avvio come amministratore è spento e l'attività c'è, la toglie. Il
    segnaposto sparisce. L'attività resta invisibile in Gestione attività → App di avvio, che mostra
    solo il valore `Run` e la cartella Esecuzione automatica.
