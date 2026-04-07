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

pub mod agent_events;
pub mod claude_jsonl_tailer;
pub mod claude_pipeline;
pub mod claude_session_log;
pub mod claude_state;
pub mod client;
pub mod control;
pub mod ensure;
#[cfg(unix)]
pub mod fd_passing;
pub mod framing;
pub mod handoff;
pub mod lifecycle;
pub mod mcp;
pub mod persistence;
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
