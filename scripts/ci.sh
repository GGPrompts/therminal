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

echo "=== All checks passed ==="
