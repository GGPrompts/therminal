#!/usr/bin/env bash
# Build therminal natively on Windows from WSL2.
#
# Usage: ./scripts/build-windows.sh [--debug]
#
# Syncs the repo to a Windows temp directory via /mnt/c (bypasses UNC path
# issues with \\wsl.localhost), then invokes the PowerShell build script
# with the native Windows Rust toolchain + MSVC.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEBUG_FLAG=""
PS_DEBUG=""

for arg in "$@"; do
    case "$arg" in
        --debug) DEBUG_FLAG="--debug"; PS_DEBUG="-Debug" ;;
    esac
done

# Windows build directory — use a non-temp location to avoid WDAC/AppLocker
# blocking build-script executables from %TEMP%.
WIN_BUILD_DIR="/mnt/c/Users/${USER}/therminal-build"
WIN_BUILD_DIR_NATIVE="C:\\Users\\${USER}\\therminal-build"

echo "=== Syncing to ${WIN_BUILD_DIR} ==="
mkdir -p "${WIN_BUILD_DIR}"
rsync -a --delete \
    --exclude target \
    --exclude .git \
    --exclude '.claude/worktrees' \
    "${REPO_ROOT}/" "${WIN_BUILD_DIR}/"
echo "=== Sync complete ==="

# Copy the build script to a native Windows path so PowerShell can read it
PS_SCRIPT="${WIN_BUILD_DIR}/scripts/build-windows.ps1"

echo "=== Starting Windows build ==="
powershell.exe -ExecutionPolicy Bypass \
    -File "$(wslpath -w "${PS_SCRIPT}")" \
    ${PS_DEBUG} \
    -RepoRoot "${WIN_BUILD_DIR_NATIVE}"

echo ""
echo "=== Done ==="
echo "Executable should be on your Desktop: therminal.exe"
