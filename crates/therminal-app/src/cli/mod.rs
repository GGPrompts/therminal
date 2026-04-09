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

#[cfg(test)]
mod parity_tests {
    //! CLI ↔ MCP parity test (tn-8ysl).
    //!
    //! This is the CLI-side mirror of the `SHARED_SURFACE` allowlist in
    //! `crates/therminal-daemon/src/mcp/mod.rs`. Both tests must agree.
    //! If you add a shared-surface operation, update both lists.
    //!
    //! The daemon-side test checks that each MCP tool name in the
    //! allowlist exists in `tool_definitions()`. This test checks that
    //! each CLI subcommand path in the allowlist parses cleanly through
    //! `clap`, so a rename or drop on the CLI side fails loudly too.

    use clap::{Parser, Subcommand};

    /// Shared surface: `(mcp_tool_name, cli_argv_tail)` pairs.
    /// Must stay in sync with `SHARED_SURFACE` in the daemon crate's
    /// `mcp::mod::tests` module.
    const SHARED_SURFACE: &[(&str, &[&str])] = &[
        ("terminal.sessions.list", &["session", "list"]),
        ("terminal.panes.list", &["pane", "list"]),
        ("terminal.panes.write", &["pane", "send", "1", "hi"]),
        ("terminal.panes.peek", &["pane", "peek", "1"]),
        ("terminal.panes.tag", &["pane", "tag", "1", "k=v"]),
        ("terminal.panes.untag", &["pane", "untag", "1", "k"]),
        ("terminal.agents.list", &["agents", "list"]),
        ("terminal.workspaces.list", &["workspace", "list"]),
        (
            "terminal.semantic.query_commands",
            &["semantic", "commands", "1"],
        ),
        (
            "terminal.semantic.get_hotspots",
            &["semantic", "hotspots", "1"],
        ),
    ];

    /// Minimal clap root that mirrors `Command` in `main.rs` just enough
    /// to exercise the shared-surface subcommands without pulling in the
    /// GUI's full CLI (which wants a winit event loop context).
    #[derive(Parser, Debug)]
    #[command(name = "therminal")]
    struct ParityRoot {
        #[command(subcommand)]
        cmd: ParityCmd,
    }

    #[derive(Subcommand, Debug)]
    enum ParityCmd {
        Pane {
            #[command(subcommand)]
            cmd: super::pane::PaneCmd,
        },
        Session {
            #[command(subcommand)]
            cmd: super::session::SessionCmd,
        },
        Workspace {
            #[command(subcommand)]
            cmd: super::workspace::WorkspaceCmd,
        },
        Agents {
            #[command(subcommand)]
            cmd: super::agents::AgentsCmd,
        },
        Semantic {
            #[command(subcommand)]
            cmd: super::semantic::SemanticCmd,
        },
    }

    #[test]
    fn shared_surface_cli_paths_parse() {
        for (mcp, argv_tail) in SHARED_SURFACE {
            let mut argv: Vec<&str> = vec!["therminal"];
            argv.extend_from_slice(argv_tail);
            match ParityRoot::try_parse_from(&argv) {
                Ok(_) => {}
                Err(e) => panic!(
                    "shared-surface CLI path {:?} failed to parse (MCP counterpart: `{mcp}`): {e}",
                    argv_tail
                ),
            }
        }
    }
}
