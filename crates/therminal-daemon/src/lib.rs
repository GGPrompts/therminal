//! Session manager, event bus, multiplexer, and MCP server.
//!
//! The daemon provides persistent session management for Therminal. It uses
//! a socket-as-lock pattern (no pidfiles) and supports zero-downtime handoff
//! when a new build is detected via `BUILD_HASH` comparison.
//!
//! ## State Machine
//!
//! ```text
//! Starting -> Binding -> Ready -> Running -> Draining -> Stopped
//! ```
//!
//! ## Usage
//!
//! ```rust,no_run
//! use therminal_daemon::{ensure_daemon, lifecycle::LifecycleConfig};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let result = ensure_daemon(LifecycleConfig::default()).await?;
//! # Ok(())
//! # }
//! ```

pub mod control;
pub mod ensure;
pub mod event_bus;
#[cfg(unix)]
pub mod fd_passing;
pub mod handoff;

// The IPC client, framing, and transport modules now live in
// `therminal-daemon-client` so lightweight consumers (the GUI, tools) can
// link them without pulling in the server / MCP / persistence stack.
// Re-exported here so existing call sites (`therminal_daemon::client::*`)
// keep working.
pub use therminal_daemon_client::{client, framing, ipc_transport};
pub mod lifecycle;
pub mod mcp;
pub mod pane_capacity;
pub mod persistence;
pub mod process_detector_task;
pub mod server;
pub mod session;
pub mod trust;

pub use ensure::{BUILD_HASH, EnsureResult, VERSION, ensure_daemon};
pub use lifecycle::{Lifecycle, LifecycleConfig};
pub use server::IpcServer;
pub use session::SessionManager;

pub use therminal_protocol as protocol;
pub use therminal_runtime as runtime;
pub use therminal_terminal as terminal;
