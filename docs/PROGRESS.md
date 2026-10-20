# Stato di ClipRec

Ultimo aggiornamento: 20 ottobre 2026, 04:20.

## Fatto

- **Fase 0** (03:30-03:40): workspace Cargo (9 crate + `xtask` + `tests`), contratti in `cliprec-core`,
  FFmpeg 9.0.2 LGPL in `third_party/ffmpeg`, `CLAUDE.md`, agenti, regole, hook.
- **Fase 1** (03:41-04:06): sette subagenti in worktree separati, tutti uniti su `main` con clippy
  pulito e test verdi.
  - `cliprec-capture`: WGC su monitor e finestre (`MinUpdateInterval` 1/(2·fps)), Desktop Duplication
    di ripiego, pacer CFR a triplo buffer (ritardo mediano 0,15 ms), modalità automatica, rilevamento
    di frame neri o fermi, `SyntheticSource` con lampo a ogni secondo QPC.
  - `cliprec-encode`: `hevc_nvenc`/`h264_nvenc` zero-copy da texture D3D11 (300-490 fps in debug),
    `hevc_qsv`/`h264_qsv` sulla UHD 770, BGRA→NV12 BT.709 con bande nere, `open_best` e `probe`. AMF
    scritto ma non provato (nessuna GPU AMD).
  - `cliprec-audio`: `WasapiSource` (app con process loopback, ingresso, uscita), silenzio continuo,
    mix con deriva corretta (errore 0,03 ms su un'ora simulata), limiter, `BeepSource`, MMCSS,
    riconnessione dei dispositivi.
  - `cliprec-buffer`: arena di blocchi da 4 MiB riciclati, scarto a GOP interi, FLAC a 24 bit per
    traccia, `VirtualLock` (bloccate su questo PC), snapshot senza copie.
  - `cliprec-mux`: MP4/MKV con AAC per traccia, nomi delle tracce, prima traccia predefinita,
    `+faststart`, `.part` rinominato, controllo spazio; clip audio FLAC/WAV per traccia o MKA.
    Clip di 60 s 1080p60 con 4 tracce scritta in 0,77 s (release).
  - `cliprec-app`: tray con stato e ultime clip, hotkey Raw Input (anche tasti del mouse), istanza
    unica, supervisore con riavvio, server named pipe, toast (non provati di notte), log rotante.
  - `cliprec-settings`: finestra Slint a sei schede con `AboutSlint`, stima RAM dal vivo, client pipe,
    italiano e inglese; 29,7 MB di working set in release.
  - `xtask` e `tests/`: `probe` (ffprobe), `sync` (lampi e bip), `gen-test-clip`, `bench-report`,
    script PresentMon e WPR, `testwindow`.
- **Fase 2, catena verticale** (04:10, commit `a4d2275`): il motore vero è collegato all'app. Con
  sorgenti sintetiche il test end-to-end salva una clip video MP4 (7,8 s, 60 fps costanti, due tracce
  AAC con i nomi) e una clip audio (un FLAC per traccia); ffprobe la valida, la sincronia tra tracce è
  0 ms e quella A/V −16 ms (un frame: il lampo cade sul primo frame dopo il secondo intero).

## In corso

- Revisione avversaria di fine Fase 1 (`reviewer`, Opus, sola lettura).
- Build release e sessione lunga con sorgenti sintetiche (memoria e frame persi nel tempo).

## Prossimo passo

- Correggere le falle trovate dal revisore; prova con cattura reale di una finestra di test;
  misure (avvio a freddo, hotkey → snapshot, hotkey → file, RAM del processo).

## Problemi aperti

- Coordinamento con pokewilds (03:47): le sessioni di pokewilds finiscono entro le 09:00 e nessuna deve
  restare attiva dopo. La sessione «Pokewilds Godot iOS build setup» dice che Luca le ha chiesto lo
  spegnimento forzato e se ne occupa lei (watcher su `.coordinamento/`, `shutdown /s /f /t 60` quando
  entrambe le sessioni hanno scritto il file di fine, o comunque alle 09:30). ClipRec non spegne il PC
  (AVVIO.md, regola 6) e scrive `.coordinamento/cliprec-finito.txt` solo con la conferma diretta di
  Luca. Vedi DECISIONS.md, voce 12.
