mod clipboard;
mod color_mapping;
mod grid_renderer;
mod mcp_stdio;
mod menu;
mod pane;
mod url_detection;
mod window;

pub use grid_renderer::{FontConfig, GridRenderer, RenderCell};
pub use therminal_terminal::hotspot_detection::{HotspotKind, TextHotspot};

use anyhow::Result;
use clap::{Parser, Subcommand};
use therminal_core::config::TherminalConfig;

#[derive(Parser, Debug)]
#[command(name = "therminal", about = "The AI-native terminal emulator")]
struct Cli {
    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// Print the current effective configuration as TOML and exit.
    ///
    /// Loads the config file (or defaults if none exists) and dumps it to
    /// stdout in pretty TOML format.  Useful for inspecting the active
    /// settings or generating a starter config file:
    ///
    ///   therminal --print-config > ~/.config/therminal/therminal.toml
    #[arg(long)]
    print_config: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start an MCP server over stdio, bridging to the daemon's MCP socket.
    ///
    /// This allows MCP clients like Claude Code to interact with therminal
    /// sessions. Configure in Claude Code's MCP settings:
    ///
    ///   { "command": "therminal", "args": ["mcp"] }
    Mcp,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.print_config {
        // Load the effective config (writes a commented default if no file
        // exists so subsequent launches have something to edit).
        let config = TherminalConfig::load();
        print!("{}", config.to_toml_string());
        return Ok(());
    }

    if let Some(Command::Mcp) = cli.command {
        // MCP stdio bridge — logs to stderr only (stdout is the MCP protocol).
        tracing_subscriber::fmt()
            .with_env_filter(if cli.verbose { "debug" } else { "warn" })
            .with_writer(std::io::stderr)
            .init();

        return mcp_stdio::run();
    }

    tracing_subscriber::fmt()
        .with_env_filter(if cli.verbose { "debug" } else { "info" })
        .init();

    tracing::info!("therminal starting");
    window::run()
}
