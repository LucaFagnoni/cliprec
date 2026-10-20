# Prova completa di ClipRec senza tastiera e senza schermo dell'utente (le stesse prove della notte
# del 20 ottobre 2026). Uso, dalla radice del repository:  scripts\prova-completa.ps1
# 1. build release; 2. clip sintetica (video+audio) validata con probe e sync;
# 3. app completa sulla finestra di `cargo xtask testwindow`: Status, TestClip, ReloadConfig, Quit.
# Non suona nulla, non scrive nel registro né nel menu Start (CLIPREC_HEADLESS), lavora in target\prova-completa.
# Continue: cargo scrive su stderr anche quando va tutto bene; si controllano i codici d'uscita.
$ErrorActionPreference = 'Continue'
Set-Location (Split-Path $PSScriptRoot -Parent)
$out = Join-Path (Get-Location) 'target\prova-completa'
Remove-Item -Recurse -Force $out -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $out | Out-Null
$env:CLIPREC_LOG_DIR = Join-Path $out 'log'
$env:CLIPREC_HEADLESS = '1'
$esiti = @()
function Esito($nome, $ok, $dettaglio) { $script:esiti += [pscustomobject]@{ Prova = $nome; Esito = $(if ($ok) { 'OK' } else { 'FALLITA' }); Dettaglio = $dettaglio } }

cargo build --release -p cliprec-app -p xtask
if ($LASTEXITCODE -ne 0) { throw 'build fallita' }
$app = Join-Path (Get-Location) 'target\release\cliprec-app.exe'

# 2. Clip sintetica
$sint = Join-Path $out 'sintetica'
& $app --prova 12 --sintetica --cartella $sint | Out-Null
Get-Process cliprec-app -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $app } | Wait-Process -Timeout 120
$video = Get-ChildItem $sint -Filter *.mp4 | Select-Object -First 1
$audio = Get-ChildItem $sint -Directory | Select-Object -First 1
if (-not $audio) { $audio = Get-ChildItem $sint -Filter *.flac | Select-Object -First 1 }
Esito 'clip sintetica salvata' ($null -ne $video -and $null -ne $audio) (Get-Content (Join-Path $sint 'prova.txt') -Raw)
if ($video) {
    $p = cargo xtask probe $video.FullName --duration 11 --tracks 2 --fps 60 2>&1 | Out-String
    Esito 'probe clip video' ($LASTEXITCODE -eq 0) $p.Trim()
    $s = cargo xtask sync $video.FullName 2>&1 | Out-String
    Esito 'sincronia clip video' ($LASTEXITCODE -eq 0) $s.Trim()
}
if ($audio) {
    $s = cargo xtask sync $audio.FullName 2>&1 | Out-String
    Esito 'sincronia clip audio' ($LASTEXITCODE -eq 0) $s.Trim()
}

# 3. App completa sulla finestra di test. Con un ClipRec già aperto (quello dell'utente) la pipe
# porterebbe a lui: Status, TestClip e Quit finirebbero sulla sua istanza. Allora si salta.
$altro = Get-Process cliprec-app -ErrorAction SilentlyContinue
if ($altro) {
    Esito 'app completa' $false "saltata: un ClipRec è già in esecuzione (PID $($altro.Id -join ', ')); chiudilo e rilancia"
    $esiti | Format-Table -AutoSize -Wrap
    exit 1
}
$cfg = Join-Path $out 'config.toml'
@"
[video]
fps = 60

[video.source]
kind = "window"
title = "ClipRec testwindow"

[[audio.sources]]
name = "Sistema"
kind = "output"

[clips]
folder = '$out\finestra'
"@ | ForEach-Object { [IO.File]::WriteAllText($cfg, $_, [Text.UTF8Encoding]::new($false)) }
# Senza questa config l'app partirebbe con i predefiniti e catturerebbe il monitor: in quel caso ci si ferma.
if (-not ((Test-Path $cfg) -and (Get-Content $cfg -Raw) -match 'ClipRec testwindow')) { throw "config di prova non scritta: $cfg" }
$env:CLIPREC_CONFIG = $cfg
$finestra = Start-Process -FilePath 'target\release\xtask.exe' -ArgumentList 'testwindow', '--seconds', '35' -PassThru
Start-Sleep -Seconds 2
$proc = Start-Process -FilePath $app -PassThru
Start-Sleep -Seconds 7
$st = & .\scripts\pipe.ps1 Status | Out-String
if ($st -notmatch 'testwindow') {
    # Mai salvare una clip di un'altra sorgente (per esempio il monitor dell'utente).
    & .\scripts\pipe.ps1 Quit | Out-Null
    throw "la sorgente non è la finestra di test: $st"
}
Esito 'stato dalla pipe' ($st -match '"buffer_running":true') $st.Trim()
& .\scripts\pipe.ps1 TestClip | Out-Null
Start-Sleep -Seconds 3
[IO.File]::WriteAllText($cfg, ((Get-Content $cfg -Raw) -replace '(?m)^fps = 60', 'fps = 30'), [Text.UTF8Encoding]::new($false))
& .\scripts\pipe.ps1 ReloadConfig | Out-Null
Start-Sleep -Seconds 4
$st2 = & .\scripts\pipe.ps1 Status | Out-String
Esito 'ricarica della configurazione (fps 30)' ($st2 -match '"fps":30') $st2.Trim()
& .\scripts\pipe.ps1 Quit | Out-Null
$proc | Wait-Process -Timeout 30 -ErrorAction SilentlyContinue
Esito 'uscita pulita' ($proc.HasExited -and $proc.ExitCode -eq 0) "codice $($proc.ExitCode)"
$clip = Get-ChildItem "$out\finestra" -Filter *.mp4 -ErrorAction SilentlyContinue | Select-Object -First 1
if ($clip) {
    $p = cargo xtask probe $clip.FullName --duration 5 --tracks 1 --names Sistema --fps 60 2>&1 | Out-String
    Esito 'clip di prova dalla pipe' ($LASTEXITCODE -eq 0) $p.Trim()
} else { Esito 'clip di prova dalla pipe' $false 'nessun file' }
$finestra | Wait-Process -Timeout 60 -ErrorAction SilentlyContinue
Remove-Item Env:CLIPREC_CONFIG

$esiti | Format-Table -AutoSize -Wrap
if ($esiti.Esito -contains 'FALLITA') { exit 1 }
