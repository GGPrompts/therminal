// tn-8lyq: On Windows release builds, compile as a GUI subsystem binary so
// Windows never allocates a console — this is what prevents the conhost
// flash when WSL Claude-Code hooks fire `therminal agent-event push` as
// a detached background process. Debug builds keep the default CONSOLE
// subsystem so `cargo run` behaves normally.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod claude_cwd;
mod cli;
mod clipboard;
mod color_mapping;
mod daemon_spawn;
pub(crate) mod git_state;
mod grid_renderer;
mod mcp_stdio;
mod menu;
mod model_context;
mod overlay;
mod pane;
pub(crate) mod system_metrics;
mod url_detection;
mod widgets;
mod window;

pub use grid_renderer::{FontConfig, GridRenderer, RenderCell};
pub use therminal_terminal::hotspot_detection::{HotspotKind, TextHotspot};

use anyhow::Result;
use clap::{Parser, Subcommand};
use therminal_core::config::TherminalConfig;

#[derive(Parser, Debug)]
#[command(
    name = "therminal",
    about = "The AI-native terminal emulator",
    long_about = "The AI-native terminal emulator.\n\
\n\
CLI-vs-MCP POLICY (for agents and orchestrators)\n\
-------------------------------------------------\n\
Use the CLI (`therminal` / `tn`) for frequent reads and writes:\n\
  pane list, pane peek, pane send, pane tag/untag,\n\
  agents list, workspace list, session list/create,\n\
  semantic commands/hotspots, events --follow.\n\
\n\
Use MCP when you need:\n\
  - Subscriptions  (terminal://pane/{id}/output, therminal://claude/events)\n\
  - Blocking waits (terminal.panes.wait_for_output)\n\
  - Conductor tick (terminal.panes.get_summary — MCP-only, ~120 B)\n\
  - Typed shape    (get_details, get_cadence, find_with_capacity)\n\
  - Admin ops      (sessions.destroy, panes.destroy — trust-tier enforced)\n\
\n\
See docs/cli.md for the full decision table and the `tn` short alias."
)]
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
    /// Pane operations (list, create, destroy, send, peek, tag, swap, …).
    #[command(subcommand)]
    Pane(cli::pane::PaneCmd),
    /// Session operations (list, create, destroy).
    #[command(subcommand)]
    Session(cli::session::SessionCmd),
    /// Workspace operations (list, switch).
    #[command(subcommand)]
    Workspace(cli::workspace::WorkspaceCmd),
    /// Agent registry queries.
    #[command(subcommand)]
    Agents(cli::agents::AgentsCmd),
    /// Stream daemon events to stdout (one JSON line per event).
    Events(cli::events::EventsArgs),
    /// Layout operations (atomic batch execution).
    #[command(subcommand)]
    Layout(cli::layout::LayoutCmd),
    /// Semantic queries (commands, hotspots).
    #[command(subcommand)]
    Semantic(cli::semantic::SemanticCmd),
    /// Push agent lifecycle events to the daemon (hook-push path).
    #[command(name = "agent-event", subcommand)]
    AgentEvent(cli::agent_event::AgentEventCmd),
    /// Print the `[[bookmarks]]` list from `therminal.toml` (tn-co6n).
    ///
    /// Text-first bookmark surface — two-column `name  url` per line, URLs
    /// become clickable via the existing URL hotspot regex. Filter with
    /// `--category <X>` or emit structured output with `--json`.
    Bookmarks(cli::bookmarks::BookmarksArgs),
    /// Launch the Ratatui TUI dashboard (therminal-tui).
    ///
    /// Locates the `therminal-tui` binary next to the current executable
    /// or on PATH and exec's it, forwarding any trailing arguments.
    Tui {
        /// Arguments forwarded to therminal-tui (e.g. --socket PATH).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// Attach to the parent process's console on Windows release builds.
///
/// Pairs with the `windows_subsystem = "windows"` attribute above: with the
/// GUI subsystem flag, Windows never allocates a console for us at startup
/// (no conhost flash), but CLI subcommands like `therminal pane list` still
/// need to write to the invoking shell. `AttachConsole(ATTACH_PARENT_PROCESS)`
/// inherits the parent console when launched from CMD / PowerShell / Windows
/// Terminal, and is a no-op when launched from a non-console parent (GUI
/// shortcut, WSL hook, Explorer double-click). Rust's stdio handles are
/// resolved lazily, so this must run before any `println!` / `eprintln!`.
#[cfg(all(target_os = "windows", not(debug_assertions)))]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // SAFETY: AttachConsole is a pure Win32 entry point with no Rust-side
    // aliasing or lifetime requirements. It runs single-threaded at process
    // startup before any worker threads exist. Return value is ignored —
    // failure just means "no console to attach to" which is the GUI / hook
    // path we explicitly want to support silently.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(all(target_os = "windows", not(debug_assertions))))]
fn attach_parent_console() {}

fn main() -> Result<()> {
    // tn-8lyq: pair with `windows_subsystem = "windows"` — attach to the
    // invoking console so CLI subcommand output still reaches the user.
    // Must run before any stdout/stderr write (clipboard::init, Cli::parse,
    // tracing-subscriber), because Rust stdio handles are cached on first use.
    attach_parent_console();

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

    // Tracing filter precedence: RUST_LOG (if set) > --verbose > default.
    // Honoring RUST_LOG is critical for targeted debugging — e.g.
    // `RUST_LOG=therminal_app::pane::remote_spawn=trace` to enable the
    // tn-wlu6 hex-dump instrumentation without flooding the rest of the
    // output. Without this, the env filter is hardcoded and RUST_LOG
    // is silently ignored.
    use tracing_subscriber::EnvFilter;
    let make_filter = |default: &str| -> EnvFilter {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default))
    };

    // Subcommands always log to stderr at warn level by default so the
    // structured stdout (TSV / JSON / MCP framing) stays clean.
    if let Some(cmd) = cli.command {
        tracing_subscriber::fmt()
            .with_env_filter(make_filter(if cli.verbose { "debug" } else { "warn" }))
            .with_writer(std::io::stderr)
            .init();
        return match cmd {
            // MCP stdio bridge — stdout is the MCP protocol, see mcp_stdio.rs.
            Command::Mcp => mcp_stdio::run(),
            // tn-k13n cache-friendly CLI surface. Each subcommand routes
            // through the daemon-client (auto-spawning the daemon if needed)
            // and writes terse TSV (or `--json`) to stdout.
            Command::Pane(c) => cli::runtime::with_runtime(|ctx| cli::pane::run(ctx, c)),
            Command::Session(c) => cli::runtime::with_runtime(|ctx| cli::session::run(ctx, c)),
            Command::Workspace(c) => cli::runtime::with_runtime(|ctx| cli::workspace::run(ctx, c)),
            Command::Agents(c) => cli::runtime::with_runtime(|ctx| cli::agents::run(ctx, c)),
            Command::Events(c) => cli::runtime::with_runtime(|ctx| cli::events::run(ctx, c)),
            Command::Layout(c) => cli::runtime::with_runtime(|ctx| cli::layout::run(ctx, c)),
            Command::Semantic(c) => cli::runtime::with_runtime(|ctx| cli::semantic::run(ctx, c)),
            Command::AgentEvent(c) => {
                cli::runtime::with_runtime(|ctx| cli::agent_event::run(ctx, c))
            }
            // tn-co6n: bookmarks are a pure config read, no daemon needed.
            Command::Bookmarks(args) => cli::bookmarks::run(args),
            Command::Tui { args } => exec_tui(&args),
        };
    }

    tracing_subscriber::fmt()
        .with_env_filter(make_filter(if cli.verbose { "debug" } else { "info" }))
        .init();

    tracing::info!("therminal starting");

    // Open a persistent connection to therminal-daemon before window
    // creation. This is the first wiring step toward making the GUI a
    // daemon client (epic tn-382v); local PTY rendering is unaffected.
    let config_for_daemon = TherminalConfig::load();
    let (daemon_client, daemon_runtime) = match connect_daemon() {
        Ok((client, handle)) => (Some(client), Some(handle)),
        Err(e) => {
            // tn-txs8 (folds tn-6q3v): if the failure looks like "no daemon
            // running", try to auto-spawn `therminal-daemon` next to the
            // current exe / on PATH / via [daemon] binary_path, then retry.
            if daemon_spawn::is_not_running_error(&e) {
                tracing::info!(
                    error = %e,
                    "no daemon detected on socket — attempting auto-spawn"
                );
                let binary_override = config_for_daemon.daemon.binary_path.as_deref();
                match daemon_spawn::auto_spawn(binary_override) {
                    Ok(path) => {
                        tracing::info!(
                            binary = %path.display(),
                            "spawned therminal-daemon, retrying connect"
                        );
                        match daemon_spawn::retry_connect(connect_daemon) {
                            Ok((client, handle)) => (Some(client), Some(handle)),
                            Err(retry_err) => {
                                tracing::error!(
                                    error = %retry_err,
                                    "auto-spawned daemon never came up — giving up"
                                );
                                eprintln!(
                                    "therminal: auto-spawned daemon never became reachable\n  cause: {retry_err:#}"
                                );
                                std::process::exit(1);
                            }
                        }
                    }
                    Err(spawn_err) => {
                        tracing::error!(error = %spawn_err, "auto-spawn failed");
                        eprintln!("therminal: {spawn_err:#}");
                        std::process::exit(1);
                    }
                }
            } else {
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
        }
    };

    window::run(daemon_client, daemon_runtime)
}

/// Locate and exec `therminal-tui`, forwarding any extra arguments.
///
/// Discovery chain (same as daemon auto-spawn):
/// 1. Next to the current executable.
/// 2. PATH lookup.
fn exec_tui(args: &[String]) -> Result<()> {
    let name = if cfg!(windows) {
        "therminal-tui.exe"
    } else {
        "therminal-tui"
    };

    // 1. Next to the current exe (cargo build puts workspace siblings here).
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return run_tui(&candidate, args);
        }
    }

    // 2. PATH lookup.
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return run_tui(&candidate, args);
        }
    }

    anyhow::bail!(
        "therminal-tui binary not found. \
         Install it next to therminal{exe} or add it to PATH.\n  \
         Build with: cargo build -p therminal-tui",
        exe = if cfg!(windows) { ".exe" } else { "" },
    );
}

fn run_tui(binary: &std::path::Path, args: &[String]) -> Result<()> {
    use anyhow::Context;
    use std::process::Command as Cmd;
    let status = Cmd::new(binary)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {}", binary.display()))?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Open a persistent IPC connection to the therminal-daemon control socket.
///
/// Pings the daemon to validate the connection, then logs the reported
/// version, protocol_version, and socket path. Returns the live client so
/// the GUI can store it for later request multiplexing.
fn connect_daemon() -> Result<(
    std::sync::Arc<therminal_daemon_client::DaemonClient>,
    tokio::runtime::Handle,
)> {
    use therminal_daemon_client::{DaemonClient, GUI_REQUEST_TIMEOUT};
    use therminal_protocol::daemon::IpcResponse;

    let socket_path = therminal_runtime::paths::socket_path("daemon");

    // Build a small multi-thread runtime that we leak intentionally — the
    // DaemonClient spawns its own connection task on it and outlives this
    // function for the rest of the process lifetime. This avoids forcing
    // `main` to be `#[tokio::main]` while keeping the existing sync winit
    // entry point intact. The handle is returned so the window/init attach
    // flow (tn-ytw2) can drive RPCs without relying on `Handle::try_current`
    // (which returns None when called from the winit event loop thread).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name("therminal-daemon-rt")
        .build()?;
    let rt = Box::leak(Box::new(rt));
    let handle = rt.handle().clone();

    let (client, pong) = rt.block_on(async {
        let client = DaemonClient::connect_with_timeout(&socket_path, GUI_REQUEST_TIMEOUT).await?;
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

    Ok((std::sync::Arc::new(client), handle))
}
