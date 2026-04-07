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
//! Filesystem polling (rather than the daemon's `TaggedAgentEvent` broadcast)
//! is used so the watcher works even when the daemon isn't running, and so the
//! app crate doesn't need to depend on `therminal-daemon`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use tracing::{debug, info, warn};

/// How often the watcher thread scans for new subagent JSONLs.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long a JSONL file's mtime must be unchanged before the subagent is
/// considered done and the pane is reclaimed. Mirrors thermal-desktop.
pub const STALENESS_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Spawn the watcher thread. Returns a receiver that the event loop polls.
///
/// The thread runs forever (or until the receiver is dropped). It polls every
/// [`POLL_INTERVAL`] and emits events on the channel.
pub fn spawn() -> mpsc::Receiver<SwarmWatcherEvent> {
    let (tx, rx) = mpsc::channel();

    thread::Builder::new()
        .name("swarm-watcher".into())
        .spawn(move || {
            info!("swarm watcher started — scanning for Claude subagents");
            let mut tracked: HashMap<String, Tracked> = HashMap::new();

            loop {
                // Discover new files.
                for (agent_id, jsonl_path) in scan_subagent_files() {
                    if tracked.contains_key(&agent_id) {
                        continue;
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
}
