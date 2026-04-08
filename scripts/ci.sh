#!/usr/bin/env bash
set -euo pipefail

echo "=== cargo fmt --check ==="
cargo fmt --all -- --check

echo "=== cargo clippy ==="
cargo clippy --workspace -- -D warnings

echo "=== cargo build ==="
cargo build --workspace

echo "=== cargo test ==="
cargo test --workspace

# tn-1kzt: run the end-to-end integration tests explicitly so it's obvious
# which step is exercising the real `therminal-daemon` subprocess + PTY.
# `cargo test --workspace` above already built and ran these, so this is
# effectively a no-op on a clean tree — but it makes failures attributable
# at a glance and gives us a single place to add flags (e.g.
# `--test-threads=1` if daemon startup contention becomes a problem).
echo "=== cargo test -p therminal-integration-tests ==="
cargo test -p therminal-integration-tests

echo "=== All checks passed ==="
