param(
    [switch]$Debug,
    [switch]$NoCopy,
    [string]$Destination
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

$profile = if ($Debug) { "debug" } else { "release" }
$buildArgs = @("build", "-p", "therminal-app", "--bin", "therminal")
if (-not $Debug) {
    $buildArgs += "--release"
}

Write-Host "=== cargo $($buildArgs -join ' ') ==="
& cargo @buildArgs
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}

$exePath = Join-Path $repoRoot "target\$profile\therminal.exe"
if (-not (Test-Path $exePath)) {
    throw "Built executable not found: $exePath"
}

Write-Host "Built: $exePath"

if (-not $NoCopy) {
    if ([string]::IsNullOrWhiteSpace($Destination)) {
        $desktop = [Environment]::GetFolderPath("Desktop")
        $Destination = Join-Path $desktop "therminal.exe"
    }

    $destDir = Split-Path -Parent $Destination
    if (-not [string]::IsNullOrWhiteSpace($destDir)) {
        New-Item -ItemType Directory -Force -Path $destDir | Out-Null
    }

    Copy-Item -Force $exePath $Destination
    Write-Host "Copied to: $Destination"
}

