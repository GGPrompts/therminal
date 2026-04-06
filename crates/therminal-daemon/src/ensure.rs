//! `ensure_daemon()` — the primary entry point for daemon lifecycle management.
//!
//! Called by the terminal app (or CLI) to guarantee a daemon is running with
//! a compatible protocol version. Handles three cases:
//!
//! 1. **No daemon running**: start a new one.
//! 2. **Daemon running, matching protocol version**: reuse it.
//! 3. **Daemon running, different protocol version**: graceful handoff.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{info, warn};

use therminal_protocol::DaemonState;

use crate::client;
use crate::handoff::{self, DaemonCheck};
use crate::lifecycle::{Lifecycle, LifecycleConfig};
use crate::mcp;
use crate::server::DaemonServer;

/// Build hash embedded at compile time by `build.rs`.
pub const BUILD_HASH: &str = env!("BUILD_HASH");

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Result of `ensure_daemon()`.
pub enum EnsureResult {
    /// An existing daemon is already running with a matching build.
    Reused,
    /// A new daemon was started (either fresh or via handoff).
    Started {
        /// Handle to the running daemon lifecycle.
        lifecycle: Arc<Lifecycle>,
    },
}

/// Ensure a daemon is running with a compatible protocol version.
///
/// This is the main entry point for daemon lifecycle management. It:
/// 1. Checks if a daemon is already running via socket probe.
/// 2. If running with matching protocol version, returns `Reused`.
/// 3. If running with different protocol version, performs graceful handoff.
/// 4. If no daemon, starts a new one.
///
/// Returns an `EnsureResult` indicating what happened.
pub async fn ensure_daemon(config: LifecycleConfig) -> Result<EnsureResult> {
    let socket_path = therminal_runtime::paths::socket_path("daemon");

    info!(
        protocol_version = therminal_protocol::PROTOCOL_VERSION,
        build_hash = BUILD_HASH,
        version = VERSION,
        socket = %socket_path.display(),
        "ensure_daemon called"
    );

    // Ensure runtime directory exists
    therminal_runtime::paths::ensure_runtime_dir().context("failed to create runtime directory")?;

    // Check existing daemon — handoff is based on protocol version, not build hash
    match handoff::check_daemon(&socket_path, therminal_protocol::PROTOCOL_VERSION).await {
        DaemonCheck::Reuse => {
            info!("reusing existing daemon");
            return Ok(EnsureResult::Reused);
        }
        DaemonCheck::NeedsHandoff { old_build_hash } => {
            info!(
                old_build_hash = %old_build_hash,
                new_build_hash = BUILD_HASH,
                protocol_version = therminal_protocol::PROTOCOL_VERSION,
                "performing protocol version handoff"
            );
            handoff::perform_handoff(&socket_path).await?;
        }
        DaemonCheck::IncompatibleDaemon => {
            warn!(
                "incompatible daemon detected on socket — attempting graceful shutdown before starting fresh"
            );
            // Try to shut down whatever is listening, even though it
            // may not understand our protocol.  If it fails, force-remove.
            match client::request_shutdown(&socket_path).await {
                Ok(_) => {
                    info!("incompatible daemon acknowledged shutdown, waiting for socket removal");
                    // Give it a moment to release the socket.
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
                    loop {
                        if !socket_path.exists() {
                            break;
                        }
                        if tokio::time::Instant::now() >= deadline {
                            warn!(
                                "incompatible daemon did not release socket in time, force-removing"
                            );
                            std::fs::remove_file(&socket_path).ok();
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "failed to send shutdown to incompatible daemon, force-removing socket"
                    );
                    if socket_path.exists() {
                        std::fs::remove_file(&socket_path).ok();
                    }
                }
            }
        }
        DaemonCheck::StartFresh => {
            // Clean any stale socket
            if socket_path.exists() {
                warn!(path = %socket_path.display(), "removing stale socket");
                std::fs::remove_file(&socket_path).ok();
            }
        }
    }

    // Start new daemon
    let lifecycle = start_daemon(socket_path, config).await?;
    Ok(EnsureResult::Started { lifecycle })
}

/// Start a new daemon instance, binding the control socket and entering
/// the accept loop.
async fn start_daemon(socket_path: PathBuf, config: LifecycleConfig) -> Result<Arc<Lifecycle>> {
    let lifecycle = Arc::new(Lifecycle::new(config));

    // Starting -> Binding
    lifecycle.transition(DaemonState::Binding)?;

    let server = DaemonServer::bind(
        socket_path,
        Arc::clone(&lifecycle),
        BUILD_HASH.to_string(),
        VERSION.to_string(),
    )
    .await
    .context("failed to bind daemon socket")?;

    // Binding -> Ready
    lifecycle.transition(DaemonState::Ready)?;

    // Ready -> Running
    lifecycle.transition(DaemonState::Running)?;

    // Spawn the idle watcher
    lifecycle.spawn_idle_watcher();

    // Load trust config for MCP enforcement.
    let app_config = therminal_core::config::TherminalConfig::load();
    let trust_config = Arc::new(app_config.trust.clone());
    let rate_limiter = Arc::new(crate::trust::RateLimiter::new(
        app_config.trust.destructive_rate_limit,
    ));

    // Start MCP server alongside the IPC server
    let mcp_shutdown = Arc::new(tokio::sync::Notify::new());
    let mcp_config = app_config.mcp.clone();
    let mcp_session_mgr = server.session_manager();
    let mcp_shutdown_clone = Arc::clone(&mcp_shutdown);
    let mcp_trust = Arc::clone(&trust_config);
    let mcp_rl = Arc::clone(&rate_limiter);
    tokio::spawn(async move {
        if let Err(e) = mcp::start_mcp_server(
            mcp_config,
            mcp_session_mgr,
            mcp_trust,
            mcp_rl,
            mcp_shutdown_clone,
        )
        .await
        {
            warn!(error = %e, "MCP server exited with error");
        }
    });

    // Spawn the server accept loop in the background
    let lc = Arc::clone(&lifecycle);
    tokio::spawn(async move {
        if let Err(e) = server.run().await {
            warn!(error = %e, "daemon server exited with error");
        }
        // Signal MCP server to stop when IPC server stops
        mcp_shutdown.notify_one();
        // Ensure lifecycle reaches Stopped
        if lc.state() != DaemonState::Stopped {
            let _ = lc.initiate_shutdown().await;
        }
    });

    info!(
        build_hash = BUILD_HASH,
        version = VERSION,
        "daemon started successfully"
    );

    Ok(lifecycle)
}
