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
