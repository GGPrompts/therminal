use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "therminal-daemon", about = "Therminal session daemon")]
struct Cli {
    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(if cli.verbose { "debug" } else { "info" })
        .init();

    tracing::info!("therminal-daemon starting");
    Ok(())
}
