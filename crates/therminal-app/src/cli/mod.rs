//! Cache-friendly CLI surface for therminal (tn-k13n).
//!
//! This module wraps the existing `therminal-daemon-client` crate so any
//! MCP client (Claude Code, Codex, shell scripts) can drive the same daemon
//! the GUI talks to without paying MCP framing costs. The principle:
//!
//! - **MCP** when the structured shape materially matters (subscriptions,
//!   resource URIs, semantic queries fed back into the tool-use loop).
//! - **CLI** for writes, commands, fire-and-forget, tiny peeks, anything
//!   called N times in a row from a shell-style consumer.
//!
//! All CLI subcommands share one connection-and-dispatch helper
//! ([`runtime::with_runtime`]) that auto-spawns `therminal-daemon` via the
//! same `daemon_spawn::auto_spawn` path the GUI uses, then routes the typed
//! [`IpcRequest`](therminal_protocol::daemon::IpcRequest) through the
//! persistent `DaemonClient`.
//!
//! Output discipline (critical for the cache-friendly story):
//!   * default output is **terse** — tab-separated, one record per line, no
//!     framing, no headers, no ANSI color, no timestamps.
//!   * `--json` flag on most subcommands produces structured output for
//!     scripts that want to parse fields by name.
//!   * Errors go to stderr, exit code is non-zero, stdout stays clean so
//!     callers can pipe `pane peek` etc. into other tools.
//!
//! Module layout — one file per subcommand group:
//! ```text
//! cli/
//! ├── mod.rs         # OutputFlags + module re-exports
//! ├── format.rs      # Tiny output helpers (TSV writers, JSON adapters)
//! ├── runtime.rs     # Daemon connection / auto-spawn / tokio glue
//! ├── pane.rs        # therminal pane …
//! ├── session.rs     # therminal session …
//! ├── workspace.rs   # therminal workspace …
//! ├── agents.rs      # therminal agents …
//! ├── events.rs      # therminal events --follow
//! └── semantic.rs    # therminal semantic commands|hotspots
//! ```

use clap::Args;

pub mod format;
pub mod runtime;

pub mod agents;
pub mod events;
pub mod pane;
pub mod semantic;
pub mod session;
pub mod workspace;

/// Shared output flags accepted by most subcommands.
///
/// Default output is intentionally terse TSV. `--json` produces a single
/// JSON document on stdout for callers that want to parse fields by name.
#[derive(Args, Debug, Default, Clone, Copy)]
pub struct OutputFlags {
    /// Emit a structured JSON document instead of terse TSV.
    #[arg(long)]
    pub json: bool,
}
