<#
.SYNOPSIS
  Traccia WPR (profilo CPU) per 10 minuti con ClipRec a riposo, per contare risvegli e CPU.

.DESCRIPTION
  Richiede un PowerShell elevato (WPR lo esige) e Windows Performance Toolkit (wpr.exe, parte del
  Windows ADK; non viene installato dallo script). Avvia ClipRec e lascia il PC in idle (gioco aperto o
  no, a seconda di cosa vuoi misurare), poi esegui lo script.
  Apri l'ETL in Windows Performance Analyzer (wpa.exe): "CPU Usage (Sampled)" e "CPU Usage (Precise)"
  filtrati su cliprec-app.exe; le context switch al secondo sono i risvegli.

.EXAMPLE
  .\scripts\bench\wpr.ps1 -Minutes 10
#>
param(
    [int]$Minutes = 10,
    [string]$OutDir = (Join-Path $PSScriptRoot '..\..\out\bench')
)
$ErrorActionPreference = 'Stop'

if (-not (Get-Command wpr.exe -ErrorAction SilentlyContinue)) {
    throw 'wpr.exe non trovato: serve Windows Performance Toolkit (Windows ADK). Non viene installato dallo script.'
}
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { throw 'WPR richiede un PowerShell eseguito come amministratore.' }

New-Item -ItemType Directory -Force $OutDir | Out-Null
$etl = Join-Path $OutDir ("cpu-{0:yyyyMMdd-HHmmss}.etl" -f (Get-Date))

wpr.exe -cancel 2>$null | Out-Null   # chiude eventuali sessioni rimaste aperte
wpr.exe -start CPU -filemode
if ($LASTEXITCODE -ne 0) { throw 'wpr -start fallito' }
try {
    Write-Host "Traccia CPU in corso per $Minutes minuti..."
    Start-Sleep -Seconds ($Minutes * 60)
} finally {
    wpr.exe -stop $etl
}
Write-Host "Traccia salvata in $etl (aprila con wpa.exe)."
