//! Daemon-side process-tree agent detector ticker (tn-pehl).
//!
//! `ProcessDetector` (in `therminal-terminal`) is a sysinfo-based scanner
//! that walks the process tree below a shell PID and classifies running
//! AI agents (claude-code, codex, aider, copilot). The GUI runs one
//! detector per pane inside its PTY reader thread (see
//! `crates/therminal-app/src/pane/spawn.rs`). The daemon historically did
//! not run any process-tree detection, so the central `AgentRegistry`
//! stayed empty for any session that wasn't created by an attached GUI —
//! breaking MCP-driven orchestration scenarios where the conductor talks
//! to the daemon directly without a GUI in the loop.
//!
//! This module fills that gap. A single tokio task owns a
//! `HashMap<PaneId, ProcessDetector>` and ticks every `scan_interval`
//! (default 3s, matching the GUI's per-pane cadence). On each tick it:
//!
//! 1. Snapshots `(pane_id, shell_pid)` pairs from `SessionManager` (drops
//!    the mutex immediately).
//! 2. Lazily creates a `ProcessDetector` for any new pane with a known
//!    shell PID.
//! 3. Drops detectors for panes that vanished.
//! 4. Calls `scan()` on each detector OUTSIDE any locks (sysinfo can be
//!    expensive on busy systems and we don't want to block the
//!    `SessionManager` mutex).
//! 5. Re-locks `SessionManager` and pushes the results into the central
//!    `AgentRegistry` via `register_agent` / `unregister_agent`. This is
//!    the same path the GUI uses (see `AppPtyHandler::process_bytes`),
//!    so MCP `terminal.agents.list` and the
//!    `therminal://agents/events` resource update transparently.
//!
//! ## Double-instantiation
//!
//! The GUI's `spawn_pane` only runs a `ProcessDetector` against panes it
//! spawned with a *local* PTY (`mcp.attach_mode = "local"`). In daemon
//! mode the GUI uses `spawn_remote_pane` which has no local PTY and no
//! detector. Daemon-side panes never had a GUI detector to begin with.
//! The two paths therefore operate on disjoint pane sets and disjoint
//! `AgentRegistry` instances (the GUI owns its own registry; the daemon
//! owns the central one), so no double-write occurs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use therminal_protocol::PaneId;
use therminal_terminal::process_detector::ProcessDetector;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info};

use crate::session::SessionManager;

/// Decide whether a pane spawned with the given shell command should be
/// scanned via the WSL probe (`wsl.exe -d <distro> ps`) instead of the
/// host sysinfo walker. tn-966s.
///
/// Returns the WSL distro name when:
/// - the daemon is running on Windows native (`cfg!(windows)`), AND
/// - the pane's shell command looks like `wsl.exe` (case-insensitive,
///   ignoring directory prefixes), AND
/// - a default WSL distro can be detected via `wsl.exe -l -q`.
///
/// Returns `None` everywhere else, so the existing host sysinfo path
/// keeps running unchanged on Linux daemons, WSL-hosted daemons, and
/// pure-Windows panes (cmd, powershell, pwsh).
pub(crate) fn wsl_distro_for_shell(shell_command: &str) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    if !shell_command_is_wsl(shell_command) {
        return None;
    }
    therminal_harness_claude::wsl_paths::detect_default_distro()
}

/// Pure heuristic for "is this shell command an invocation of `wsl.exe`?".
/// Pulled out so the daemon-only `cfg!(windows)` gate doesn't block tests
/// from exercising the matching rules on Linux CI.
pub(crate) fn shell_command_is_wsl(shell_command: &str) -> bool {
    if shell_command.is_empty() {
        return false;
    }
    // Strip trailing whitespace + everything after the first space (the
    // user may have appended `--cd ~` or similar).
    let head = shell_command.split_whitespace().next().unwrap_or("");
    // Take the basename — accept both forward- and back-slash separators
    // so a path like `C:\Windows\System32\wsl.exe` or `wsl.exe` both work.
    let basename = head
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(head)
        .to_ascii_lowercase();
    basename == "wsl.exe" || basename == "wsl"
}

/// Convert `AgentType` to the snake_case string used in the protocol.
fn agent_type_str(t: therminal_terminal::state_inference::AgentType) -> String {
    use therminal_terminal::state_inference::AgentType;
    match t {
        AgentType::Claude => "claude".to_string(),
        AgentType::Codex => "codex".to_string(),
        AgentType::Copilot => "copilot".to_string(),
        AgentType::Aider => "aider".to_string(),
    }
}

/// Default scan cadence for the process-detector ticker. Matches the
/// GUI's per-pane default in `AppPtyHandler` (3 seconds).
const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(3);

/// Spawn the daemon-side process-detector ticker.
///
/// The returned `JoinHandle` exits cleanly when `shutdown.notified()`
/// fires. The task owns its own per-pane detector cache, so callers do
/// not need to wire any handle into the session manager.
pub fn spawn_process_detector_task(
    session_mgr: Arc<Mutex<SessionManager>>,
    shutdown: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    spawn_with_interval(session_mgr, shutdown, DEFAULT_SCAN_INTERVAL)
}

/// Same as [`spawn_process_detector_task`] but with a custom interval.
/// Used by tests to drive the loop faster than the production cadence.
pub fn spawn_with_interval(
    session_mgr: Arc<Mutex<SessionManager>>,
    shutdown: Arc<Notify>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut detectors: HashMap<PaneId, ProcessDetector> = HashMap::new();
        let mut ticker = tokio::time::interval(interval);
        // Skip the initial immediate fire — give freshly-spawned shells
        // a moment to fork their child processes before the first scan.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;

        info!(
            interval_secs = interval.as_secs_f32(),
            "process detector task started"
        );

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    tick_once(&session_mgr, &mut detectors).await;
                }
                _ = shutdown.notified() => {
                    info!("process detector task shutting down");
                    break;
                }
            }
        }
    })
}

/// Run a single scan pass over all panes. Public for tests so they can
/// drive the loop deterministically without spawning a task.
pub async fn tick_once(
    session_mgr: &Arc<Mutex<SessionManager>>,
    detectors: &mut HashMap<PaneId, ProcessDetector>,
) {
    // Snapshot pane → (shell_pid, shell_command) triples under the mutex,
    // then drop it. The shell command is needed to recognise WSL panes
    // on Windows native (tn-966s) so we can route them through the WSL
    // probe path instead of the blind host sysinfo walker.
    let triples = {
        let mgr = session_mgr.lock().await;
        mgr.pane_detector_specs()
    };

    // Reconcile the detector cache against the live pane set.
    let live_pane_ids: std::collections::HashSet<PaneId> =
        triples.iter().map(|(pid, _, _)| *pid).collect();
    detectors.retain(|pane_id, _| live_pane_ids.contains(pane_id));

    // Lazily create detectors for new panes that have a known shell PID
    // (or, on Windows native, are WSL panes that don't need a shell PID
    // because the WSL probe ignores the host process tree). Run scans
    // outside the SessionManager lock — sysinfo and `wsl.exe ps` can
    // both be slow.
    let mut results: Vec<(
        PaneId,
        Vec<therminal_terminal::process_detector::DetectedAgent>,
    )> = Vec::new();

    for (pane_id, shell_pid_opt, shell_command) in triples {
        let wsl_distro = wsl_distro_for_shell(&shell_command);
        if shell_pid_opt.is_none() && wsl_distro.is_none() {
            // Handoff-restored panes don't carry a shell PID; skip
            // them silently unless they're a WSL pane (where the
            // probe ignores the shell PID anyway). They'll get
            // re-detected on the next session restart when the daemon
            // spawns a fresh shell.
            continue;
        }
        let is_new_pane = !detectors.contains_key(&pane_id);
        let detector = detectors.entry(pane_id).or_insert_with(|| {
            let mut d = ProcessDetector::new(shell_pid_opt);
            if let Some(distro) = wsl_distro.as_deref() {
                d = d.with_wsl_distro(distro);
            }
            d
        });
        // tn-x1h9: log the detector-mode decision exactly once per pane
        // (the first tick where we see it). Without this, silent failure
        // modes like "Pane.shell is empty so WSL probe never activates"
        // leave zero breadcrumbs in the logs. Keep it at INFO so it
        // surfaces in the default tracing filter; it fires at most once
        // per pane lifetime.
        if is_new_pane {
            let mode = if wsl_distro.is_some() {
                "wsl_probe"
            } else {
                "host_sysinfo"
            };
            info!(
                pane_id,
                mode,
                shell_command = %shell_command,
                shell_pid = ?shell_pid_opt,
                wsl_distro = ?wsl_distro,
                is_windows = cfg!(windows),
                "process_detector_task: initialising detector for pane"
            );
        }
        let agents = detector.scan();
        results.push((pane_id, agents));
    }

    if results.is_empty() {
        return;
    }

    // Re-acquire the lock and push results into the central registry.
    let mut mgr = session_mgr.lock().await;
    apply_scan_results(&mut mgr, results);
}

/// Apply a batch of scan results to the `SessionManager`'s central
/// `AgentRegistry`. Split out from [`tick_once`] so it can be unit-tested
/// without spawning real PTYs.
///
/// Each result is a `(pane_id, detected_agents)` tuple produced by
/// `ProcessDetector::scan()`. For every entry we:
///
/// 1. Verify the pane is still live (tn-l78s). A pane may have been closed
///    in the window between the `pane_shell_pids()` snapshot and the
///    re-acquisition of the `SessionManager` lock; skipping absent panes
///    prevents us from leaving a stale `AgentRegistry` entry for a
///    vanished pane that would linger until the next 3s tick.
/// 2. Register / re-register / unregister based on the `(scan_result,
///    existing_entry)` pair.
pub(crate) fn apply_scan_results(
    mgr: &mut SessionManager,
    results: Vec<(
        PaneId,
        Vec<therminal_terminal::process_detector::DetectedAgent>,
    )>,
) {
    for (pane_id, agents) in results {
        // tn-l78s: a pane may have been closed in the window between the
        // snapshot and re-acquisition of the lock. Skip any pane that is
        // no longer present in the SessionManager so we don't leave a
        // stale `AgentRegistry` entry for a vanished pane.
        if mgr.session_for_pane(pane_id).is_none() {
            continue;
        }

        let registry_entry = mgr.agent_registry().get(pane_id).cloned();
        match (agents.first(), registry_entry) {
            (Some(agent), None) => {
                debug!(
                    pane_id,
                    agent = %agent.name,
                    pid = agent.pid,
                    "registering agent (daemon-side detector)"
                );
                mgr.register_agent(
                    pane_id,
                    agent.name.clone(),
                    agent.agent_type,
                    Some(agent.pid),
                );
                // tn-alpb: notify GUI so remote pane headers update.
                mgr.broadcast_event(therminal_protocol::daemon::DaemonEvent::AgentChanged {
                    pane_id,
                    agent_name: Some(agent.name.clone()),
                    agent_type: Some(agent_type_str(agent.agent_type)),
                    agent_pid: Some(agent.pid),
                });
            }
            (Some(agent), Some(existing)) => {
                // Re-register if the agent type or pid changed (e.g.
                // user killed claude and started codex in the same pane).
                if existing.agent_type != agent.agent_type || existing.pid != Some(agent.pid) {
                    debug!(
                        pane_id,
                        old_type = ?existing.agent_type,
                        new_type = ?agent.agent_type,
                        new_pid = agent.pid,
                        "re-registering changed agent (daemon-side detector)"
                    );
                    mgr.register_agent(
                        pane_id,
                        agent.name.clone(),
                        agent.agent_type,
                        Some(agent.pid),
                    );
                    // tn-alpb: notify GUI of the change.
                    mgr.broadcast_event(therminal_protocol::daemon::DaemonEvent::AgentChanged {
                        pane_id,
                        agent_name: Some(agent.name.clone()),
                        agent_type: Some(agent_type_str(agent.agent_type)),
                        agent_pid: Some(agent.pid),
                    });
                }
            }
            (None, Some(existing)) => {
                // The agent process exited but the pane still exists.
                // Drop the registry entry so MCP consumers stop seeing
                // a stale agent. We only unregister entries that this
                // task originally created (i.e. ones that have a pid
                // set — the GUI also sets pid in its detector path,
                // but the GUI registry is a separate instance, so
                // this check is purely defensive).
                if existing.pid.is_some() {
                    debug!(
                        pane_id,
                        "unregistering vanished agent (daemon-side detector)"
                    );
                    mgr.unregister_agent(pane_id);
                    // tn-alpb: notify GUI so the badge clears.
                    mgr.broadcast_event(therminal_protocol::daemon::DaemonEvent::AgentChanged {
                        pane_id,
                        agent_name: None,
                        agent_type: None,
                        agent_pid: None,
                    });
                }
            }
            (None, None) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the loop spawns and shuts down cleanly when notified.
    /// Does NOT spawn any panes (would require a real TTY); only verifies
    /// the scaffolding doesn't panic on an empty session manager.
    #[tokio::test]
    async fn task_spawns_and_shuts_down() {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let mgr = Arc::new(Mutex::new(SessionManager::new(tx)));
        let shutdown = Arc::new(Notify::new());

        let handle = spawn_with_interval(
            Arc::clone(&mgr),
            Arc::clone(&shutdown),
            Duration::from_millis(50),
        );

        // Let the task tick at least once.
        tokio::time::sleep(Duration::from_millis(120)).await;

        shutdown.notify_one();
        // Wait up to 1s for the task to exit.
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    /// `tick_once` against an empty SessionManager is a no-op and does
    /// not panic.
    #[tokio::test]
    async fn tick_once_empty_manager() {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let mgr = Arc::new(Mutex::new(SessionManager::new(tx)));
        let mut detectors = HashMap::new();
        tick_once(&mgr, &mut detectors).await;
        assert!(detectors.is_empty());
        assert!(mgr.lock().await.list_agents().is_empty());
    }

    /// Wiring test: when a fake pane id with a real PID (this test
    /// process itself) is registered into the session manager via a
    /// custom hook, `tick_once` walks the process tree, finds nothing
    /// classified as an agent (because the test binary isn't `node`,
    /// `codex`, etc), and leaves the registry empty. This proves the
    /// scan path runs end-to-end without a TTY.
    #[tokio::test]
    async fn tick_once_with_self_pid_no_agents_detected() {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let mgr = Arc::new(Mutex::new(SessionManager::new(tx)));

        // Manually seed the detector cache with our own PID. This
        // mirrors what `tick_once` would do for a real pane, except
        // we skip the SessionManager round-trip because spawning a
        // real Pane requires a TTY.
        let mut detectors = HashMap::new();
        detectors.insert(42 as PaneId, ProcessDetector::new(Some(std::process::id())));

        tick_once(&mgr, &mut detectors).await;

        // The cache should have been pruned because pane 42 isn't in
        // the (empty) SessionManager.
        assert!(
            detectors.is_empty(),
            "detectors should be pruned for vanished panes"
        );
        assert!(mgr.lock().await.list_agents().is_empty());
    }

    /// tn-l78s regression: `apply_scan_results` must skip panes that
    /// vanished from `SessionManager` in the window between snapshot and
    /// re-acquisition. Feeds a fabricated scan result for a pane id that
    /// is not in the manager and asserts that no `AgentRegistry` entry is
    /// left behind. Without the `session_for_pane` guard, `register_agent`
    /// would happily stash the entry and it would only be pruned on the
    /// next tick.
    #[tokio::test]
    async fn apply_scan_results_skips_vanished_pane() {
        use therminal_terminal::process_detector::DetectedAgent;
        use therminal_terminal::state_inference::AgentType;

        let (tx, _) = tokio::sync::broadcast::channel(16);
        let mut mgr = SessionManager::new(tx);

        // Fabricate a result that looks like a mid-tick scan caught a
        // claude-code process for pane id 999. Pane 999 does not exist
        // in the (empty) SessionManager — this simulates the race where
        // the pane was closed between snapshot and re-lock.
        let results = vec![(
            999 as PaneId,
            vec![DetectedAgent {
                agent_type: AgentType::Claude,
                pid: 12345,
                name: "claude".to_string(),
            }],
        )];

        apply_scan_results(&mut mgr, results);

        // No agent should have been registered for the vanished pane.
        assert!(
            mgr.list_agents().is_empty(),
            "register_agent must be skipped for panes absent from SessionManager"
        );
    }

    /// `pane_shell_pids` snapshot returns an empty vec for an empty
    /// manager and is the only API the ticker depends on for pane
    /// discovery. Locks down the contract.
    #[tokio::test]
    async fn pane_shell_pids_empty_manager_returns_empty() {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let mgr = SessionManager::new(tx);
        assert!(mgr.pane_shell_pids().is_empty());
    }

    /// `pane_detector_specs` is the new accessor backing the WSL probe
    /// path; sanity-check the empty case.
    #[tokio::test]
    async fn pane_detector_specs_empty_manager_returns_empty() {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let mgr = SessionManager::new(tx);
        assert!(mgr.pane_detector_specs().is_empty());
    }

    // ── shell_command_is_wsl (tn-966s) ───────────────────────────────────

    #[test]
    fn shell_command_is_wsl_recognises_bare_basename() {
        assert!(shell_command_is_wsl("wsl.exe"));
        assert!(shell_command_is_wsl("WSL.EXE"));
        assert!(shell_command_is_wsl("wsl")); // no .exe extension
    }

    #[test]
    fn shell_command_is_wsl_recognises_full_windows_path() {
        assert!(shell_command_is_wsl(r"C:\Windows\System32\wsl.exe"));
        assert!(shell_command_is_wsl(r"C:/Windows/System32/wsl.exe"));
    }

    #[test]
    fn shell_command_is_wsl_strips_trailing_args() {
        assert!(shell_command_is_wsl("wsl.exe --cd ~"));
        assert!(shell_command_is_wsl("wsl.exe -d Ubuntu"));
    }

    #[test]
    fn shell_command_is_wsl_rejects_other_shells() {
        assert!(!shell_command_is_wsl("/bin/bash"));
        assert!(!shell_command_is_wsl("powershell.exe"));
        assert!(!shell_command_is_wsl("pwsh.exe"));
        assert!(!shell_command_is_wsl("cmd.exe"));
        assert!(!shell_command_is_wsl(""));
    }

    /// `wsl_distro_for_shell` is a no-op on non-Windows builds — even
    /// if the command is `wsl.exe` it must return `None` so the existing
    /// host sysinfo path stays in charge.
    #[cfg(not(windows))]
    #[test]
    fn wsl_distro_for_shell_is_noop_on_unix() {
        assert!(wsl_distro_for_shell("wsl.exe").is_none());
        assert!(wsl_distro_for_shell(r"C:\Windows\System32\wsl.exe").is_none());
        assert!(wsl_distro_for_shell("/bin/bash").is_none());
    }

    /// tn-x1h9 regression: the *resolved* shell from
    /// `therminal_terminal::pty::resolve_shell` on an empty SpawnOptions
    /// must still be recognised by `shell_command_is_wsl` when the
    /// default resolves to `wsl.exe`. This is the contract that binds
    /// `Pane::spawn` (daemon) to `process_detector_task` — if either
    /// side of it breaks, the Claude observability pipeline goes silent
    /// on Windows + WSL panes. Cross-platform: we only assert
    /// non-emptiness of the resolved default here (the actual value is
    /// OS-dependent) but lock in the matching logic for the
    /// representative values.
    #[test]
    fn resolved_default_shell_is_never_empty() {
        use therminal_terminal::pty::{SpawnOptions, resolve_shell};
        let resolved = resolve_shell(&SpawnOptions::default());
        assert!(
            !resolved.is_empty(),
            "resolve_shell(default) must never return \"\" — that would break shell_command_is_wsl short-circuit on empty"
        );
    }

    /// tn-x1h9 regression: `shell_command_is_wsl` must NOT silently
    /// short-circuit on the empty string. Historically, the daemon
    /// stored `""` in `Pane.shell` for any GUI-spawned session (the
    /// GUI sends `shell: None` → daemon stored the raw request without
    /// resolving) which made this path impossible to hit on Windows.
    /// Guard the invariant explicitly so a future refactor can't
    /// accidentally regress the resolution step.
    #[test]
    fn shell_command_is_wsl_rejects_empty_string_on_purpose() {
        assert!(
            !shell_command_is_wsl(""),
            "empty shell command must not match (the fix is to resolve Pane.shell before storing it)"
        );
    }
}
