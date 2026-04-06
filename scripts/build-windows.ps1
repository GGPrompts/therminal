param(
    [switch]$Debug,
    [switch]$NoCopy,
    [string]$Destination,
    [string]$RepoRoot
)

$ErrorActionPreference = "Stop"

$scriptRepoRoot = Split-Path -Parent $PSScriptRoot
$repoRoot = if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
    $scriptRepoRoot
} else {
    $RepoRoot
}

if (-not (Test-Path (Join-Path $repoRoot "Cargo.toml"))) {
    throw "Repo root does not contain Cargo.toml: $repoRoot"
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw @"
Windows cargo was not found on PATH.

This script performs a native Windows build, so it needs a Windows Rust toolchain.
Your WSL cargo installation is not visible to Windows PowerShell.

Install Rust on Windows, reopen PowerShell, and try again:
  winget install --id Rustlang.Rustup -e
"@
}

Set-Location $repoRoot

$profile = if ($Debug) { "debug" } else { "release" }
$buildArgs = @("build", "-p", "therminal-app", "--bin", "therminal")
if (-not $Debug) {
    $buildArgs += "--release"
}

Write-Host "=== cargo $($buildArgs -join ' ') ==="

if (Get-Command link.exe -ErrorAction SilentlyContinue) {
    & cargo @buildArgs
} else {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    $vsDevCmd = $null

    if (Test-Path $vswhere) {
        $installPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($installPath)) {
            $candidate = Join-Path $installPath "Common7\Tools\VsDevCmd.bat"
            if (Test-Path $candidate) {
                $vsDevCmd = $candidate
            }
        }
    }

    if ([string]::IsNullOrWhiteSpace($vsDevCmd)) {
        throw @"
Windows link.exe was not found on PATH, and VsDevCmd.bat could not be located.

Install Visual Studio Build Tools with the C++ workload, then retry:
  winget install --id Microsoft.VisualStudio.2022.BuildTools -e

Required workload:
  Desktop development with C++
"@
    }

    Write-Host "=== bootstrapping MSVC environment with $vsDevCmd ==="
    $cmdLine = "call `"$vsDevCmd`" -no_logo && cargo $($buildArgs -join ' ')"
    & cmd.exe /s /c $cmdLine
}

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
