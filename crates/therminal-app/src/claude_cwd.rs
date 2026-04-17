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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tracing::{debug, warn};

use therminal_harness_claude::state::{ClaudeSessionState, ClaudeStatePoller, ClaudeStatus};
use therminal_terminal::hotspot_detection::resolve_relative_to_cwd;

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
    /// lookup falls through to `None`. Likewise, if the OS refuses to
    /// create the background thread (resource exhaustion, ulimit hit),
    /// we log a warning and return the empty tracker — the GUI must
    /// not panic just because best-effort cwd tracking is unavailable.
    pub fn spawn() -> Arc<Self> {
        let tracker = Self::new();
        let tracker_bg = Arc::clone(&tracker);
        let spawn_result = thread::Builder::new()
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
            });
        if let Err(e) = spawn_result {
            warn!(
                error = %e,
                "claude cwd tracker disabled (failed to spawn background thread)"
            );
        }
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

    /// Shared chrome metadata lookup by session UUID (tn-sl9k).
    ///
    /// On Windows+WSL the agent PID in `DaemonEvent::AgentChanged` is a
    /// Windows PID while the state files contain Linux PIDs, so the
    /// PID-based `chrome_meta_for_pid` lookup never matches. This method
    /// provides a direct session_id-based fallback: the daemon populates
    /// `session_id` on `AgentChanged` from its `PaneCapacityCache`, the
    /// forwarder stores it in `PaneStatus.claude_session_id`, and the
    /// render path calls this method when the PID path returns `None`.
    pub fn chrome_meta_for_session(&self, session_id: &str) -> Option<ClaudeChromeMeta> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let meta = g.by_session.get(session_id)?;
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

    /// Public session -> cwd lookup used by the hotspot resolvers in the
    /// `open_in_editor` / `open_folder_in_pane` fallback path (tn-shbw).
    pub fn cwd_for_session_public(&self, session_id: &str) -> Option<PathBuf> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.by_session.get(session_id).and_then(|m| m.cwd.clone())
    }
}

/// Resolve a (tilde-expanded) hotspot path with a harness-cwd fallback (tn-shbw).
///
/// Precedence, strictest → loosest:
///
/// 1. If the input path is already absolute, [`resolve_relative_to_cwd`] returns
///    it unchanged — we just stat it and return it either way.
/// 2. Otherwise, join against `shell_cwd` (the pane's OSC 7 cwd). If the result
///    stats as the expected type (`stat_ok` returns `true`), use it.
/// 3. Otherwise, if `harness_cwd` is set, re-join against the Claude session's
///    `working_dir`. If the result stats, use it.
/// 4. Fall through to the shell-cwd attempt so the caller's "file not found"
///    error still blames the more likely source (the visible shell).
///
/// The tn-gidy tool-call-marker `resolved_text` override is enforced *outside*
/// this function: `handle_hotspot_click` in `mouse.rs` substitutes the
/// resolved absolute path into the click action before `open_in_editor` is
/// ever called, so by the time we reach here the path is already absolute
/// and step 1 short-circuits. The harness fallback only fires for bare
/// `FilePath` hotspots in Claude-linked panes — exactly the gap tn-shbw
/// targets.
///
/// `stat_ok` is passed a concrete `&str` so tests can stub the filesystem.
/// Production callers pass either `|p| std::fs::metadata(p).map(|m|
/// m.is_file()).unwrap_or(false)` (editor path) or the `is_dir` equivalent
/// (folder path).
pub(crate) fn resolve_with_harness_fallback<F>(
    expanded: &str,
    shell_cwd: Option<&str>,
    harness_cwd: Option<&Path>,
    stat_ok: F,
) -> String
where
    F: Fn(&str) -> bool,
{
    let shell_resolved = resolve_relative_to_cwd(expanded, shell_cwd).into_owned();
    if stat_ok(&shell_resolved) {
        return shell_resolved;
    }
    if let Some(h) = harness_cwd {
        let harness_cwd_s = h.to_string_lossy();
        let harness_resolved = resolve_relative_to_cwd(expanded, Some(harness_cwd_s.as_ref()));
        if stat_ok(&harness_resolved) {
            return harness_resolved.into_owned();
        }
    }
    shell_resolved
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

    // ── tn-sl9k: session-based fallback tests ────────────────────────

    #[test]
    fn chrome_meta_for_session_returns_metadata() {
        let t = ClaudeCwdTracker::new();
        t.replace_from(&[mk_state_with_title(
            "sid-a",
            Some(4242),
            Some("/home/u/repo"),
            Some("fix login bug"),
        )]);
        // Session-based lookup bypasses the PID entirely.
        let meta = t
            .chrome_meta_for_session("sid-a")
            .expect("expected metadata via session_id");
        assert_eq!(meta.session_title.as_deref(), Some("fix login bug"));
        assert_eq!(meta.cwd, Some(PathBuf::from("/home/u/repo")));
    }

    #[test]
    fn chrome_meta_for_session_returns_none_for_unknown_session() {
        let t = ClaudeCwdTracker::new();
        t.replace_from(&[mk_state("sid-a", Some(4242), Some("/home/u/repo"))]);
        assert!(t.chrome_meta_for_session("sid-nonexistent").is_none());
    }

    #[test]
    fn session_fallback_works_when_pid_mismatches() {
        // Simulates Windows+WSL: state file has Linux PID 1234, but the
        // AgentRegistry has Windows PID 56789. PID-based lookup fails,
        // session-based lookup succeeds.
        let t = ClaudeCwdTracker::new();
        t.replace_from(&[mk_state_with_title(
            "sid-wsl",
            Some(1234), // Linux PID in state file
            Some("/home/u/project"),
            Some("WSL session"),
        )]);
        // Windows PID doesn't match any state file entry.
        assert!(t.chrome_meta_for_pid(56789).is_none());
        // But session-based lookup works.
        let meta = t
            .chrome_meta_for_session("sid-wsl")
            .expect("session-based lookup should succeed");
        assert_eq!(meta.session_title.as_deref(), Some("WSL session"));
    }

    // ── tn-shbw: harness-cwd fallback for generic FilePath hotspots ──
    //
    // These tests exercise `resolve_with_harness_fallback` — the pure
    // resolver called by `open_in_editor` / `open_folder_in_pane`. The
    // scenarios mirror the task acceptance criteria:
    //   (a) harness cwd beats shell cwd when file exists only in harness cwd
    //   (b) shell cwd still wins when the pane has no harness link
    //   (c) tool-call marker resolved_text still takes precedence (via the
    //       click layer substituting an absolute path before we're called,
    //       which short-circuits through branch 1 below)
    //   (d) JsonlTail pane (no OSC 7) resolves purely via harness cwd

    use super::resolve_with_harness_fallback;
    use std::collections::HashSet;

    fn mk_stat_fn(existing: &[&str]) -> impl Fn(&str) -> bool {
        let set: HashSet<String> = existing.iter().map(|s| s.to_string()).collect();
        move |p| set.contains(p)
    }

    #[test]
    fn fallback_a_harness_cwd_beats_shell_cwd_when_only_harness_has_file() {
        // File is 2026-04-17.md; shell cwd /home/u/beads doesn't contain
        // it, but harness cwd /home/u/notes does. Fallback must pick harness.
        let stat_ok = mk_stat_fn(&["/home/u/notes/2026-04-17.md"]);
        let harness = PathBuf::from("/home/u/notes");
        let out = resolve_with_harness_fallback(
            "2026-04-17.md",
            Some("/home/u/beads"),
            Some(harness.as_path()),
            stat_ok,
        );
        assert_eq!(out, "/home/u/notes/2026-04-17.md");
    }

    #[test]
    fn fallback_b_shell_cwd_still_wins_without_harness_link() {
        // No harness cwd is provided — must honor shell_cwd and return
        // exactly what resolve_relative_to_cwd produces, even if stat_ok
        // says the file doesn't exist (caller's plan_open_in_editor will
        // toast "file not found", which is the pre-shbw behavior).
        let stat_ok = mk_stat_fn(&[]); // nothing exists
        let out = resolve_with_harness_fallback("foo.md", Some("/home/u/beads"), None, stat_ok);
        assert_eq!(out, "/home/u/beads/foo.md");
    }

    #[test]
    fn fallback_b_shell_cwd_wins_when_file_present_in_shell_cwd() {
        // File is visible in both shell and harness cwds; shell wins
        // because it's tried first (the pane's OSC 7 is the more
        // specific signal for "where the user currently is").
        let stat_ok = mk_stat_fn(&["/home/u/beads/foo.md", "/home/u/notes/foo.md"]);
        let harness = PathBuf::from("/home/u/notes");
        let out = resolve_with_harness_fallback(
            "foo.md",
            Some("/home/u/beads"),
            Some(harness.as_path()),
            stat_ok,
        );
        assert_eq!(out, "/home/u/beads/foo.md");
    }

    #[test]
    fn fallback_c_absolute_resolved_path_short_circuits_both_cwds() {
        // Simulates the tn-gidy path: the click layer already substituted
        // the resolved absolute path, so by the time we reach the resolver
        // the input is absolute. resolve_relative_to_cwd leaves it alone,
        // stat confirms it, and neither shell_cwd nor harness_cwd is
        // consulted. The returned path must be the original absolute path
        // verbatim — **no** accidental re-rooting under a different cwd.
        let stat_ok = mk_stat_fn(&["/abs/path/to/file.rs"]);
        let harness = PathBuf::from("/harness/cwd");
        let out = resolve_with_harness_fallback(
            "/abs/path/to/file.rs",
            Some("/shell/cwd"),
            Some(harness.as_path()),
            stat_ok,
        );
        assert_eq!(out, "/abs/path/to/file.rs");
    }

    #[test]
    fn fallback_d_jsonl_tail_pane_resolves_via_harness_cwd() {
        // JsonlTail panes have no OSC 7 cwd — shell_cwd is None. The
        // harness-cwd fallback must still reach the file; otherwise the
        // hotspot is dead for every JSONL-observed session.
        let stat_ok = mk_stat_fn(&["/home/u/project/src/lib.rs"]);
        let harness = PathBuf::from("/home/u/project");
        let out = resolve_with_harness_fallback(
            "src/lib.rs",
            None, // no OSC 7 on a JsonlTail pane
            Some(harness.as_path()),
            stat_ok,
        );
        assert_eq!(out, "/home/u/project/src/lib.rs");
    }

    #[test]
    fn fallback_preserves_shell_resolution_when_neither_cwd_contains_file() {
        // If neither cwd locates the file, return the shell-cwd attempt so
        // the caller's "file not found: <shell path>" message still blames
        // the more likely source (the visible shell, not the hidden harness).
        let stat_ok = mk_stat_fn(&[]);
        let harness = PathBuf::from("/home/u/notes");
        let out = resolve_with_harness_fallback(
            "missing.md",
            Some("/home/u/beads"),
            Some(harness.as_path()),
            stat_ok,
        );
        assert_eq!(out, "/home/u/beads/missing.md");
    }

    #[test]
    fn fallback_directory_variant_with_is_dir_stat() {
        // Same function, caller passes an is_dir stat. Used by
        // open_folder_in_pane / open_folder_in_file_manager.
        let stat_ok = mk_stat_fn(&["/home/u/project/src"]);
        let harness = PathBuf::from("/home/u/project");
        let out = resolve_with_harness_fallback(
            "src",
            Some("/home/u/beads"),
            Some(harness.as_path()),
            stat_ok,
        );
        assert_eq!(out, "/home/u/project/src");
    }
}
