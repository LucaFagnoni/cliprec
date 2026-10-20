<#
.SYNOPSIS
  Cattura fps con PresentMon 2.4 (CLI, CSV) su un processo di gioco, in una delle tre fasi del protocollo.

.DESCRIPTION
  Protocollo (SPEC, "Requisiti di prestazione"): stesso gioco, stessa scena, stesso percorso, ripetuto per
    off      ClipRec chiuso
    idle     ClipRec attivo, buffer in riempimento, nessun salvataggio
    saving   ClipRec attivo; durante la cattura premi la hotkey "Clip video" (una volta, a meta' cattura)
  Poi confronta i CSV:  cargo xtask bench-report out\bench\off.csv out\bench\idle.csv
  Obiettivo: perdita di fps medi sotto il 2%.

  PresentMon NON e' incluso e questo script non lo scarica. Scarica PresentMon 2.4.x CLI (release di
  GameTechDev/PresentMon, file PresentMon-2.4.x-x64.exe) e passa il percorso con -PresentMonExe, oppure
  mettilo nel PATH. Dalla 2.3.1 non servono privilegi di amministratore.

.EXAMPLE
  .\scripts\bench\presentmon.ps1 -Phase off -ProcessName game.exe -PresentMonExe C:\Tools\PresentMon-2.4.1-x64.exe
#>
param(
    [Parameter(Mandatory)][ValidateSet('off', 'idle', 'saving')][string]$Phase,
    [Parameter(Mandatory)][string]$ProcessName,
    [int]$Seconds = 120,
    [int]$WarmupSeconds = 10,
    [string]$PresentMonExe = 'PresentMon.exe',
    [string]$OutDir = (Join-Path $PSScriptRoot '..\..\out\bench')
)
$ErrorActionPreference = 'Stop'

if (-not (Get-Command $PresentMonExe -ErrorAction SilentlyContinue)) {
    throw "PresentMon non trovato ('$PresentMonExe'). Scaricalo a mano da https://github.com/GameTechDev/PresentMon/releases (non viene scaricato dallo script)."
}
if (-not (Get-Process -Name ($ProcessName -replace '\.exe$', '') -ErrorAction SilentlyContinue)) {
    throw "Il processo '$ProcessName' non e' in esecuzione: avvia il gioco prima."
}
$cliprec = Get-Process -Name 'cliprec-app' -ErrorAction SilentlyContinue
if ($Phase -eq 'off' -and $cliprec) { throw "Fase 'off': chiudi ClipRec (cliprec-app.exe) prima di misurare." }
if ($Phase -ne 'off' -and -not $cliprec) { throw "Fase '$Phase': avvia ClipRec (cliprec-app.exe) prima di misurare." }

New-Item -ItemType Directory -Force $OutDir | Out-Null
$csv = Join-Path $OutDir "$Phase.csv"
Write-Host "Riscaldamento di $WarmupSeconds s, poi cattura di $Seconds s in $csv"
if ($Phase -eq 'saving') { Write-Host "Premi la hotkey 'Clip video' una volta a meta' cattura (circa al secondo $([int]($Seconds / 2)))." }
Start-Sleep -Seconds $WarmupSeconds

& $PresentMonExe --process_name $ProcessName --output_file $csv --timed $Seconds --terminate_after_timed --no_console_stats
if ($LASTEXITCODE -ne 0) { throw "PresentMon e' terminato con codice $LASTEXITCODE" }
Write-Host "Fatto. Report:  cargo xtask bench-report $csv"
