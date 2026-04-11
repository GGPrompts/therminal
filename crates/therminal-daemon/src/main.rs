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

    // Tracing filter precedence: RUST_LOG (if set) > --verbose > default.
    // Without honoring RUST_LOG, targeted debugging like
    // `RUST_LOG=therminal_daemon::server=trace` is silently ignored.
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(if cli.verbose { "debug" } else { "info" }));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if cli.help_control {
        println!("{}", therminal_daemon::control::protocol_reference());
        return Ok(());
    }

    if cli.control_mode {
        return run_control_mode().await;
    }

    // On Windows the GUI launches us with DETACHED_PROCESS, so we have no
    // console.  ConPTY's CreatePseudoConsole then spawns a fresh conhost.exe
    // for every pane — each one briefly flashing a visible window.  Allocate
    // a hidden console once so ConPTY can reuse it for all pseudoconsoles.
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{AllocConsole, GetConsoleWindow};
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
        // SAFETY: AllocConsole, GetConsoleWindow, and ShowWindow are pure
        // Win32 entry points with no Rust-side aliasing or lifetime
        // requirements. This runs single-threaded at process startup before
        // any worker threads are spawned, so there is no concurrent caller
        // racing on the process console. AllocConsole's only precondition
        // is that the process not already own a console — when the GUI
        // launches us with DETACHED_PROCESS that holds; if it doesn't (e.g.
        // a future caller starts the daemon from an existing terminal),
        // AllocConsole returns 0 (FALSE) and we log + skip the hide step
        // rather than panicking. ShowWindow's HWND is null-checked before
        // use, and SW_HIDE is a documented constant.
        unsafe {
            // AllocConsole returns BOOL (i32): non-zero on success, zero on
            // failure (e.g. the process already has a console). Don't panic
            // on failure — alloc-failure during console bootstrap is a soft
            // condition; ConPTY just won't get the hidden-conhost reuse
            // optimisation. Log a warning and continue.
            if AllocConsole() == 0 {
                let err = std::io::Error::last_os_error();
                tracing::warn!(
                    error = %err,
                    "AllocConsole failed; ConPTY conhost windows may flash for each pane"
                );
            } else {
                let hwnd = GetConsoleWindow();
                if !hwnd.is_null() {
                    ShowWindow(hwnd, SW_HIDE);
                }
            }
        }
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
