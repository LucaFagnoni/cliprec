# Stato di ClipRec

Ultimo aggiornamento: 20 ottobre 2026, 04:40.

## Fatto

- **Fase 0** (03:30-03:40): workspace Cargo (9 crate + `xtask` + `tests`), contratti in `cliprec-core`,
  FFmpeg 9.0.2 LGPL in `third_party/ffmpeg`, `CLAUDE.md`, agenti, regole, hook.
- **Fase 1** (03:41-04:06): sette subagenti in worktree separati, tutti uniti su `main`.
  - `cliprec-capture`: WGC su monitor e finestre, Desktop Duplication di ripiego, pacer CFR a triplo
    buffer, modalità automatica, frame neri o fermi segnalati, `SyntheticSource` con lampo a ogni secondo.
  - `cliprec-encode`: `hevc_nvenc`/`h264_nvenc` zero-copy da texture D3D11, `hevc_qsv`/`h264_qsv` sulla
    UHD 770, BGRA→NV12 BT.709 con bande nere, `open_best`/`open_after`/`probe`. AMF scritto, non provato.
  - `cliprec-audio`: `WasapiSource` (app con process loopback, ingresso, uscita), silenzio continuo,
    mix con deriva corretta (0,03 ms su un'ora simulata), limiter, MMCSS, riconnessione, `BeepSource`.
  - `cliprec-buffer`: arena di blocchi riciclati e bloccati in RAM, scarto a GOP interi, FLAC per
    traccia, snapshot senza copie, `AudioRefs` (decompressione fuori dal lock).
  - `cliprec-mux`: MP4/MKV con AAC per traccia e nomi, prima traccia predefinita, `+faststart`,
    `.part` univoco rinominato, nomi mai sovrascritti, controllo spazio; clip audio FLAC/WAV o MKA.
  - `cliprec-app`: tray, hotkey Raw Input (anche tasti del mouse), istanza unica, supervisore, named
    pipe, toast (non provati di notte), log rotante.
  - `cliprec-settings`: finestra Slint a sei schede con `AboutSlint`, stima RAM, client pipe, it/en.
  - `xtask`/`tests`: `probe`, `sync`, `gen-test-clip`, `bench-report`, `testwindow`, script PresentMon/WPR.
- **Fase 2** (04:10): motore collegato; catena verticale verificata da un test end-to-end.
- **Fase 3** (04:20-04:40): revisione avversaria (25 voci) e correzioni dei proprietari dei crate:
  encoder aperto fuori dal pacer con fallback e backoff, stati delle sorgenti notificati, solo audio se
  il video non parte, buffer che sopravvive a riavvii e panic, ricarica della config solo se serve,
  device perso e cambio di GPU gestiti, keyframe solo IDR, `.part` univoci, sorgenti audio in errore
  ritentate, segnaposto della dimensione del bersaglio, test di iniezione dei guasti, primo avvio.
- **Fase 4, parte fattibile** (04:40): `cargo xtask dist` e `notices`, installer Inno Setup (non
  compilato), `README.md`, `THIRD_PARTY_NOTICES.md`, piano per la build FFmpeg ridotta.

## Misure (release, RTX 3080, sorgenti sintetiche 1080p60 HEVC)

| Metrica | Soglia | Misurato |
| --- | --- | --- |
| Avvio a freddo → encoder pronto | < 1,5 s | 0,32 s |
| Hotkey → snapshot | < 20 ms | 0,7 ms (video), 0,14 ms (audio) |
| Hotkey → file, clip di 60 s | < 1 s | 0,69 s (con la clip audio scritta in parallelo) |
| Durata della clip | ≥ N, ≤ N + 1 GOP | 60,77 s per N = 60 |
| Sincronia tra tracce | ±20 ms | 0 ms |
| Sincronia A/V (sintetica) | ±20 ms | −16 ms (un frame, artefatto della misura) |
| CPU del processo (sessione lunga) | < 2% | circa 4% di un core = 0,15% della CPU |
| RAM del processo esclusi i buffer | < 50 MB | working set circa 16 MB (98 MB totali con 82 MB di buffer); memoria privata impegnata 336 MB con buffer di 19 MB, quasi tutta del driver NVENC/CUDA (da confermare con VMMap) |
| Distribuzione | < 40 MB | 129,9 MB oggi (FFmpeg completa); 26-30 MB stimati con la build ridotta |

## In corso

- Sessione lunga dalle 04:18 alle 07:48 circa (`target/sessione-lunga/sessione-lunga.csv`): al minuto
  16 memoria privata stabile a 406 MB, 2 frame persi (all'avvio), una clip ogni 10 minuti.
- Seconda revisione avversaria (fine Fase 3).

## Prossimo passo

- Correzioni dalla seconda revisione; misura A/V con cattura reale (lampo della finestra di test più
  un bip, da fare di giorno perché il bip suona dagli altoparlanti).

## Problemi aperti

- Coordinamento con pokewilds (03:47): le sessioni di pokewilds finiscono entro le 09:00 e nessuna deve
  restare attiva dopo. La sessione «Pokewilds Godot iOS build setup» dice che Luca le ha chiesto lo
  spegnimento forzato e se ne occupa lei (watcher su `.coordinamento/`, `shutdown /s /f /t 60` quando
  entrambe le sessioni hanno scritto il file di fine, o comunque alle 09:30). ClipRec non spegne il PC
  (AVVIO.md, regola 6) e scrive `.coordinamento/cliprec-finito.txt` solo con la conferma diretta di
  Luca. Vedi DECISIONS.md, voce 12.
