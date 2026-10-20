# Stato di ClipRec

Ultimo aggiornamento: 20 ottobre 2026, 03:40.

## Fatto

- Fase 0: workspace Cargo (9 crate + `xtask` + `tests`), contratti in `cliprec-core` con test,
  FFmpeg 9.0.2 LGPL in `third_party/ffmpeg` (link verificato da un test), `CLAUDE.md`, agenti in
  `.claude/agents/`, regole in `.claude/rules/`, hook in `.claude/settings.json`.

## In corso

- Fase 1: subagenti in parallelo su capture, encode, audio, buffer+mux, app-shell, test-harness.

## Prossimo passo

- Integrazione della catena verticale: una clip video MP4 e una clip audio salvate su disco.

## Problemi aperti

- Nessuno per ora.
