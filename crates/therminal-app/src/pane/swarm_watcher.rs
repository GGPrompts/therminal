//! Swarm watcher — auto-spawns therminal panes that tail Claude subagent JSONL streams.
//!
//! This is the therminal-native equivalent of thermal-desktop's `swarm_watcher`.
//! Where thermal-desktop spawned external kitty windows via `hyprctl`, this
//! module sends events to the winit event loop to split a pane and run
//! `tail -F` against the subagent JSONL.
//!
//! # Discovery
//!
//! Claude Code stores subagent transcripts at:
//! `~/.claude/projects/{project-hash}/{parent-sid}/subagents/agent-{id}.jsonl`
//!
//! A background OS thread polls every [`POLL_INTERVAL`] for new files matching
//! that layout. On first sight of an unseen `agent_id`, a
//! [`SpawnSubagent`](SwarmWatcherEvent::SpawnSubagent) event is dispatched.
//! When the file's mtime has been unchanged for [`STALENESS_TIMEOUT`], a
//! [`ReclaimSubagent`](SwarmWatcherEvent::ReclaimSubagent) event is dispatched
//! and the pane is closed.
//!
//! # Scope filtering
//!
//! When [`SwarmWatchScope::Current`] is configured, the watcher only emits
//! spawn events for subagents whose parent Claude Code session is owned by
//! THIS therminal instance. Ownership is determined by matching the parent
//! session ID (extracted from the subagent JSONL path) against the set of
//! Claude session IDs known to be running in this instance's panes, supplied
//! via [`PaneSessionIdProvider`]. This avoids PID-namespace mismatches that
//! occur when the GUI runs as a Windows native process but panes execute
//! inside WSL2 (tn-twfg).
//!
//! Filesystem polling (rather than the daemon's `TaggedAgentEvent` broadcast)
//! is used so the watcher works even when the daemon isn't running, and so the
//! app crate doesn't need to depend on `therminal-daemon`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use therminal_core::config::SwarmWatchScope;
use tracing::{debug, info, warn};

/// How often the watcher thread scans for new subagent JSONLs.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long a JSONL file's mtime must be unchanged before the subagent is
/// considered done and the pane is reclaimed. Mirrors thermal-desktop.
pub const STALENESS_TIMEOUT: Duration = Duration::from_secs(30);

/// Claude session IDs known to belong to panes in this therminal instance.
/// Supplied by the host application and updated each tick from
/// `PaneStatus.claude_session_id`. The watcher thread reads this set
/// (under a lock) to determine which subagent parent sessions are "owned".
pub type PaneSessionIdProvider = Arc<Mutex<HashSet<String>>>;

/// Events emitted by the watcher thread, consumed by the winit event loop.
///
/// Wrapped in `UserEvent` variants by the caller — kept as a separate enum so
/// this module can be tested without pulling in `winit`.
#[derive(Debug, Clone)]
pub enum SwarmWatcherEvent {
    /// A new subagent appeared. Open a pane that tails its JSONL, or a
    /// plain terminal pane when no JSONL path is available (hook-driven
    /// subagents without file-scanner fallback).
    SpawnSubagent {
        agent_id: String,
        jsonl_path: Option<PathBuf>,
    },
    /// The subagent's JSONL has gone stale. Close the pane that tails it.
    ReclaimSubagent { agent_id: String },
}

/// Per-tracked subagent state.
struct Tracked {
    jsonl_path: PathBuf,
    last_mtime: Option<SystemTime>,
    /// When mtime stopped changing (None = still active).
    unchanged_since: Option<Instant>,
    /// Whether we've already emitted a Reclaim for this agent.
    reclaimed: bool,
}

/// Resolve the Claude projects directory.
///
/// On Windows native with WSL, Claude Code's `.claude/projects/` lives inside
/// the WSL filesystem. We probe for the WSL distro and home directory and
/// return a `\\wsl.localhost\<distro>\home\<user>\.claude\projects` UNC path
/// that Windows can traverse. Falls back to the Windows home if WSL detection
/// fails.
fn claude_projects_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(distro) = crate::window::wsl_paths::detect_default_distro() {
            if let Some(home) = crate::window::wsl_paths::detect_wsl_home() {
                if let Some(p) = therminal_harness_claude::wsl_paths::linux_to_unc(
                    &distro,
                    &format!("{home}/.claude/projects"),
                ) {
                    return p;
                }
            }
        }
    }
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".claude")
        .join("projects")
}

/// Extract the parent session id from a subagent JSONL path.
///
/// Layout: `~/.claude/projects/{project-hash}/{parent-sid}/subagents/agent-*.jsonl`
fn parent_sid_from_path(path: &Path) -> Option<String> {
    let session_dir = path.parent()?.parent()?;
    session_dir
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

/// Walk `~/.claude/projects/*/*/subagents/` and yield every existing
/// `agent-*.jsonl` path.
fn scan_subagent_files() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let projects = claude_projects_dir();
    let project_iter = match std::fs::read_dir(&projects) {
        Ok(it) => it,
        Err(_) => return out,
    };
    for project in project_iter.flatten() {
        let project_path = project.path();
        if !project_path.is_dir() {
            continue;
        }
        let session_iter = match std::fs::read_dir(&project_path) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for session in session_iter.flatten() {
            let sub_dir = session.path().join("subagents");
            if !sub_dir.is_dir() {
                continue;
            }
            let agent_iter = match std::fs::read_dir(&sub_dir) {
                Ok(it) => it,
                Err(_) => continue,
            };
            for agent in agent_iter.flatten() {
                let path = agent.path();
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                // Match `agent-{id}.jsonl` (skip `.meta.json` siblings).
                let Some(rest) = name.strip_prefix("agent-") else {
                    continue;
                };
                let Some(id) = rest.strip_suffix(".jsonl") else {
                    continue;
                };
                out.push((id.to_string(), path));
            }
        }
    }
    out
}

// ── Owned-session-set resolution (tn-twfg) ───────────────────────────
//
// Prior to tn-twfg this module walked `/tmp/claude-code-state/*.json`,
// read each file's `pid` field, built a sysinfo process-descendant tree
// from the pane root PIDs, and matched.  That broke on Windows+WSL
// because the GUI's host-side PIDs live in a different namespace from the
// WSL-side PIDs recorded in the state files.
//
// The new approach is session-ID-based: the host application supplies a
// `PaneSessionIdProvider` containing the Claude session IDs that are
// currently known to be running in this therminal instance's panes
// (populated from `PaneStatus.claude_session_id`, which the daemon
// forwarder already resolves cross-namespace).  The watcher simply
// checks whether a subagent's parent session ID (extracted from the
// JSONL path) is in that set.

/// Pure scope-filter helper: returns whether a discovered subagent should be
/// considered "in scope" given the current configuration and an owned set.
///
/// Extracted as a free function so unit tests can exercise the logic without
/// running an entire watcher thread.
pub(crate) fn subagent_in_scope(
    scope: SwarmWatchScope,
    owned: &HashSet<String>,
    jsonl_path: &Path,
) -> bool {
    match scope {
        SwarmWatchScope::All => true,
        SwarmWatchScope::Current => parent_sid_from_path(jsonl_path)
            .map(|sid| owned.contains(&sid))
            .unwrap_or(false),
    }
}

/// Spawn the watcher thread. Returns a receiver that the event loop polls.
///
/// The thread runs forever (or until the receiver is dropped). It polls every
/// [`POLL_INTERVAL`] and emits events on the channel.
///
/// `scope` controls scope filtering. When set to [`SwarmWatchScope::Current`],
/// `pane_session_ids` MUST be supplied — it provides the live set of Claude
/// session IDs running in this therminal instance's panes, against which
/// subagent parent session IDs are matched (tn-twfg). With
/// [`SwarmWatchScope::All`] the provider is ignored.
pub fn spawn(
    scope: SwarmWatchScope,
    pane_session_ids: Option<PaneSessionIdProvider>,
) -> mpsc::Receiver<SwarmWatcherEvent> {
    let (tx, rx) = mpsc::channel();

    thread::Builder::new()
        .name("swarm-watcher".into())
        .spawn(move || {
            info!(
                ?scope,
                "swarm watcher started — scanning for Claude subagents"
            );
            let mut tracked: HashMap<String, Tracked> = HashMap::new();

            loop {
                // Snapshot known session IDs (cheap clone under the lock).
                let owned: HashSet<String> = match (&scope, pane_session_ids.as_ref()) {
                    (SwarmWatchScope::Current, Some(p)) => {
                        p.lock().map(|g| g.clone()).unwrap_or_default()
                    }
                    (SwarmWatchScope::Current, None) => {
                        // No provider supplied — fall back to scope All
                        // with a debug log rather than silently filtering
                        // everything out (tn-twfg fallback).
                        debug!(
                            "swarm watcher: Current scope but no session-id provider; \
                             falling back to All"
                        );
                        HashSet::new()
                    }
                    _ => HashSet::new(),
                };
                let effective_scope =
                    if scope == SwarmWatchScope::Current && pane_session_ids.is_none() {
                        SwarmWatchScope::All
                    } else {
                        scope
                    };

                // Discover new files.
                for (agent_id, jsonl_path) in scan_subagent_files() {
                    if tracked.contains_key(&agent_id) {
                        continue;
                    }

                    // Freshness gate: skip files whose mtime is older than
                    // STALENESS_TIMEOUT. This prevents a flood of pane
                    // creation on startup when old subagent JSONLs from
                    // previous sessions litter the projects directory.
                    let mtime = std::fs::metadata(&jsonl_path)
                        .and_then(|m| m.modified())
                        .ok();
                    let is_stale = mtime
                        .and_then(|m| m.elapsed().ok())
                        .map(|age| age >= STALENESS_TIMEOUT)
                        .unwrap_or(true);
                    if is_stale {
                        debug!(
                            agent = %agent_id,
                            jsonl = %jsonl_path.display(),
                            "swarm watcher: skipping stale subagent file"
                        );
                        continue;
                    }

                    if effective_scope == SwarmWatchScope::Current {
                        if owned.is_empty() {
                            // Session IDs not yet available — fall back to
                            // scope All so we don't silently drop everything.
                            debug!(
                                agent = %agent_id,
                                jsonl = %jsonl_path.display(),
                                "swarm watcher: no session IDs available yet, \
                                 admitting subagent (fallback to All)"
                            );
                        } else if !subagent_in_scope(effective_scope, &owned, &jsonl_path) {
                            debug!(
                                agent = %agent_id,
                                jsonl = %jsonl_path.display(),
                                "swarm watcher: skipping out-of-scope subagent"
                            );
                            continue;
                        }
                    }
                    info!(
                        agent = %agent_id,
                        jsonl = %jsonl_path.display(),
                        "swarm watcher: new subagent detected"
                    );
                    let event = SwarmWatcherEvent::SpawnSubagent {
                        agent_id: agent_id.clone(),
                        jsonl_path: Some(jsonl_path.clone()),
                    };
                    if tx.send(event).is_err() {
                        debug!("swarm watcher: receiver dropped, exiting");
                        return;
                    }
                    tracked.insert(
                        agent_id,
                        Tracked {
                            jsonl_path,
                            last_mtime: None,
                            unchanged_since: None,
                            reclaimed: false,
                        },
                    );
                }

                // Check staleness for tracked files.
                let now = Instant::now();
                let mut to_remove = Vec::new();
                for (agent_id, t) in tracked.iter_mut() {
                    if t.reclaimed {
                        to_remove.push(agent_id.clone());
                        continue;
                    }
                    let mtime = std::fs::metadata(&t.jsonl_path)
                        .and_then(|m| m.modified())
                        .ok();
                    let changed = mtime != t.last_mtime;
                    t.last_mtime = mtime;
                    if changed {
                        t.unchanged_since = None;
                        continue;
                    }
                    let since = *t.unchanged_since.get_or_insert(now);
                    if now.duration_since(since) >= STALENESS_TIMEOUT {
                        info!(
                            agent = %agent_id,
                            "swarm watcher: subagent stale, reclaiming pane"
                        );
                        let event = SwarmWatcherEvent::ReclaimSubagent {
                            agent_id: agent_id.clone(),
                        };
                        if tx.send(event).is_err() {
                            return;
                        }
                        t.reclaimed = true;
                    }
                }
                for id in to_remove {
                    tracked.remove(&id);
                }

                thread::sleep(POLL_INTERVAL);
            }
        })
        .unwrap_or_else(|e| {
            warn!(error = %e, "failed to spawn swarm watcher thread");
            panic!("swarm watcher thread spawn failed: {e}");
        });

    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_returns_empty_for_missing_dir() {
        // We can't easily override $HOME mid-test, but we can at least
        // exercise scan_subagent_files() and ensure it doesn't panic.
        let _ = scan_subagent_files();
    }

    #[test]
    fn parent_sid_extracted_from_subagent_path() {
        let p = PathBuf::from(
            "/home/u/.claude/projects/abc-hash/parent-sid-123/subagents/agent-xyz.jsonl",
        );
        assert_eq!(parent_sid_from_path(&p).as_deref(), Some("parent-sid-123"));
    }

    #[test]
    fn parent_sid_none_for_short_path() {
        let p = PathBuf::from("/foo.jsonl");
        assert_eq!(parent_sid_from_path(&p), None);
    }

    #[test]
    fn scope_all_admits_everything_regardless_of_owned() {
        let owned: HashSet<String> = HashSet::new();
        let p = PathBuf::from("/home/u/.claude/projects/h/some-sid/subagents/agent-1.jsonl");
        assert!(subagent_in_scope(SwarmWatchScope::All, &owned, &p));
    }

    #[test]
    fn scope_current_filters_by_owned_set() {
        let mut owned: HashSet<String> = HashSet::new();
        owned.insert("owned-sid".to_string());

        let owned_path =
            PathBuf::from("/home/u/.claude/projects/h/owned-sid/subagents/agent-1.jsonl");
        let foreign_path =
            PathBuf::from("/home/u/.claude/projects/h/foreign-sid/subagents/agent-2.jsonl");

        assert!(subagent_in_scope(
            SwarmWatchScope::Current,
            &owned,
            &owned_path
        ));
        assert!(!subagent_in_scope(
            SwarmWatchScope::Current,
            &owned,
            &foreign_path
        ));
    }

    #[test]
    fn scope_current_rejects_unparseable_path() {
        let owned: HashSet<String> = HashSet::new();
        let p = PathBuf::from("/foo.jsonl");
        assert!(!subagent_in_scope(SwarmWatchScope::Current, &owned, &p));
    }

    #[test]
    fn scope_current_with_empty_owned_rejects_all() {
        // When no session IDs are known, Current scope rejects subagents
        // (the watcher loop has a separate fallback to All for this case).
        let owned: HashSet<String> = HashSet::new();
        let p = PathBuf::from("/home/u/.claude/projects/h/some-sid/subagents/agent-1.jsonl");
        assert!(!subagent_in_scope(SwarmWatchScope::Current, &owned, &p));
    }
}
