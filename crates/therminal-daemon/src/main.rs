use anyhow::Result;
use clap::Parser;
use tracing::info;

use therminal_daemon::lifecycle::LifecycleConfig;
use therminal_daemon::{ensure_daemon, EnsureResult, BUILD_HASH, VERSION};

#[derive(Parser, Debug)]
#[command(name = "therminal-daemon", about = "Therminal session daemon")]
struct Cli {
    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// Run in foreground (don't daemonize)
    #[arg(long)]
    foreground: bool,

    /// Keep-alive duration in seconds after last session closes (0 = exit immediately)
    #[arg(long, default_value = "300")]
    keep_alive: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(if cli.verbose { "debug" } else { "info" })
        .init();

    info!(
        build_hash = BUILD_HASH,
        version = VERSION,
        "therminal-daemon starting"
    );

    let config = LifecycleConfig {
        keep_alive: if cli.keep_alive == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(cli.keep_alive))
        },
        ..Default::default()
    };

    match ensure_daemon(config).await? {
        EnsureResult::Reused => {
            info!("existing daemon is running with matching build, exiting");
        }
        EnsureResult::Started { lifecycle } => {
            info!("daemon started, waiting for shutdown");
            // Wait for the lifecycle to reach Stopped
            let mut state_rx = lifecycle.watch_state();
            while *state_rx.borrow_and_update() != therminal_protocol::DaemonState::Stopped {
                if state_rx.changed().await.is_err() {
                    break;
                }
            }
            info!("daemon stopped");
        }
    }

    Ok(())
}
