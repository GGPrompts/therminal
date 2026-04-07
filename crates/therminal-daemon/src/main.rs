use anyhow::{Context, Result};
use clap::Parser;
use tokio::io::AsyncWriteExt;
use tracing::info;

use therminal_daemon::ipc_transport::connect_client;
use therminal_daemon::lifecycle::LifecycleConfig;
use therminal_daemon::{BUILD_HASH, EnsureResult, VERSION, ensure_daemon};

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

    /// Enter control mode: connect to the daemon via a text-based protocol
    /// for programmatic control (similar to tmux -CC).
    #[arg(long)]
    control_mode: bool,

    /// Print the control-mode protocol reference and exit.
    #[arg(long)]
    help_control: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(if cli.verbose { "debug" } else { "info" })
        .init();

    if cli.help_control {
        println!("{}", therminal_daemon::control::protocol_reference());
        return Ok(());
    }

    if cli.control_mode {
        return run_control_mode().await;
    }

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

/// Run as a control-mode client: connect to the daemon socket, send the
/// control-mode handshake, then bridge stdin/stdout to the text protocol.
async fn run_control_mode() -> Result<()> {
    let socket_path = therminal_runtime::paths::socket_path("daemon");

    let mut stream = connect_client(&socket_path).await.with_context(|| {
        format!(
            "failed to connect to daemon at {}. Is the daemon running?",
            socket_path.display()
        )
    })?;

    // Send control-mode handshake
    stream
        .write_all(b"mode: control\n")
        .await
        .context("failed to send control-mode handshake")?;
    stream.flush().await?;

    // Bridge stdin -> socket and socket -> stdout. tokio::io::split for
    // cross-platform support (UnixStream::into_split is Unix-only).
    let (reader, writer) = tokio::io::split(stream);

    let stdout_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut reader = reader;
        tokio::io::copy(&mut reader, &mut stdout).await
    });

    let stdin_task = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut writer = writer;
        tokio::io::copy(&mut stdin, &mut writer).await
    });

    // Wait for either direction to finish
    tokio::select! {
        r = stdout_task => {
            r.context("stdout task panicked")?.context("stdout copy failed")?;
        }
        r = stdin_task => {
            r.context("stdin task panicked")?.context("stdin copy failed")?;
        }
    }

    Ok(())
}
