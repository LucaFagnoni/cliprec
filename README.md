# ClipRec

Registratore sempre attivo per Windows, scritto in Rust. Tiene in RAM gli ultimi minuti di
gioco e, quando premi una hotkey, salva su disco una clip.

- **Due hotkey, nient'altro:** *Clip video* (ultimi 60 s di video e audio, MP4) e *Clip audio*
  (ultimi 120 s di solo audio, un file FLAC per sorgente).
- **Buffer solo in RAM:** contiene pacchetti già compressi; il disco si tocca solo al salvataggio.
- **Tracce audio separate:** gioco, microfono, Discord, uscita di sistema... ognuna nella sua
  traccia, con nome, tutte sincronizzate.
- **Nessuna iniezione nei giochi:** cattura con Windows.Graphics.Capture (ripiego: Desktop
  Duplication), quindi nessun conflitto con gli anti-cheat. Nessun overlay.
- Vive nella tray; le impostazioni sono in un processo separato (`cliprec-settings`) che si chiude
  a fine uso.

Specifica completa in `SPEC.md`, decisioni in `docs/DECISIONS.md`, stato in `docs/PROGRESS.md`.

## Requisiti

- Windows 10 versione 2004 (build 19041) o successiva, 64 bit.
- GPU con encoder hardware: NVIDIA (NVENC), Intel (Quick Sync) o AMD (AMF).
- Rust 1.93 (edition 2024) e i Build Tools di Visual Studio 2022 per compilare.

## Compilare

```powershell
cargo xtask ffmpeg-ridotta         # una volta: compila FFmpeg 9 LGPL ridotta in third_party\ffmpeg-ridotta (~10 min)
cargo xtask ffmpeg                 # una volta: FFmpeg completa con ffmpeg/ffprobe, solo per i test
cargo build --release              # target\release\cliprec-app.exe e cliprec-settings.exe
cargo xtask dist                   # target\dist\ClipRec: exe, DLL FFmpeg e runtime VC++, licenze
cargo xtask notices                # rigenera THIRD_PARTY_NOTICES.md
# pacchetti da distribuire, in target\dist:
& "$env:LOCALAPPDATA\Programs\Inno Setup 7\ISCC.exe" /Qp installer\cliprec.iss   # ClipRec-Setup.exe
tar.exe -a -c -f target\dist\ClipRec-0.1.0-portable.zip -C target\dist ClipRec      # zip portable
```

`.cargo\config.toml` contiene percorsi assoluti (`FFMPEG_DIR`, `FFMPEG_TOOLS_DIR`, `LIBCLANG_PATH`) da adattare su un
altro PC. L'installer (Inno Setup 7, installato per utente da jrsoftware.org) si compila dopo
`cargo xtask dist`; è per utente, senza amministratore, ed è descritto nello script stesso. Se sul
PC manca il runtime Visual C++, l'installer lo scarica da Microsoft durante l'installazione. Senza
firma del codice Windows SmartScreen mostra «Editore sconosciuto» al primo avvio. La
build FFmpeg ridotta (solo i componenti usati, LGPL 3) è descritta in `installer\FFMPEG-RIDOTTA.md`;
`cargo xtask ffmpeg-ridotta` scarica MSYS2 in `third_party\toolchain` e la compila.

## Provarlo senza toccare il gioco

```powershell
# registra 10 s con sorgenti sintetiche, salva una clip video e una audio, scrive prova.txt ed esce
target\release\cliprec-app.exe --prova 10 --sintetica --cartella C:\temp\prova

cargo xtask probe C:\temp\prova\clip.mp4 --tracks 2   # valida il file con ffprobe
cargo xtask sync  C:\temp\prova\clip.mp4              # misura la sincronia (lampi e bip)
cargo xtask gen-test-clip C:\temp\ref.mp4             # clip di riferimento

# con l'app in esecuzione: parla con il core sulla named pipe
scripts\pipe.ps1 Status TestClip Quit

cliprec-app --sessione-lunga 60 C:\temp\soak          # memoria e frame persi nel tempo
```

Variabili d'ambiente per i test: `CLIPREC_CONFIG` (percorso di `config.toml`), `CLIPREC_LOG_DIR`
(cartella dei log), `CLIPREC_HEADLESS=1` (niente toast, suoni, avvio automatico, collegamento nel
menu Start né finestra impostazioni al primo avvio).

## Uso

| Azione | Hotkey predefinita |
| --- | --- |
| Clip video | Ctrl+Alt+F10 |
| Clip audio | Ctrl+Alt+F9 |

Le hotkey si cambiano nelle impostazioni (anche con i tasti laterali del mouse). Facoltativo: le
stesse azioni da un controller XInput, per esempio `video_pad = "Back+A"` nella sezione `[hotkeys]`
(tasti A, B, X, Y, LB, RB, LS, RS, Back, Start, DPadUp/Down/Left/Right). Il menu della tray mostra
i secondi coperti e la RAM usata, avvia o ferma il buffer, elenca le ultime clip e apre le
impostazioni. Al primo avvio ClipRec scrive la configurazione predefinita (video automatico, audio
di sistema e microfono, più Discord se è aperto) e apre le impostazioni; il pulsante «Prova» salva
una clip di 5 s. Interfaccia in italiano e inglese (`language = "it"`, `"en"` o `"auto"`).

## Aggiornamenti

All'avvio e ogni giorno ClipRec cerca una versione nuova tra le release di
[github.com/LucaFagnoni/cliprec](https://github.com/LucaFagnoni/cliprec/releases) e chiede se
installarla: scarica lo zip portable, ne verifica lo SHA-256, sostituisce i propri file e si
riavvia, senza rilanciare il setup e senza permessi di amministratore. Si disattiva in
Impostazioni → Informazioni; dalla tray, «Cerca aggiornamenti…».

Per pubblicare una versione: alzare `version` in `Cargo.toml` del workspace, poi
`cargo build --release`, `cargo xtask dist`, l'installer con `/DAppVersion=x.y.z`, lo zip
`ClipRec-x.y.z-portable.zip` e il suo `.sha256`, e una release `vx.y.z` con i tre file.

## Dove sono i file

| Cosa | Percorso |
| --- | --- |
| Configurazione | `%APPDATA%\ClipRec\config.toml` (portable: `config.toml` accanto all'exe) |
| Log (rotanti) | `%LOCALAPPDATA%\ClipRec\logs` |
| Clip | `%USERPROFILE%\Videos\ClipRec` (cambiabile) |

## Limiti noti

- **Fullscreen esclusivo**: quando un gioco a schermo intero esclusivo sfugge alla cattura (frame
  neri o fermi: un secondo se Windows segnala l'esclusivo, altrimenti 3-5) ClipRec passa da solo alla Desktop Duplication
  del monitor, sia con la cattura del gioco sia con quella del monitor, e torna indietro quando il
  gioco esce dallo schermo intero. Nessuna iniezione nel gioco, quindi nessun rischio con gli
  anti-cheat; resta escluso solo ciò che Windows protegge (contenuti DRM).
- **Frame persi sotto carico**: i frame ripetuti e i riempitivi dei tick persi non costano più
  lavoro alla scheda video (si rimanda la superficie già convertita), quindi sotto carico se ne
  perdono meno. Se il gioco satura la GPU qualche tick può ancora saltare, contato in «frame persi»;
  i buchi fino a 250 ms si riempiono ripetendo il frame, così il file resta a frequenza costante.
  Il colore 4:4:4 li aumenta.
- **AMD (AMF)**: scritto e con un ripiego in VBR semplice, ma provato solo senza una Radeon. Se
  l'encoder AMD non compare, il log (`%LOCALAPPDATA%\ClipRec\logs`) dice perché.
- **Giochi avviati come amministratore**: mentre un programma elevato è in primo piano le hotkey
  della tastiera passano a `RegisterHotKey`, che Windows consegna anche da lì. Restano scoperte le
  hotkey col mouse (o una combinazione già presa da un altro programma): ClipRec lo segnala, e la
  soluzione è «Avvia come amministratore» (Impostazioni → Avanzate) o «Riavvia come amministratore».
- **Audio in modalità esclusiva** (WASAPI exclusive, es. osu!): l'audio esclusivo non passa dal
  mixer e nessun programma lo può registrare. Con «Blocca la modalità esclusiva» (attiva di
  serie) ClipRec toglie ai dispositivi che registra il permesso di essere presi in esclusiva, così
  il gioco usa la modalità condivisa; un gioco già aperto va riavviato una volta.
- **Router audio** (SteelSeries Sonar, Voicemeeter, Wave Link): rifanno il mix su dispositivi
  virtuali. «Aggiungi i canali del router» (Impostazioni → Audio) registra ogni canale (gioco, chat,
  media) in una traccia, senza il microfono. «Tutto tranne un'app» registra i flussi di tutti i
  programmi e quindi anche il mix del router: con un router meglio i canali.
- **Firma del codice**: gli exe non sono firmati (nessun certificato): al primo avvio SmartScreen
  mostra «Editore sconosciuto».

## Licenze

ClipRec usa FFmpeg (LGPL, DLL separate e sostituibili) e Slint (royalty-free 2.0, attribuzione
nella pagina Informazioni). Elenco completo in `THIRD_PARTY_NOTICES.md`.
