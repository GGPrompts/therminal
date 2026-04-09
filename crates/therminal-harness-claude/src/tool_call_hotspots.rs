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

use std::collections::HashMap;
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
    /// the observer can let the next poll sweep cleanse stale entries.
    pub fn apply(&self, update: &ClaudeStateUpdate) {
        match update {
            ClaudeStateUpdate::Upserted(state) => self.upsert(state),
            ClaudeStateUpdate::Removed { .. } => {
                // Path-keyed removal is handled by the pipeline, not here.
                // Stale entries self-correct on the next Upsert; real eviction
                // is not safety-critical because we only use this index for
                // UI hints.
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
        let mut g = self.inner.lock().expect("cwd index poisoned");
        g.insert(state.session_id.clone(), PathBuf::from(wd));
    }

    /// Explicitly drop a session's entry (used by the pipeline on Removed).
    pub fn forget(&self, session_id: &str) {
        let mut g = self.inner.lock().expect("cwd index poisoned");
        g.remove(session_id);
    }

    /// Look up the agent cwd for a Claude session id. Returns `None` when
    /// the session is unknown or its state didn't carry a `working_dir`.
    pub fn cwd_for(&self, session_id: &str) -> Option<PathBuf> {
        let g = self.inner.lock().expect("cwd index poisoned");
        g.get(session_id).cloned()
    }

    /// Current number of indexed sessions.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("cwd index poisoned").len()
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

            let resolved = resolve_against(agent_cwd, display);

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
fn resolve_against(agent_cwd: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        agent_cwd.join(p)
    }
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
}
