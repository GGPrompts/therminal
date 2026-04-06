//! Zero-downtime daemon handoff.
//!
//! When a new build detects a running daemon with a different PROTOCOL_VERSION,
//! it performs a graceful handoff:
//!
//! 1. Sends `RequestHandoffFds` to the old daemon (Unix only)
//! 2. Old daemon responds with a temporary socket path for FD transfer
//! 3. New daemon connects to that socket and receives PTY master FDs via SCM_RIGHTS
//! 4. Old daemon shuts down and releases the main socket
//! 5. New daemon binds the main socket, creates session manager, and
//!    reconstructs sessions around the received FDs
//! 6. If FD passing fails at any step, falls back to graceful restart (sessions lost)

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
    /// Daemon is running with matching build hash -- reuse it.
    Reuse,
    /// No daemon running -- start fresh.
    StartFresh,
    /// Daemon running with different build hash -- handoff needed.
    NeedsHandoff { old_build_hash: String },
    /// Something is listening on the socket but doesn't speak our protocol
    /// (e.g. an old daemon with a legacy wire format). We connected
    /// successfully but the ping failed with a decode error or timeout.
    IncompatibleDaemon,
}

/// Received handoff data from the old daemon, held until the new daemon
/// creates its session manager.
///
/// The FDs are owned by this struct. When the struct is dropped without
/// the FDs being consumed (via `take_fds`), they are closed.
#[cfg(unix)]
#[derive(Debug)]
pub struct ReceivedHandoff {
    /// Session/pane metadata matching the FD array.
    pub payload: therminal_protocol::daemon::HandoffPayload,
    /// PTY master FDs received via SCM_RIGHTS. `None` after `take_fds()`.
    fds: Option<Vec<std::os::unix::io::RawFd>>,
}

#[cfg(unix)]
impl ReceivedHandoff {
    /// Take ownership of the received FDs. Returns `None` if already taken.
    pub fn take_fds(&mut self) -> Option<Vec<std::os::unix::io::RawFd>> {
        self.fds.take()
    }
}

#[cfg(unix)]
impl Drop for ReceivedHandoff {
    fn drop(&mut self) {
        // Safety net: close any FDs that were never consumed.
        if let Some(fds) = &self.fds {
            for fd in fds {
                unsafe {
                    libc::close(*fd);
                }
            }
        }
    }
}

/// Check the state of the existing daemon.
///
/// - Tries to connect and ping the daemon at `socket_path`.
/// - Returns `Reuse` if protocol versions match.
/// - Returns `NeedsHandoff` if protocol versions differ.
/// - Returns `StartFresh` if no daemon is reachable.
pub async fn check_daemon(socket_path: &Path, our_protocol_version: u32) -> DaemonCheck {
    // First, check if anything is listening on the socket at all.
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
                warn!(
                    "socket accepted connection but returned unexpected response \
                     -- treating as incompatible daemon"
                );
                DaemonCheck::IncompatibleDaemon
            } else {
                DaemonCheck::StartFresh
            }
        }
        Err(e) => {
            if can_connect {
                warn!(
                    error = %e,
                    "socket accepted connection but ping failed \
                     -- incompatible daemon detected"
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
/// On Unix, attempts FD-passing handoff first: requests the old daemon to
/// send PTY master FDs via SCM_RIGHTS so sessions survive the restart.
/// Returns a `ReceivedHandoff` if FDs were successfully received. The caller
/// should pass this to `SessionManager::restore_from_handoff()` after
/// creating the session manager.
///
/// If FD passing fails, falls back to graceful restart (sessions lost).
///
/// In all cases, waits for the old daemon to release the socket before returning.
#[cfg(unix)]
pub async fn perform_handoff(socket_path: &Path) -> Result<Option<ReceivedHandoff>> {
    info!(path = %socket_path.display(), "initiating handoff");

    // Try FD-passing handoff first.
    match receive_handoff_fds(socket_path).await {
        Ok(Some(handoff)) => {
            info!(
                pane_count = handoff.payload.panes.len(),
                "FD-passing handoff: FDs received, waiting for old daemon to release socket"
            );
            wait_for_socket_removal(socket_path).await?;
            return Ok(Some(handoff));
        }
        Ok(None) => {
            info!("old daemon has no sessions, waiting for socket release");
            wait_for_socket_removal(socket_path).await?;
            return Ok(None);
        }
        Err(e) => {
            warn!(
                error = %e,
                "FD-passing handoff failed, falling back to graceful restart"
            );
        }
    }

    // Fallback: graceful restart (sessions lost).
    perform_graceful_restart(socket_path).await?;
    Ok(None)
}

/// Non-Unix perform_handoff: always does graceful restart.
#[cfg(not(unix))]
pub async fn perform_handoff(socket_path: &Path) -> Result<()> {
    info!(path = %socket_path.display(), "initiating handoff");
    perform_graceful_restart(socket_path).await
}

/// Attempt to receive PTY master FDs from the old daemon via SCM_RIGHTS.
#[cfg(unix)]
async fn receive_handoff_fds(socket_path: &Path) -> Result<Option<ReceivedHandoff>> {
    use therminal_protocol::daemon::IpcRequest;

    // Ask the old daemon to prepare FD handoff.
    let response = client::send_request(socket_path, IpcRequest::RequestHandoffFds).await?;

    match response {
        therminal_protocol::IpcResponse::HandoffReady {
            handoff_socket,
            pane_count,
        } => {
            if pane_count == 0 || handoff_socket.is_empty() {
                info!("old daemon has no sessions to hand off");
                return Ok(None);
            }

            info!(
                handoff_socket = %handoff_socket,
                pane_count,
                "old daemon ready for FD handoff"
            );

            // Connect to the handoff socket and receive FDs.
            let handoff_path = std::path::Path::new(&handoff_socket);
            let (payload, fds) = crate::fd_passing::receive_handoff_fds(handoff_path).await?;

            if fds.len() != pane_count {
                warn!(
                    expected = pane_count,
                    received = fds.len(),
                    "FD count mismatch during handoff"
                );
            }

            Ok(Some(ReceivedHandoff {
                payload,
                fds: Some(fds),
            }))
        }
        therminal_protocol::IpcResponse::Error { message } => {
            anyhow::bail!("old daemon rejected handoff: {message}");
        }
        other => {
            anyhow::bail!("unexpected response to RequestHandoffFds: {other:?}");
        }
    }
}

/// Fallback handoff: send GracefulShutdown and wait for socket removal.
async fn perform_graceful_restart(socket_path: &Path) -> Result<()> {
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

    wait_for_socket_removal(socket_path).await
}

/// Wait for the daemon socket to be removed (up to `HANDOFF_TIMEOUT`).
async fn wait_for_socket_removal(socket_path: &Path) -> Result<()> {
    let deadline = tokio::time::Instant::now() + HANDOFF_TIMEOUT;
    loop {
        if !socket_path.exists() {
            info!("old daemon socket removed, handoff successful");
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            // Ping one more time before force-removing.
            if client::ping(socket_path).await.is_ok() {
                anyhow::bail!(
                    "handoff timeout but old daemon is still responding on {}",
                    socket_path.display()
                );
            }
            warn!("handoff timeout -- forcibly removing old socket");
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
