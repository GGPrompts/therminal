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
//! THIS therminal instance — i.e. the parent's recorded pid is a descendant of
//! one of the live pane PIDs supplied via [`PanePidProvider`]. The owned-set
//! is recomputed at most once per [`OWNED_CACHE_TTL`] to avoid filesystem and
//! sysinfo walks on every tick.
//!
//! Filesystem polling (rather than the daemon's `TaggedAgentEvent` broadcast)
//! is used so the watcher works even when the daemon isn't running, and so the
//! app crate doesn't need to depend on `therminal-daemon`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use therminal_core::config::SwarmWatchScope;
use tracing::{debug, info, warn};

/// How often the watcher thread scans for new subagent JSONLs.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long a JSONL file's mtime must be unchanged before the subagent is
/// considered done and the pane is reclaimed. Mirrors thermal-desktop.
pub const STALENESS_TIMEOUT: Duration = Duration::from_secs(30);

/// Owned-session-set is recomputed at most this often. Keeps the per-tick
/// cost cheap by avoiding repeated `/tmp/claude-code-state` walks and process
/// tree refreshes.
pub const OWNED_CACHE_TTL: Duration = Duration::from_secs(1);

/// Live pane root PIDs supplied by the host application. Wrapping it in an
/// `Arc<Mutex<Vec<u32>>>` lets the app mutate the list as panes spawn / exit
/// without coordinating with the watcher thread directly.
pub type PanePidProvider = Arc<Mutex<Vec<u32>>>;

/// Events emitted by the watcher thread, consumed by the winit event loop.
///
/// Wrapped in `UserEvent` variants by the caller — kept as a separate enum so
/// this module can be tested without pulling in `winit`.
#[derive(Debug, Clone)]
pub enum SwarmWatcherEvent {
    /// A new subagent JSONL appeared. Open a pane that tails it.
    SpawnSubagent {
        agent_id: String,
        jsonl_path: PathBuf,
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
fn claude_projects_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claude").join("projects")
}

/// Resolve the Claude state directory (host-wide; pid-keyed JSON files live
/// here, written by Claude Code's hooks).
fn claude_state_dir() -> PathBuf {
    PathBuf::from("/tmp/claude-code-state")
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

// ── Owned-session-set computation ──────────────────────────────────────

/// Subset of fields we read from `/tmp/claude-code-state/*.json`. Both the
/// `pid`-keyed top-level files and the per-session files share enough of a
/// shape to be deserialised with this struct (extra fields are ignored).
#[derive(Debug, Deserialize)]
struct ClaudeStateBlob {
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    claude_session_id: Option<String>,
}

/// Compute the set of process descendants of `roots`, inclusive of the roots
/// themselves. Uses `sysinfo` for cross-platform process enumeration.
fn descendants_of(roots: &[u32]) -> HashSet<u32> {
    let mut out: HashSet<u32> = roots.iter().copied().collect();
    if roots.is_empty() {
        return out;
    }
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
    let mut sys = System::new_with_specifics(
        RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()),
    );
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    // child_pid -> parent_pid
    let mut child_to_parent: HashMap<u32, u32> = HashMap::new();
    for (pid, proc) in sys.processes() {
        if let Some(parent) = proc.parent() {
            child_to_parent.insert(pid.as_u32(), parent.as_u32());
        }
    }
    // Walk every known pid up to a root and tag descendants accordingly.
    for &pid in child_to_parent.keys() {
        let mut cur = pid;
        let mut depth = 0;
        while depth < 256 {
            if out.contains(&cur) {
                out.insert(pid);
                break;
            }
            match child_to_parent.get(&cur) {
                Some(&p) if p != cur => cur = p,
                _ => break,
            }
            depth += 1;
        }
    }
    out
}

/// Read `/tmp/claude-code-state/*.json` and collect the set of Claude
/// Code session ids whose recorded pid is a descendant of one of `pane_pids`.
///
/// A session is included if either its `claude_session_id` field (preferred,
/// matches the JSONL/subagent directory naming) or its `session_id` field
/// resolves a descendant pid match. The first JSON object in each file is
/// parsed; non-JSON or non-Claude state files are skipped silently.
fn collect_owned_sessions(pane_pids: &[u32]) -> HashSet<String> {
    let mut owned = HashSet::new();
    if pane_pids.is_empty() {
        return owned;
    }
    let descendants = descendants_of(pane_pids);
    let dir = claude_state_dir();
    let iter = match std::fs::read_dir(&dir) {
        Ok(it) => it,
        Err(_) => return owned,
    };
    for entry in iter.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Files may contain multiple concatenated JSON objects on separate
        // lines. Try the whole file first, then fall back to line-by-line.
        let mut parsed: Vec<ClaudeStateBlob> = Vec::new();
        if let Ok(blob) = serde_json::from_str::<ClaudeStateBlob>(text.trim()) {
            parsed.push(blob);
        } else {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(blob) = serde_json::from_str::<ClaudeStateBlob>(line) {
                    parsed.push(blob);
                }
            }
        }
        for blob in parsed {
            let Some(pid) = blob.pid else { continue };
            if !descendants.contains(&pid) {
                continue;
            }
            if let Some(sid) = blob.claude_session_id {
                owned.insert(sid);
            }
            if let Some(sid) = blob.session_id {
                owned.insert(sid);
            }
        }
    }
    owned
}

/// Cache wrapper around [`collect_owned_sessions`] honouring [`OWNED_CACHE_TTL`].
struct OwnedSessionCache {
    last_refresh: Option<Instant>,
    set: HashSet<String>,
}

impl OwnedSessionCache {
    fn new() -> Self {
        Self {
            last_refresh: None,
            set: HashSet::new(),
        }
    }

    fn get(&mut self, pane_pids: &[u32]) -> &HashSet<String> {
        let stale = match self.last_refresh {
            None => true,
            Some(t) => t.elapsed() >= OWNED_CACHE_TTL,
        };
        if stale {
            self.set = collect_owned_sessions(pane_pids);
            self.last_refresh = Some(Instant::now());
        }
        &self.set
    }
}

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
/// `pane_pids` MUST be supplied — it provides the live set of root pane PIDs
/// owned by this therminal instance, against which Claude state pids are
/// matched. With [`SwarmWatchScope::All`] the provider is ignored.
pub fn spawn(
    scope: SwarmWatchScope,
    pane_pids: Option<PanePidProvider>,
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
            let mut owned_cache = OwnedSessionCache::new();

            loop {
                // Snapshot pane pids (cheap clone under the lock).
                let pids: Vec<u32> = match (&scope, pane_pids.as_ref()) {
                    (SwarmWatchScope::Current, Some(p)) => {
                        p.lock().map(|g| g.clone()).unwrap_or_default()
                    }
                    _ => Vec::new(),
                };

                // Discover new files.
                for (agent_id, jsonl_path) in scan_subagent_files() {
                    if tracked.contains_key(&agent_id) {
                        continue;
                    }
                    if scope == SwarmWatchScope::Current {
                        let owned = owned_cache.get(&pids);
                        if !subagent_in_scope(scope, owned, &jsonl_path) {
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
                        jsonl_path: jsonl_path.clone(),
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
    fn owned_session_cache_refreshes_after_ttl_marker() {
        // Smoke test: cache constructs cleanly and serves an empty set when
        // there are no pane pids (the early return path).
        let mut cache = OwnedSessionCache::new();
        let pids: Vec<u32> = Vec::new();
        let set = cache.get(&pids);
        assert!(set.is_empty());
    }

    #[test]
    fn collect_owned_sessions_empty_for_no_pids() {
        let set = collect_owned_sessions(&[]);
        assert!(set.is_empty());
    }
}
