//! Pure planner / helpers for editor invocation: `OpenInEditorPlan`,
//! `plan_open_in_editor`, `resolve_editor_chain`, `which_on_path`,
//! `shell_quote`. All side-effect-free so they can be unit tested without
//! touching the real filesystem or environment.

/// Outcome of planning an `open_in_editor` call, decoupled from the real
/// filesystem / process spawn so it can be unit tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenInEditorPlan {
    /// Spawn `editor +line path` as a detached OS process (GUI editors).
    Spawn {
        editor: String,
        path: String,
        line: String,
    },
    /// Spawn the editor inside a new terminal pane (TUI editors like
    /// vim, nvim, helix, micro, nano, tfe, etc.). The caller should
    /// split the focused pane and write a shell command into the PTY.
    SpawnInPane {
        editor: String,
        path: String,
        line: String,
    },
    /// No editor resolved from the chain, but the file exists — try
    /// `xdg-open` / `open` as a last resort.
    OpenFallback { path: String },
    /// Pre-flight validation failed. `String` is the user-facing message
    /// (suitable for a toast).
    Fail(String),
}

/// Pure planner for [`crate::window::App::open_in_editor`]. Separates path
/// parsing, filesystem validation, and editor resolution from actual IO so
/// each failure branch can be asserted from a unit test.
pub(crate) fn plan_open_in_editor<M, R>(
    path_with_loc: &str,
    chain: &[String],
    mut is_file_fn: M,
    mut resolve_fn: R,
) -> OpenInEditorPlan
where
    M: FnMut(&str) -> std::io::Result<bool>,
    R: FnMut(&[String]) -> Option<String>,
{
    // Split path from optional :line[:col].
    let (path, line) = match path_with_loc.find(':') {
        Some(idx) if path_with_loc[idx + 1..].starts_with(|c: char| c.is_ascii_digit()) => {
            let rest = &path_with_loc[idx + 1..];
            let line_str = rest.split(':').next().unwrap_or("1");
            (&path_with_loc[..idx], line_str)
        }
        _ => (path_with_loc, "1"),
    };

    let basename = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string();

    // Validate hotspot is a real file — hotspot paths come from terminal
    // screen content and may be attacker-controlled.
    match is_file_fn(path) {
        Ok(true) => {}
        Ok(false) => {
            return OpenInEditorPlan::Fail(format!("{basename} is not a regular file"));
        }
        Err(_) => {
            return OpenInEditorPlan::Fail(format!("file not found: {basename}"));
        }
    }

    match resolve_fn(chain) {
        Some(editor) => {
            if is_tui_editor(&editor) {
                OpenInEditorPlan::SpawnInPane {
                    editor,
                    path: path.to_string(),
                    line: line.to_string(),
                }
            } else {
                OpenInEditorPlan::Spawn {
                    editor,
                    path: path.to_string(),
                    line: line.to_string(),
                }
            }
        }
        None => OpenInEditorPlan::OpenFallback {
            path: path.to_string(),
        },
    }
}

/// Known TUI editors that require a terminal pane to render. The check
/// extracts the head token (first whitespace-delimited word) and compares
/// its basename against a known list. GUI editors (`code`, `subl`, etc.)
/// are intentionally absent — they work fine as detached OS processes.
pub(crate) fn is_tui_editor(cmd: &str) -> bool {
    const TUI_EDITORS: &[&str] = &[
        "tfe", "micro", "vim", "nvim", "vi", "nano", "helix", "hx", "emacs", "ne", "mcedit", "joe",
        "jed", "kakoune", "kak", "amp", "zee", "ox", "dte",
    ];

    let head = cmd.split_whitespace().next().unwrap_or("");
    // Extract basename: `/usr/bin/nvim` -> `nvim`
    let basename = std::path::Path::new(head)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(head);
    TUI_EDITORS.contains(&basename)
}

/// Resolve an entry from the editor fallback chain to a concrete command.
///
/// Each entry is either a literal command (e.g. `"nvim"`) or an env-var
/// token (`"$EDITOR"`, `"$VISUAL"`). Literal commands are probed via the
/// supplied `which_fn`; env-var tokens are first expanded via `env_fn`
/// and the result is then probed. The first successful resolution wins.
/// Returns `None` if every entry fails.
///
/// This is factored out of [`crate::window::App::open_in_editor`] so it
/// can be unit tested without touching the real filesystem or environment.
pub(crate) fn resolve_editor_chain<W, E>(
    chain: &[String],
    mut which_fn: W,
    mut env_fn: E,
) -> Option<String>
where
    W: FnMut(&str) -> bool,
    E: FnMut(&str) -> Option<String>,
{
    for entry in chain {
        let candidate: Option<String> = if let Some(var) = entry.strip_prefix('$') {
            env_fn(var)
        } else {
            Some(entry.clone())
        };
        let Some(cmd) = candidate else {
            continue;
        };
        if cmd.is_empty() {
            continue;
        }
        // If the user's $EDITOR contains args ("code --wait"), take the
        // head token for the PATH probe but return the full string so the
        // args are preserved downstream.
        let head = cmd.split_whitespace().next().unwrap_or("");
        if head.is_empty() {
            continue;
        }
        if which_fn(head) {
            return Some(cmd);
        }
    }
    None
}

/// Cross-platform PATH lookup. Returns `true` if `cmd` resolves to an
/// executable file on any PATH entry. On Windows, also tries the common
/// extensions from `PATHEXT`.
pub(crate) fn which_on_path(cmd: &str) -> bool {
    // If it contains a path separator, probe it directly.
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

/// Minimal POSIX shell single-quote escape for embedding a path in a
/// command line. Wraps in `'...'` and escapes embedded single quotes.
pub(crate) fn shell_quote(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn chain(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resolve_picks_first_hit_on_path() {
        let c = chain(&["$VISUAL", "$EDITOR", "code", "nvim", "vim", "nano"]);
        let env: HashMap<&str, &str> = HashMap::new();
        let resolved = resolve_editor_chain(
            &c,
            |cmd| matches!(cmd, "nvim" | "vim" | "nano"),
            |v| env.get(v).map(|s| s.to_string()),
        );
        assert_eq!(resolved.as_deref(), Some("nvim"));
    }

    #[test]
    fn resolve_expands_env_tokens_first() {
        let c = chain(&["$VISUAL", "$EDITOR", "vim"]);
        let mut env: HashMap<&str, &str> = HashMap::new();
        env.insert("EDITOR", "hx");
        let resolved =
            resolve_editor_chain(&c, |cmd| cmd == "hx", |v| env.get(v).map(|s| s.to_string()));
        assert_eq!(resolved.as_deref(), Some("hx"));
    }

    #[test]
    fn resolve_skips_unset_env_tokens() {
        let c = chain(&["$VISUAL", "$EDITOR", "nano"]);
        let resolved = resolve_editor_chain(&c, |cmd| cmd == "nano", |_| None);
        assert_eq!(resolved.as_deref(), Some("nano"));
    }

    #[test]
    fn resolve_preserves_editor_args_but_probes_head() {
        let c = chain(&["$EDITOR", "vim"]);
        let mut env: HashMap<&str, &str> = HashMap::new();
        env.insert("EDITOR", "code --wait");
        let resolved = resolve_editor_chain(
            &c,
            |cmd| cmd == "code",
            |v| env.get(v).map(|s| s.to_string()),
        );
        assert_eq!(resolved.as_deref(), Some("code --wait"));
    }

    // ── plan_open_in_editor (end-to-end failure paths) ──

    fn ok_file(_: &str) -> std::io::Result<bool> {
        Ok(true)
    }
    fn not_regular(_: &str) -> std::io::Result<bool> {
        Ok(false)
    }
    fn not_found(_: &str) -> std::io::Result<bool> {
        Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }

    #[test]
    fn plan_file_not_found_yields_fail_with_basename() {
        let c = chain(&["nvim"]);
        let plan = plan_open_in_editor("/nope/missing.rs:42", &c, not_found, |_| None);
        assert_eq!(
            plan,
            OpenInEditorPlan::Fail("file not found: missing.rs".to_string())
        );
    }

    #[test]
    fn plan_not_regular_file_yields_fail_with_basename() {
        let c = chain(&["nvim"]);
        let plan = plan_open_in_editor("/tmp/somedir", &c, not_regular, |_| None);
        assert_eq!(
            plan,
            OpenInEditorPlan::Fail("somedir is not a regular file".to_string())
        );
    }

    #[test]
    fn plan_no_editor_resolved_yields_open_fallback() {
        let c = chain(&["$EDITOR", "vim"]);
        let plan = plan_open_in_editor("/tmp/foo.rs:10", &c, ok_file, |_| None);
        assert_eq!(
            plan,
            OpenInEditorPlan::OpenFallback {
                path: "/tmp/foo.rs".to_string()
            }
        );
    }

    #[test]
    fn plan_tui_editor_yields_spawn_in_pane() {
        let c = chain(&["nvim"]);
        let plan = plan_open_in_editor("/tmp/foo.rs:42:7", &c, ok_file, |_| {
            Some("nvim".to_string())
        });
        assert_eq!(
            plan,
            OpenInEditorPlan::SpawnInPane {
                editor: "nvim".to_string(),
                path: "/tmp/foo.rs".to_string(),
                line: "42".to_string(),
            }
        );
    }

    #[test]
    fn plan_gui_editor_yields_spawn() {
        let c = chain(&["code"]);
        let plan = plan_open_in_editor("/tmp/foo.rs:10", &c, ok_file, |_| Some("code".to_string()));
        assert_eq!(
            plan,
            OpenInEditorPlan::Spawn {
                editor: "code".to_string(),
                path: "/tmp/foo.rs".to_string(),
                line: "10".to_string(),
            }
        );
    }

    #[test]
    fn plan_no_line_defaults_to_1() {
        let c = chain(&["vim"]);
        let plan = plan_open_in_editor("/tmp/bar.md", &c, ok_file, |_| Some("vim".to_string()));
        match plan {
            OpenInEditorPlan::SpawnInPane { line, .. } => assert_eq!(line, "1"),
            other => panic!("expected SpawnInPane, got {other:?}"),
        }
    }

    // ── is_tui_editor ─────────────────────────────────────────────────

    #[test]
    fn is_tui_detects_known_tui_editors() {
        for name in &[
            "tfe", "micro", "vim", "nvim", "vi", "nano", "helix", "hx", "emacs", "kak",
        ] {
            assert!(is_tui_editor(name), "{name} should be TUI");
        }
    }

    #[test]
    fn is_tui_rejects_gui_editors() {
        for name in &["code", "subl", "gedit", "notepad++", "kate"] {
            assert!(!is_tui_editor(name), "{name} should NOT be TUI");
        }
    }

    #[test]
    fn is_tui_extracts_head_token() {
        assert!(is_tui_editor("nvim --clean"));
        assert!(is_tui_editor("emacs -nw"));
        assert!(!is_tui_editor("code --wait"));
    }

    #[test]
    fn is_tui_handles_absolute_path() {
        assert!(is_tui_editor("/usr/bin/nvim"));
        assert!(!is_tui_editor("/usr/bin/code"));
    }

    // ── plan_open_in_editor: TUI vs GUI via $EDITOR ─────────────────

    #[test]
    fn plan_env_editor_vim_yields_spawn_in_pane() {
        let c = chain(&["$EDITOR"]);
        let plan = plan_open_in_editor("/tmp/foo.rs:5", &c, ok_file, |_| Some("vim".to_string()));
        assert_eq!(
            plan,
            OpenInEditorPlan::SpawnInPane {
                editor: "vim".to_string(),
                path: "/tmp/foo.rs".to_string(),
                line: "5".to_string(),
            }
        );
    }

    #[test]
    fn plan_env_editor_code_yields_spawn() {
        let c = chain(&["$EDITOR"]);
        let plan = plan_open_in_editor("/tmp/foo.rs:5", &c, ok_file, |_| Some("code".to_string()));
        assert_eq!(
            plan,
            OpenInEditorPlan::Spawn {
                editor: "code".to_string(),
                path: "/tmp/foo.rs".to_string(),
                line: "5".to_string(),
            }
        );
    }

    #[test]
    fn plan_tfe_yields_spawn_in_pane() {
        let c = chain(&["tfe"]);
        let plan = plan_open_in_editor("/tmp/foo.rs:1", &c, ok_file, |_| Some("tfe".to_string()));
        assert_eq!(
            plan,
            OpenInEditorPlan::SpawnInPane {
                editor: "tfe".to_string(),
                path: "/tmp/foo.rs".to_string(),
                line: "1".to_string(),
            }
        );
    }

    #[test]
    fn plan_micro_yields_spawn_in_pane() {
        let c = chain(&["micro"]);
        let plan =
            plan_open_in_editor("/tmp/foo.rs:20", &c, ok_file, |_| Some("micro".to_string()));
        assert_eq!(
            plan,
            OpenInEditorPlan::SpawnInPane {
                editor: "micro".to_string(),
                path: "/tmp/foo.rs".to_string(),
                line: "20".to_string(),
            }
        );
    }

    #[test]
    fn resolve_returns_none_when_nothing_on_path() {
        let c = chain(&["$EDITOR", "code", "nvim"]);
        let resolved = resolve_editor_chain(&c, |_| false, |_| None);
        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_skips_empty_entries() {
        let c = chain(&["", "$VISUAL", "vim"]);
        let resolved = resolve_editor_chain(&c, |cmd| cmd == "vim", |_| None);
        assert_eq!(resolved.as_deref(), Some("vim"));
    }
}
