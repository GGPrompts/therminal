# tn.ps1 — PowerShell wrapper around the `therminal` CLI.
#
# Windows equivalent of scripts/tn (bash). See that file for the
# CLI-vs-MCP usage policy and full subcommand reference.
#
# Binary discovery order:
#   1. THERMINAL_BIN env var (explicit override)
#   2. therminal on PATH
#   3. $env:USERPROFILE\Desktop\therminal.exe (common dev build location)
#
# Installation: copy this file to a directory on $env:PATH. The .exe
# copy is preferred — it works without execution-policy concerns.

if ($env:THERMINAL_BIN -and (Test-Path $env:THERMINAL_BIN)) {
    & $env:THERMINAL_BIN @args
    exit $LASTEXITCODE
}

$found = Get-Command therminal -ErrorAction SilentlyContinue
if ($found) {
    & therminal @args
    exit $LASTEXITCODE
}

$desktopBin = Join-Path $env:USERPROFILE "Desktop\therminal.exe"
if (Test-Path $desktopBin) {
    & $desktopBin @args
    exit $LASTEXITCODE
}

Write-Error "tn: therminal binary not found"
Write-Error "  Checked: THERMINAL_BIN env, PATH, `$env:USERPROFILE\Desktop"
Write-Error "  Fix: set THERMINAL_BIN or add therminal.exe to PATH"
exit 1
