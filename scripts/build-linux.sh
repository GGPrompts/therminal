#!/usr/bin/env bash
# Build therminal natively on Linux/WSL2.
#
# Usage: ./scripts/build-linux.sh [--debug] [--run]
#
# Builds both therminal and therminal-daemon in release mode (default).
# Pass --debug for unoptimized dev builds. Pass --run to launch after building.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="release"
CARGO_FLAG="--release"
RUN_AFTER=false

for arg in "$@"; do
    case "$arg" in
        --debug) PROFILE="debug"; CARGO_FLAG="" ;;
        --run)   RUN_AFTER=true ;;
    esac
done

echo "=== Building therminal ($PROFILE) ==="
cargo build --manifest-path "$REPO_ROOT/Cargo.toml" $CARGO_FLAG \
    --bin therminal --bin therminal-daemon

BIN_DIR="$REPO_ROOT/target/$PROFILE"
echo ""
echo "=== Build complete ==="
echo "  therminal:        $BIN_DIR/therminal"
echo "  therminal-daemon: $BIN_DIR/therminal-daemon"

if $RUN_AFTER; then
    echo ""
    echo "=== Launching therminal ==="
    exec "$BIN_DIR/therminal"
fi
