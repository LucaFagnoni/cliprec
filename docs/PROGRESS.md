# Stato di ClipRec

Ultimo aggiornamento: 20 ottobre 2026, 03:45.

## Fatto

- Fase 0 (commit `f36888e`, `1902394`): workspace Cargo (9 crate + `xtask` + `tests`), contratti in
  `cliprec-core` con test, FFmpeg 9.0.2 LGPL in `third_party/ffmpeg` (link verificato da un test),
  `CLAUDE.md`, agenti in `.claude/agents/`, regole in `.claude/rules/`, hook in `.claude/settings.json`.

## In corso

- Fase 1, avviata alle 03:41: sei subagenti in worktree separati, consegna entro le 06:15.
  - `video-capture` (Opus, xhigh): `cliprec-capture`.
  - `video-encoder` (Opus, xhigh): `cliprec-encode`.
  - `audio-engine` (Opus, high): `cliprec-audio`.
  - `replay-buffer` (Opus, high): `cliprec-buffer` e `cliprec-mux`.
  - `app-shell` (Sonnet, medium): `cliprec-app`.
  - `test-harness` (Sonnet, medium): `xtask` e `tests/`.
  - `settings-ui` parte quando si libera un posto (massimo 6 subagenti insieme).
- Bozza del motore di integrazione in `crates/cliprec-app/src/engine.rs` (non ancora collegata),
  con la modalità di prova `run_test_clip` a sorgenti sintetiche.

## Prossimo passo

- Unire i rami uno alla volta, collegare il motore, salvare una clip video MP4 e una clip audio
  con le sorgenti sintetiche e validarle con `cargo xtask probe` e `cargo xtask sync`.

## Problemi aperti

- Coordinamento con pokewilds (03:47): le sessioni di pokewilds finiscono entro le 09:00 e nessuna deve
  restare attiva dopo. La sessione «Pokewilds Godot iOS build setup» dice che Luca le ha chiesto lo
  spegnimento forzato e se ne occupa lei (watcher su `.coordinamento/`, `shutdown /s /f /t 60` quando
  entrambe le sessioni hanno scritto il file di fine, o comunque alle 09:30). ClipRec non spegne il PC
  (AVVIO.md, regola 6) e scrive `.coordinamento/cliprec-finito.txt` solo con la conferma diretta di
  Luca. Vedi DECISIONS.md, voce 12.
