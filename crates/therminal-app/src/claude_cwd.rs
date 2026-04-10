//! App-side Claude session cwd tracker (tn-ykxb).
//!
//! Spawns a background thread running [`ClaudeStatePoller`] so the
//! renderer can resolve `Update(foo.rs)` / `Read(bar.rs)` tool-call
//! markers against the *agent's* working directory, even when the
//! agent has hopped between git worktrees without touching the shell's
//! OSC 7 cwd.
//!
//! This lives in `therminal-app` (not `therminal-terminal` or
//! `therminal-core`) because of the scope boundary: core crates must
//! not depend on `therminal-harness-claude`. The daemon already runs
//! its own poller for the capacity cache; we read the same state
//! directory from a second poller here — duplicating an inotify watch
//! is cheap and keeps the GUI independent of a live daemon connection.
//!
//! Two maps live behind the tracker's lock:
//!
//! * `session -> agent cwd` — queried by the hotspot detector.
//! * `pid -> session` — used to go from an [`AgentEntry`](therminal_terminal::agent_registry::AgentEntry)'s
//!   pid to a Claude session id.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tracing::{debug, warn};

use therminal_harness_claude::state::{ClaudeSessionState, ClaudeStatePoller, ClaudeStatus};

/// Shared Claude metadata used by chrome surfaces for one pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeChromeMeta {
    pub session_title: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Current Claude session status (idle, processing, streaming, etc.).
    pub status: ClaudeStatus,
    /// Name of the currently executing tool, if status is `ToolUse`.
    pub current_tool: Option<String>,
    /// Number of active subagents spawned by this session.
    pub subagent_count: u32,
}

impl ClaudeChromeMeta {
    /// Shared header composition rule:
    /// 1. session title
    /// 2. working-dir basename
    pub(crate) fn header_title(&self) -> Option<String> {
        if let Some(ref title) = self.session_title {
            return Some(title.clone());
        }
        self.cwd
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    }

    /// Human-readable status label for pane header and status bar badges.
    pub(crate) fn status_label(&self) -> &'static str {
        match self.status {
            ClaudeStatus::Idle => "idle",
            ClaudeStatus::Processing => "processing",
            ClaudeStatus::Streaming => "streaming",
            ClaudeStatus::Thinking => "thinking",
            ClaudeStatus::ToolUse => "tool use",
            ClaudeStatus::AwaitingInput => "waiting",
        }
    }

    /// Compose the agent state badge text for the pane header.
    pub(crate) fn header_badge(&self) -> String {
        let mut badge = format!("claude \u{00b7} {}", self.status_label());
        if self.subagent_count > 0 {
            let noun = if self.subagent_count == 1 {
                "subagent"
            } else {
                "subagents"
            };
            badge.push_str(&format!(" \u{00b7} {} {noun}", self.subagent_count));
        }
        badge
    }

    /// Compose the enriched status bar text.
    pub(crate) fn status_bar_text(&self) -> String {
        match &self.session_title {
            Some(title) => format!("claude \u{00b7} {title} \u{00b7} {}", self.status_label()),
            None => format!("claude \u{00b7} {}", self.status_label()),
        }
    }
}

/// Per-session metadata cached from the Claude state poller.
#[derive(Debug, Clone, Default)]
struct SessionMeta {
    /// Agent working directory (from `working_dir`).
    cwd: Option<PathBuf>,
    /// User-authored session title (from `session_title`).
    session_title: Option<String>,
    /// Current status (idle, processing, streaming, etc.).
    status: ClaudeStatus,
    /// Currently executing tool name.
    current_tool: Option<String>,
    /// Number of active subagents.
    subagent_count: u32,
}

/// Thread-safe bundle of the two lookups the renderer needs.
#[derive(Debug, Default)]
pub struct ClaudeCwdTracker {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Claude session id -> agent metadata (cwd + session_title).
    by_session: HashMap<String, SessionMeta>,
    /// OS process id -> Claude session id (for the `AgentEntry.pid` path).
    pid_to_session: HashMap<u32, String>,
}

impl ClaudeCwdTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Spawn the background poller thread. Returns an `Arc` to the
    /// tracker that the renderer holds. If inotify initialisation fails
    /// (stripped-down container, no `/tmp/claude-code-state`, etc.) the
    /// tracker is still returned; it simply stays empty and every
    /// lookup falls through to `None`.
    pub fn spawn() -> Arc<Self> {
        let tracker = Self::new();
        let tracker_bg = Arc::clone(&tracker);
        thread::Builder::new()
            .name("claude-cwd-tracker".into())
            .spawn(move || {
                let mut poller = match ClaudeStatePoller::new() {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(error = %e, "claude cwd tracker disabled (watcher init failed)");
                        return;
                    }
                };
                // Seed from the initial snapshot before any poll tick.
                tracker_bg.replace_from(&poller.get_all());
                loop {
                    thread::sleep(Duration::from_millis(500));
                    let snapshot = poller.poll();
                    tracker_bg.replace_from(&snapshot);
                }
            })
            .expect("failed to spawn claude-cwd-tracker thread");
        tracker
    }

    /// Replace the contents of both maps from a full session snapshot.
    /// Rebuilding from scratch keeps the tracker consistent with the
    /// poller's own dedupe / dead-session eviction, at the cost of
    /// `O(n)` per tick where `n` is the number of live Claude sessions
    /// (typically << 100).
    fn replace_from(&self, states: &[ClaudeSessionState]) {
        let mut by_session: HashMap<String, SessionMeta> = HashMap::new();
        let mut pid_to_session: HashMap<u32, String> = HashMap::new();
        for s in states {
            if s.session_id.is_empty() {
                continue;
            }
            let meta = SessionMeta {
                cwd: s.working_dir.as_deref().map(PathBuf::from),
                session_title: s.session_title.clone(),
                status: s.status,
                current_tool: s.current_tool.clone(),
                subagent_count: s.subagent_count.unwrap_or(0),
            };
            // Always insert when we have valid session data so chrome
            // surfaces can show status even before cwd/title are known.
            by_session.insert(s.session_id.clone(), meta);
            if let Some(pid) = s.pid
                && pid > 0
                && let Ok(pid_u32) = u32::try_from(pid)
            {
                pid_to_session.insert(pid_u32, s.session_id.clone());
            }
        }
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        debug!(
            sessions = by_session.len(),
            pids = pid_to_session.len(),
            "claude cwd tracker refreshed"
        );
        g.by_session = by_session;
        g.pid_to_session = pid_to_session;
    }

    /// Shared chrome metadata lookup for a Claude pid.
    pub fn chrome_meta_for_pid(&self, pid: u32) -> Option<ClaudeChromeMeta> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let sid = g.pid_to_session.get(&pid)?;
        let meta = g.by_session.get(sid)?;
        Some(ClaudeChromeMeta {
            session_title: meta.session_title.clone(),
            cwd: meta.cwd.clone(),
            status: meta.status,
            current_tool: meta.current_tool.clone(),
            subagent_count: meta.subagent_count,
        })
    }

    /// Test-only pid -> cwd lookup retained for the existing tracker
    /// regression tests. Production chrome should use `chrome_meta_for_pid`
    /// so all surfaces share the same source object.
    #[cfg(test)]
    pub fn cwd_for_pid(&self, pid: u32) -> Option<PathBuf> {
        self.chrome_meta_for_pid(pid).and_then(|meta| meta.cwd)
    }

    /// Direct session -> cwd lookup. Exposed for tests.
    #[cfg(test)]
    pub fn cwd_for_session(&self, session_id: &str) -> Option<PathBuf> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.by_session.get(session_id).and_then(|m| m.cwd.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use therminal_harness_claude::state::{ClaudeSessionState, ClaudeStatus};

    fn mk_state(sid: &str, pid: Option<i64>, wd: Option<&str>) -> ClaudeSessionState {
        ClaudeSessionState {
            session_id: sid.to_string(),
            status: ClaudeStatus::Idle,
            working_dir: wd.map(|s| s.to_string()),
            pid,
            ..Default::default()
        }
    }

    #[test]
    fn replace_builds_pid_and_session_maps() {
        let t = ClaudeCwdTracker::new();
        t.replace_from(&[
            mk_state("sid-a", Some(4242), Some("/home/u/repo-a")),
            mk_state("sid-b", Some(4243), Some("/home/u/repo-b")),
        ]);
        assert_eq!(t.cwd_for_pid(4242), Some(PathBuf::from("/home/u/repo-a")),);
        assert_eq!(t.cwd_for_pid(4243), Some(PathBuf::from("/home/u/repo-b")),);
        assert_eq!(
            t.cwd_for_session("sid-a"),
            Some(PathBuf::from("/home/u/repo-a")),
        );
        assert_eq!(t.cwd_for_pid(9999), None);
    }

    #[test]
    fn replace_evicts_stale_entries() {
        let t = ClaudeCwdTracker::new();
        t.replace_from(&[mk_state("sid-a", Some(4242), Some("/home/u/repo-a"))]);
        assert!(t.cwd_for_pid(4242).is_some());
        // New snapshot without sid-a — full replace must drop it.
        t.replace_from(&[mk_state("sid-b", Some(4243), Some("/home/u/repo-b"))]);
        assert_eq!(t.cwd_for_pid(4242), None);
        assert!(t.cwd_for_pid(4243).is_some());
    }

    #[test]
    fn worktree_hop_updates_cwd() {
        let t = ClaudeCwdTracker::new();
        t.replace_from(&[mk_state("sid-a", Some(4242), Some("/home/u/wt-a"))]);
        assert_eq!(t.cwd_for_pid(4242), Some(PathBuf::from("/home/u/wt-a")));
        t.replace_from(&[mk_state("sid-a", Some(4242), Some("/home/u/wt-b"))]);
        assert_eq!(t.cwd_for_pid(4242), Some(PathBuf::from("/home/u/wt-b")));
    }

    #[test]
    fn missing_working_dir_returns_none_cwd() {
        let t = ClaudeCwdTracker::new();
        t.replace_from(&[mk_state("sid-a", Some(4242), None)]);
        assert_eq!(t.cwd_for_pid(4242), None);
    }

    #[test]
    fn empty_session_id_is_skipped() {
        let t = ClaudeCwdTracker::new();
        t.replace_from(&[mk_state("", Some(4242), Some("/w"))]);
        assert_eq!(t.cwd_for_pid(4242), None);
    }

    fn mk_state_with_title(
        sid: &str,
        pid: Option<i64>,
        wd: Option<&str>,
        title: Option<&str>,
    ) -> ClaudeSessionState {
        ClaudeSessionState {
            session_id: sid.to_string(),
            status: ClaudeStatus::Idle,
            working_dir: wd.map(|s| s.to_string()),
            session_title: title.map(|s| s.to_string()),
            pid,
            ..Default::default()
        }
    }

    #[test]
    fn header_title_prefers_session_title() {
        let t = ClaudeCwdTracker::new();
        t.replace_from(&[mk_state_with_title(
            "sid-a",
            Some(4242),
            Some("/home/u/repo"),
            Some("fix login bug"),
        )]);
        assert_eq!(
            t.chrome_meta_for_pid(4242)
                .and_then(|meta| meta.header_title()),
            Some("fix login bug".to_string()),
        );
    }

    #[test]
    fn header_title_falls_back_to_working_dir_basename() {
        let t = ClaudeCwdTracker::new();
        t.replace_from(&[mk_state_with_title(
            "sid-a",
            Some(4242),
            Some("/home/u/my-project"),
            None,
        )]);
        assert_eq!(
            t.chrome_meta_for_pid(4242)
                .and_then(|meta| meta.header_title()),
            Some("my-project".to_string()),
        );
    }

    #[test]
    fn header_title_none_when_no_data() {
        let t = ClaudeCwdTracker::new();
        t.replace_from(&[mk_state_with_title("sid-a", Some(4242), None, None)]);
        assert_eq!(
            t.chrome_meta_for_pid(4242)
                .and_then(|meta| meta.header_title()),
            None
        );
    }

    #[test]
    fn chrome_meta_round_trips_title_and_cwd() {
        let t = ClaudeCwdTracker::new();
        t.replace_from(&[mk_state_with_title(
            "sid-a",
            Some(4242),
            Some("/home/u/repo"),
            Some("fix login bug"),
        )]);
        let meta = t.chrome_meta_for_pid(4242).expect("expected metadata");
        assert_eq!(meta.session_title.as_deref(), Some("fix login bug"));
        assert_eq!(meta.cwd, Some(PathBuf::from("/home/u/repo")));
        assert_eq!(meta.header_title().as_deref(), Some("fix login bug"));
    }

    #[test]
    fn header_badge_shows_status() {
        let meta = ClaudeChromeMeta {
            session_title: None,
            cwd: None,
            status: ClaudeStatus::Streaming,
            current_tool: None,
            subagent_count: 0,
        };
        assert!(meta.header_badge().contains("streaming"));
    }

    #[test]
    fn header_badge_includes_subagent_count() {
        let meta = ClaudeChromeMeta {
            session_title: None,
            cwd: None,
            status: ClaudeStatus::Thinking,
            current_tool: None,
            subagent_count: 3,
        };
        assert!(meta.header_badge().contains("3 subagents"));
    }

    #[test]
    fn status_bar_text_with_title() {
        let meta = ClaudeChromeMeta {
            session_title: Some("fix login bug".into()),
            cwd: None,
            status: ClaudeStatus::Streaming,
            current_tool: None,
            subagent_count: 0,
        };
        let text = meta.status_bar_text();
        assert!(text.contains("fix login bug") && text.contains("streaming"));
    }

    #[test]
    fn chrome_meta_carries_status_and_subagent_count() {
        let t = ClaudeCwdTracker::new();
        let mut state = mk_state("sid-a", Some(4242), Some("/home/u/repo"));
        state.status = ClaudeStatus::Streaming;
        state.subagent_count = Some(2);
        t.replace_from(&[state]);
        let meta = t.chrome_meta_for_pid(4242).expect("expected metadata");
        assert_eq!(meta.status, ClaudeStatus::Streaming);
        assert_eq!(meta.subagent_count, 2);
    }
}
