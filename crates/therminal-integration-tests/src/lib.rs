//! End-to-end integration test harness for therminal (tn-1kzt).
//!
//! Spawns a real `therminal-daemon` subprocess in an isolated temp runtime
//! directory, drives it via the native IPC client
//! (`therminal-daemon-client`), and tears everything down on drop.
//!
//! The goal is to dogfood the daemon IPC surface the same way an external
//! orchestrator (Claude Code, the GUI, a script) would — catching
//! regressions in the PTY ↔ grid ↔ semantic ↔ IPC integration points that
//! unit tests miss. The stdio MCP bridge is intentionally out of scope here;
//! tests talk directly to the daemon socket for speed.
//!
//! ## Isolation
//!
//! Each harness instance sets the `XDG_*` environment variables on the
//! spawned daemon so that its config, data, cache, and runtime (socket)
//! directories all live under a fresh `tempfile::TempDir`. Multiple
//! `DaemonHarness` instances therefore never collide on the canonical
//! `~/.config/therminal/therminal.toml` or `$XDG_RUNTIME_DIR/therminal/`.
//!
//! ## Scenarios
//!
//! See the tests under `tests/` for concrete scenarios. The helpers in this
//! module (`DaemonHarness`, `wait_for_output`) are the shared fixtures.

#![deny(rust_2018_idioms)]

pub mod harness;

pub use harness::{DaemonHarness, wait_for_output};
