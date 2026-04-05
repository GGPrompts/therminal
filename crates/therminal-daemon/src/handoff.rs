//! Zero-downtime daemon handoff.
//!
//! When a new build detects a running daemon with a different BUILD_HASH,
//! it performs a graceful handoff:
//!
//! 1. New daemon binds a temporary socket (`daemon.sock.new`)
//! 2. Sends `GracefulShutdown` to the old daemon
//! 3. Waits for old daemon to drain and release `daemon.sock`
//! 4. Atomic rename: `daemon.sock.new` -> `daemon.sock`
//! 5. Emits `HandoffComplete`
//!
//! On crash or timeout (5s), rolls back: removes `.sock.new`, keeps old daemon.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{error, info, warn};

use crate::client;

/// Default handoff timeout.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(5);

/// Result of an `ensure_daemon` check against a running daemon.
pub enum DaemonCheck {
    /// Daemon is running with matching build hash — reuse it.
    Reuse,
    /// No daemon running — start fresh.
    StartFresh,
    /// Daemon running with different build hash — handoff needed.
    NeedsHandoff { old_build_hash: String },
}

/// Check the state of the existing daemon.
///
/// - Tries to connect and ping the daemon at `socket_path`.
/// - Returns `Reuse` if build hashes match.
/// - Returns `NeedsHandoff` if hashes differ.
/// - Returns `StartFresh` if no daemon is reachable.
pub async fn check_daemon(socket_path: &Path, our_build_hash: &str) -> DaemonCheck {
    match client::ping(socket_path).await {
        Ok(therminal_protocol::DaemonResponse::Pong { build_hash, .. }) => {
            if build_hash == our_build_hash {
                info!(build_hash = %our_build_hash, "existing daemon matches our build");
                DaemonCheck::Reuse
            } else {
                info!(
                    old = %build_hash,
                    new = %our_build_hash,
                    "build hash mismatch, handoff needed"
                );
                DaemonCheck::NeedsHandoff {
                    old_build_hash: build_hash,
                }
            }
        }
        Ok(other) => {
            warn!(?other, "unexpected response to ping");
            DaemonCheck::StartFresh
        }
        Err(e) => {
            info!(error = %e, "no reachable daemon, starting fresh");
            DaemonCheck::StartFresh
        }
    }
}

/// Perform the handoff from the old daemon to our new instance.
///
/// - Sends `GracefulShutdown` to the old daemon.
/// - Waits for the old socket to disappear (up to `HANDOFF_TIMEOUT`).
/// - Returns `Ok(())` when the old daemon has released the socket.
pub async fn perform_handoff(socket_path: &Path) -> Result<()> {
    info!(path = %socket_path.display(), "initiating handoff");

    // Send graceful shutdown to old daemon
    match client::request_shutdown(socket_path).await {
        Ok(therminal_protocol::DaemonResponse::ShutdownAck) => {
            info!("old daemon acknowledged shutdown");
        }
        Ok(other) => {
            warn!(?other, "unexpected shutdown response");
        }
        Err(e) => {
            warn!(error = %e, "failed to send shutdown, old daemon may already be gone");
        }
    }

    // Wait for old socket to disappear
    let deadline = tokio::time::Instant::now() + HANDOFF_TIMEOUT;
    loop {
        if !socket_path.exists() {
            info!("old daemon socket removed, handoff successful");
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            // Ping one more time before force-removing — the daemon may have recovered.
            if client::ping(socket_path).await.is_ok() {
                anyhow::bail!(
                    "handoff timeout but old daemon is still responding on {}",
                    socket_path.display()
                );
            }
            warn!("handoff timeout — forcibly removing old socket");
            std::fs::remove_file(socket_path).with_context(|| {
                format!(
                    "failed to remove stale socket during handoff: {}",
                    socket_path.display()
                )
            })?;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Perform atomic socket rename for zero-downtime handoff.
///
/// The new daemon binds to `<socket>.new`, then once the old daemon is gone,
/// renames it to the canonical path.
pub fn atomic_socket_rename(temp_path: &Path, canonical_path: &Path) -> Result<()> {
    std::fs::rename(temp_path, canonical_path).with_context(|| {
        format!(
            "atomic rename failed: {} -> {}",
            temp_path.display(),
            canonical_path.display()
        )
    })?;
    info!(
        from = %temp_path.display(),
        to = %canonical_path.display(),
        "atomic socket rename complete"
    );
    Ok(())
}

/// Rollback a failed handoff by cleaning up the temporary socket.
pub fn rollback_handoff(temp_path: &Path) {
    if temp_path.exists() {
        if let Err(e) = std::fs::remove_file(temp_path) {
            error!(error = %e, path = %temp_path.display(), "failed to clean up temp socket during rollback");
        } else {
            info!(path = %temp_path.display(), "handoff rollback: temp socket cleaned up");
        }
    }
}

/// Get the temporary socket path used during handoff.
pub fn temp_socket_path(socket_path: &Path) -> PathBuf {
    let mut temp = socket_path.as_os_str().to_owned();
    temp.push(".new");
    PathBuf::from(temp)
}
