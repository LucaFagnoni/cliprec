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
