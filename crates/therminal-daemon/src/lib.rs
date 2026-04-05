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
pub mod ensure;
pub mod handoff;
pub mod lifecycle;
pub mod server;

pub use ensure::{ensure_daemon, EnsureResult, BUILD_HASH, VERSION};
pub use lifecycle::{Lifecycle, LifecycleConfig};

pub use therminal_protocol as protocol;
pub use therminal_runtime as runtime;
pub use therminal_terminal as terminal;
