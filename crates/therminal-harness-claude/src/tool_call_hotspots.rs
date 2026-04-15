//! Claude Code tool-call hotspot detector (tn-gidy).
//!
//! Claude Code prints a compact, one-line marker per tool invocation to its
//! TUI, e.g.
//!
//! ```text
//! Update(crates/therminal-terminal/src/foo.rs)
//! Read(Cargo.toml)
//! Edit(src/bar.rs)
//! Write(/tmp/out.txt)
//! MultiEdit(src/baz.rs)
//! ```
//!
//! The path inside the parentheses is relative to the *agent's* working
//! directory, which is not necessarily the same as the shell's OSC 7 cwd:
//! a Claude Code session that hops between git worktrees updates its own
//! internal cwd without ever touching the enclosing shell's cwd. Core's
//! generic "resolve relative path against pane cwd" hotspot path would
//! therefore mis-resolve exactly during a worktree hop. So resolution
//! happens here, in the harness crate, using the agent cwd the JSONL
//! tailer already tracks.
//!
//! ## Included tools
//!
//! | Tool        | Reason                                                  |
//! |-------------|---------------------------------------------------------|
//! | `Update`    | Emits a single file path.                               |
//! | `Read`      | Emits a single file path.                               |
//! | `Edit`      | Emits a single file path.                               |
//! | `Write`     | Emits a single file path.                               |
//! | `MultiEdit` | Emits a single file path (the one being edited).        |
//!
//! ## Excluded tools (deliberately)
//!
//! | Tool     | Reason                                                          |
//! |----------|-----------------------------------------------------------------|
//! | `Bash`   | Argument is a shell command, not a path (`Bash(cargo build)`).  |
//! | `Grep`   | Argument is a regex pattern.                                    |
//! | `Glob`   | Argument is a glob pattern, not a concrete path.                |
//! | `Task`   | Argument is a subagent description.                             |
//! | `WebFetch`/`WebSearch` | Arguments are URLs/queries.                         |
//!
//! Conservative inclusion is intentional — a false positive here would
//! route a click into the editor-fallback chain on something that isn't
//! a real file path, which is user-visibly worse than "that line didn't
//! become clickable".
//!
//! ## Manual verification checklist
//!
//! 1. Run Claude Code inside a therminal pane.
//! 2. Ask it to `Read(Cargo.toml)` — the printed line should render with a
//!    hotspot underline on the parenthesised path, hover cursor = pointer.
//! 3. Click → the configured editor opens the file. The absolute path sent
//!    to the editor must match the agent's cwd, not the shell's.
//! 4. In a worktree-hopping scenario: make the agent cd into a sibling
//!    worktree (e.g. via the `Bash` tool), then ask it to `Read(src/lib.rs)`
//!    — the click must still resolve against the *new* worktree cwd.
//! 5. A printed `Bash(echo hi)` line must NOT become a hotspot.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use regex::Regex;
use therminal_terminal::hotspot_detection::{HotspotKind, TextHotspot};

use crate::state::{ClaudeSessionState, ClaudeStateUpdate};

/// Thread-safe session_id → agent cwd index.
///
/// Populated by feeding [`ClaudeStateUpdate`]s through [`ClaudeSessionCwdIndex::apply`],
/// which mirrors the `working_dir` field from each upserted state file.
/// Queried by the renderer bridge (via [`ClaudeSessionCwdIndex::cwd_for`])
/// and by the pipeline's tool-call resolver.
///
/// The index tracks only *live* sessions: [`ClaudeStateUpdate::Removed`]
/// drops the entry. Subagents are included — they carry their own
/// `working_dir`, which may differ from the parent's during a worktree hop.
///
/// Because [`ClaudeStateUpdate::Removed`] carries only the deleted file
/// path (not the `session_id` we key on), some eviction paths cannot match
/// an entry back to its key. To bound the index in the long-running daemon,
/// callers wired into the pipeline tick should call
/// [`ClaudeSessionCwdIndex::retain_live`] every poll cycle with the set of
/// session ids the [`ClaudeStatePoller`](crate::state::ClaudeStatePoller)
/// just observed; any entry whose id is missing from that set is dropped.
/// See tn-ossc.
///
/// **Why the harness, not core**: core's generic resolver would join
/// `Update(src/foo.rs)` against the pane's OSC 7 cwd. When the agent hops
/// worktrees that cwd is wrong. The harness already tracks the *agent's*
/// cwd from the JSONL + hook state pipeline, so resolution must happen
/// here. See tn-gidy.
#[derive(Debug, Clone, Default)]
pub struct ClaudeSessionCwdIndex {
    inner: Arc<Mutex<HashMap<String, PathBuf>>>,
}

impl ClaudeSessionCwdIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a single [`ClaudeStateUpdate`] into the index.
    ///
    /// On `Upserted`, the session's `working_dir` (if any) is stored under
    /// `session_id`. Missing `working_dir` is treated as "no change" rather
    /// than "clear" — some hook writers omit the field. On `Removed`, all
    /// sessions whose state file matches the removed path are dropped; for
    /// simplicity we drop by session_id when the caller has one, otherwise
    /// the next [`Self::retain_live`] sweep cleanses stale entries.
    pub fn apply(&self, update: &ClaudeStateUpdate) {
        match update {
            ClaudeStateUpdate::Upserted(state) => self.upsert(state),
            ClaudeStateUpdate::Removed { .. } => {
                // `ClaudeStateUpdate::Removed` carries only the deleted state
                // file path — not the `session_id` we key the index by — so
                // we cannot evict the entry here without re-deriving the id
                // from the filename, which would couple the index to hook
                // path layout. The pipeline calls `forget(session_id)`
                // explicitly when it knows the id, and the per-tick
                // [`Self::retain_live`] sweep evicts anything still left
                // behind by comparing against the poller's live snapshot.
                // See tn-ossc.
            }
        }
    }

    fn upsert(&self, state: &ClaudeSessionState) {
        let Some(wd) = state.working_dir.as_deref() else {
            return;
        };
        if state.session_id.is_empty() {
            return;
        }
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.insert(state.session_id.clone(), PathBuf::from(wd));
    }

    /// Explicitly drop a session's entry (used by the pipeline on Removed).
    pub fn forget(&self, session_id: &str) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.remove(session_id);
    }

    /// Drop every entry whose `session_id` is not in `live_session_ids`.
    ///
    /// Called from the pipeline tick with the set of session ids the
    /// [`ClaudeStatePoller`](crate::state::ClaudeStatePoller) just observed.
    /// This is the long-running daemon's defence against the
    /// [`ClaudeStateUpdate::Removed`] arm being unable to evict by id (it
    /// only carries a file path) — anything still in the index but missing
    /// from the live snapshot is an orphan and gets dropped here. See tn-ossc.
    ///
    /// Returns the number of evicted entries (purely informational; callers
    /// may surface it as a debug log).
    pub fn retain_live<'a, I>(&self, live_session_ids: I) -> usize
    where
        I: IntoIterator<Item = &'a str>,
    {
        let live: HashSet<&str> = live_session_ids.into_iter().collect();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let before = g.len();
        g.retain(|sid, _| live.contains(sid.as_str()));
        before - g.len()
    }

    /// Convenience wrapper around [`Self::retain_live`] that takes a poller
    /// snapshot directly. Mirrors the shape of `ClaudeStatePoller::poll()`'s
    /// return value so the pipeline call site is a one-liner.
    pub fn retain_live_from_snapshot(&self, snapshot: &[ClaudeSessionState]) -> usize {
        self.retain_live(snapshot.iter().map(|s| s.session_id.as_str()))
    }

    /// Look up the agent cwd for a Claude session id. Returns `None` when
    /// the session is unknown or its state didn't carry a `working_dir`.
    pub fn cwd_for(&self, session_id: &str) -> Option<PathBuf> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.get(session_id).cloned()
    }

    /// Current number of indexed sessions.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Tools whose single positional argument is a file path we want to make
/// clickable. See module docstring for the inclusion rationale.
const PATH_TOOLS: &[&str] = &["Update", "Read", "Edit", "Write", "MultiEdit"];

/// Compiled matcher for `Tool(arg)` lines. We match the tool name and
/// capture the interior of the parentheses; the caller then checks the
/// tool name against `PATH_TOOLS`. The interior is `[^()\n]+` so we don't
/// grab nested-paren tool signatures (rare in practice).
static TOOL_CALL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([A-Z][A-Za-z]+)\(([^()\n]+)\)").unwrap());

/// Scan `rows` for Claude Code tool-call markers and return a `TextHotspot`
/// per matched `Tool(path)` whose tool is in [`PATH_TOOLS`]. Each returned
/// hotspot has:
///
/// - `text` = the relative path as printed (the display string).
/// - `resolved_text` = the absolute path, joined against `agent_cwd`.
/// - `start_col` / `end_col` spanning only the path (not the `Tool(` prefix
///   or closing `)`), so clicks on `Update` itself do nothing.
///
/// `agent_cwd` is the *agent's* current working directory — pass the
/// `working_dir` field from the harness's `ClaudeSessionState` for the
/// Claude session bound to this pane. If the path is already absolute, it
/// is used as-is.
pub fn detect_claude_tool_call_hotspots(rows: &[String], agent_cwd: &Path) -> Vec<TextHotspot> {
    let mut out = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        for cap in TOOL_CALL_RE.captures_iter(row) {
            let Some(name_m) = cap.get(1) else { continue };
            let Some(arg_m) = cap.get(2) else { continue };
            let tool = name_m.as_str();
            if !PATH_TOOLS.contains(&tool) {
                continue;
            }
            let display = arg_m.as_str().trim();
            if display.is_empty() {
                continue;
            }

            // Byte offsets of the path inside `row`. For the typical
            // ASCII-only Claude TUI line these equal character columns;
            // for multi-byte rows we fall back to a char count which is
            // still correct for display width of plain ASCII paths.
            let start_byte = arg_m.start();
            let end_byte = arg_m.end();
            let start_col = row[..start_byte].chars().count();
            let end_col = row[..end_byte].chars().count();

            // Reject paths containing `..` so a hostile JSONL can't push
            // the resolver outside `agent_cwd` (e.g. `Read(../../etc/passwd)`).
            // When rejected, skip the hotspot entirely — the user can still
            // see the printed text, just not click it.
            let Some(resolved) = resolve_against(agent_cwd, display) else {
                continue;
            };

            out.push(TextHotspot {
                kind: HotspotKind::FilePath,
                text: display.to_string(),
                row: row_idx,
                start_col,
                end_col,
                is_dir: false,
                pattern_source: None,
                resolved_text: Some(resolved.to_string_lossy().into_owned()),
            });
        }
    }
    out
}

/// Join `rel` against `agent_cwd`. If `rel` is already absolute it is
/// returned unchanged. The result is a logical join — we do not call
/// `canonicalize` because that would stat the filesystem (the detector
/// must stay a pure function).
///
/// Returns `None` if `rel` contains a `..` component, to prevent a
/// hostile JSONL from coaxing the editor-fallback chain into opening
/// files outside `agent_cwd`. Absolute paths are returned as-is — the
/// caller has already opted in to those by typing/printing them.
fn resolve_against(agent_cwd: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    if is_absolute_any_platform(rel) {
        Some(p.to_path_buf())
    } else {
        Some(agent_cwd.join(p))
    }
}

/// Platform-independent absolute path check. On Windows, `Path::is_absolute()`
/// does not recognize Linux paths like `/home/...` as absolute — they get
/// re-joined against `agent_cwd` and produce garbage. This helper treats a
/// leading `/` as Linux-absolute (covers the cross-boundary case where agent
/// paths are always Linux) and falls back to `Path::is_absolute()` for native
/// Windows paths (`C:\...`, UNC `\\...`).
fn is_absolute_any_platform(s: &str) -> bool {
    s.starts_with('/') || Path::new(s).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_update_tool_call() {
        let cwd = Path::new("/home/u/repo");
        let hs = detect_claude_tool_call_hotspots(&rows(&["Update(crates/foo/src/bar.rs)"]), cwd);
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].text, "crates/foo/src/bar.rs");
        assert_eq!(
            hs[0].resolved_text.as_deref(),
            Some("/home/u/repo/crates/foo/src/bar.rs")
        );
        // Span covers only the path, not `Update(` or `)`.
        assert_eq!(hs[0].start_col, 7);
        assert_eq!(hs[0].end_col, 28);
    }

    #[test]
    fn detects_read_edit_write_multiedit() {
        let cwd = Path::new("/work");
        for (line, expected) in [
            ("Read(Cargo.toml)", "/work/Cargo.toml"),
            ("Edit(src/a.rs)", "/work/src/a.rs"),
            ("Write(/tmp/out.txt)", "/tmp/out.txt"),
            ("MultiEdit(src/b.rs)", "/work/src/b.rs"),
        ] {
            let hs = detect_claude_tool_call_hotspots(&rows(&[line]), cwd);
            assert_eq!(hs.len(), 1, "expected one hotspot for `{line}`");
            assert_eq!(hs[0].resolved_text.as_deref(), Some(expected));
        }
    }

    #[test]
    fn absolute_path_is_not_rejoined() {
        let cwd = Path::new("/home/u/repo");
        let hs = detect_claude_tool_call_hotspots(&rows(&["Read(/etc/hosts)"]), cwd);
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].resolved_text.as_deref(), Some("/etc/hosts"));
    }

    #[test]
    fn bash_is_not_a_hotspot() {
        let cwd = Path::new("/work");
        let hs = detect_claude_tool_call_hotspots(&rows(&["Bash(echo hi)"]), cwd);
        assert!(hs.is_empty(), "Bash must not produce a hotspot");
    }

    #[test]
    fn grep_and_glob_are_not_hotspots() {
        let cwd = Path::new("/work");
        assert!(
            detect_claude_tool_call_hotspots(&rows(&["Grep(TODO)"]), cwd).is_empty(),
            "Grep arg is a pattern, not a path"
        );
        assert!(
            detect_claude_tool_call_hotspots(&rows(&["Glob(**/*.rs)"]), cwd).is_empty(),
            "Glob arg is a pattern, not a path"
        );
    }

    #[test]
    fn task_and_webfetch_are_not_hotspots() {
        let cwd = Path::new("/work");
        assert!(
            detect_claude_tool_call_hotspots(&rows(&["Task(subagent: refactor something)"]), cwd)
                .is_empty()
        );
        assert!(
            detect_claude_tool_call_hotspots(&rows(&["WebFetch(https://example.com)"]), cwd)
                .is_empty()
        );
    }

    #[test]
    fn non_tool_prose_is_ignored() {
        let cwd = Path::new("/work");
        // Lowercase verbs should never match (Tool names are capitalised).
        let hs = detect_claude_tool_call_hotspots(&rows(&["will update(foo.rs) the file"]), cwd);
        assert!(hs.is_empty());
    }

    #[test]
    fn empty_arg_is_ignored() {
        let cwd = Path::new("/work");
        // `[^()\n]+` requires at least one char, so `Read()` won't match at
        // all — but guard defensively anyway.
        let hs = detect_claude_tool_call_hotspots(&rows(&["Read()"]), cwd);
        assert!(hs.is_empty());
    }

    #[test]
    fn cwd_index_tracks_worktree_hop() {
        // Simulates a Claude session that starts in one worktree, then the
        // agent hops to a sibling worktree and Claude's hook re-writes its
        // state file with the new working_dir. The resolver must pick up
        // the updated cwd on the next apply().
        use crate::state::{ClaudeSessionState, ClaudeStateUpdate, ClaudeStatus};

        let idx = ClaudeSessionCwdIndex::new();

        let sid = "deadbeef-cafe-babe-0000-000000000001".to_string();
        let mut state = ClaudeSessionState {
            session_id: sid.clone(),
            status: ClaudeStatus::Idle,
            working_dir: Some("/home/u/wt-a".into()),
            ..Default::default()
        };
        idx.apply(&ClaudeStateUpdate::Upserted(Box::new(state.clone())));
        let cwd1 = idx.cwd_for(&sid).expect("cwd indexed after first upsert");
        assert_eq!(cwd1, PathBuf::from("/home/u/wt-a"));

        // Resolve a tool call against the initial cwd.
        let hs1 = detect_claude_tool_call_hotspots(&rows(&["Read(src/lib.rs)"]), &cwd1);
        assert_eq!(
            hs1[0].resolved_text.as_deref(),
            Some("/home/u/wt-a/src/lib.rs")
        );

        // Agent hops worktrees — Claude's hook updates working_dir.
        state.working_dir = Some("/home/u/wt-b".into());
        idx.apply(&ClaudeStateUpdate::Upserted(Box::new(state.clone())));
        let cwd2 = idx.cwd_for(&sid).expect("cwd indexed after hop");
        assert_eq!(cwd2, PathBuf::from("/home/u/wt-b"));

        // The *same* printed tool-call line now resolves into the new worktree.
        // This is the load-bearing assertion: core's generic pane-cwd resolver
        // would still send the click to `wt-a`, which is the whole reason
        // resolution lives in the harness.
        let hs2 = detect_claude_tool_call_hotspots(&rows(&["Read(src/lib.rs)"]), &cwd2);
        assert_eq!(
            hs2[0].resolved_text.as_deref(),
            Some("/home/u/wt-b/src/lib.rs")
        );
    }

    #[test]
    fn cwd_index_forget_removes_entry() {
        use crate::state::{ClaudeSessionState, ClaudeStatus};
        let idx = ClaudeSessionCwdIndex::new();
        let state = ClaudeSessionState {
            session_id: "sid-1".into(),
            status: ClaudeStatus::Idle,
            working_dir: Some("/w".into()),
            ..Default::default()
        };
        idx.apply(&ClaudeStateUpdate::Upserted(Box::new(state)));
        assert!(idx.cwd_for("sid-1").is_some());
        idx.forget("sid-1");
        assert!(idx.cwd_for("sid-1").is_none());
    }

    #[test]
    fn cwd_index_retain_live_evicts_missing_entries() {
        // Regression test for tn-ossc: an entry whose session id is missing
        // from the poller's live snapshot must be dropped on the next sweep.
        // Otherwise sessions that exit *without* a re-upsert (e.g. the
        // `Removed` notify event arrives but the file path can't be matched
        // back to a session_id) leak forever in a long-running daemon.
        use crate::state::{ClaudeSessionState, ClaudeStatus};

        let idx = ClaudeSessionCwdIndex::new();

        // Insert three live sessions.
        for sid in ["sid-a", "sid-b", "sid-c"] {
            let state = ClaudeSessionState {
                session_id: sid.into(),
                status: ClaudeStatus::Idle,
                working_dir: Some(format!("/w/{sid}")),
                ..Default::default()
            };
            idx.apply(&ClaudeStateUpdate::Upserted(Box::new(state)));
        }
        assert_eq!(idx.len(), 3);

        // Simulate a poll snapshot where `sid-b` has gone away — its hook
        // file was removed but the `Removed` arm of `apply` couldn't evict
        // by id. The sweep must drop it.
        let snapshot = vec![
            ClaudeSessionState {
                session_id: "sid-a".into(),
                status: ClaudeStatus::Idle,
                ..Default::default()
            },
            ClaudeSessionState {
                session_id: "sid-c".into(),
                status: ClaudeStatus::Idle,
                ..Default::default()
            },
        ];
        let evicted = idx.retain_live_from_snapshot(&snapshot);
        assert_eq!(evicted, 1, "exactly one orphan should be evicted");
        assert_eq!(idx.len(), 2);
        assert!(idx.cwd_for("sid-a").is_some(), "sid-a must survive");
        assert!(idx.cwd_for("sid-b").is_none(), "sid-b must be evicted");
        assert!(idx.cwd_for("sid-c").is_some(), "sid-c must survive");

        // A second sweep with the same snapshot is a no-op.
        let evicted = idx.retain_live_from_snapshot(&snapshot);
        assert_eq!(evicted, 0);
        assert_eq!(idx.len(), 2);

        // An empty snapshot evicts everything (e.g. the user closed all
        // Claude sessions).
        let evicted = idx.retain_live_from_snapshot(&[]);
        assert_eq!(evicted, 2);
        assert!(idx.is_empty());
    }

    #[test]
    fn parent_dir_traversal_is_rejected() {
        let cwd = Path::new("/home/u/repo");
        let hs = detect_claude_tool_call_hotspots(
            &rows(&["Read(../../etc/passwd)", "Edit(src/../../../etc/shadow)"]),
            cwd,
        );
        assert!(
            hs.is_empty(),
            "paths containing `..` must not produce hotspots"
        );
    }

    #[test]
    fn normal_relative_path_resolves() {
        assert_eq!(
            resolve_against(Path::new("/work"), "src/lib.rs"),
            Some(PathBuf::from("/work/src/lib.rs"))
        );
    }

    #[test]
    fn absolute_path_passes_through() {
        assert_eq!(
            resolve_against(Path::new("/work"), "/etc/hosts"),
            Some(PathBuf::from("/etc/hosts"))
        );
    }

    #[test]
    fn parent_dir_returns_none() {
        assert_eq!(resolve_against(Path::new("/work"), "../etc/passwd"), None);
        assert_eq!(resolve_against(Path::new("/work"), "src/../../etc"), None);
    }

    #[test]
    fn multiple_matches_across_rows() {
        let cwd = Path::new("/w");
        let hs = detect_claude_tool_call_hotspots(
            &rows(&["Read(a.rs)", "plain text line", "Edit(b.rs)", "Bash(true)"]),
            cwd,
        );
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].row, 0);
        assert_eq!(hs[0].resolved_text.as_deref(), Some("/w/a.rs"));
        assert_eq!(hs[1].row, 2);
        assert_eq!(hs[1].resolved_text.as_deref(), Some("/w/b.rs"));
    }

    // --- Platform-independent absolute path detection (tn-cqdf) ---
    // These tests validate `is_absolute_any_platform` and `resolve_against`
    // on every platform, including the cross-boundary case where a Windows
    // daemon resolves Linux-style agent paths.

    #[test]
    fn is_absolute_linux_home_path() {
        // A leading `/` is always treated as absolute, even on Windows.
        assert!(is_absolute_any_platform("/home/user/file.rs"));
    }

    #[test]
    fn is_absolute_linux_tmp_path() {
        assert!(is_absolute_any_platform("/tmp/test.txt"));
    }

    #[test]
    fn is_absolute_relative_path_not_absolute() {
        assert!(!is_absolute_any_platform("src/main.rs"));
        assert!(!is_absolute_any_platform("./lib.rs"));
        assert!(!is_absolute_any_platform("file.txt"));
    }

    #[test]
    fn resolve_linux_absolute_on_any_platform() {
        // Linux-absolute paths must pass through unchanged, regardless
        // of the host platform.
        assert_eq!(
            resolve_against(Path::new("/work"), "/home/user/file.rs"),
            Some(PathBuf::from("/home/user/file.rs"))
        );
        assert_eq!(
            resolve_against(Path::new("/work"), "/tmp/test.txt"),
            Some(PathBuf::from("/tmp/test.txt"))
        );
    }

    #[test]
    fn resolve_relative_still_joins() {
        // Relative paths are joined against agent_cwd as before.
        assert_eq!(
            resolve_against(Path::new("/home/user/project"), "src/main.rs"),
            Some(PathBuf::from("/home/user/project/src/main.rs"))
        );
    }

    #[test]
    fn resolve_dot_relative_joins() {
        // `./lib.rs` is relative and should be joined against agent_cwd.
        assert_eq!(
            resolve_against(Path::new("/home/user/project"), "./lib.rs"),
            Some(PathBuf::from("/home/user/project/./lib.rs"))
        );
    }
}
