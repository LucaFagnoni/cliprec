# ClipRec — prompt di sviluppo

20 ottobre 2026 · Luca

## Come usare questo documento

Dalla sezione «Ruolo, obiettivo e posizionamento» in poi, questo è il prompt da dare a Claude Code così com'è.

- **Cartella del progetto:** `D:\Repositories\cliprec`. Questo documento è `SPEC.md`; la ricerca sullo stack è in `docs/RESEARCH.md`; le istruzioni operative per la sessione notturna sono in `AVVIO.md` e prevalgono su questa specifica dove dice di fermarsi per la conferma.
- Servono Rust (ultima stable), Git e Visual Studio Build Tools (MSVC).
- Le voci **\[DEFAULT\]** sono scelte fatte dove la richiesta lasciava spazio; le voci **\[PROPOSTA\]** sono aggiunte nate dalla revisione. Senza conferma di Luca valgono come decise; ogni scelta va annotata in `docs/DECISIONS.md`.
- Cattura, encoder hardware e audio si testano solo sul PC reale, con GPU e dispositivi veri.

## Ruolo, obiettivo e posizionamento

Sei un ingegnere senior di software multimediale per Windows (Direct3D 11, Windows.Graphics.Capture, WASAPI, encoder hardware) e scrivi in Rust. Devi costruire **ClipRec**: un registratore sempre attivo che tiene in RAM gli ultimi minuti di gioco e, alla pressione di una hotkey, salva su disco una clip video+audio oppure solo audio, con le tracce audio separate per sorgente.

ClipRec fa solo due cose, entrambe a partire da un buffer in RAM sempre attivo:

1. **Clip video + audio:** l'equivalente del Replay Buffer di OBS e delle clip di SteelSeries Moments.
2. **Clip solo audio:** le stesse tracce audio senza video. Con la cattura video spenta, l'app diventa un registratore audio a ritroso.

Non registra video in continuo, non fa streaming, non ha scene né editor. Il confronto con le alternative riguarda solo il replay:

| Aspetto | Concorrenza a ottobre 2026 | Requisito ClipRec |
| --- | --- | --- |
| Primo avvio | OBS 32.2: buffer predefinito di 20 s e 512 MB, nessun tasto «Salva replay» assegnato; se la memoria finisce prima del tempo, le clip escono più corte senza avviso | Funziona al primo avvio con le due hotkey già assegnate; mostra quanti secondi il buffer copre davvero e quanta RAM usa |
| Audio per app | NVIDIA App separa solo il microfono; AMD scrive il microfono in un file a parte; Moments separa la chat solo con Sonar o un secondo dispositivo; Medal separa gioco, microfono e Discord ma richiede un account | Qualsiasi app o dispositivo su una traccia propria, oppure tutto in una traccia unica, senza driver virtuali né account |
| Solo audio | Nessun clipper diffuso ha una hotkey solo audio accanto al replay video; l'unico prodotto Windows trovato è VoIPVault (settembre 2026, a pagamento, microfono più un'app) | Hotkey «Clip audio» dallo stesso buffer, con tutte le sorgenti scelte |
| Dove sta il buffer | OBS in RAM (il buffer su disco è una PR ancora aperta); la registrazione di Steam usa il disco e gli utenti chiedono un'opzione solo RAM | Solo RAM, pagine bloccate in memoria; il disco si usa solo per scrivere la clip |
| Peso e privacy | GG è un hub di più app; Medal e Outplayed hanno account, overlay o pubblicità; NVIDIA App costa circa il 2-3% di fps in registrazione | Un processo in tray, senza account, overlay, pubblicità né rete; obiettivo sotto il 2% di fps |
| Anti-cheat | La cattura gioco di OBS e quella di Medal iniettano codice nel gioco, con conflitti segnalati su giochi Riot e FACEIT | Nessuna iniezione: solo API di cattura del sistema |

Fonti: [OBS Studio, release](https://github.com/obsproject/obs-studio/releases) · [DOR, buffer OBS](https://clip.dor.gg/en/blog/obs-replay-buffer-not-working) · [TechSpot, NVIDIA App](https://techspot.com/review/3140-nvidia-app-performance) · [AMD, Instant Replay](https://www.amd.com/en/resources/support-articles/faqs/DH3-023.html) · [Moments](https://hub.tcno.co/software/steelseries/moments) · [Medal, multitraccia](https://support.medal.tv/support/solutions/articles/48001220541-multi-track-recording) · [VoIPVault](https://store.steampowered.com/app/5074570/) · [Steam, registrazione](https://steamcommunity.com/groups/SteamClientBeta/discussions/5/4416424085344580826) · [PR buffer su disco](https://obs-versions.com/community/pulls/13558) · [Medal, schermo nero e hook](https://support.medal.tv/support/solutions/articles/48000922110-black-clips-black-screen)

## Piattaforma, linguaggio e stack

Rust su Windows x64 con le API native del sistema; FFmpeg 9 solo come libreria per encoder e contenitori. Versioni verificate al 20 ottobre 2026.

- **Sistema operativo:** minimo Windows 10 2004 (build 19041), che basta per cattura di finestre e audio per applicazione: OBS e GStreamer usano il process loopback da quella build, anche se la documentazione Microsoft indica la 20348. Consigliato Windows 11 24H2 (26100), che sblocca la cattura oltre 60 fps (`MinUpdateInterval`) e la cattura senza bordo giallo.
- **Toolchain:** Rust 1.99.1 o successivo (la 1.99.0 ha un bug di compilazione sulle chiamate FFI), edizione 2024, target `x86_64-pc-windows-msvc`. Profilo release con `lto = "fat"`, `codegen-units = 1`, `strip = true`; `panic` resta `unwind` perché ogni thread di lavoro è avviato sotto `catch_unwind` e un supervisore lo può riavviare senza perdere il buffer.
- **API di sistema:** crate `windows` 0.62.x e solo quella famiglia. La 0.100 appena uscita non si mescola con la 0.62, su cui si basano `wasapi` e `windows-capture`.
- **Cattura video:** implementazione propria di Windows.Graphics.Capture e Desktop Duplication sopra `windows` 0.62, con `windows-capture` 2.0.1 come riferimento. Non va usato così com'è: ha un frame pool da un solo buffer, usa sempre la GPU predefinita e non produce frame rate costante.
- **Audio:** `wasapi` 0.26 (l'unico crate mantenuto con il process loopback), `rubato` 5.0.1 per resampling e correzione della deriva, `flacenc` 0.5.1 per il FLAC in RAM, `rtrb` come coda lock-free tra thread.
- **Encoder e contenitori:** FFmpeg 9.0.x in build LGPL a librerie condivise con nvenc, amf, qsv/vpl, d3d11va, AAC e FLAC nativi, libopus, muxer mp4 e matroska. Header NVENC dal ramo 13.0 di nv-codec-headers, non da master, che pretende driver 610 o superiori. Binding con `ffmpeg-the-third` 6.x, il fork attivo con supporto a FFmpeg 9; la parte zero-copy (hwframes D3D11) richiede comunque FFI `unsafe`. Per partire va bene una build LGPL condivisa già pronta, poi una build ridotta propria. **\[DEFAULT\]**
- **Fallback software:** SVT-AV1 (licenza BSD) solo se manca un encoder hardware. Niente x264 o x265 (GPL), niente fdk-aac (non libera).
- **Processo di registrazione:** solo loop di messaggi Win32 più `tray-icon` 0.26 e `muda` 0.21; nessun framework grafico nel processo che cattura e codifica.
- **Impostazioni:** processo separato `cliprec-settings` in Slint 1.18, lanciato dalla tray e chiuso a fine uso, così tutta la sua memoria torna al sistema. La licenza royalty-free di Slint richiede l'attribuzione (widget `AboutSlint` nella pagina Informazioni). **\[DEFAULT\]**
- **Integrazione con Windows:** notifiche toast con `tauri-winrt-notification` e un AppUserModelID registrato dal collegamento nel menu Start; istanza unica con mutex nominato; avvio automatico dalla chiave HKCU Run (`auto-launch`).
- **Config, log, errori:** `serde` + `toml` 1.x in `%APPDATA%\ClipRec\config.toml` (modalità portable se il file sta accanto all'exe), `tracing` su file rotante, `thiserror` 2 nelle librerie e `anyhow` solo nei binari.
- **Distribuzione:** installer Inno Setup 7 oppure Velopack (aggiornamenti delta). Firma con certificato OV, es. Certum: Azure Artifact Signing non è disponibile per i privati in Italia.
- **Strumenti di misura:** `cargo-nextest`, `divan` per i micro-benchmark, PresentMon 2.4 per l'impatto sugli fps, Windows Performance Recorder/Analyzer e VMMap per CPU e memoria.

Fonti: [Rust 1.99.1](https://blog.rust-lang.org/2026/10/15/Rust-1.99.1/) · [wasapi](https://crates.io/crates/wasapi) · [windows-capture](https://crates.io/crates/windows-capture) · [ffmpeg-the-third](https://github.com/shssoichiro/ffmpeg-the-third) · [FFmpeg 9.0 Changelog](https://github.com/FFmpeg/FFmpeg/blob/release/9.0/Changelog) · [OBS, audio per applicazione](https://obsproject.com/kb/application-audio-capture-guide) · [Slint, release](https://github.com/slint-ui/slint/releases) · [Microsoft, opzioni di firma](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options)

## Architettura e pipeline

Il percorso caldo (cattura → GPU → encoder → buffer) non tocca mai il disco e non alloca memoria; il salvataggio legge dal buffer senza fermarlo.

&#91;embedded content: pipeline di ClipRec · cattura, buffer in RAM, salvataggio\]

Video e audio arrivano ai buffer su due percorsi separati con lo stesso orologio; la hotkey fa solo copiare riferimenti e rimpacchettare in un file.

Modello dei thread:

- **Video:** callback del frame pool WGC → pacer a frequenza fissa → conversione su GPU → encoder, con coda massima di 2-3 frame. Se l'encoder è in ritardo scarta frame, contandoli come persi, invece di accumulare.
- **Audio:** un thread per sorgente, registrato in MMCSS, che passa i pacchetti con timestamp QPC su code lock-free (`rtrb`) a un thread che, in traccia unica, mixa; poi comprime in FLAC e scrive nel buffer.
- **Salvataggio:** thread a bassa priorità, uno per clip in corso.
- **Messaggi Win32:** un thread per Raw Input, tray e notifiche.
- **Supervisore:** avvia ogni thread sotto `catch_unwind`; se un componente cade lo riavvia e i buffer restano intatti.
- **Impostazioni:** processo separato `cliprec-settings`. Parla con il core su una named pipe locale (stato dal vivo, «ricarica configurazione») e scrive solo il file di configurazione.

## Sorgenti video e cattura

Windows.Graphics.Capture (WGC) è il metodo principale per tutte le sorgenti; Desktop Duplication è il fallback per i monitor. Nessuna iniezione di codice nei giochi: è la scelta che evita i conflitti con gli anti-cheat, al prezzo di non catturare il fullscreen esclusivo.

Sorgenti selezionabili:

1. **Monitor:** uno a scelta.
2. **Finestra specifica:** scelta da un elenco con titolo, icona e nome dell'eseguibile.
3. **Gioco specifico:** agganciato per nome dell'exe, ritrovato da solo se il gioco si riavvia o cambia finestra.
4. **Automatica \[DEFAULT\]:** se in primo piano c'è un'app a schermo intero presente nella lista giochi (o qualsiasi app fullscreen, se l'opzione è attiva), cattura quella; altrimenti il monitor principale.

Regole di cattura:

- **Zero copie:** i frame restano texture D3D11 sulla GPU dalla cattura all'encoder. Nessun readback su CPU nel percorso normale.
- **Un solo device D3D11** condiviso da cattura, conversione ed encoder, creato sull'adapter che mostra la sorgente. Sui portatili con due GPU, Desktop Duplication fallisce sulla GPU dedicata per scelta di Microsoft: lì si usa WGC, e catturare dall'adapter sbagliato è stato misurato a 4-6 fps.
- **Oltre 60 fps:** su Windows 11 24H2 e successivi imposta esplicitamente `MinUpdateInterval` (almeno 1 ms), altrimenti WGC può restare a 60 fps o sotto. Sulle build precedenti, la cattura di un monitor oltre 60 fps passa da Desktop Duplication.
- **Frame rate costante in uscita:** un clock a frequenza fissa (30, 60, 120 o 144 fps) prende l'ultima texture arrivata e la ripete se non c'è nulla di nuovo, come fa OBS; i timestamp vengono dal QPC. Gli editor gestiscono male il frame rate variabile.
- **Bordo giallo** disattivato dove il sistema lo consente (build 20348 e successive, con un consenso dell'utente una tantum tramite `RequestAccessAsync`); cursore includibile o escludibile.
- **Fullscreen esclusivo:** WGC non lo cattura, e su 24H2 anche Desktop Duplication si blocca con alcuni giochi OpenGL in fullscreen esclusivo. ClipRec rileva frame neri o fermi, avvisa subito con una notifica che suggerisce la modalità finestra senza bordi, e non salva mai in silenzio una clip vuota.
- **Priorità GPU:** il contesto di cattura ed encoding usa la priorità GPU HIGH (non REALTIME, che può bloccare NVENC), come OBS e Sunshine, così con la GPU al 100% l'encoder non perde frame.
- **Cambio risoluzione o ridimensionamento:** ricrea il frame pool senza fermare il buffer e scala nella risoluzione d'uscita fissa con bande nere, così una clip non cambia formato a metà.
- **Finestra minimizzata o chiusa:** ultimo frame o nero, stato segnalato in tray, mai un crash.
- **HDR:** cattura in SDR con tone mapping semplice; niente HDR.

Fonti: [MinUpdateInterval](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.minupdateinterval) · [Desktop Duplication su GPU ibride](https://learn.microsoft.com/en-us/troubleshoot/windows-client/shell-experience/error-when-dda-capable-app-is-against-gpu) · [OBS #14021, fullscreen esclusivo su 24H2](https://github.com/obsproject/obs-studio/issues/14021) · [Discord, cattura finestre](https://support.discord.com/hc/en-us/articles/9410427556375--Windows-Capturing-Application-Window-for-Screen-Share-and-Go-Live)

## Codifica hardware, risoluzione e formati

Il video viene codificato una sola volta, in tempo reale, dall'encoder hardware della GPU; il salvataggio di una clip non ricodifica mai.

- **Rilevamento all'avvio:** prova ad aprire davvero ogni encoder (NVENC, AMF, Quick Sync) con le opzioni scelte, invece di dedurlo dal vendor, e mostra solo le combinazioni che funzionano. Su AMD: niente B-frame in HEVC su nessuna generazione, B-frame AV1 da verificare a runtime, e su RDNA3 l'AV1 a 1080p va impostato con l'allineamento corretto per non uscire a 1082 righe.
- **Codec:** H.264 per la massima compatibilità, HEVC, AV1 (RTX 40 e successive, Radeon RX 7000 e successive, Intel Arc e integrate da Meteor Lake). **\[DEFAULT\]** HEVC se disponibile, perché Discord lo riproduce dal 2024; AV1 non è predefinito finché la riproduzione su Discord non è confermata.
- **Zero copie verso l'encoder:** texture D3D11 passate come hwframes `AV_PIX_FMT_D3D11` direttamente a `*_nvenc` e `*_amf`. Per Intel, `*_qsv` non le accetta: serve un contesto QSV derivato con `av_hwframe_ctx_create_derived`.
- **Conversione su GPU:** BGRA → NV12 e ridimensionamento con `ID3D11VideoProcessor` o compute shader. Spazio colore BT.709, range limitato, metadati colore scritti nel file.
- **Controllo del bitrate:** VBR a qualità costante con tetto massimo **\[DEFAULT\]**, così l'occupazione di RAM è prevedibile. Valori di partenza per il tetto: H.264 a 16, 25 e 50 Mbit/s per 1080p60, 1440p60 e 4K60; HEVC e AV1 circa un terzo in meno.
- **Preset \[DEFAULT\]:** NVENC P5 con tuning qualità, lookahead spento (meno latenza e VRAM), 2 B-frame dove supportati; equivalenti per AMF e QSV. Keyframe ogni 1 s, che decide la precisione del taglio senza ricodifica.
- **Opzioni utente:** codec, risoluzione d'uscita (nativa, 1440p, 1080p, 720p), fps (30, 60, 120, 144), livello di qualità e tetto di bitrate. Il resto resta nascosto in Avanzate.
- **Errori dell'encoder** (sessione rifiutata, dispositivo perso, driver aggiornato a caldo): passa all'encoder o codec successivo disponibile e notifica, senza fermare l'app. Le GeForce oggi permettono 12 sessioni NVENC contemporanee, quindi una sessione di ClipRec convive con OBS o Discord.
- **Contenitore:** MP4 normale con indice in testa **\[DEFAULT\]**. Il file si scrive solo al salvataggio, quando tutti i pacchetti sono già noti, quindi l'indice in testa non costa nulla. MKV opzionale.

Fonti: [NVIDIA, matrice encoder](https://developer.nvidia.com/video-encode-decode-support-matrix) · [FFmpeg, encoder](https://ffmpeg.org/ffmpeg-codecs.html) · [AMD AMF](https://github.com/GPUOpen-LibrariesAndSDKs/AMF) · [Discord, HEVC](https://discord.com/blog/discord-patch-notes-july-1-2024)

## Buffer in RAM e salvataggio clip

Il buffer vive esclusivamente in RAM e contiene pacchetti già compressi; il disco viene toccato solo quando premi una hotkey e la clip viene scritta. 60 s di 1440p60 in HEVC a 40 Mbit/s occupano circa 300 MB, mentre gli stessi frame non compressi supererebbero i 50 GB.

- **Due hotkey, due durate:** «Clip video» salva gli ultimi N secondi di video + audio; «Clip audio» salva gli ultimi M secondi di solo audio. N e M si impostano nelle impostazioni (60 s e 120 s **\[DEFAULT\]**). Nessun'altra hotkey.
- **Struttura:** ring buffer video di pacchetti compressi con indice dei keyframe; un ring buffer audio per sorgente, o uno solo se le sorgenti sono unite. Tutti i timestamp sullo stesso orologio (QPC).
- **Dimensione:** il buffer video copre N secondi più un GOP; quello audio copre il maggiore tra N e M. Raggiunto il limite, scarta i dati più vecchi (il video a GOP interi).
- **Solo RAM, garantito:** nessun file temporaneo, nessuno spill su disco, nessuna cache. Le pagine del buffer vanno bloccate in memoria (`VirtualLock`, dopo aver alzato il working set con `SetProcessWorkingSetSizeEx`) così Windows non le sposta nel file di paging; se il blocco non riesce, l'app lo segnala in UI e continua.
- **Tetto di memoria:** configurabile, con stima dal vivo (bitrate × durata + audio). All'avvio, se la RAM libera non basta, l'app propone di ridurre durata o qualità invece di partire.
- **Allocazione:** blocchi preallocati e riutilizzati, nessuna allocazione per pacchetto nel percorso caldo, nessuna frammentazione dopo ore di sessione.
- **Taglio:** la clip parte dal keyframe che precede l'istante richiesto; l'audio è allineato al primo frame video e i timestamp riportati a zero.
- **Salvataggio senza fermare la cattura:** sotto un lock brevissimo si prende uno snapshot dei riferimenti ai pacchetti (`Arc`, nessuna copia); un thread a bassa priorità scrive il file finale come `.part` e lo rinomina a scrittura completata, così un file a metà non sembra mai una clip valida.
- **Pressioni ravvicinate:** una seconda pressione durante un salvataggio crea un'altra clip, mai un errore.
- **Nomi file e spazio:** modello configurabile, es. `{gioco}_{AAAA-MM-GG}_{hh-mm-ss}.mp4`; spazio libero controllato prima di scrivere.

## Sorgenti e tracce audio

Scegli le sorgenti audio da registrare: ognuna finisce su una traccia separata oppure, se lo decidi prima di registrare, tutte vengono unite in un'unica traccia già durante la cattura.

Tipi di sorgente:

- **Applicazione:** l'audio di una singola app, es. Discord, il gioco, il browser, catturato con il process loopback di WASAPI insieme ai processi figli. Si aggancia al processo radice dell'app (Chrome e simili suonano da un processo figlio) e viene ritrovato per nome dell'exe ogni 3 s se l'app si riavvia, come fa OBS.
- **Dispositivo di ingresso:** es. Microfono 1, Microfono 2, un'interfaccia audio.
- **Dispositivo di uscita:** tutto ciò che suona su cuffie o casse (loopback del dispositivo).
- Le app che usano WASAPI in modalità esclusiva non sono catturabili da nessun loopback: ClipRec lo rileva e lo segnala.

Configurazione:

- L'utente compone un elenco di sorgenti (es. Gioco, Discord, Microfono 1, Microfono 2), le ordina e dà a ciascuna un nome e un volume. Fino a 8 sorgenti **\[DEFAULT\]**.
- **Modalità tracce separate \[DEFAULT\]:** una traccia per sorgente, nell'ordine scelto, con il nome scritto nel file: Premiere, DaVinci Resolve e Audacity mostrano «Discord», «Microfono 1» e così via.
- **Modalità traccia unica:** tutte le sorgenti vengono sommate in tempo reale in una sola traccia, con limiter dopo la somma. È una scelta fatta prima di registrare, non una post-elaborazione: in questa modalità le singole sorgenti non vengono conservate.
- **Ordine delle tracce:** la prima traccia è l'unica che suona in Esplora file, Lettore multimediale, Instagram e, con ogni probabilità, nel player di Discord. **\[PROPOSTA\]** In modalità tracce separate, un'opzione «aggiungi in testa una traccia con il mix di tutto», spenta per default: la clip suona completa ovunque e le sorgenti restano separate per il montaggio.
- La stessa configurazione vale per le clip video e per le clip solo audio.

Sincronia e robustezza:

- **Orologio comune:** ogni pacchetto WASAPI porta la posizione nel dispositivo e l'istante QPC del suo primo campione (unità da 100 ns); tutte le sorgenti e il video si allineano su questo.
- **Client process loopback:** non supporta `GetMixFormat`, `GetCurrentPadding` né `GetDevicePeriod`. Si apre in float 32 bit, 48 kHz stereo con `AUTOCONVERTPCM` e `SRC_DEFAULT_QUALITY`, guidato da eventi ma con attesa massima di 10 ms, perché quando l'app tace i pacchetti smettono di arrivare.
- **Silenzio continuo:** i vuoti calcolati dai timestamp QPC vengono riempiti con silenzio digitale, mai buchi nella traccia.
- **Deriva dei clock:** i dispositivi divergono tipicamente di 20-50 ppm, fino a circa 180 ms all'ora. In tracce separate ogni pacchetto è ancorato al QPC e lo scarto dentro una clip resta trascurabile; nella traccia unica ogni sorgente passa da un resampler adattivo (`rubato`) verso l'orologio principale prima della somma. Obiettivo: ±20 ms dopo 1 ora.
- **Canali:** un microfono mono viene copiato su entrambi i canali a guadagno unitario; il mix si fa in float 32 bit con limiter dopo la somma.
- **Dispositivi scollegati o cambiati** (es. cuffie USB): `IMMNotificationClient` li segnala; la traccia prosegue in silenzio e si ricollega da sola, e può seguire il dispositivo predefinito di Windows.
- **Privacy del microfono:** se in Windows è spento «Consenti alle app desktop di accedere al microfono», la cattura del microfono fallisce: ClipRec lo dice chiaramente con un collegamento all'impostazione. Windows 11 sta provando un consenso per singola app desktop che etichetta come «Unsigned» gli exe non firmati, un motivo in più per firmare.
- Thread audio registrati in MMCSS («Pro Audio»), per evitare glitch quando il gioco satura la CPU.

Fonti: [Microsoft, esempio Application Loopback](https://github.com/microsoft/Windows-classic-samples/blob/main/Samples/ApplicationLoopback/cpp/LoopbackCapture.cpp) · [GetBuffer e QPC](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudiocaptureclient-getbuffer) · [Flussi in modalità esclusiva](https://learn.microsoft.com/en-us/windows/win32/coreaudio/exclusive-mode-streams) · [Deriva e resampling](https://www.forasoft.com/learn/audio-for-video/articles-audio/drift-correction-resampling-frame-drop)

Formati:

- **Nel buffer:** FLAC per traccia con `flacenc` (circa 126 volte il tempo reale su un solo core), circa la metà dei 23 MB al minuto del PCM float stereo. Le clip audio restano senza perdita.
- **Nelle clip video:** AAC-LC 48 kHz, 192 kbit/s per traccia **\[DEFAULT\]**, con l'encoder AAC nativo di FFmpeg: quello di Media Foundation manca sulle edizioni N di Windows e accetta solo 16 bit. Ogni traccia porta il nome nei campi `title` e `handler_name`; la prima è marcata come predefinita.

## Clip solo audio

La hotkey «Clip audio» salva gli ultimi M secondi delle sorgenti audio scelte, dallo stesso buffer usato per le clip video.

- **Durata propria:** la clip audio ha una sua durata M, indipendente da quella video. Il buffer audio può essere più lungo di quello video perché costa poco: circa 6 MB al minuto per traccia stereo a 16 bit in FLAC.
- **Modalità solo audio:** si può spegnere del tutto la cattura video. L'app non inizializza né GPU né encoder video e resta un registratore audio a ritroso, con hotkey e tray come sempre.
- **Allineamento al campione:** tutte le tracce partono dallo stesso campione, così si importano in una DAW e restano sincronizzate.

Formati d'uscita, a scelta:

1. **Con tracce separate:** un file per sorgente (FLAC o WAV), tutti con lo stesso istante d'inizio, in una cartella per clip **\[DEFAULT\]**: è il formato che ogni DAW importa senza sorprese. In alternativa un unico file MKA multitraccia, oppure un WAV polifonico (REAPER lo divide in tracce, Audition legge i nomi dei canali).
2. **Con traccia unica:** un solo file WAV, FLAC, Opus o AAC.

## Hotkey, notifiche, UI e impostazioni

L'app vive nella tray e si usa quasi solo con le hotkey; la finestra serve per configurare, non per stare aperta.

Hotkey:

- **Solo due hotkey:** «Clip video» e «Clip audio», preassegnate al primo avvio (Ctrl+Alt+F10 e Ctrl+Alt+F9 **\[DEFAULT\]**) e modificabili. Avvio e arresto del buffer si fanno dal menu della tray.
- **Rilevamento con Raw Input** (`RIDEV_INPUTSINK`): ascolta in modo passivo anche con il gioco in primo piano, non sottrae la combinazione agli altri programmi e riconosce i tasti laterali del mouse. `RegisterHotKey` non si usa perché si appropria della combinazione e non gestisce i tasti del mouse; l'hook a basso livello resta solo come ripiego, perché Windows lo rimuove se risponde lentamente e alcuni antivirus lo segnalano.
- **Giochi avviati come amministratore:** Windows isola i processi elevati e la hotkey potrebbe non arrivare. ClipRec rileva un gioco elevato in primo piano, lo segnala e propone di riavviarsi come amministratore.
- **Ripetizione:** l'autorepeat di un tasto tenuto premuto non genera clip multiple; una nuova pressione distinta sì.
- **Controller (opzionale):** le due azioni si possono assegnare anche a un tasto del controller (XInput).
- Conflitti con altre hotkey note (es. NVIDIA App usa Alt+F10) rilevati e segnalati già in configurazione.

Notifiche:

- Suono breve configurabile più notifica di Windows («Clip salvata · 60 s · 48 MB») con i pulsanti Apri e Cartella. Le notifiche richiedono un AppUserModelID: l'installer crea il collegamento nel menu Start che lo registra; in modalità portable lo crea il primo avvio.
- Nessun overlay disegnato dentro il gioco.
- Icona della tray che mostra lo stato: buffer attivo, salvataggio in corso, errore.
- Notifiche d'errore chiare e mai ripetute a raffica: fullscreen esclusivo non catturabile, microfono bloccato dalla privacy di Windows, RAM insufficiente, disco pieno.

Interfaccia:

- **Menu tray:** secondi coperti e RAM usata, avvia/ferma buffer, ultime 5 clip, apri cartella, impostazioni, esci.
- **Finestra impostazioni** (processo separato in Slint) a schede: Video, Audio (sorgenti e modalità tracce), Hotkey, Salvataggio, Avanzate.
- **Pannello di stato:** encoder in uso, fps effettivi, frame persi, RAM del buffer, secondi coperti.
- **Primo avvio:** rileva GPU ed encoder, sceglie il preset, propone le sorgenti (gioco in primo piano, microfono e uscita predefiniti, Discord se è aperto) e assegna le due hotkey. Un pulsante «Prova» salva una clip di 5 s e verifica audio e video prima di andare in gioco.
- Lingue italiano e inglese; avvio con Windows opzionale, ridotto in tray.
- **Privacy:** nessun account, nessuna telemetria, nessun accesso di rete.

## Requisiti di prestazione e criteri di accettazione

«Ultraleggera» va misurata: queste soglie **\[DEFAULT\]** valgono come criterio di accettazione per ogni milestone, su un PC da gioco di fascia media con GPU dedicata.

| Metrica | Soglia | Come si misura |
| --- | --- | --- |
| Calo di fps del gioco | < 2% medio, 1% low entro il 3% | PresentMon 2.4 in CSV, benchmark del gioco con e senza ClipRec |
| CPU del processo, 1440p60 HEVC hardware, 4 sorgenti audio | < 2% medio | Windows Performance Recorder/Analyzer, 10 min di gioco |
| RAM del processo esclusi i buffer, impostazioni chiuse | < 50 MB | VMMap, working set privato |
| Durata reale della clip | Mai meno di N s, al massimo un GOP in più | `ffprobe` su ogni clip dei test |
| Hotkey → snapshot del buffer | < 20 ms | Log con timestamp |
| Hotkey → file completo, clip di 60 s su SSD | < 1 s | Log con timestamp |
| Sincronia audio/video e tra tracce dopo 1 h | ±20 ms | Sorgente di test sintetica (sotto) |
| Avvio a freddo fino a buffer attivo | < 1,5 s | Log con timestamp |
| Sessione continua | 8 h senza crescita di memoria oltre i buffer né frame persi in modo sistematico | VMMap e contatori interni |
| Dimensione della distribuzione | < 40 MB con FFmpeg ridotta | Cartella di release |

Test automatici obbligatori:

- Unit test e property test (`proptest`) sul ring buffer: scarto a GOP interi, taglio al keyframe, tetto di memoria, snapshot durante la scrittura. Test anche per mix, limiter e correzione della deriva.
- Validazione con `ffprobe` di ogni file prodotto: durata, numero e nomi delle tracce, traccia predefinita, fps costanti.
- **Sorgente di test sintetica:** schermo con timecode e lampo bianco ogni secondo, bip sincrono su ogni sorgente audio; uno script misura in automatico lo scarto A/V e tra tracce nelle clip salvate.
- **Iniezione di guasti:** dispositivo audio scollegato, app sorgente chiusa e riaperta, encoder che fallisce, disco pieno, cambio di risoluzione, gioco in fullscreen esclusivo. Nessun caso deve far crashare l'app o produrre una clip corrotta senza avviso.
- **Benchmark ripetibili:** script PresentMon e WPR in `xtask` che producono un rapporto confrontabile tra versioni.

## Fuori ambito

ClipRec resta un'app di sole clip: queste funzioni non vanno implementate né predisposte.

- Registrazione video continua con avvia/ferma: si salvano solo clip dal buffer. Hotkey aggiuntive o durate multiple. Streaming live, scene, composizione di più sorgenti, webcam, overlay.
- Auto-clip da eventi di gioco, come fa Moments con GameSense.
- Editor con anteprima e taglio: la clip si apre nel player predefinito.
- Caricamento su cloud o social.
- HDR a 10 bit.
- Più monitor composti in un'unica immagine.
- macOS e Linux.

## Piano di lavoro a fasi

Il lavoro procede in cinque fasi: le parti dipendenti le fa l'agente principale da solo, quelle indipendenti i subagenti in parallelo. Alla fine di ogni fase l'agente si ferma, così posso provare sul mio PC prima di proseguire.

Regole:

- Prima di scrivere codice, l'agente principale propone la struttura dei crate e i contratti tra i componenti, ed elenca i rischi tecnici. Aspetta la mia conferma.
- A fine fase: build in release, test passati, `cargo clippy --all-targets -- -D warnings` pulito, istruzioni precise su cosa provare e quali numeri misurare.
- Niente `unwrap()` o `expect()` nei percorsi di runtime; errori tipizzati con `thiserror`; in caso di problema l'app degrada e notifica invece di andare in crash.
- `unsafe` solo nei moduli FFI, ogni blocco con un commento `// SAFETY:` e controllato dal revisore.
- `CLAUDE.md` sotto le 200 righe, con decisioni, comandi di build e test, stato delle fasi; regole specifiche per area in `.claude/rules/*.md` con il campo `paths:`.
- Versioni delle dipendenze fissate in `Cargo.lock`; nessun aggiornamento senza un motivo scritto.
- Se un requisito si scontra con un limite reale di un'API, va detto con un'alternativa, non aggirato in silenzio.

Fasi:

1. **Fase 0 · Fondamenta (solo agente principale):** workspace Cargo con i crate `cliprec-core`, `cliprec-capture`, `cliprec-encode`, `cliprec-audio`, `cliprec-buffer`, `cliprec-mux`, `cliprec-app`, `cliprec-settings` e `xtask`. Contratti in `cliprec-core`: tipi `GpuFrame`, `EncodedPacket`, `AudioChunk`, trait delle sorgenti, API del buffer e del writer, schema della configurazione, tutti documentati. FFmpeg 9 LGPL preparato da `xtask`; `CLAUDE.md`, file in `.claude/agents/`, hook. Commit. Accettazione: workspace che compila senza warning e contratti approvati da me.
2. **Fase 1 · Componenti in parallelo:** sei subagenti lavorano ciascuno sul proprio crate contro i contratti, con input sintetici (texture generate, pacchetti finti, toni audio). Criteri di accettazione per crate nella sezione sulla squadra.
3. **Fase 2 · Integrazione (agente principale con test-harness):** merge dei rami uno alla volta, poi la catena completa: cattura → encoder → buffer → hotkey → MP4 multitraccia, più la clip solo audio. Accettazione: fps, CPU, salvataggio e sincronia entro soglia sul mio PC.
4. **Fase 3 · Robustezza e rifinitura in parallelo:** iniezione di guasti, sessione di 8 ore, fallback AMD e Intel, primo avvio e impostazioni complete, notifiche; revisione avversaria completa.
5. **Fase 4 · Distribuzione:** build FFmpeg ridotta, installer, firma, README, misure finali della tabella prestazioni.

## Squadra di subagenti, modelli ed effort

L'agente principale gira su Opus 5.5 e coordina otto subagenti più un revisore; ognuno possiede un solo crate, così il lavoro in parallelo non si sovrappone.

| Subagente | Possiede | Modello | Effort | Accettazione |
| --- | --- | --- | --- | --- |
| Agente principale | `cliprec-core`, integrazione, merge | Opus 5.5 | `high`; `xhigh` per contratti e integrazione | Workspace verde dopo ogni merge |
| `video-capture` | `cliprec-capture` | `opus` | `xhigh` | Monitor e finestra a 60 e 144 fps costanti, ridimensionamento senza crash, fallback Desktop Duplication |
| `video-encoder` | `cliprec-encode` | `opus` | `xhigh` | Texture D3D11 sintetiche → H.264, HEVC e AV1 senza copie su NVENC, AMF e QSV disponibili; keyframe ogni 1 s |
| `audio-engine` | `cliprec-audio` | `opus` | `high` | App, microfono e uscita catturati con QPC; silenzio riempito; traccia unica con deriva corretta |
| `replay-buffer` | `cliprec-buffer`, `cliprec-mux` | `opus` | `high` | Property test verdi; clip MP4, MKA, FLAC e WAV valide per `ffprobe`; taglio al keyframe; pagine bloccate in RAM |
| `app-shell` | `cliprec-app` | `sonnet` | `medium` | Tray, Raw Input con tasti del mouse, notifiche, istanza unica, supervisore |
| `settings-ui` | `cliprec-settings` | `sonnet` | `medium` | Tutte le schede, named pipe con il core, attribuzione Slint |
| `test-harness` | `tests/`, `xtask` | `sonnet` | `medium` | Sorgente sintetica, controlli `ffprobe`, script PresentMon e WPR |
| `packaging` (Fase 4) | installer, build FFmpeg ridotta, README | `sonnet`; `haiku` per i testi | `medium`; `low` per i testi | Installer funzionante e firmato |
| `reviewer` | nulla: solo lettura | `opus` | `high` | Elenco di falle a fine Fase 1 e Fase 3 |

Come scegliere e cambiare modello ed effort:

- La tabella è il punto di partenza; l'agente principale adatta modello ed effort al singolo compito. Lavoro meccanico (rinomine, boilerplate, testi) va a `haiku` con effort `low`.
- Se un subagente fallisce due volte i criteri di accettazione, viene rilanciato con un livello di effort in più (`low` → `medium` → `high` → `xhigh` → `max`). Su cattura ed encoder, se fallisce ancora, si passa a `fable` (Fable 5.1) con effort `high`: è il modello più capace ma costa di più e può consumare crediti di utilizzo.
- Modello ed effort si fissano nei campi `model` ed `effort` del file del subagente; il parametro della singola chiamata li sovrascrive quando serve.

Regole di parallelismo:

- **Contratti prima:** nessun subagente parte finché i contratti della Fase 0 non sono committati. In `.claude/settings.json` va impostato `"worktree": {"baseRef": "head"}`: senza, i worktree partono dal ramo remoto predefinito e non vedono i commit locali.
- **Un crate a testa,** in un worktree separato (`isolation: worktree`). `cliprec-core` lo modifica solo l'agente principale: se un subagente ha bisogno di cambiare un contratto, lo chiede nel rapporto e si ferma.
- **Al massimo 6 subagenti contemporanei.** Il limite tecnico è 20, ma oltre questa soglia il collo di bottiglia diventano i merge.
- **Prompt di ogni subagente:** obiettivo, file posseduti, contratto, criteri di accettazione e formato del rapporto (cosa è fatto, test, problemi aperti, richieste sui contratti).
- **Compilazione:** `sccache` condiviso tra i worktree e una `CARGO_TARGET_DIR` per worktree, per non bloccarsi sul lock di cargo.
- **Hook:** `PostToolUse` su `Edit|Write` esegue `cargo check -p <crate>` nella `cwd` ricevuta dall'hook; un hook `Stop` nel file di ogni subagente gli impedisce di chiudere se `clippy` o i test del suo crate falliscono.
- **Merge:** Claude Code non unisce i rami da solo. L'agente principale integra i rami `worktree-*` uno alla volta ed esegue tutti i test dopo ognuno.
- **Revisione avversaria:** a fine Fase 1 e Fase 3 il `reviewer`, con contesto pulito, riceve solo questa specifica e il codice e cerca falle; le correzioni le fa il subagente che possiede il crate.
- Le «agent teams» sperimentali non si usano: i loro membri non hanno worktree separati.

Esempio di file `.claude/agents/video-encoder.md`:

```markdown
---
name: video-encoder
description: Implementa cliprec-encode, encoder hardware FFmpeg 9 senza copie da texture D3D11. Da usare per qualsiasi lavoro sull'encoder video.
tools: Read, Edit, Write, Bash, Grep, Glob
model: opus
effort: xhigh
isolation: worktree
---
Possiedi solo crates/cliprec-encode. Implementa il trait VideoEncoder di cliprec-core
senza modificarlo. Criteri di accettazione: vedi SPEC.md, sezione «Squadra di subagenti».
A fine lavoro riporta: cosa è fatto, test eseguiti, problemi aperti, richieste sui contratti.
```

Fonti: [Subagenti](https://code.claude.com/docs/en/sub-agents) · [Modelli](https://code.claude.com/docs/en/model-config) · [Effort](https://platform.claude.com/docs/en/build-with-claude/effort) · [Worktree](https://code.claude.com/docs/en/worktrees) · [Hook](https://code.claude.com/docs/en/hooks) · [Agent teams](https://code.claude.com/docs/en/agent-teams)
