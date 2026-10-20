---
paths:
  - "crates/cliprec-buffer/**"
  - "crates/cliprec-mux/**"
---
# Buffer e salvataggio
- Il buffer non tocca mai il disco; il file si scrive solo al salvataggio, come `.part` poi rinominato.
- Scarto a GOP interi; la clip parte dal keyframe che precede l'istante richiesto.
- Lo snapshot prende riferimenti sotto un lock brevissimo, senza copiare i byte dei pacchetti.
