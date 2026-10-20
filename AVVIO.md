# Avvio della sessione notturna di ClipRec

## Come avviarla

1. Apri Claude Code nella cartella `D:\Repositories\cliprec`: dall'app desktop (Code → nuova sessione → cartella) oppure da un terminale con `cd D:\Repositories\cliprec` e poi `claude`.
2. Scegli il modello **Opus 5.5** e la modalità permessi **Auto**.
3. Incolla il prompt qui sotto e invia.

## Prompt da incollare

```text
Sei l'agente principale del progetto ClipRec in D:\Repositories\cliprec. Leggi SPEC.md (la specifica completa) e docs/RESEARCH.md (la ricerca sullo stack).

Stanotte lavori senza supervisione fino alle 09:00 (ora italiana) del 20 ottobre 2026: Luca dorme e non può rispondere. Regole per stanotte:

1. Non aspettare conferme. Dove SPEC.md dice di fermarti per la mia conferma, adotta le scelte marcate [DEFAULT] e [PROPOSTA], annota ogni decisione in docs/DECISIONS.md e prosegui.
2. Esegui le fasi nell'ordine (0, 1, 2, 3). Dai priorità a una catena verticale funzionante, cioè una clip video e una clip audio salvate davvero su disco, prima della rifinitura.
3. Usa i subagenti come descritto nella sezione «Squadra di subagenti, modelli ed effort», con il modello e l'effort indicati per ciascuno.
4. Se la cartella non è un repository git, esegui git init. Fai commit frequenti su main; nessun push.
5. Tieni aggiornato docs/PROGRESS.md: fatto, in corso, prossimo passo, problemi aperti.
6. Resta dentro D:\Repositories\cliprec: non toccare altre cartelle, in particolare D:\Repositories\pokewilds, dove lavora un'altra sessione. Non spegnere e non riavviare il PC.
7. Di notte niente audio dagli altoparlanti e niente finestre a schermo intero: per i test usa sorgenti sintetiche.
8. Dalle 08:40 non iniziare lavoro nuovo: integra solo ciò che compila e passa i test, fai commit e scrivi in cima a docs/PROGRESS.md un riepilogo per Luca (cosa funziona, come provarlo, cosa manca, decisioni da confermare). Alle 09:00 fermati.
```

## Spegnimento automatico del PC (facoltativo)

Lo programmi tu, a un orario in cui sia questa sessione sia quella di `pokewilds` hanno finito. Esempio per le 09:30, da un terminale:

```text
schtasks /create /tn "SpegniDopoNotte" /tr "shutdown /s /f /t 120" /sc once /st 09:30
```

Per annullarlo prima dell'orario:

```text
schtasks /delete /tn "SpegniDopoNotte" /f
```

Durante i 120 secondi di conto alla rovescia puoi ancora fermarlo con `shutdown /a`.

Prima di andare a dormire controlla che il PC non vada in sospensione: Impostazioni → Sistema → Alimentazione → Sospensione: «Mai».
