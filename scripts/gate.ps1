# Cancello di fine lavoro: clippy -D warnings e test sui crate modificati.
# Uso: gate.ps1 [-Crate cliprec-xxx] [-Hook]
# Con -Hook legge il JSON dell'hook (SubagentStop) da stdin, lavora solo nei worktree in
# .claude\worktrees e blocca al massimo 3 volte (exit 2) per non creare cicli infiniti.
param([string]$Crate, [switch]$Hook)
$ErrorActionPreference = 'Continue'
$cwd = (Get-Location).Path
if ($Hook) {
    $payload = [Console]::In.ReadToEnd() | ConvertFrom-Json
    if ($payload.cwd) { $cwd = $payload.cwd }
    if ($cwd -notmatch '[\\/]\.claude[\\/]worktrees[\\/]') { exit 0 }
}
Push-Location $cwd
try {
    $crates = @()
    if ($Crate) { $crates = @($Crate) } else {
        $changed = @(git diff --name-only HEAD) + @(git ls-files --others --exclude-standard)
        $crates = $changed | ForEach-Object { if ($_ -match '^crates/(cliprec-[a-z]+)/') { $Matches[1] } } | Sort-Object -Unique
    }
    if (-not $crates) { exit 0 }
    $failed = @()
    foreach ($c in $crates) {
        $clippy = & cargo clippy -p $c --all-targets --message-format short -- -D warnings 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) { $failed += "clippy $c`n" + ($clippy -split "`n" | Select-String 'error|warning' | Select-Object -First 20 | Out-String) }
        $test = & cargo test -p $c 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) { $failed += "test $c`n" + ($test -split "`n" | Select-Object -Last 25 | Out-String) }
    }
    if (-not $failed) { exit 0 }
    if ($Hook) {
        $counter = Join-Path $cwd 'target\.gate-blocks'
        $n = if (Test-Path $counter) { [int](Get-Content $counter) } else { 0 }
        if ($n -ge 3) { exit 0 }
        New-Item -ItemType Directory -Force (Split-Path $counter) | Out-Null
        Set-Content $counter ($n + 1)
    }
    [Console]::Error.WriteLine("Cancello fallito: correggi prima di chiudere.`n" + ($failed -join "`n"))
    exit 2
} finally { Pop-Location }
