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
cargo xtask ffmpeg                 # una volta: scarica FFmpeg 9 LGPL in third_party\ffmpeg
cargo build --release              # target\release\cliprec-app.exe e cliprec-settings.exe
cargo xtask redist                 # una volta: scarica vc_redist.x64.exe (Microsoft) per l'installer
cargo xtask dist                   # target\dist\ClipRec: exe, DLL FFmpeg e runtime VC++, licenze
cargo xtask notices                # rigenera THIRD_PARTY_NOTICES.md
```

`.cargo\config.toml` contiene percorsi assoluti (`FFMPEG_DIR`, `LIBCLANG_PATH`) da adattare su un
altro PC. L'installer (Inno Setup 7) si compila con `ISCC.exe installer\cliprec.iss` dopo
`cargo xtask dist`; è per utente, senza amministratore, ed è descritto nello script stesso. La
build FFmpeg ridotta, necessaria per stare sotto i 40 MB, è pianificata in
`installer\FFMPEG-RIDOTTA.md`.

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

## Dove sono i file

| Cosa | Percorso |
| --- | --- |
| Configurazione | `%APPDATA%\ClipRec\config.toml` (portable: `config.toml` accanto all'exe) |
| Log (rotanti) | `%LOCALAPPDATA%\ClipRec\logs` |
| Clip | `%USERPROFILE%\Videos\ClipRec` (cambiabile) |

## Limiti noti

- **Fullscreen esclusivo non catturabile** (nessuna iniezione nel gioco): ClipRec avvisa e suggerisce
  la modalità finestra senza bordi.
- **AMF (AMD) non provato**: le opzioni sono scritte ma su questo PC non c'è una Radeon.
- I toast e il collegamento nel menu Start non sono stati provati di notte; l'avvio automatico
  scrive la chiave `HKCU\...\Run`.
- Giochi avviati come amministratore: Windows può non consegnare le hotkey.
- App che usano WASAPI esclusivo non sono catturabili in loopback.
- La distribuzione è oggi di circa 134 MB (FFmpeg completa); l'obiettivo di 40 MB richiede la
  build ridotta. Nessun certificato di firma disponibile: gli exe non sono firmati.

## Licenze

ClipRec usa FFmpeg (LGPL, DLL separate e sostituibili) e Slint (royalty-free 2.0, attribuzione
nella pagina Informazioni). Elenco completo in `THIRD_PARTY_NOTICES.md`.
