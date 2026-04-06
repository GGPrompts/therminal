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

pub mod client;
pub mod control;
pub mod ensure;
pub mod framing;
pub mod handoff;
pub mod lifecycle;
pub mod mcp;
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
