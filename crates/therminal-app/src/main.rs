mod clipboard;
mod color_mapping;
mod grid_renderer;
mod mcp_stdio;
mod menu;
mod overlay;
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
    // Initialize clipboard before any threads exist. On WSL2 this performs
    // a transient WAYLAND_DISPLAY env mutation to force arboard onto the
    // X11 backend; it is only sound while we are still single-threaded.
    clipboard::init();

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

    // Open a persistent connection to therminal-daemon before window
    // creation. This is the first wiring step toward making the GUI a
    // daemon client (epic tn-382v); local PTY rendering is unaffected.
    let daemon_client = match connect_daemon() {
        Ok(client) => Some(client),
        Err(e) => {
            let socket_hint = if cfg!(windows) {
                r"\\.\pipe\therminal-daemon (start therminal-daemon.exe)".to_string()
            } else {
                therminal_runtime::paths::socket_path("daemon")
                    .display()
                    .to_string()
            };
            tracing::error!(
                error = %e,
                socket = %socket_hint,
                "failed to connect to therminal-daemon — start it and retry"
            );
            eprintln!(
                "therminal: could not connect to daemon at {socket_hint}\n  cause: {e:#}\n  hint:  start `therminal-daemon` and try again"
            );
            std::process::exit(1);
        }
    };

    window::run(daemon_client)
}

/// Open a persistent IPC connection to the therminal-daemon control socket.
///
/// Pings the daemon to validate the connection, then logs the reported
/// version, protocol_version, and socket path. Returns the live client so
/// the GUI can store it for later request multiplexing.
fn connect_daemon() -> Result<std::sync::Arc<therminal_daemon_client::DaemonClient>> {
    use therminal_daemon_client::DaemonClient;
    use therminal_protocol::daemon::IpcResponse;

    let socket_path = therminal_runtime::paths::socket_path("daemon");

    // Build a small multi-thread runtime that we leak intentionally — the
    // DaemonClient spawns its own connection task on it and outlives this
    // function for the rest of the process lifetime. This avoids forcing
    // `main` to be `#[tokio::main]` while keeping the existing sync winit
    // entry point intact.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name("therminal-daemon-rt")
        .build()?;
    let rt = Box::leak(Box::new(rt));

    let (client, pong) = rt.block_on(async {
        let client = DaemonClient::connect(&socket_path).await?;
        let pong = client.ping().await?;
        Ok::<_, anyhow::Error>((client, pong))
    })?;

    if let IpcResponse::Pong {
        protocol_version,
        version,
        ..
    } = pong
    {
        tracing::info!(
            socket = %socket_path.display(),
            version = %version,
            protocol_version,
            "connected to therminal-daemon"
        );
    } else {
        anyhow::bail!("unexpected daemon response to Ping: {:?}", pong);
    }

    Ok(std::sync::Arc::new(client))
}
