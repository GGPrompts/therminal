//! Daemon connection plumbing for the CLI.
//!
//! Builds a small multi-thread tokio runtime, connects to the daemon
//! control socket, and (if the daemon is missing) auto-spawns
//! `therminal-daemon` via the same `daemon_spawn` chain the GUI uses so
//! the CLI is usable on a clean machine without manual setup.

use std::sync::Arc;

use anyhow::{Context, Result};
use therminal_core::config::TherminalConfig;
use therminal_daemon_client::DaemonClient;
use therminal_protocol::daemon::IpcResponse;
use tokio::runtime::Runtime;

use crate::daemon_spawn;

/// Bundle of a tokio runtime + a connected `DaemonClient`. The CLI uses
/// `block_on` against `rt` to drive async daemon calls from the synchronous
/// subcommand handlers.
pub struct CliCtx {
    pub rt: Runtime,
    pub client: Arc<DaemonClient>,
}

impl CliCtx {
    /// Convenience wrapper around `rt.block_on(client.send_request(req))`
    /// that returns a typed [`IpcResponse`] or maps an `Error` response
    /// to an anyhow error so subcommand handlers can `?`-propagate.
    pub fn send(&self, req: therminal_protocol::daemon::IpcRequest) -> Result<IpcResponse> {
        let resp = self.rt.block_on(self.client.send_request(req))?;
        if let IpcResponse::Error { message } = &resp {
            anyhow::bail!("daemon error: {message}");
        }
        Ok(resp)
    }
}

/// Run a closure with a connected daemon, auto-spawning the daemon if it
/// isn't running. The runtime is dropped after the closure returns so the
/// CLI exits cleanly without leaving stray tokio threads.
pub fn with_runtime<F, T>(f: F) -> Result<T>
where
    F: FnOnce(&CliCtx) -> Result<T>,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name("therminal-cli-rt")
        .build()
        .context("failed to build tokio runtime")?;

    let client = connect_with_autospawn(&rt)?;
    let ctx = CliCtx { rt, client };
    f(&ctx)
}

/// Open a daemon connection, retrying once after `auto_spawn` if the
/// initial connect fails because no daemon is running.
fn connect_with_autospawn(rt: &Runtime) -> Result<Arc<DaemonClient>> {
    let socket_path = therminal_runtime::paths::socket_path("daemon");

    match rt.block_on(DaemonClient::connect(&socket_path)) {
        Ok(client) => Ok(Arc::new(client)),
        Err(e) if daemon_spawn::is_not_running_error(&e) => {
            let config = TherminalConfig::load();
            let binary_override = config.daemon.binary_path.as_deref();
            daemon_spawn::auto_spawn(binary_override).with_context(|| {
                format!(
                    "no daemon at {} and auto-spawn failed",
                    socket_path.display()
                )
            })?;
            // Reuse the existing retry helper which polls for ~1s.
            let client = daemon_spawn::retry_connect(|| {
                rt.block_on(DaemonClient::connect(&socket_path))
                    .map(Arc::new)
            })?;
            Ok(client)
        }
        Err(e) => Err(e.context(format!(
            "failed to connect to daemon at {}",
            socket_path.display()
        ))),
    }
}
