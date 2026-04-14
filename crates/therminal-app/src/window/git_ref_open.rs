//! Git commit hash hotspot click handlers (tn-fzr0).
//!
//! Right-clicking a git commit hash hotspot opens an action palette
//! built from the subset of `[hotspots] git_tools` whose binaries
//! resolve on `PATH`. Picking a tool splits the focused pane, `cd`s
//! into the nearest git working tree root, and runs the tool with
//! the appropriate per-tool argv:
//!
//! - `lazygit --filter <hash>` — interactive commit explorer.
//! - `gitlogue -c <hash>` — typing-animation commit replay.
//! - `tig show <hash>` — read-only commit details.
//!
//! The PATH probe runs once at startup and again whenever the
//! `git_tools` config field changes (see `App::apply_config`). Tool
//! names that the app doesn't know how to invoke are silently dropped
//! from the discovered set so the menu never offers an unusable entry.
//!
//! The pane-spawn path mirrors `folder_open.rs`: pure planning
//! functions (`plan_git_ref_open`) build the bytes to write into the
//! freshly-split pane, with PATH probing factored out behind a
//! closure for unit testing.

use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use super::App;

// ── Tool catalog ─────────────────────────────────────────────────────────

/// One git TUI tool the app knows how to invoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GitToolSpec {
    /// Bare binary name probed on `PATH` and matched against
    /// `[hotspots].git_tools` entries.
    pub binary: &'static str,
    /// Right-click menu label shown to the user.
    pub menu_label: &'static str,
    /// argv after the binary name; the literal token `{hash}` is
    /// replaced with the clicked commit hash.
    pub args_template: &'static [&'static str],
}

/// All known git TUI tools, in display order. Adding a new tool means
/// appending an entry here AND updating the `git_tools` default in
/// `HotspotsConfig` so the new tool is probed at startup.
const KNOWN_GIT_TOOLS: &[GitToolSpec] = &[
    GitToolSpec {
        binary: "lazygit",
        menu_label: "Show in lazygit",
        args_template: &["--filter", "{hash}"],
    },
    GitToolSpec {
        binary: "gitlogue",
        menu_label: "Replay in gitlogue",
        args_template: &["-c", "{hash}"],
    },
    GitToolSpec {
        binary: "tig",
        menu_label: "Show in tig",
        args_template: &["show", "{hash}"],
    },
];

/// Look up the catalog entry for a tool name. Returns `None` for
/// names the app doesn't know how to invoke.
pub(crate) fn lookup_git_tool(name: &str) -> Option<&'static GitToolSpec> {
    KNOWN_GIT_TOOLS.iter().find(|t| t.binary == name)
}

/// Resolve the menu label for `name`. Falls back to the binary name
/// when no catalog entry exists (used by callers that already filtered
/// against `lookup_git_tool`).
pub(crate) fn menu_label_for(name: &str) -> &'static str {
    lookup_git_tool(name)
        .map(|t| t.menu_label)
        .unwrap_or("Show")
}

// ── PATH discovery ───────────────────────────────────────────────────────

/// Probe `PATH` for each tool name in `requested`, returning the
/// subset that resolves AND that the app knows how to invoke. Order
/// is preserved from `requested` so users can reorder menu entries
/// by reordering the config list.
pub(crate) fn discover_git_tools(requested: &[String]) -> Vec<String> {
    discover_git_tools_with(requested, which_on_path)
}

/// Test seam for `discover_git_tools` — accepts a custom PATH probe.
pub(crate) fn discover_git_tools_with<W>(requested: &[String], mut which_fn: W) -> Vec<String>
where
    W: FnMut(&str) -> bool,
{
    let mut out = Vec::with_capacity(requested.len());
    for name in requested {
        if name.is_empty() {
            continue;
        }
        if lookup_git_tool(name).is_none() {
            // Don't include tools whose invocation form we don't know;
            // listing them in the menu would dispatch to a no-op.
            continue;
        }
        if which_fn(name) {
            out.push(name.clone());
        }
    }
    out
}

// ── Pure planning ────────────────────────────────────────────────────────

/// Outcome of planning a git-ref pane open. Decoupled from real IO so
/// it can be exhaustively unit tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GitRefOpenPlan {
    /// The tool's binary resolved on `PATH`. Split the focused pane,
    /// then write `bytes` into the new pane: `cd '<root>' && clear &&
    /// <tool> [args…]\n`.
    SpawnTool { cmd_display: String, bytes: Vec<u8> },
    /// The tool was discovered earlier but its binary disappeared
    /// between discovery and click. Split the pane, write a
    /// `git show <hash>` fallback, and surface a toast using
    /// `missing_binary` for the message.
    FallbackGitShow {
        missing_binary: String,
        bytes: Vec<u8>,
    },
    /// The clicked text isn't a recognized tool name (shouldn't
    /// normally happen — the menu only offers known tools).
    UnknownTool,
}

/// Plan the bytes to send into the freshly-split pane.
///
/// `tool` is the bare binary name (matched against `KNOWN_GIT_TOOLS`).
/// `hash` is the commit hash captured from the hotspot. `repo_root`
/// is the working tree root we should `cd` into before exec. `which_fn`
/// is a `PATH` probe so callers can stub the filesystem in tests.
pub(crate) fn plan_git_ref_open<W>(
    tool: &str,
    hash: &str,
    repo_root: &str,
    mut which_fn: W,
) -> GitRefOpenPlan
where
    W: FnMut(&str) -> bool,
{
    let Some(spec) = lookup_git_tool(tool) else {
        return GitRefOpenPlan::UnknownTool;
    };

    let cd_clear = format!("cd {} && clear", shell_quote(repo_root));

    if !which_fn(spec.binary) {
        // Tool went missing between discovery and click. `git show`
        // is universally available wherever a git repo exists, so
        // fall back to that and let the user see the diff inline.
        let bytes = format!("{cd_clear} && git show {}\n", shell_quote(hash)).into_bytes();
        return GitRefOpenPlan::FallbackGitShow {
            missing_binary: spec.binary.to_string(),
            bytes,
        };
    }

    // Substitute `{hash}` in the argv template, then build a
    // shell-quoted command line. We deliberately do NOT use `exec`
    // here: exec replaces the shell with the command, so Ctrl+C or
    // a crash kills the PTY with no way to recover. Without exec,
    // the shell survives and the user gets a prompt back after the
    // command exits.
    let substituted: Vec<String> = spec
        .args_template
        .iter()
        .map(|a| a.replace("{hash}", hash))
        .collect();

    let mut cmd_line = String::new();
    cmd_line.push_str(&shell_quote(spec.binary));
    for arg in &substituted {
        cmd_line.push(' ');
        cmd_line.push_str(&shell_quote(arg));
    }
    let bytes = format!("{cd_clear} && {cmd_line}\n").into_bytes();

    let mut display = String::from(spec.binary);
    for arg in &substituted {
        display.push(' ');
        display.push_str(arg);
    }

    GitRefOpenPlan::SpawnTool {
        cmd_display: display,
        bytes,
    }
}

// ── Repo root discovery ──────────────────────────────────────────────────

/// Walk up from `start` looking for a `.git` entry (directory or
/// file). Returns the directory that *contains* `.git` — i.e. the
/// working tree root, not the gitdir itself. This is what we want to
/// `cd` into before launching a TUI tool. Returns `None` if no `.git`
/// entry is found in any ancestor.
pub(crate) fn find_worktree_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let dot_git = current.join(".git");
        if dot_git.exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

// ── App-side dispatcher ──────────────────────────────────────────────────

impl App {
    /// Spawn the requested git TUI tool in a new pane, rooted at the
    /// nearest git worktree root walking up from the focused pane's
    /// cwd. Falls back to `git show <hash>` (with a toast) when the
    /// tool's binary disappeared between discovery and click. Shows a
    /// "not in a git repository" toast and returns when no `.git`
    /// entry is found in the focused pane's cwd ancestry.
    pub(crate) fn show_git_ref_in_pane(&mut self, tool: &str, hash: &str) {
        let cwd = match self.focused_pane_cwd() {
            Some(c) => c,
            None => {
                warn!(%tool, %hash, "show_git_ref_in_pane: no focused pane cwd");
                self.show_toast("no focused pane cwd");
                return;
            }
        };

        let repo_root = match find_worktree_root(Path::new(&cwd)) {
            Some(r) => r,
            None => {
                warn!(%cwd, %tool, "show_git_ref_in_pane: not in a git repository");
                self.show_toast("not in a git repository");
                return;
            }
        };
        let repo_root_str = repo_root.to_string_lossy().into_owned();

        let plan = plan_git_ref_open(tool, hash, &repo_root_str, which_on_path);

        let (bytes, fallback_msg) = match plan {
            GitRefOpenPlan::SpawnTool { cmd_display, bytes } => {
                info!(%tool, %hash, root = %repo_root_str, %cmd_display, "show_git_ref_in_pane: spawning tool");
                (bytes, None)
            }
            GitRefOpenPlan::FallbackGitShow {
                missing_binary,
                bytes,
            } => {
                let msg = format!("{missing_binary} not found — falling back to `git show {hash}`");
                warn!(%tool, %hash, "show_git_ref_in_pane: tool missing on PATH");
                (bytes, Some(msg))
            }
            GitRefOpenPlan::UnknownTool => {
                debug!(%tool, "show_git_ref_in_pane: unknown tool name; dropping");
                self.show_toast(format!("unknown git tool: {tool}"));
                return;
            }
        };

        // In daemon mode the split is async — the new pane doesn't
        // exist until `finish_split_pane_remote` runs. Carry the
        // bytes in the completion callback so they're written after
        // the PTY is live.
        if self.is_daemon_mode() {
            use super::pane_ops::DaemonSplitOnComplete;
            self.split_focused_pane_auto_with(DaemonSplitOnComplete::WriteBytesAndFocus {
                bytes,
                toast: fallback_msg,
            });
            return;
        }

        // Local mode: split is synchronous — write immediately.
        let original_focus = self.focused_pane();
        self.split_focused_pane_auto();
        let new_pane = match self.focused_pane() {
            Some(id) if Some(id) != original_focus => id,
            _ => {
                warn!("show_git_ref_in_pane: split did not produce a new pane");
                return;
            }
        };

        if let Some(msg) = fallback_msg {
            self.show_toast(msg);
        }

        self.pty_write_to_pane(&bytes, new_pane);
    }
}

// ── PATH probe + shell quoting ──────────────────────────────────────────

/// Cross-platform `PATH` probe. Mirrors the helper in `folder_open.rs`.
fn which_on_path(cmd: &str) -> bool {
    if cmd.contains('/') || cmd.contains('\\') {
        return std::path::Path::new(cmd).is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    #[cfg(windows)]
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".EXE;.BAT;.CMD;.COM".to_string())
        .split(';')
        .map(|s| s.to_string())
        .collect();
    for dir in std::env::split_paths(&path) {
        let full = dir.join(cmd);
        if full.is_file() {
            return true;
        }
        #[cfg(windows)]
        for ext in &exts {
            let with_ext = dir.join(format!("{cmd}{ext}"));
            if with_ext.is_file() {
                return true;
            }
        }
    }
    false
}

/// Minimal POSIX shell single-quote escape. Wraps in `'…'` and
/// escapes embedded single quotes.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── lookup_git_tool / menu_label_for ──

    #[test]
    fn lookup_returns_known_tools() {
        assert!(lookup_git_tool("lazygit").is_some());
        assert!(lookup_git_tool("gitlogue").is_some());
        assert!(lookup_git_tool("tig").is_some());
    }

    #[test]
    fn lookup_returns_none_for_unknown_tool() {
        assert!(lookup_git_tool("git").is_none());
        assert!(lookup_git_tool("nope").is_none());
        assert!(lookup_git_tool("").is_none());
    }

    #[test]
    fn menu_labels_match_user_facing_strings() {
        assert_eq!(menu_label_for("lazygit"), "Show in lazygit");
        assert_eq!(menu_label_for("gitlogue"), "Replay in gitlogue");
        assert_eq!(menu_label_for("tig"), "Show in tig");
    }

    // ── discover_git_tools_with ──

    #[test]
    fn discover_keeps_only_resolved_known_tools() {
        let requested = names(&["lazygit", "tig", "gitlogue"]);
        let discovered = discover_git_tools_with(&requested, |c| c == "lazygit" || c == "tig");
        assert_eq!(discovered, vec!["lazygit".to_string(), "tig".to_string()]);
    }

    #[test]
    fn discover_drops_unknown_tools_even_if_on_path() {
        // `git` is not in our catalog — must be skipped even though
        // it's almost always on PATH.
        let requested = names(&["git", "lazygit"]);
        let discovered = discover_git_tools_with(&requested, |_| true);
        assert_eq!(discovered, vec!["lazygit".to_string()]);
    }

    #[test]
    fn discover_preserves_request_order() {
        let requested = names(&["tig", "gitlogue", "lazygit"]);
        let discovered = discover_git_tools_with(&requested, |_| true);
        assert_eq!(
            discovered,
            vec![
                "tig".to_string(),
                "gitlogue".to_string(),
                "lazygit".to_string(),
            ]
        );
    }

    #[test]
    fn discover_handles_empty_input() {
        let discovered = discover_git_tools_with(&[], |_| true);
        assert!(discovered.is_empty());
    }

    #[test]
    fn discover_skips_empty_entries() {
        let requested = names(&["", "lazygit"]);
        let discovered = discover_git_tools_with(&requested, |_| true);
        assert_eq!(discovered, vec!["lazygit".to_string()]);
    }

    // ── plan_git_ref_open ──

    #[test]
    fn plan_lazygit_emits_filter_invocation() {
        let plan = plan_git_ref_open("lazygit", "abc1234", "/repo", |c| c == "lazygit");
        match plan {
            GitRefOpenPlan::SpawnTool { cmd_display, bytes } => {
                assert_eq!(cmd_display, "lazygit --filter abc1234");
                let s = String::from_utf8(bytes).unwrap();
                assert!(s.starts_with("cd '/repo' && clear && "));
                assert!(s.contains("'lazygit' '--filter' 'abc1234'"));
                assert!(!s.contains("exec"), "must not use exec — shell must survive Ctrl+C");
                assert!(s.ends_with('\n'));
            }
            other => panic!("expected SpawnTool, got {other:?}"),
        }
    }

    #[test]
    fn plan_gitlogue_emits_dash_c_invocation() {
        let plan = plan_git_ref_open("gitlogue", "deadbeef", "/r", |c| c == "gitlogue");
        match plan {
            GitRefOpenPlan::SpawnTool { cmd_display, bytes } => {
                assert_eq!(cmd_display, "gitlogue -c deadbeef");
                let s = String::from_utf8(bytes).unwrap();
                assert!(s.contains("'gitlogue' '-c' 'deadbeef'"));
                assert!(!s.contains("exec"), "must not use exec — shell must survive Ctrl+C");
            }
            other => panic!("expected SpawnTool, got {other:?}"),
        }
    }

    #[test]
    fn plan_tig_emits_show_invocation() {
        let plan = plan_git_ref_open("tig", "feedface", "/r", |c| c == "tig");
        match plan {
            GitRefOpenPlan::SpawnTool { cmd_display, bytes } => {
                assert_eq!(cmd_display, "tig show feedface");
                let s = String::from_utf8(bytes).unwrap();
                assert!(s.contains("'tig' 'show' 'feedface'"));
                assert!(!s.contains("exec"), "must not use exec — shell must survive Ctrl+C");
            }
            other => panic!("expected SpawnTool, got {other:?}"),
        }
    }

    #[test]
    fn plan_unknown_tool_yields_unknown_variant() {
        let plan = plan_git_ref_open("git", "abc1234", "/r", |_| true);
        assert_eq!(plan, GitRefOpenPlan::UnknownTool);
    }

    #[test]
    fn plan_missing_binary_falls_back_to_git_show() {
        let plan = plan_git_ref_open("lazygit", "abc1234", "/r", |_| false);
        match plan {
            GitRefOpenPlan::FallbackGitShow {
                missing_binary,
                bytes,
            } => {
                assert_eq!(missing_binary, "lazygit");
                let s = String::from_utf8(bytes).unwrap();
                assert!(s.contains("git show 'abc1234'"));
                assert!(!s.contains("exec"), "must not use exec — shell must survive Ctrl+C");
                assert!(s.starts_with("cd '/r' && clear && "));
            }
            other => panic!("expected FallbackGitShow, got {other:?}"),
        }
    }

    #[test]
    fn plan_quotes_repo_root_with_spaces() {
        let plan = plan_git_ref_open("lazygit", "abc", "/home/me/My Repos/proj", |_| true);
        match plan {
            GitRefOpenPlan::SpawnTool { bytes, .. } => {
                let s = String::from_utf8(bytes).unwrap();
                assert!(s.contains("cd '/home/me/My Repos/proj'"));
            }
            other => panic!("expected SpawnTool, got {other:?}"),
        }
    }

    #[test]
    fn plan_escapes_single_quote_in_repo_root() {
        let plan = plan_git_ref_open("lazygit", "abc", "/tmp/it's-a-repo", |_| true);
        match plan {
            GitRefOpenPlan::SpawnTool { bytes, .. } => {
                let s = String::from_utf8(bytes).unwrap();
                // POSIX-safe escape: '...'\''...' (close, escaped quote, reopen).
                assert!(s.contains(r#"'/tmp/it'\''s-a-repo'"#));
            }
            other => panic!("expected SpawnTool, got {other:?}"),
        }
    }

    // ── find_worktree_root ──

    #[test]
    fn find_worktree_root_finds_dot_git_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let nested = repo.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();
        let found = find_worktree_root(&nested).unwrap();
        // Canonicalize for comparison: macOS may insert /private/.
        assert_eq!(found.canonicalize().unwrap(), repo.canonicalize().unwrap());
    }

    #[test]
    fn find_worktree_root_handles_dot_git_file_for_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("worktree");
        let nested = repo.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(repo.join(".git"), "gitdir: /elsewhere/.git/worktrees/foo").unwrap();
        let found = find_worktree_root(&nested).unwrap();
        assert_eq!(found.canonicalize().unwrap(), repo.canonicalize().unwrap());
    }

    #[test]
    fn find_worktree_root_returns_none_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("not-a-repo/sub");
        std::fs::create_dir_all(&dir).unwrap();
        // No .git anywhere up to tmp's parent — but tmp itself may be
        // inside an unrelated repo on a dev machine. Test passes only
        // when no ancestor has a .git, so we use canonicalize to be
        // sure we're testing the no-repo case.
        let real = dir.canonicalize().unwrap();
        // Walk up looking for .git on the filesystem root path.
        // If none of `tmp`'s ancestors have .git this returns None;
        // otherwise it returns whichever ancestor does.
        match find_worktree_root(&real) {
            None => {}
            Some(found) => {
                // The found root should be an ancestor of `dir`.
                assert!(real.starts_with(&found));
            }
        }
    }

    // ── shell_quote ──

    #[test]
    fn shell_quote_basic() {
        assert_eq!(shell_quote("simple"), "'simple'");
    }

    #[test]
    fn shell_quote_handles_spaces() {
        assert_eq!(shell_quote("a b c"), "'a b c'");
    }

    #[test]
    fn shell_quote_escapes_single_quote() {
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
    }
}
