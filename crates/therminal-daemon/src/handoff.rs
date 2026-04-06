//! Zero-downtime daemon handoff.
//!
//! When a new build detects a running daemon with a different PROTOCOL_VERSION,
//! it performs a graceful handoff:
//!
//! 1. Sends `GracefulShutdown` to the old daemon
//! 2. Waits for old daemon to drain and release `daemon.sock`
//! 3. New daemon binds the canonical socket path

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::client;

/// Default handoff timeout.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(5);

/// Result of an `ensure_daemon` check against a running daemon.
#[derive(Debug)]
pub enum DaemonCheck {
    /// Daemon is running with matching build hash — reuse it.
    Reuse,
    /// No daemon running — start fresh.
    StartFresh,
    /// Daemon running with different build hash — handoff needed.
    NeedsHandoff { old_build_hash: String },
    /// Something is listening on the socket but doesn't speak our protocol
    /// (e.g. an old daemon with a legacy wire format). We connected
    /// successfully but the ping failed with a decode error or timeout.
    IncompatibleDaemon,
}

/// Check the state of the existing daemon.
///
/// - Tries to connect and ping the daemon at `socket_path`.
/// - Returns `Reuse` if protocol versions match.
/// - Returns `NeedsHandoff` if protocol versions differ.
/// - Returns `StartFresh` if no daemon is reachable.
pub async fn check_daemon(socket_path: &Path, our_protocol_version: u32) -> DaemonCheck {
    // First, check if anything is listening on the socket at all.
    // We do this by attempting a raw TCP-level connect before running
    // the full IPC ping, so we can distinguish "nothing listening"
    // from "something listening but speaking the wrong protocol".
    let can_connect = tokio::net::UnixStream::connect(socket_path).await.is_ok();

    match client::ping(socket_path).await {
        Ok(therminal_protocol::IpcResponse::Pong {
            protocol_version,
            build_hash,
            ..
        }) => {
            if protocol_version == our_protocol_version {
                info!(
                    protocol_version,
                    build_hash = %build_hash,
                    "existing daemon matches our protocol version"
                );
                DaemonCheck::Reuse
            } else {
                info!(
                    old_protocol = protocol_version,
                    new_protocol = our_protocol_version,
                    old_build = %build_hash,
                    "protocol version mismatch, handoff needed"
                );
                DaemonCheck::NeedsHandoff {
                    old_build_hash: build_hash,
                }
            }
        }
        Ok(other) => {
            warn!(?other, "unexpected response to ping");
            if can_connect {
                warn!("socket accepted connection but returned unexpected response — treating as incompatible daemon");
                DaemonCheck::IncompatibleDaemon
            } else {
                DaemonCheck::StartFresh
            }
        }
        Err(e) => {
            if can_connect {
                warn!(
                    error = %e,
                    "socket accepted connection but ping failed — incompatible daemon detected"
                );
                DaemonCheck::IncompatibleDaemon
            } else {
                info!(error = %e, "no reachable daemon, starting fresh");
                DaemonCheck::StartFresh
            }
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
        Ok(therminal_protocol::IpcResponse::ShutdownAck) => {
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
