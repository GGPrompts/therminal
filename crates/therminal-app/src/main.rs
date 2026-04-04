mod clipboard;
mod color_mapping;
mod grid_renderer;
mod url_detection;
mod window;

pub use grid_renderer::{FontConfig, GridRenderer, RenderCell};

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "therminal", about = "The AI-native terminal emulator")]
struct Cli {
    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(if cli.verbose { "debug" } else { "info" })
        .init();

    tracing::info!("therminal starting");
    window::run()
}
