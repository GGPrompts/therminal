#!/usr/bin/env bash
# Launch therminal with logging to both stderr and a log file.
# Usage: ./scripts/run.sh [--verbose]
set -euo pipefail

LOGDIR="$(dirname "$0")/../logs"
mkdir -p "$LOGDIR"
LOGFILE="$LOGDIR/therminal-$(date +%Y%m%d-%H%M%S).log"

echo "=== Building therminal ==="
cargo build --bin therminal

echo "=== Launching therminal ==="
echo "    Log file: $LOGFILE"
echo "    Press Ctrl+C here to stop tailing (terminal keeps running)"
echo ""

# Run therminal in background, tee stderr to log file.
# The 2>&1 merges stderr (where tracing writes) into stdout for tee.
cargo run --bin therminal -- "$@" 2>&1 | tee "$LOGFILE"
