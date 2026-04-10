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

# --- UNC path handling ---
# Building from a UNC path (e.g. \\wsl.localhost\...) breaks cargo, cmd.exe,
# and MSVC tools. Mirror the source to a native Windows temp directory.
# This must happen BEFORE the Cargo.toml check because PowerShell 5.1
# cannot Test-Path on \\wsl.localhost\ UNC paths.
# Use a non-temp location to avoid WDAC/AppLocker blocking build-script
# executables. %TEMP% is commonly restricted by Application Control policies.
$localBuildDir = Join-Path $env:USERPROFILE "therminal-build"
if ($repoRoot -match '^\\\\') {
    Write-Host "=== UNC source detected, syncing to $localBuildDir ==="
    & robocopy $repoRoot $localBuildDir /MIR /XD target .git /XF "*.lock" /NFL /NDL /NJH /NJS /NS /NC /NP
    # robocopy exit codes 0-7 are success/informational
    if ($LASTEXITCODE -gt 7) {
        throw "robocopy failed with exit code $LASTEXITCODE"
    }
    # Copy Cargo.lock separately (robocopy /XF excluded all .lock files above,
    # but we need Cargo.lock for reproducible builds)
    $cargoLock = Join-Path $repoRoot "Cargo.lock"
    & robocopy (Split-Path $cargoLock) $localBuildDir "Cargo.lock" /NFL /NDL /NJH /NJS /NS /NC /NP
    # Invalidate stale cargo fingerprints for workspace crates so incremental
    # compilation picks up all mirrored source changes.  The target/ dir is
    # excluded from robocopy (/XD target) so old fingerprints can survive
    # across syncs and confuse rustc into skipping changed crates.
    $fingerprintDir = Join-Path $localBuildDir "target"
    foreach ($profile in @("release", "debug")) {
        $fpDir = Join-Path $fingerprintDir "$profile\.fingerprint"
        if (Test-Path $fpDir) {
            Get-ChildItem -Directory $fpDir -Filter "therminal-*" | Remove-Item -Recurse -Force
        }
    }
    $repoRoot = $localBuildDir
    Write-Host "=== building from $repoRoot ==="
}

if (-not (Test-Path (Join-Path $repoRoot "Cargo.toml"))) {
    throw "Repo root does not contain Cargo.toml: $repoRoot"
}

# --- Ensure cargo is on PATH ---
# When invoked from WSL, the user's Windows PATH additions (e.g. .cargo\bin)
# may not be inherited. Add the default rustup location if needed.
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE ".cargo" }
    $cargoBin = Join-Path $cargoHome "bin"
    if (Test-Path (Join-Path $cargoBin "cargo.exe")) {
        $env:PATH = "$cargoBin;$env:PATH"
        Write-Host "=== Added $cargoBin to PATH ==="
    } else {
        throw @"
Windows cargo was not found on PATH or in $cargoBin.

This script performs a native Windows build, so it needs a Windows Rust toolchain.
Your WSL cargo installation is not visible to Windows PowerShell.

Install Rust on Windows, reopen PowerShell, and try again:
  winget install --id Rustlang.Rustup -e
"@
    }
}

# --- Windows Defender exclusion for build artifacts ---
# Build-script executables in target/ are often blocked by WDAC/SmartScreen.
# Attempt to add an exclusion (requires admin; silently skipped otherwise).
$targetDir = Join-Path $repoRoot "target"
try {
    Add-MpExclusion -Path $targetDir -ErrorAction Stop
    Write-Host "=== Added Defender exclusion for $targetDir ==="
} catch {
    # Not admin or not applicable — check if WDAC is the issue
    Write-Host "=== Note: Could not add Defender exclusion (not admin). ==="
    Write-Host "    If build fails with 'Application Control policy' errors, run:"
    Write-Host "    PowerShell (Admin): Add-MpExclusion -Path '$targetDir'"
}

Set-Location $repoRoot

$profile = if ($Debug) { "debug" } else { "release" }
$buildArgs = @("build", "-p", "therminal-app", "--bin", "therminal", "-p", "therminal-daemon", "--bin", "therminal-daemon")
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
    $cmdLine = "cd /d `"$repoRoot`" && call `"$vsDevCmd`" -no_logo && cargo $($buildArgs -join ' ')"
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

    # --- Also deploy therminal-daemon.exe alongside therminal.exe ---
    # The MCP stdio bridge (therminal mcp) requires a running daemon to
    # bind the named pipe. The GUI does not embed the daemon, so we ship
    # the daemon binary as a sibling and the user starts it manually.
    # See docs/integrations/wsl2.md.
    $daemonExe = Join-Path $repoRoot "target\$profile\therminal-daemon.exe"
    if (Test-Path $daemonExe) {
        $daemonDest = Join-Path (Split-Path -Parent $Destination) "therminal-daemon.exe"
        Copy-Item -Force $daemonExe $daemonDest
        Write-Host "Copied daemon to: $daemonDest"
    } else {
        Write-Host "WARNING: therminal-daemon.exe not found at $daemonExe - MCP bridge will not work"
    }

    # --- Copy resources alongside the executable ---
    # The app looks for <exe_dir>/../resources or <data_dir>/resources.
    # Copy to <data_dir>/resources (%APPDATA%/therminal/resources) since
    # putting a resources/ folder on the Desktop would be messy.
    $resourcesSrc = Join-Path $repoRoot "resources"
    $dataDir = Join-Path $env:APPDATA "therminal"
    $resourcesDst = Join-Path $dataDir "resources"
    if (Test-Path $resourcesSrc) {
        New-Item -ItemType Directory -Force -Path $dataDir | Out-Null
        Copy-Item -Recurse -Force $resourcesSrc $dataDir
        Write-Host "Resources copied to: $resourcesDst"
    } else {
        Write-Host "WARNING: resources/ not found in repo, shell integration will not work"
    }

    # --- Copy bundled pattern packs alongside resources ---
    # The pattern engine resolves shipped packs from
    # <THERMINAL_RESOURCES_DIR>/plugins/examples. Copy them into the
    # resources tree so they're found at runtime.
    $pluginsSrc = Join-Path $repoRoot "plugins\examples"
    $pluginsDst = Join-Path $resourcesDst "plugins\examples"
    if (Test-Path $pluginsSrc) {
        New-Item -ItemType Directory -Force -Path $pluginsDst | Out-Null
        Copy-Item -Recurse -Force (Join-Path $pluginsSrc "*") $pluginsDst
        Write-Host "Pattern packs copied to: $pluginsDst"
    }
}
