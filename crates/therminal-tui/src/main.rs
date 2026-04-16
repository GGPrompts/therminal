//! therminal-tui — Ratatui dashboard for the therminal daemon.
//!
//! Connects to the running therminal-daemon via IPC and renders a
//! tabbed TUI with sessions, panes, and agents views. Designed to run
//! inside a therminal pane (gets hotspots for free) but works in any
//! terminal emulator.

mod app;
mod backend;
mod pages;
mod palette;

use anyhow::Result;
use clap::Parser;

/// Ratatui dashboard for the therminal daemon.
#[derive(Parser, Debug)]
#[command(name = "therminal-tui", version, about)]
struct Cli {
    /// Override the daemon socket path (default: auto-detect).
    #[arg(long)]
    socket: Option<String>,
}

fn main() -> Result<()> {
    // Initialize tracing (respects RUST_LOG).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let socket_path = cli
        .socket
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| therminal_runtime::paths::socket_path("daemon"));

    app::run(socket_path)
}
