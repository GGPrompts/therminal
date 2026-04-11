//! MCP server transport: Unix domain socket + Windows named pipe listeners.
//!
//! Handles the cross-platform IPC endpoint lifecycle:
//! - **Unix**: Unix domain socket at `<runtime_dir>/mcp.sock` with owner-only
//!   permissions and stale-socket cleanup.
//! - **Windows**: named pipe at `\\.\pipe\therminal-mcp` with one instance per
//!   accepted connection.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::ServiceExt;
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tracing::warn;
use tracing::{debug, error, info};

use therminal_core::config::TrustConfig;

use therminal_harness_claude::jsonl_tailer::TaggedAgentEvent;

use crate::session::SessionManager;
use crate::trust::RateLimiter;
use therminal_terminal::agent_registry::TaggedAgentEvent as TaggedAgentLifecycleEvent;
use therminal_terminal::semantic_patterns::PatternEngine;

use super::TherminalMcpServer;

/// Start the MCP server, accepting connections on the platform-appropriate IPC
/// endpoint.
///
/// - **Unix**: listens on a Unix domain socket at `<runtime_dir>/mcp.sock`
/// - **Windows**: listens on a named pipe at `\\.\pipe\therminal-mcp`
///
/// Each accepted connection is served independently. The server runs until
/// the `shutdown` notify is triggered. Trust enforcement uses the provided
/// `TrustConfig` to gate tool access per agent.
#[allow(clippy::too_many_arguments)]
pub async fn start_mcp_server(
    config: therminal_core::config::McpConfig,
    session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    trust_config: Arc<TrustConfig>,
    rate_limiter: Arc<RateLimiter>,
    claude_events: Option<tokio::sync::broadcast::Sender<TaggedAgentEvent>>,
    agent_events: Option<tokio::sync::broadcast::Sender<TaggedAgentLifecycleEvent>>,
    pattern_engine: Option<Arc<PatternEngine>>,
    event_bus: Option<Arc<crate::event_bus::EventBus>>,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<()> {
    if !config.enabled {
        info!("MCP server disabled by config");
        return Ok(());
    }

    let socket_path = config.resolved_socket_path();

    #[cfg(unix)]
    {
        start_mcp_server_unix(
            &socket_path,
            session_mgr,
            trust_config,
            rate_limiter,
            claude_events,
            agent_events,
            pattern_engine,
            event_bus,
            shutdown,
        )
        .await?;
    }

    #[cfg(windows)]
    {
        start_mcp_server_windows(
            &socket_path,
            session_mgr,
            trust_config,
            rate_limiter,
            claude_events,
            agent_events,
            pattern_engine,
            event_bus,
            shutdown,
        )
        .await?;
    }

    Ok(())
}

/// Spawn an MCP connection handler for a single client stream.
///
/// Accepts anything that can be split into an async reader + writer (Unix
/// socket halves, named-pipe halves, etc.).
#[allow(clippy::too_many_arguments)]
fn spawn_mcp_connection<R, W>(
    reader: R,
    writer: W,
    session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    trust_config: Arc<TrustConfig>,
    rate_limiter: Arc<RateLimiter>,
    claude_events: Option<tokio::sync::broadcast::Sender<TaggedAgentEvent>>,
    agent_events: Option<tokio::sync::broadcast::Sender<TaggedAgentLifecycleEvent>>,
    pattern_engine: Option<Arc<PatternEngine>>,
    event_bus: Option<Arc<crate::event_bus::EventBus>>,
) where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        let server = TherminalMcpServer::new_with_bus(
            session_mgr,
            trust_config,
            rate_limiter,
            claude_events,
            agent_events,
            pattern_engine,
            event_bus,
        );
        match server.serve((reader, writer)).await {
            Ok(running) => {
                if let Err(e) = running.waiting().await {
                    debug!(error = %e, "MCP connection task ended");
                }
            }
            Err(e) => {
                debug!(error = %e, "MCP connection init failed");
            }
        }
    });
}

/// Unix implementation: listen on a Unix domain socket.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn start_mcp_server_unix(
    socket_path: &Path,
    session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    trust_config: Arc<TrustConfig>,
    rate_limiter: Arc<RateLimiter>,
    claude_events: Option<tokio::sync::broadcast::Sender<TaggedAgentEvent>>,
    agent_events: Option<tokio::sync::broadcast::Sender<TaggedAgentLifecycleEvent>>,
    pattern_engine: Option<Arc<PatternEngine>>,
    event_bus: Option<Arc<crate::event_bus::EventBus>>,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<()> {
    // Clean stale socket
    match std::fs::remove_file(socket_path) {
        Ok(()) => debug!(path = %socket_path.display(), "removed stale MCP socket"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::anyhow!(
                "failed to remove stale MCP socket {}: {e}",
                socket_path.display()
            ));
        }
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind MCP socket: {}", socket_path.display()))?;

    // Set socket permissions (owner-only)
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        if let Err(e) = std::fs::set_permissions(socket_path, perms) {
            tracing::warn!(error = %e, "failed to set MCP socket permissions — socket may be world-accessible");
        }
    }

    info!(path = %socket_path.display(), "MCP server listening");

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _addr)) => {
                        // tn-yuu4: best-effort SO_PEERCRED audit log so the
                        // operator can correlate any MCP client back to the
                        // OS-level process that opened the socket. This is
                        // diagnostic only — trust enforcement uses the
                        // per-connection id assigned inside `TherminalMcpServer`.
                        match stream.peer_cred() {
                            Ok(cred) => {
                                info!(
                                    uid = cred.uid(),
                                    gid = cred.gid(),
                                    pid = ?cred.pid(),
                                    "MCP connection accepted"
                                );
                            }
                            Err(e) => {
                                tracing::debug!(
                                    error = %e,
                                    "MCP connection accepted (peer_cred unavailable)"
                                );
                            }
                        }
                        let (reader, writer) = stream.into_split();
                        spawn_mcp_connection(
                            reader,
                            writer,
                            Arc::clone(&session_mgr),
                            Arc::clone(&trust_config),
                            Arc::clone(&rate_limiter),
                            claude_events.clone(),
                            agent_events.clone(),
                            pattern_engine.clone(),
                            event_bus.clone(),
                        );
                    }
                    Err(e) => {
                        error!(error = %e, "MCP accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            _ = shutdown.notified() => {
                info!("MCP server shutting down");
                break;
            }
        }
    }

    // Clean up socket
    cleanup_socket(socket_path);
    Ok(())
}

/// Windows implementation: listen on a named pipe.
///
/// Named pipes on Windows are not like Unix sockets -- there is no persistent
/// listener object. Instead, each pipe instance can serve exactly one client.
/// The standard pattern is:
///   1. Create a pipe instance with `ServerOptions::create()`.
///   2. Call `connect()` to wait for a client.
///   3. Hand the connected instance off to a handler.
///   4. Immediately create a new pipe instance for the next client.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn start_mcp_server_windows(
    socket_path: &Path,
    session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    trust_config: Arc<TrustConfig>,
    rate_limiter: Arc<RateLimiter>,
    claude_events: Option<tokio::sync::broadcast::Sender<TaggedAgentEvent>>,
    agent_events: Option<tokio::sync::broadcast::Sender<TaggedAgentLifecycleEvent>>,
    pattern_engine: Option<Arc<PatternEngine>>,
    event_bus: Option<Arc<crate::event_bus::EventBus>>,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let pipe_name = socket_path.to_string_lossy();

    // The first pipe instance is created outside the loop.
    let mut pipe = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&*pipe_name)
        .with_context(|| format!("failed to create MCP named pipe: {pipe_name}"))?;

    info!(path = %pipe_name, "MCP server listening on named pipe");

    loop {
        tokio::select! {
            result = pipe.connect() => {
                match result {
                    Ok(()) => {
                        // Hand the connected pipe to a handler.
                        let (reader, writer) = tokio::io::split(pipe);
                        spawn_mcp_connection(
                            reader,
                            writer,
                            Arc::clone(&session_mgr),
                            Arc::clone(&trust_config),
                            Arc::clone(&rate_limiter),
                            claude_events.clone(),
                            agent_events.clone(),
                            pattern_engine.clone(),
                            event_bus.clone(),
                        );

                        // Create a new pipe instance for the next client.
                        pipe = ServerOptions::new()
                            .create(&*pipe_name)
                            .with_context(|| {
                                format!(
                                    "failed to create next MCP named pipe instance: {pipe_name}"
                                )
                            })?;
                    }
                    Err(e) => {
                        error!(error = %e, "MCP named pipe connect failed");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            _ = shutdown.notified() => {
                info!("MCP server shutting down");
                break;
            }
        }
    }

    // Named pipes are cleaned up automatically when all handles are dropped,
    // so no explicit file removal is needed (unlike Unix sockets).
    Ok(())
}

/// Remove the MCP socket file (Unix only).
#[cfg(unix)]
fn cleanup_socket(path: &Path) {
    if path.exists() {
        if let Err(e) = std::fs::remove_file(path) {
            warn!(error = %e, path = %path.display(), "failed to remove MCP socket on cleanup");
        } else {
            debug!(path = %path.display(), "MCP socket cleaned up");
        }
    }
}
