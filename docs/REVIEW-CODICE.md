# Revisione critica del codice di ClipRec

20 ottobre 2026, commit `55db777`. Due revisori indipendenti a contesto pulito (Opus, effort xhigh,
sola lettura): **A** sulla pipeline (cattura, encoder, buffer, mux, audio), **B** su app e robustezza
(hotkey, tray, notifiche, impostazioni, pipe, supervisore, errori, `unsafe`, guasti, misure,
distribuzione e licenze). Hanno ricevuto solo SPEC.md, DECISIONS.md, PROGRESS.md e il codice. I loro
44 rilievi sono stati uniti e tolti i doppioni: **38 falle, 1 critica, 9 maggiori, 28 minori**.

## Giudizio

La catena funziona davvero dall'inizio alla fine: build e `clippy -D warnings` puliti, test verdi, clip
sintetiche valide per ffprobe e in sincronia, nessun `unwrap`/`expect` a runtime, buffer solo in RAM.
Ma non è pronta per gli utenti: la risoluzione può bloccarsi sul segnaposto di una finestra minimizzata,
l'audio vero può perdersi (latenza ≥ 60 ms, inizio della clip, coda del Mix) e la distribuzione non si
può pubblicare (manca il runtime VC++, avvisi di licenza incompleti). Le correzioni sono localizzate.

### Le 5 correzioni più importanti

1. **C1** · Risoluzione d'uscita fissa (quella del monitor del bersaglio) e mai encoder aperto su un
   segnaposto o su una finestra minimizzata; avviso quando un cambio di parametri svuota il buffer (m2).
2. **M3** · Riempimento di silenzio che non scarta mai l'audio vero: trattenimento adattivo alla
   latenza del dispositivo, tolleranza sui salti di 10-20 ms.
3. **M4 + m3** · Audio completo a inizio e fine clip: conservare l'audio per N + 0,5 s + un GOP, e
   chiudere lo snapshot audio a `now − MIX_LATENCY` in traccia unica e con il Mix.
4. **M2** · Config illeggibile all'avvio: niente ripartenza silenziosa con i predefiniti (che catturano
   schermo e microfono contro la scelta dell'utente); errore notificato e buffer fermo.
5. **M1 + M8** · Distribuzione pubblicabile: CRT statico (o runtime VC++ incluso), build FFmpeg
   fissata per URL e hash, avvisi di licenza completi (librerie statiche in FFmpeg, testo GPLv3, testi
   delle licenze dei crate).

## Piano delle correzioni (20 ottobre 2026, 10:10)

Richiesta di Luca: correggere critiche e maggiori, le minori solo se banali e isolate; M1 con il
redistributable nell'installer; uscita scalabile (es. 1080p da uno schermo 1440p); encoder di riserva;
le lacune rispetto a OBS e Moments; valutare l'iniezione DLL. Ogni correzione ha un test di regressione
che fallisce prima e passa dopo (se impossibile, il motivo è nel rapporto del subagente).

1. **Contratti** (`cliprec-core`, agente principale, da solo e per primo): `VideoSource::native_size`
   (C1), ritardo per sorgente audio, richieste IPC per salvare le clip (automazione), codec disponibili ed
   esito della «Prova» nello stato, formati audio Opus/AAC/WAV polifonico (m27). Commit prima di tutto.
2. **In parallelo, un subagente per crate in worktree separati** (nessun file in comune):

| Gruppo | Subagente (modello, effort) | Falle | File |
| --- | --- | --- | --- |
| Cattura | video-capture (Opus, xhigh) | C1, m17, m18 | `crates/cliprec-capture` |
| Encoder | video-encoder (Opus, xhigh) | M6 (ripiego software SVT-AV1), m19 | `crates/cliprec-encode` |
| Audio | audio-engine (Opus, high) | M3, M9 (thread audio), m7, m9, ritardo per sorgente | `crates/cliprec-audio` |
| Buffer e mux | replay-buffer (Opus, high) | M4, M5, M7, m8, m27 | `crates/cliprec-buffer`, `crates/cliprec-mux` |
| Shell dell'app | app-shell (Sonnet, medium) | M2, M9 (gamepad, IPC), m5 (server), m6, m12 (app), m13, m21, m22, m26, IPC «salva clip» | `crates/cliprec-app` tranne `engine.rs` |
| Impostazioni | settings-ui (Sonnet, medium) | m5 (client), m14, m24, m25, ritardo e formati nell'interfaccia | `crates/cliprec-settings` |
| Distribuzione | packaging (Sonnet, medium), parte quando si libera un posto | M1 (redistributable VC++ nell'installer), M8, m12 (installer) | `installer/`, `xtask/src/{dist,notices}.rs`, `THIRD_PARTY_NOTICES.md` |

3. **Motore** (`crates/cliprec-app/src/engine.rs`, agente principale, in parallelo ai subagenti): C1
   (uscita fissa dalla dimensione nativa), M5 (coda dei salvataggi), M6 (clip audio se manca l'encoder),
   M9 (thread di apertura dell'encoder), m1, m2, m3, m10, m11, m14, m15, m20, m23, m24.
4. **Integrazione** un ramo alla volta; dopo ognuno `cargo build --release`, tutti i test, `cargo clippy
   --workspace --all-targets -- -D warnings`. Poi m4 (nuove misure), tabella aggiornata, verifica finale
   da un subagente nuovo (Opus, high).

**Rimandate**: m16 (consenso `RequestAccessAsync`: apre un dialogo di sistema, va progettato con
l'interfaccia), m28 (spostare gli `unsafe` in un modulo FFI: rifattorizzazione su molti file, senza
effetto per l'utente, meglio dopo queste correzioni), iniezione DLL (valutata sotto, non implementata).

### Valutazione: iniezione DLL per il fullscreen esclusivo

- **Come si fa** (OBS Game Capture): una DLL caricata nel processo del gioco aggancia la presentazione di
  ogni API grafica (D3D9/10/11/12, OpenGL, Vulkan con layer implicito), copia il backbuffer in una texture
  condivisa tra processi e la segnala al registratore. È l'unico modo di catturare il vero fullscreen
  esclusivo.
- **Quanto serve davvero**: dal 2019-2020 Windows presenta tramite DWM la gran parte dei giochi DX11/DX12
  «a schermo intero» (ottimizzazioni a schermo intero e flip model): WGC li cattura già. Il caso scoperto
  resta per alcuni giochi OpenGL/DX9, per chi disattiva le ottimizzazioni e per la regressione di 24H2 con
  OpenGL (OBS #14021).
- **Costi**: un aggancio per API grafica, sincronizzazione GPU tra processi, helper a 32 bit per i giochi
  vecchi, firma del codice obbligatoria e manutenzione continua a ogni aggiornamento dei driver e dei
  giochi. Stima: 4-8 settimane per una prima versione affidabile limitata a D3D11, D3D12 e Vulkan.
- **Rischi**: gli anti-cheat (Riot Vanguard, FACEIT, in parte EAC e BattlEye) bloccano o segnalano le DLL
  iniettate non riconosciute; OBS stesso ha avuto conflitti con i giochi Riot dopo un cambio di
  certificato. Per l'utente il rischio è un crash, una cattura bloccata o, nel caso peggiore, una sanzione
  sull'account. Contraddice il posizionamento di SPEC.md («Nessuna iniezione: solo API di cattura del
  sistema»), che è uno dei vantaggi su OBS e Medal.
- **Raccomandazione**: non implementarla nel prodotto base; mantenere rilevamento, avviso e suggerimento
  della finestra senza bordi. Se in futuro servisse, solo come modulo separato «Cattura gioco
  (sperimentale)», spento per impostazione predefinita, attivabile per gioco da un elenco esplicito di
  giochi senza anti-cheat, con DLL firmata e solo D3D11/D3D12/Vulkan. Decisione da confermare
  (DECISIONS.md, voce 27).

## Falle, in ordine di gravità (con lo stato dopo le correzioni del 20 ottobre)

Verifica: **CONFERMATA** = dimostrata leggendo il codice in modo univoco o con un test/comando;
**PLAUSIBILE** = percorso nel codice chiaro, effetto non dimostrato in esecuzione. Rev. = revisore.

| ID | Gravità | Area | File:riga | Problema | Scenario | Correzione | Verifica | Stato |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| C1 | critica | cattura, motore | `cliprec-capture/src/source.rs:197-201`, `wgc.rs:101,138,170-171`; `cliprec-app/src/engine.rs:457-460`; `cliprec-encode/src/lib.rs:62-65,75-82` | La risoluzione «nativa» si decide sul primo frame consegnato, che può essere il segnaposto di una finestra minimizzata (146×28); l'encoder non si riapre mai su una dimensione più grande. | Gioco minimizzato con Alt+Tab mentre ClipRec parte o ricarica la config: con QSV tutte le clip della sessione escono 146×28; con NVENC l'apertura fallisce e il video manca fino al tentativo successivo (fino a 30 s). Stessa radice: launcher piccolo seguito dal gioco grande. | Uscita fissa alla risoluzione del monitor del bersaglio; mai aprire su segnaposto o finestra minimizzata; riaprire se la sorgente supera l'uscita. | CONFERMATA (rev. A: programma di prova, primo frame 146×28; `ffmpeg color=s=146x28`: QSV accetta, NVENC rifiuta) | **corretta**: `0a46cf0`, `d613826` (cattura), `93b296e` (motore). Test: `c1_encoder_uses_native_size_not_first_frame`, `minimized_window_at_start_never_yields_tiny_frames`, `native_size_is_the_target_monitor`, `minimized_leftover_frames_are_stale`; scalatura: `output_can_be_scaled_below_native` |
| M1 | maggiore | distribuzione | `xtask/src/dist.rs:12-15`; `installer/cliprec.iss:49-50` | `cliprec-app.exe` e `cliprec-settings.exe` importano `VCRUNTIME140.dll`, che né `xtask dist` né l'installer includono; il CRT statico non è attivo. | PC senza Visual C++ Redistributable: «VCRUNTIME140.dll non trovato», l'app non parte. La prova «dist da sola» era su questo PC, che ha i Build Tools. | `-C target-feature=+crt-static`, oppure DLL accanto agli exe, oppure redistributable nell'installer. | CONFERMATA (rev. B: `dumpbin /dependents`) | **corretta**: `90c9094` (redistributable nell'installer, runtime nella cartella portable). Test: `dist_folder_is_self_contained`, `installer_ships_vc_redist_with_check`. Installer non compilato (Inno Setup assente) |
| M2 | maggiore | app | `cliprec-app/src/main.rs:50-54` | Un `config.toml` non valido all'avvio va solo nel log; l'app parte con i predefiniti senza avvisare. | Utente con «solo audio, solo Discord» sbaglia una riga a mano: al riavvio ClipRec cattura monitor e microfono senza dirlo. (La ricarica a caldo invece notifica, `engine.rs:731-733`.) | `EngineEvent::Error` all'avvio; non partire con sorgenti diverse da quelle scelte: buffer fermo e impostazioni aperte. | CONFERMATA (rev. B, lettura) | **corretta**: `c23afce`, `ceb8c1e`. Test: `m2_unreadable_config_keeps_buffer_stopped_and_reports` |
| M3 | maggiore | audio | `cliprec-audio/src/dsp.rs:61,64,122-128`; `wasapi_source.rs:462-464` | `Continuity` riempie di silenzio fino ad adesso − 50 ms e scarta i pacchetti che arrivano sopra il silenzio già emesso; tolleranza sui salti di soli 2 ms. | Microfono o dispositivo virtuale (Voicemeeter, Sonar) con latenza ≥ 60 ms: traccia muta al 100% con stato `Running`. Jitter di ±2,5 ms: 6,2 s di audio vero persi in 120 s. | Trattenimento adattivo (latenza osservata + margine); mai scartare dati veri sopra il riempimento; tolleranza 10-20 ms. | CONFERMATA (rev. A: latenza 20-45 ms → 100% dei campioni, 60-150 ms → 0%); latenza reale dei dispositivi PLAUSIBILE | **corretta**: `5998b94`. Test: `continuity_keeps_all_real_audio_with_high_latency`, `continuity_keeps_all_real_audio_with_jitter`, `mix_keeps_late_source` |
| M4 | maggiore | buffer | `cliprec-buffer/src/lib.rs:337-339` contro `:202-209,252-259` | L'audio si conserva per `max(N, M)` + 85 ms, il video per N + 0,5 s + un GOP. | Con M ≤ N (es. N = M = 60) il primo secondo circa di ogni clip video è muto su tutte le tracce, senza avviso. | `audio_keep = max(N + 0,5 + GOP, M) + un blocco`. | CONFERMATA (rev. A: N = M = 5 s → 872 ms senza audio) | **corretta**: `cc090c0`. Test: `audio_covers_the_whole_video_clip` |
| M5 | maggiore | buffer, mux, motore | `cliprec-buffer/src/audio.rs:191`; `cliprec-mux/src/ffi.rs:416`; `cliprec-app/src/engine.rs:329,340,346`; `cliprec-settings/ui/settings.slint:393` | Ogni salvataggio decomprime tutte le tracce in PCM f32 insieme (secondi × 384 kB × tracce), fuori dal tetto di memoria e senza limite di salvataggi concorrenti. | `--prova 120` a 2 tracce: +134 MB al salvataggio. Durata audio 1800 s e 4 tracce: circa 2,8 GB per pressione; con 7200 s e 8 tracce l'allocazione fallisce, il processo abortisce e il buffer è perso. | Codifica a blocchi in streaming dai blocchi FLAC; coda dei salvataggi. | CONFERMATA per il picco (rev. B, misura); abort PLAUSIBILE | **corretta**: `7312ca9` (mux in streaming), `93b296e` (coda), `5fb7b95` (motore senza `render`). Test: `saving_keeps_memory_bounded` (44 MB → 131 kB), `m5_saves_are_written_one_at_a_time` |
| M6 | maggiore | encoder, motore | `cliprec-encode/src/lib.rs:94-104,132-144`; `cliprec-app/src/engine.rs:313-317,689-696` | Manca il ripiego software SVT-AV1 chiesto da SPEC.md (la rinuncia non è in DECISIONS). Senza encoder la hotkey video non salva nemmeno l'audio. | GPU senza encoder (GT 1030, VM), sessioni NVENC esaurite o AMF che fallisce: tentativi infiniti con toast d'errore; Ctrl+Alt+F10 → «il buffer video è ancora vuoto». | `libsvtav1` (già nella DLL) come ultimo candidato; senza encoder pronto la hotkey video salva la clip audio con un avviso. | CONFERMATA (rev. A e B, lettura e stringa nella DLL) | **corretta**: `c3faafa` (SVT-AV1), `93b296e` (clip audio se manca il video). Test: `open_after_falls_back_to_svtav1`, `m6_video_hotkey_saves_audio_when_no_video_is_ready`; integrazione `m6_software_encoder_clip_is_valid_mp4` |
| M7 | maggiore | buffer, audio | `cliprec-buffer/src/audio.rs:136-139`; `cliprec-audio/src/engine.rs:36-40`; `cliprec-buffer/tests/alloc.rs:78` | La compressione FLAC alloca a ogni blocco sul thread di cattura MMCSS (tracce separate), sotto i lock del buffer, invece di passare da una coda. | 38 allocazioni e circa 507 KB per blocco da 85 ms per traccia: con 8 sorgenti e il Mix decine di MB/s di allocazioni continue sul thread audio. Il test accetta fino a 1000 allocazioni e non lo rileva. | Compressione su un thread non tempo reale alimentato da `rtrb`, strutture riusate. | CONFERMATA (rev. A, uscita di `cargo test -p cliprec-buffer`) | **corretta**: `b4dc2f5`. Test: `hot_path_allocations` (420 → 0 allocazioni sul thread audio) |
| M8 | maggiore | licenze | `xtask/src/dist.rs:53-57`; `xtask/src/main.rs:23-24`; `THIRD_PARTY_NOTICES.md:6-13` | `FFmpeg-BUILD.txt` cita «n9.0.2» ma la build è `n9.0.2-28-g669f5ab083-20261019` da un «latest» non fissato; le DLL contengono librerie statiche (dav1d, libvpx, libaom, SVT-AV1, openh264, LAME, libjxl…) senza avvisi; manca il testo GPLv3 (LGPLv3 §4b); i crate hanno solo l'identificativo SPDX. | Distribuire `target/dist/ClipRec` così com'è viola le licenze; il sorgente indicato non corrisponde alla build. | URL e hash fissati; licenze delle dipendenze FFmpeg, testo GPLv3 e testi dei crate (`cargo about`). | CONFERMATA (rev. B: `ffmpeg -version`, `xtask dist`, grep) | **corretta**: `b860cf8`. Test: `ffmpeg_download_is_pinned`, `build_info_names_exact_build`, `static_libs_section_covers_configure`, `dist_folder_has_gpl_text`, `every_crate_license_is_covered`. Titolari di `uavs3d` e `zvbi` da verificare sui sorgenti |
| M9 | maggiore | robustezza | `cliprec-audio/src/wasapi_source.rs:186,283-292`; `cliprec-audio/src/engine.rs:139-141`; `cliprec-app/src/engine.rs:470`; `cliprec-app/src/gamepad.rs:70` | Thread di cattura audio, mixer, `encoder-open`, gamepad e connessioni IPC fuori da `catch_unwind` (SPEC: ogni thread supervisionato). | Panic in un thread audio: lo stato resta `Running`, nessuno lo riavvia, la traccia diventa silenzio nelle clip senza avvisi. | `catch_unwind` nel corpo dei thread, stato `Error` e riavvio della sorgente, come per il pacer. | Assenza CONFERMATA (rev. B); effetto PLAUSIBILE | **corretta**: `2ff42a6` (audio), `7a9dcaf` (gamepad, IPC), `93b296e` (apertura encoder). Test: `panic_in_capture_thread_restarts_source`, `panic_in_thread_restarts_source`, `mixer_survives_panic`, `m9_panic_in_the_gamepad_thread_is_survived`, `m9_panic_in_a_request_does_not_kill_the_connection`, `m9_panic_while_opening_the_encoder_is_retried` |
| m1 | minore | motore | `cliprec-app/src/engine.rs:285-299,314,335,745-753` | Lo snapshot non è legato all'istante della pressione: `create_dir_all` sul disco avviene prima, e il thread del motore può essere occupato in `retry_video` o in un riavvio. | Cartella su NAS lento o motore in riavvio: lo snapshot parte oltre 0,5 s dopo, l'inizio è già scartato, clip più corta di N senza avviso. | Istante QPC preso nella shell e passato nel comando; snapshot prima di qualunque accesso al disco. | Ordine CONFERMATO; effetto PLAUSIBILE (rev. A e B) | **corretta** (snapshot prima del disco): `93b296e`. Test: `m1_snapshot_is_taken_before_touching_the_disk`. Resta sincrono sul thread del motore il nuovo tentativo del video (`retry_video`, ogni 5-60 s) |
| m2 | minore | motore, buffer | `cliprec-app/src/engine.rs:300-306,577-581,645-652,723-726`; `cliprec-buffer/src/lib.rs:170-178` | Un cambio dei parametri video svuota il buffer avvisando solo se cambia il nome dell'encoder; una modifica solo audio (es. volume del microfono) riavvia anche cattura ed encoder. | Finestra ridimensionata, poi una ricarica qualunque o un device perso: la clip successiva dura pochi secondi senza spiegazione. | Riavviare solo le parti cambiate; in `save` confrontare la durata reale con N e dire perché è più corta. | CONFERMATA (rev. A e B, lettura) | **corretta**: `93b296e`. Test: `m2_audio_only_change_does_not_restart_video`, `m2_short_clip_is_reported` |
| m3 | minore | audio, motore | `cliprec-audio/src/engine.rs:22,191-194`; `cliprec-app/src/engine.rs:286`; `cliprec-buffer/src/audio.rs:191` | In traccia unica e nella traccia «Mix» gli ultimi ~150 ms di ogni clip sono silenzio (`MIX_LATENCY`). | Ogni clip in modalità traccia unica finisce con 157 ms muti. | Snapshot audio fino a `now − MIX_LATENCY`, o rinviato di 200 ms. | CONFERMATA (rev. A: Mix 157 ms, tracce separate 10 ms) | **corretta**: `93b296e`, `943c944` (latenza reale del mixer). Test: `m3_mix_clip_has_no_silent_tail` |
| m4 | minore | misure | `docs/PROGRESS.md:107,109` | Le misure dichiarate non reggono: RAM esclusi i buffer ~60-70 MB di working set privato (dichiarati ~16 MB, soglia 50 MB); la sessione di 3,5 h girava con il binario precedente all'arena a 1/4 crescente (`8fbab7b`) e con contenuto a ~160 kbit/s, quindi crescita e riciclo dell'arena attuale non sono stati provati per ore. | Il criterio «RAM < 50 MB esclusi i buffer» non è rispettato e PROGRESS dice il contrario. | Rifare le misure con il binario attuale e contenuto ad alto bitrate; correggere PROGRESS. | CONFERMATA (rev. A e B: misure CIM, date dei commit) | **corretta la documentazione**: misure rifatte con il binario attuale (PROGRESS.md). La soglia **non è rispettata**: working set privato 111 MB con 19 MB di buffer, circa 92 MB esclusi i buffer contro 50 MB. Da analizzare con VMMap (driver NVENC/D3D) |
| m5 | minore | pipe | `cliprec-app/src/ipc_server.rs:121-149`; `cliprec-settings/src/pipe.rs:17-18`; `cliprec-settings/src/main.rs:288-298` | Una sola istanza in ascolto e client senza `WaitNamedPipeW`. | Il «Salva» delle impostazioni coincide con la richiesta di stato: «ClipRec non è in esecuzione», config non applicata. | Creare l'istanza successiva prima di servire; nel client attendere e ritentare. | CONFERMATA (rev. B: 2201 `ERROR_PIPE_BUSY` su 3000 con due client) | **corretta**: `299624e` (server), `c06ce3b` (client). Test: `m5_two_concurrent_clients_never_find_the_pipe_busy`, `busy_pipe_is_waited_for_not_reported_as_offline` |
| m6 | minore | pipe | `cliprec-app/src/ipc_server.rs:65,100,127-133` | `read_line` senza limite; 16 istanze esauribili; log d'errore ogni secondo. | Un processo dello stesso utente invia GB senza a capo: allocazione fallita, abort, buffer perso. Con 16 client fermi il diciassettesimo va in timeout. | Lettura limitata (`take(64 KiB)`), timeout per connessione, log con limite di frequenza. | CONFERMATA (rev. B: 150 MB → memoria da 74 a 331 MB) | **corretta**: `42c0aa5`. Test: `m6_oversized_line_is_cut_and_memory_stays_bounded` (timeout e limite del log con test scritti insieme alla correzione) |
| m7 | minore | notifiche, audio | `cliprec-app/src/notify.rs:161-168`; `cliprec-core/src/config.rs:140,292` | Il suono di salvataggio esce sul dispositivo predefinito, catturato in loopback dalla sorgente «Sistema». | Due clip a 20 s di distanza: la seconda contiene il «ding» della prima. | Loopback «Sistema» con esclusione del PID di ClipRec, o suono spento se l'uscita è catturata. | CONFERMATA (rev. B, lettura) | **corretta**: `a472379` (loopback che esclude ClipRec). Test: `default_output_excludes_own_process`; la prova sul suono reale non è riproducibile senza altoparlanti |
| m8 | minore | mux | `cliprec-mux/src/ffi.rs:210-220`; `cliprec-app/src/engine.rs:588-592` | La codifica AAC usa un thread per traccia a priorità normale: la priorità bassa vale solo per il thread di salvataggio. | Fino a 9 thread a priorità normale per clip, in concorrenza con il gioco. | Priorità bassa dentro ogni thread di codifica, o codifica in sequenza. | CONFERMATA (rev. A, lettura) | **corretta**: `370a7f0`. Test: `encoder_threads_run_at_low_priority` |
| m9 | minore | audio | `cliprec-audio/src/wasapi_source.rs:404-414` | Per le sorgenti app (process loopback) la modalità esclusiva non viene rilevata (SPEC.md:139). | osu! in WASAPI esclusivo: traccia muta, stato `Running`, nessun avviso. | Dopo qualche secondo di silenzio con l'app attiva, controllare le sessioni esclusive e avvisare. | CONFERMATA (rev. A, lettura) | **rimandata**: nessun rilevamento affidabile e piccolo; l'unico segnale (`AUDCLNT_E_DEVICE_IN_USE`) non dice quale app tiene il dispositivo e darebbe falsi allarmi |
| m10 | minore | tray | `cliprec-app/src/engine.rs:233-236,250-254`; `cliprec-app/src/icon.rs:18-20` | Qualunque `Waiting` diventa `last_error`. | Discord chiuso o PC senza microfono: icona rossa fissa e toast; gli errori veri non si distinguono. | `last_error` solo per `Error`; `Waiting` come stato informativo. | CONFERMATA (rev. B) | **corretta**: `93b296e`. Test: `m10_waiting_sources_are_not_errors` |
| m11 | minore | motore | `cliprec-app/src/engine.rs:707,754-758` | «Ferma buffer» non libera né sblocca la RAM e non scarta i dati, ma lo stato mostra 0 MB. | Si ferma il buffer per dare RAM al gioco: nessuna RAM liberata; alla ripartenza le clip uniscono contenuto vecchio e un salto. | Allo stop sostituire o svuotare e sbloccare il buffer. | CONFERMATA (rev. B) | **corretta**: `93b296e`. Test: `m11_stop_buffer_frees_the_buffer` |
| m12 | minore | installer | `installer/cliprec.iss:46-60`; `cliprec-app/src/shell.rs:558-560`; `cliprec-app/src/autostart.rs:31-37` | La config predefinita ha `start_with_windows = false`: al primo avvio `auto-launch` cancella il valore `Run` scritto dall'installer. | L'utente sceglie «Avvia con Windows» nell'installer: dal riavvio successivo ClipRec non parte. | L'installer scrive anche la config, o l'app adotta lo stato del registro al primo avvio. | CONFERMATA (rev. B, lettura di auto-launch 0.6.0) | **corretta**: `d357e91`. Test: `m12_first_run_adopts_the_installer_autostart_value` |
| m13 | minore | app | `cliprec-app/src/main.rs:38-39,76-80`; `cliprec-app/src/engine.rs:737` | «Riavvia come amministratore»: la nuova istanza aspetta 10 s, la vecchia può impiegarne 15. | Salvataggio lungo in corso: la nuova istanza rinuncia, la vecchia esce, nessuna istanza attiva. | Attesa ≥ 20 s o passaggio esplicito; avvisare che il buffer riparte da zero. | PLAUSIBILE (rev. B) | **corretta**: `ab09fbf` (attesa 25 s). Test: `m13_restart_waits_longer_than_the_engine_shutdown` |
| m14 | minore | encoder, impostazioni | `cliprec-encode/src/lib.rs:160`; `cliprec-app/src/main.rs:126-136`; `engine.rs:577` | `probe` non viene mai chiamata: le impostazioni offrono sempre H.264, HEVC e AV1. | AV1 scelto su una RTX 3080: si apre `hevc_nvenc` senza alcun avviso. | `probe` al primo avvio, risultato nello stato IPC, scelte filtrate, avviso se il codec differisce. | CONFERMATA (rev. B) | **corretta**: `93b296e` (rilevamento), `f6f60ad` (impostazioni). Test: `m14_status_lists_available_codecs`, `codec_choices_follow_available_codecs` |
| m15 | minore | test | `cliprec-app/src/engine.rs:1141` | Il test end-to-end della sincronia è instabile sotto carico: un tick perso sposta il contenuto fino a ~33 ms a 60 fps. | Fallito in un'esecuzione completa su due; da solo 3 su 3 (A/V tra −6,5 e −13 ms). | Tolleranza legata ai tick persi, o test in un ambiente meno carico. | Fallimento CONFERMATO; causa PLAUSIBILE (rev. A) | **corretta**: `df453ec` (lampo sintetico mai in ritardo), test pesanti già in fila. Test: `flash_frame_is_skipped_when_its_tick_is_lost` |
| m16 | minore | cattura | `cliprec-capture/src/wgc.rs:124-129` | `RequestAccessAsync` non viene mai chiamata (SPEC: consenso una tantum per togliere il bordo giallo). | Il bordo giallo resta visibile nelle clip di finestre. | Chiedere il consenso al primo avvio, dalle impostazioni. | Assenza CONFERMATA; effetto PLAUSIBILE (rev. A) | **rimandata**: il consenso apre un dialogo di sistema, va progettato con l'interfaccia |
| m17 | minore | cattura | `cliprec-capture/src/dda.rs:1-2`; `source.rs:181` | Con Desktop Duplication il cursore non viene mai disegnato. | 144 fps prima della 24H2 con `capture_cursor = true`: clip senza cursore. | Disegnare la forma del puntatore da `GetFramePointerShape`. | CONFERMATA (rev. A) | **corretta**: `c2c5048`. Test: `cursor_is_blended_on_the_frame`, `monochrome_and_masked_shapes_convert` |
| m18 | minore | cattura | `cliprec-capture/src/wgc.rs:25`; `dda.rs:37` | Nessun HDR né tone mapping (SPEC: SDR con tone mapping semplice). | Monitor HDR: colori slavati o tagliati nelle clip. | Cattura FP16 e tone mapping nella conversione. | Assenza CONFERMATA; effetto PLAUSIBILE (rev. A) | **rimandata**: nessuno schermo HDR per verificare; con il pool BGRA8 il sistema converte già in SDR (luci tagliate, non slavate). Tone mapping vero: pool FP16 più shader, riusando quelli di m17 |
| m19 | minore | encoder | `cliprec-encode/src/ffi.rs:306` | 2 B-frame fissi su HEVC NVENC. | Pascal/Maxwell: FFmpeg rifiuta («Max B-frames exceed»), si scivola su H.264 e si perde l'HEVC predefinito. | Ritentare con `max_b_frames = 0` prima di cambiare codec. | PLAUSIBILE (rev. A) | **corretta**: `abae0de`. Test: `bframe_rejection_retries_without_bframes` (il caso reale richiede una GPU Pascal) |
| m20 | minore | motore | `cliprec-app/src/engine.rs:675-680` | `cleanup_stale` legge la cartella delle clip prima di avviare il buffer. | Cartella su NAS irraggiungibile: avvio del buffer bloccato (obiettivo < 1,5 s). | Pulizia su un thread a parte, dopo l'avvio. | PLAUSIBILE (rev. A) | **corretta senza test**: `93b296e` (pulizia su un thread a parte); non riproducibile senza un disco di rete lento |
| m21 | minore | hotkey | `cliprec-app/src/rawinput.rs:20-25` | Il mouse è sempre registrato in Raw Input: un `WM_INPUT` per ogni movimento (1-8 kHz). | Lavoro inutile durante il gioco anche senza hotkey sul mouse. | Registrare il mouse solo se una hotkey usa `MouseN`. | Registrazione CONFERMATA; costo PLAUSIBILE (rev. B) | **corretta**: `51eec9c`. Test: `m21_mouse_registered_only_when_a_hotkey_uses_it` |
| m22 | minore | tray | `cliprec-app/src/shell.rs:441-477` | Nessun gestore di `TrayIconEvent`: gli eventi finiscono in un canale senza limite che nessuno legge. | Ogni passaggio del mouse sull'icona fa crescere la coda per tutta la sessione. | `TrayIconEvent::set_event_handler(Some(\|_\| {}))`. | CONFERMATA (rev. B, lettura di tray-icon 0.26.1) | **corretta senza test**: `77b40e7`; non riproducibile (`TrayIconEvent::send` non è pubblico, servirebbe un clic reale) |
| m23 | minore | stato | `cliprec-app/src/engine.rs:246` | «fps effettivi» mostra gli fps configurati. | Il pannello di stato dice 60 anche quando se ne codificano 40. | Calcolarli da frame consegnati e persi. | CONFERMATA (rev. B) | **corretta**: `93b296e`. Test: `m23_status_fps_is_effective` |
| m24 | minore | impostazioni | `cliprec-settings/src/main.rs:310-325` | Il pulsante «Prova» non verifica nulla (SPEC: verifica audio e video). | Mostra «Clip di prova richiesta» anche se l'audio è muto o il video nero. | L'IPC restituisce la clip; controllo di tracce non silenziose e frame non neri. | CONFERMATA (rev. B) | **corretta**: `93b296e` (esito nello stato), `f6f60ad` (impostazioni). Test: `m24_test_clip_reports_a_verdict`, `test_result_is_presented_when_it_arrives` |
| m25 | minore | impostazioni | `cliprec-settings/src/sys.rs:115-121` | L'elenco delle finestre mostra solo il titolo (SPEC: titolo, icona, eseguibile). | Due finestre con lo stesso titolo non si distinguono. | Aggiungere eseguibile e icona. | CONFERMATA (rev. B) | **corretta in parte**: `d56f8ae` (eseguibile accanto al titolo). Test: `window_label_shows_the_executable`. Icona rimandata (conversione `HICON` → immagine Slint) |
| m26 | minore | audio, notifiche | `cliprec-audio/src/wasapi_source.rs:38-39` | Microfono negato dalla privacy: solo testo, nessun collegamento a `ms-settings:privacy-microphone`. | L'utente deve cercare da solo l'impostazione. | Pulsante nel toast che apre l'impostazione. | CONFERMATA (rev. B) | **corretta**: `262d8a9`. Test: `m26_microphone_privacy_errors_get_the_settings_button` |
| m27 | minore | mux | `cliprec-core/src/config.rs:231-241`; `cliprec-mux/src/lib.rs:301-305` | Mancano Opus e AAC per la traccia unica e il WAV polifonico chiesti da SPEC.md. | Chi vuole condividere una clip audio leggera deve convertirla a mano. | Aggiungere i formati. | CONFERMATA (rev. A) | **corretta**: `b10588b`. Test: `audio_clips_polywav_opus_aac`. Il WAV polifonico si ferma a 4 GB (RF64 se servisse) |
| m28 | minore | codice | `cliprec-app/src/engine.rs:89-91,532-555,590-591,608-609,865-871`; `shell.rs` (15 blocchi); `gamepad.rs:86`; `notify.rs:167`; `i18n.rs:190`; `autostart.rs:54` | `unsafe` fuori dai moduli FFI, contro la regola di CLAUDE.md (i `// SAFETY:` ci sono e sono veritieri). | Nessun malfunzionamento; regola violata e revisione più difficile. | Spostare le chiamate in `sys.rs` o in un modulo `win32.rs`. | CONFERMATA (rev. A e B, grep) | **rimandata**: spostare gli `unsafe` in un modulo FFI tocca molti file senza effetto per l'utente |

## I vincoli di Luca

| Vincolo | Esito | Prova |
| --- | --- | --- |
| 1. Esattamente due hotkey: clip video e clip audio | **Rispettato** | `cliprec-app/src/hotkey.rs:209-245` produce solo `SaveVideoClip` e `SaveAudioClip`; `HotkeyConfig` (`cliprec-core/src/config.rs:245-258`) ha solo video e audio; il controller facoltativo è legato alle stesse due azioni (`gamepad.rs:92`); «Prova» passa dalla pipe, non da una hotkey. Test `hotkey::tests` verdi. |
| 2. Sorgenti da app o dispositivo, tracce separate oppure tutte unite, scelta prima di registrare | **Rispettato, con riserve sulla qualità** (M3, M4, m3) | Sorgenti App/Input/Output in `config.rs:176-183`, `TrackMode` in `config.rs:151-157`, scelta all'avvio in `cliprec-audio/src/engine.rs:84-148`. Clip generate dai revisori: 2 tracce con i nomi in modalità separata, una traccia «Mix» in traccia unica; test `merged_and_mix_track_modes_end_to_end` verde. La cattura per app è provata solo su `explorer.exe`. |
| 3. Buffer esclusivamente in RAM, disco solo al salvataggio | **Rispettato** | Nessuna scrittura su file in capture, encode, audio e buffer (grep); arene con `VirtualLock` che funziona davvero (working set minimo uguale ai blocchi bloccati). Il disco si usa solo in `cliprec-mux` al salvataggio, più gli accessi di contorno di m1 e m20. Limiti: se `VirtualLock` fallisce le pagine possono finire nel file di paging (lo dice solo il pannello); con «Ferma buffer» i dati restano in RAM (m11). |
| 4. Stack moderno come da SPEC.md | **Non rispettato in tre punti; dopo le correzioni resta solo la toolchain** | `rustc 1.93.0` invece di ≥ 1.99.1 (DECISIONS, voce 1: aggiornare con `rustup update`); ripiego SVT-AV1 aggiunto (M6, `c3faafa`); FFmpeg 9.0.2 LGPL ora fissata alla release BtbN `autobuild-2026-10-19-13-06` con SHA-256 (M8, `b860cf8`). Il resto corrisponde: `windows` 0.62.2 soltanto, wasapi 0.26.0, rubato 5.0.1, flacenc 0.5.1, rtrb 0.4.0, ffmpeg-the-third 6.0.0, tray-icon 0.26.1, muda 0.21.2, Slint 1.18.1 con `AboutSlint`. |

## Lacune rispetto a OBS Replay Buffer e SteelSeries Moments (stato dopo le correzioni)

| Lacuna | Stato |
| --- | --- |
| Fullscreen esclusivo (OBS e Moments agganciano il gioco) | **Valutata, non implementata**: vedi «Valutazione: iniezione DLL» e DECISIONS 27. Restano rilevamento, avviso e suggerimento della finestra senza bordi |
| Risoluzione d'uscita fissa | **Fatta** (C1): «Nativa» = monitor del bersaglio; 1440p, 1080p e 720p scalano con bande nere (`output_can_be_scaled_below_native`) |
| Encoder software di riserva | **Fatto** (M6): SVT-AV1 dopo NVENC, AMF e QSV; 226 fps a 1080p60 e 112 a 1440p60 su rumore (release) |
| Clip corte senza avviso | **Fatto**: avviso per RAM, per clip più corta di N (m2), per clip ferma prima della pressione; audio che copre tutto il video (M4); snapshot prima del disco (m1) |
| Ritardo audio per sorgente («Sync Offset» di OBS) | **Fatto**: `delay_ms` da -1000 a 1000 ms (`6238337`), campo «ms» nelle impostazioni (`b9a892a`) |
| HDR | **Rimandata** (m18): serve uno schermo HDR per verificarla |
| B-frame adattati alla GPU, cursore con Desktop Duplication | **Fatti** (m19, m17) |
| Primo avvio: rilevamento degli encoder, «Prova» che verifica | **Fatti** (m14, m24): codec disponibili nello stato e nelle impostazioni, esito della prova con video e tracce audio |
| Automazione (Stream Deck, macro) | **Fatta**: `IpcRequest::SaveVideoClip` / `SaveAudioClip` sulla named pipe (`70c772f`) |
| «Ding» di salvataggio nelle clip | **Fatto** (m7): la sorgente «Sistema» predefinita esclude ClipRec |
| Distribuzione: runtime, licenze | **Fatti** (M1, M8). **Rimandati**: build FFmpeg ridotta per stare sotto i 40 MB (oggi 134 MB; richiede una toolchain MSYS2 o la cross-compilazione, piano in `installer/FFMPEG-RIDOTTA.md`), firma del codice (nessun certificato), installer compilato (Inno Setup non installato) |
| Mai provati dal vivo | **Restano da provare di giorno**: hotkey reali, toast, UAC, controller, AMF, monitor oltre 60 Hz, gioco vero con PresentMon, HDR |

ClipRec resta avanti su: audio per app senza Sonar né dispositivi virtuali, hotkey solo audio dallo stesso
buffer, tracce separate o unite a scelta, buffer davvero solo in RAM con pagine bloccate, file `.part` mai
scambiati per clip valide.

## Cosa hanno eseguito i revisori

- `cargo build --workspace --release`, `cargo clippy --workspace --all-targets -- -D warnings`: ok, pulito.
- Test di tutti i crate: verdi, salvo `vertical_chain_with_synthetic_sources` fallito una volta su due
  esecuzioni complete (m15). I test di cattura che usano lo schermo reale il revisore B non li ha eseguiti.
- `cliprec-app --prova N --sintetica` (10, 25, 65, 120 s) e varianti MKV/traccia unica: `xtask probe` ok,
  `xtask sync` A/V tra −1,4 e −13 ms, 0 ms tra tracce; snapshot 0,2-0,5 ms; scrittura 0,2-0,9 s.
- `dumpbin /dependents`, `ffmpeg -version`, `cargo xtask dist`, programmi di prova propri (in
  `target/revisore-a/` e `target/revB/`): misure citate nelle falle.

Nessuna correzione è stata applicata: si decide dopo la lettura di questa revisione.
