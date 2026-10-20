# Hook PostToolUse (Edit|Write): `cargo check -p <crate>` sul crate del file modificato,
# nella cartella `cwd` ricevuta dall'hook (segue il worktree). Exit 2 = errori mostrati a Claude.
$ErrorActionPreference = 'Stop'
$payload = [Console]::In.ReadToEnd() | ConvertFrom-Json
$file = $payload.tool_input.file_path
if (-not $file -or $file -notmatch '\.(rs|toml|slint)$') { exit 0 }
if ($file -notmatch '[\\/]crates[\\/](cliprec-[a-z]+)[\\/]') { exit 0 }
$crate = $Matches[1]
$cwd = if ($payload.cwd) { $payload.cwd } else { (Get-Location).Path }
Push-Location $cwd
try {
    $out = & cargo check -p $crate --all-targets --message-format short 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) {
        [Console]::Error.WriteLine("cargo check -p $crate fallito:`n" + ($out -split "`n" | Select-String -Pattern 'error|warning' | Select-Object -First 30 | Out-String))
        exit 2
    }
} finally { Pop-Location }
exit 0
