mod clipboard;
mod color_mapping;
mod grid_renderer;
mod menu;
mod pane;
mod url_detection;
mod window;

pub use grid_renderer::{FontConfig, GridRenderer, RenderCell};

use anyhow::Result;
use clap::Parser;
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

    tracing_subscriber::fmt()
        .with_env_filter(if cli.verbose { "debug" } else { "info" })
        .init();

    tracing::info!("therminal starting");
    window::run()
}
