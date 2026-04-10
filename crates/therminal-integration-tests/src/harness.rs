//! Test fixtures that spawn and drive a real `therminal-daemon` subprocess.
//!
//! See the crate-level docs in `lib.rs` for the design overview and
//! isolation model.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tempfile::TempDir;
use tokio::time::sleep;
use tracing::{debug, warn};

use therminal_daemon_client::{DaemonClient, ping};
use therminal_protocol::IpcResponse;
use therminal_protocol::daemon::{IpcRequest, PaneSummary};

/// How long to wait for the daemon subprocess to become reachable after
/// spawn before giving up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait during graceful shutdown before force-killing the child.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// An isolated daemon instance: subprocess + temp runtime dir + client.
///
/// Drop semantics send `GracefulShutdown` to the daemon, then force-kills
/// the child if it's still alive, then drops the `TempDir` (which removes
/// the runtime + data + config + cache directories).
///
/// Harness construction is async because spawning the subprocess is
/// synchronous but waiting for it to become reachable requires the tokio
/// runtime (ping + sleep).
pub struct DaemonHarness {
    /// The isolated temp dir that holds XDG_* directory overrides.
    /// Kept alive for the lifetime of the harness so the daemon's files
    /// remain on disk until drop.
    _tempdir: TempDir,
    /// Absolute path to the daemon's control socket (Unix) or named pipe
    /// (Windows) — the same path the daemon binds on startup.
    socket_path: PathBuf,
    /// Handle to the spawned daemon subprocess.
    ///
    /// `Option` so Drop can `take()` and move ownership into the cleanup
    /// path without tripping the borrow checker.
    child: Option<Child>,
    /// Persistent IPC client connected to the daemon.
    client: DaemonClient,
}

impl DaemonHarness {
    /// Spawn a fresh daemon in an isolated temp runtime dir and return a
    /// ready-to-use harness.
    ///
    /// The caller should `.await` this inside a tokio test; on success the
    /// daemon has already answered at least one `Ping`.
    pub async fn spawn() -> Result<Self> {
        Self::spawn_with_setup(|_config_dir| Ok(())).await
    }

    /// Same as [`Self::spawn`], but runs `setup` against the hermetic
    /// config directory (`$XDG_CONFIG_HOME`) before launching the daemon.
    /// Used by scenarios that need to stage files the daemon reads at
    /// startup (pattern packs, therminal.toml, trust config, ...).
    pub async fn spawn_with_setup<F>(setup: F) -> Result<Self>
    where
        F: FnOnce(&Path) -> Result<()>,
    {
        // Set up fresh isolated XDG directories under a tempdir. The
        // daemon's `therminal-runtime` crate reads XDG_RUNTIME_DIR for its
        // socket path and XDG_{CONFIG,DATA,CACHE}_HOME for everything else
        // — isolating all four keeps tests hermetic.
        let tempdir = tempfile::tempdir().context("failed to create isolated tempdir")?;
        let base = tempdir.path();
        let runtime_dir = base.join("runtime");
        let config_dir = base.join("config");
        let data_dir = base.join("data");
        let cache_dir = base.join("cache");
        for dir in [&runtime_dir, &config_dir, &data_dir, &cache_dir] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create XDG dir {}", dir.display()))?;
        }

        // Let callers stage files under the hermetic config dir before
        // the daemon starts. The daemon reads its config, trust TOML,
        // and pattern packs at startup, so this is the one moment where
        // tests can drop files into `$XDG_CONFIG_HOME/therminal/...`.
        setup(&config_dir).context("harness pre-spawn setup failed")?;

        // Mirror `therminal_runtime::paths::socket_path("daemon")` on the
        // host OS so we know where to connect without pulling in that
        // crate as a dependency.
        let socket_path = compute_socket_path(&runtime_dir, "daemon");

        // Locate the built daemon binary. We rely on cargo having built it
        // as a transitive dev-dependency — see `Cargo.toml`. The binary
        // lives next to the test harness under `target/<profile>/`.
        let daemon_bin = locate_daemon_binary()?;
        debug!(path = %daemon_bin.display(), "spawning therminal-daemon");

        // Spawn with `--foreground` so we can manage the subprocess
        // directly (no double-fork), and a minimal `keep_alive` so an
        // orphaned daemon (test panic before Drop) self-terminates quickly.
        let child = Command::new(&daemon_bin)
            .arg("--foreground")
            .arg("--keep-alive")
            .arg("0")
            // Hermetic environment — clear everything the host user has
            // set and then lay down the minimum the daemon needs. We
            // intentionally do NOT pass HOME through; the daemon derives
            // all of its paths from XDG_* overrides.
            .env_clear()
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("XDG_CONFIG_HOME", &config_dir)
            .env("XDG_DATA_HOME", &data_dir)
            .env("XDG_CACHE_HOME", &cache_dir)
            // Forward a handful of vars required for spawning a shell PTY
            // and for debuggability. PATH is required so the daemon can
            // `exec` the user's shell; $SHELL picks which shell.
            .envs(passthrough_env(&[
                "PATH", "SHELL", "USER", "LOGNAME", "TERM",
            ]))
            // Never spam test output with daemon logs; callers can set
            // `RUST_LOG=debug` if they want to see them.
            .env("RUST_LOG", "error")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn {}", daemon_bin.display()))?;

        // Wait for the daemon's socket to become reachable. The daemon
        // prints no machine-readable "ready" signal, so we poll Ping with
        // a deadline. If either the wait or the subsequent connect fails,
        // kill the child so we don't leave a subprocess behind.
        let mut child_opt = Some(child);
        let startup = async {
            wait_for_ping(&socket_path, STARTUP_TIMEOUT).await?;
            DaemonClient::connect(&socket_path)
                .await
                .context("failed to connect DaemonClient after ping succeeded")
        }
        .await;
        let client = match startup {
            Ok(c) => c,
            Err(e) => {
                if let Some(child) = child_opt.take() {
                    let _ = kill_child(child);
                }
                return Err(e);
            }
        };

        Ok(Self {
            _tempdir: tempdir,
            socket_path,
            child: child_opt,
            client,
        })
    }

    /// Borrow the persistent IPC client.
    pub fn client(&self) -> &DaemonClient {
        &self.client
    }

    /// The socket path this daemon is bound to. Useful for tests that
    /// want to send one-shot requests via `therminal_daemon_client::send_request`.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Create a session and return `(session_id, default_pane_id)`.
    ///
    /// Wraps the two-step dance of `CreateSession` (which returns only
    /// the session id) followed by `ListPanes { session_id }` to pick up
    /// the default pane.
    pub async fn create_session_with_pane(&self, name: Option<&str>) -> Result<(u64, u64)> {
        let resp = self
            .client
            .send_request(IpcRequest::CreateSession {
                name: name.map(str::to_string),
                cols: None,
                rows: None,
            })
            .await?;
        let session_id = match resp {
            IpcResponse::SessionCreated { session_id } => session_id,
            other => bail!("expected SessionCreated, got {other:?}"),
        };

        let resp = self
            .client
            .send_request(IpcRequest::ListPanes {
                session_id: Some(session_id),
            })
            .await?;
        let pane = match resp {
            IpcResponse::Panes { panes } => panes
                .into_iter()
                .find(|p| p.session_id == session_id)
                .ok_or_else(|| anyhow!("no panes in newly-created session {session_id}"))?,
            other => bail!("expected Panes, got {other:?}"),
        };
        Ok((session_id, pane.pane_id))
    }

    /// Convenience: list all panes across all sessions as the daemon
    /// sees them right now.
    pub async fn list_panes(&self) -> Result<Vec<PaneSummary>> {
        let resp = self
            .client
            .send_request(IpcRequest::ListPanes { session_id: None })
            .await?;
        match resp {
            IpcResponse::Panes { panes } => Ok(panes),
            other => bail!("expected Panes, got {other:?}"),
        }
    }
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        // Best-effort shutdown: send GracefulShutdown via a blocking
        // one-shot client, then kill if still running. We're in Drop so
        // we cannot .await — use tokio's `block_on` on a short-lived
        // current-thread runtime, matching how daemon-client exposes its
        // async API. If there's no ambient runtime, we fall back to a
        // scratch runtime.
        if let Some(child) = self.child.take() {
            let socket_path = self.socket_path.clone();
            let res = std::thread::spawn(move || -> Result<()> {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(async {
                    // Ignore errors — if the daemon is already gone,
                    // kill_child below will clean up.
                    let _ = tokio::time::timeout(
                        SHUTDOWN_TIMEOUT,
                        therminal_daemon_client::request_shutdown(&socket_path),
                    )
                    .await;
                });
                Ok(())
            })
            .join();
            if let Err(e) = res {
                warn!(?e, "shutdown thread panicked");
            }
            let _ = kill_child(child);
        }
    }
}

// ── Waiter helpers ────────────────────────────────────────────────────────

/// Poll `get_content`-style grid captures on `pane_id` until `predicate`
/// returns true or the timeout elapses.
///
/// Returns the captured grid text that satisfied the predicate. The
/// `predicate` receives the joined grid text (trailing whitespace trimmed
/// per row, joined with newlines).
///
/// This is the one piece of "wait for the PTY to actually produce bytes"
/// polling that every scenario needs. Scenarios that care about richer
/// semantic events (OSC 133 marks, hotspots) can layer on top.
pub async fn wait_for_output<F>(
    client: &DaemonClient,
    pane_id: u64,
    timeout: Duration,
    mut predicate: F,
) -> Result<String>
where
    F: FnMut(&str) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let resp = client
            .send_request(IpcRequest::CapturePane { pane_id })
            .await?;
        let text = match resp {
            IpcResponse::PaneCaptured { lines, .. } => lines
                .iter()
                .map(|line| line.trim_end())
                .collect::<Vec<_>>()
                .join("\n"),
            IpcResponse::Error { message } => bail!("CapturePane failed: {message}"),
            other => bail!("unexpected response to CapturePane: {other:?}"),
        };
        if predicate(&text) {
            return Ok(text);
        }
        if Instant::now() >= deadline {
            // Surface the most recent capture so a failing predicate can
            // be debugged from the test output.
            bail!(
                "wait_for_output timed out after {}ms on pane {pane_id}. Last captured text:\n{text}",
                timeout.as_millis()
            );
        }
        sleep(Duration::from_millis(50)).await;
    }
}

// ── Subprocess lifecycle helpers ──────────────────────────────────────────

/// Poll `Ping` until the daemon answers or we run out of time.
async fn wait_for_ping(socket_path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<anyhow::Error> = None;
    while Instant::now() < deadline {
        match ping(socket_path).await {
            Ok(IpcResponse::Pong { .. }) => return Ok(()),
            Ok(other) => {
                last_err = Some(anyhow!("unexpected ping response: {other:?}"));
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    Err(last_err.unwrap_or_else(|| anyhow!("daemon did not become reachable in time")))
}

/// Try graceful `wait` first, escalate to `kill` on timeout.
fn kill_child(mut child: Child) -> Result<()> {
    // try_wait first — if the daemon already exited cleanly there's
    // nothing to do.
    match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(e) => return Err(e.into()),
    }
    // Still running — force-kill.
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

/// Read the requested env vars from the test process and return only
/// those that are set. Used to forward a carefully curated allow-list
/// to the spawned daemon subprocess.
fn passthrough_env<'a>(keys: &'a [&'a str]) -> Vec<(String, String)> {
    keys.iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// Mirror `therminal_runtime::paths::socket_path(name)` without pulling
/// in that crate.
///
/// - **Unix**: `<runtime_dir>/therminal/<name>.sock`
/// - **Windows**: `\\.\pipe\therminal-<name>` (the runtime_dir override is
///   ignored — named pipes have no filesystem location).
fn compute_socket_path(runtime_dir: &Path, name: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!(r"\\.\pipe\therminal-{name}"))
    } else {
        runtime_dir.join("therminal").join(format!("{name}.sock"))
    }
}

/// Walk up from the current test binary to find `target/<profile>/therminal-daemon`.
///
/// Cargo sets `CARGO_BIN_EXE_<name>` only for integration tests of the
/// same package where the binary lives. Because we're a separate crate,
/// we do the traversal manually: `current_exe()` under `cargo test` is
/// something like `target/debug/deps/therminal_integration_tests-HASH`,
/// so the daemon binary is at `../../therminal-daemon[.exe]`.
fn locate_daemon_binary() -> Result<PathBuf> {
    // Allow an explicit override for out-of-tree runs (e.g. packagers).
    if let Ok(explicit) = std::env::var("THERMINAL_DAEMON_BIN") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Ok(p);
        }
        bail!(
            "THERMINAL_DAEMON_BIN is set but {} does not exist",
            p.display()
        );
    }

    let exe = std::env::current_exe().context("failed to read current_exe")?;
    // target/<profile>/deps/<test-binary>
    //                 ^^^^ parent of deps is where the daemon binary lives.
    let deps_dir = exe
        .parent()
        .ok_or_else(|| anyhow!("current_exe has no parent: {}", exe.display()))?;
    let target_profile_dir = deps_dir
        .parent()
        .ok_or_else(|| anyhow!("cannot find target profile dir from {}", deps_dir.display()))?;

    let bin_name = if cfg!(windows) {
        "therminal-daemon.exe"
    } else {
        "therminal-daemon"
    };
    let candidate = target_profile_dir.join(bin_name);
    if candidate.is_file() {
        return Ok(candidate);
    }

    // One more fallback: some workspace layouts nest the binary under
    // `target/<profile>/` but the test might have been launched from a
    // different deps dir (e.g. `cargo nextest`). Walk up a little.
    let mut ancestor = target_profile_dir.to_path_buf();
    for _ in 0..4 {
        let c = ancestor.join(bin_name);
        if c.is_file() {
            return Ok(c);
        }
        match ancestor.parent() {
            Some(p) => ancestor = p.to_path_buf(),
            None => break,
        }
    }

    // Last resort: run `cargo build -p therminal-daemon` on the fly.
    // This handles the `cargo test -p therminal-integration-tests` case
    // where the daemon binary isn't a transitive target of the test
    // binary and hasn't been built yet. Cargo gracefully handles
    // re-entrant invocations (the CARGO env var lets the inner process
    // coordinate with the outer one).
    if let Some(cargo) = std::env::var_os("CARGO") {
        debug!("daemon binary not found — building therminal-daemon on the fly");
        let status = Command::new(cargo)
            .args(["build", "-p", "therminal-daemon"])
            .status();
        if let Ok(s) = status
            && s.success()
        {
            let candidate = target_profile_dir.join(bin_name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    bail!(
        "could not locate {bin_name} from test binary {}. \
         Try `cargo build -p therminal-daemon` first, or set THERMINAL_DAEMON_BIN.",
        exe.display()
    )
}
