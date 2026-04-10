//! Editor and clipboard operations: copy_selection, paste_clipboard,
//! clear_selection, open_in_editor, open_absolute_in_editor,
//! open_in_wsl_pane_editor, plan_open_in_editor, resolve_editor_chain,
//! which_on_path, shell_quote.

use crate::window::App;

use super::normalize_paste_text;

impl App {
    // ── Clipboard operations ───────────────────────────────────────────

    /// Copy the current selection to the clipboard (for Ctrl+Shift+C keybinding).
    pub(crate) fn copy_selection(&mut self) {
        let pane_id = match self.selection_pane.or(self.focused_pane()) {
            Some(id) => id,
            None => return,
        };
        let layout = match self.get_layout() {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane(pane_id) {
            Some(p) => p,
            None => return,
        };
        if let Some(term) = pane.backend.term() {
            let term_guard = term.lock();
            if let Some(text) = term_guard.selection_to_string()
                && !text.is_empty()
            {
                crate::clipboard::copy_to_clipboard(&text);
            }
        }
    }

    /// Paste clipboard contents to the focused pane's PTY.
    ///
    /// Always wraps the payload with the bracketed-paste envelope
    /// (`\e[200~` ... `\e[201~`) regardless of the locally-tracked
    /// `TermMode::BRACKETED_PASTE` flag. The local flag is unreliable in
    /// daemon-client mode (tn-b77d): the GUI's `Term` is bootstrapped from
    /// a one-shot snapshot and never sees subsequent `\e[?2004h` mode-set
    /// sequences emitted by TUIs after attach. Modern TUIs (Claude Code,
    /// vim, helix, less, micro, fish) handle the envelope correctly even
    /// when they didn't request it; legacy line editors that don't
    /// recognize the markers display them as harmless garbage at worst,
    /// which is strictly better than the current bug where every embedded
    /// `\n` is interpreted as Enter and submits per line.
    ///
    /// Clipboard text is also normalized: `\r\n` and bare `\r` collapse to
    /// `\n` to avoid TUIs treating CR as a submit (common when pasting
    /// from Windows-origin clipboards on WSL2).
    ///
    /// See tn-5akk (the paste symptom) and tn-b77d (the underlying
    /// mode-flag drift in tn-382v Phase B).
    pub(crate) fn paste_clipboard(&mut self) {
        let raw = crate::clipboard::paste_from_clipboard();
        if raw.is_empty() {
            return;
        }
        let text = normalize_paste_text(&raw);
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        let layout = match self.get_layout_mut() {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane_mut(focused) {
            Some(p) => p,
            None => return,
        };
        if let Err(e) = pane.write_input(b"\x1b[200~") {
            tracing::warn!("paste write failed: {e}");
        }
        if let Err(e) = pane.write_input(text.as_bytes()) {
            tracing::warn!("paste write failed: {e}");
        }
        if let Err(e) = pane.write_input(b"\x1b[201~") {
            tracing::warn!("paste write failed: {e}");
        }
    }

    /// Open a file path in the user's `$EDITOR` or via `xdg-open` / `open`.
    ///
    /// The path may include `:line` or `:line:col` suffixes. If `$EDITOR` supports
    /// `+line` syntax (vim, nvim, nano, code, etc.), we pass it; otherwise we
    /// fall back to `xdg-open` / `open` with just the file path.
    pub(crate) fn open_in_editor(&mut self, path_with_loc: &str) {
        use std::process::Command;

        // tn-q8ce: on native Windows with a WSL pane, Linux-shaped
        // hotspot paths (`~/foo.rs`, `/home/marci/foo.rs`,
        // `./src/main.rs`) must never be touched by the Windows host:
        // - Expanding `~` via `std::env::var("HOME")` would use the
        //   Windows user profile (`C:\Users\<user>`), not the WSL
        //   `/home/<user>` the hotspot actually references.
        // - Joining relative paths against the focused pane's WSL cwd
        //   via `resolve_relative_to_cwd` is correct, but then
        //   handing off to a Windows editor via `Command::new` can't
        //   open a `/home/...` path.
        //
        // Detect the WSL-destined shape from the **focused pane's
        // cwd** (a POSIX-absolute cwd unambiguously identifies a WSL
        // shell — Windows shells never emit that shape) BEFORE any
        // host-side manipulation, and route the **raw** hotspot text
        // into a new WSL pane. `open_in_wsl_pane_editor` writes a
        // shell one-liner into the new pane's PTY so `~`, `$EDITOR`,
        // relative paths, and everything else expand in the WSL
        // shell, not the Windows host.
        let cwd = self.focused_pane_cwd();
        #[cfg(windows)]
        if crate::window::wsl_paths::is_wsl_pane_path(cwd.as_deref(), path_with_loc) {
            self.open_in_wsl_pane_editor(path_with_loc, cwd.as_deref());
            return;
        }

        // Expand `~/…` before the pre-flight stat in `plan_open_in_editor`
        // — otherwise `~/foo.rs:42` stat's as a literal `~/foo.rs` and
        // the hotspot silently fails the "is a regular file" check.
        let home = crate::window::platform_home_dir();
        let expanded =
            therminal_terminal::hotspot_detection::expand_tilde(path_with_loc, home.as_deref());
        // tn-vm2j: join relative paths against the focused pane's
        // OSC 7 shell cwd. A raw `./src/main.rs:42` from shell output
        // would otherwise stat as "not a regular file" and silently
        // fail the editor hand-off.
        let resolved = therminal_terminal::hotspot_detection::resolve_relative_to_cwd(
            expanded.as_ref(),
            cwd.as_deref(),
        );
        let path_with_loc = resolved.as_ref();

        let chain = self.config.hotspots.editor_chain.clone();
        let outcome = plan_open_in_editor(
            path_with_loc,
            &chain,
            |p| std::fs::metadata(p).map(|m| m.is_file()),
            |c| resolve_editor_chain(c, which_on_path, |var| std::env::var(var).ok()),
        );

        match outcome {
            OpenInEditorPlan::Spawn { editor, path, line } => {
                let arg = format!("+{line}");
                match Command::new(&editor).arg(&arg).arg(&path).spawn() {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("failed to launch editor ({editor}): {e}");
                        self.show_toast(format!("$EDITOR ({editor}) failed to launch"));
                    }
                }
            }
            OpenInEditorPlan::OpenFallback { path } => {
                if let Err(e) = open::that(&path) {
                    tracing::warn!(
                        "open_in_editor: no editor from chain {:?} resolved, and fallback failed: {e}",
                        chain
                    );
                    self.show_toast("no $EDITOR set, and fallback failed");
                }
            }
            OpenInEditorPlan::Fail(msg) => {
                tracing::warn!("open_in_editor: {msg}");
                self.show_toast(msg);
            }
        }
    }

    /// Open a known-absolute file directly in the user's configured
    /// editor, bypassing the hotspot cwd-resolution machinery.
    ///
    /// `open_in_editor` runs every path through
    /// [`resolve_relative_to_cwd`], which joins any
    /// not-starting-with-`/` path against the focused pane's shell cwd.
    /// On Windows with a WSL focused pane, that turns
    /// `C:\Users\marci\AppData\Roaming\therminal\therminal.toml` into
    /// `/home/marci/.../C:\Users\marci\...\therminal.toml` — nonsense.
    ///
    /// Instead, take the path verbatim, try the `editor_chain` resolver
    /// (so `$EDITOR` still wins), and spawn the editor directly. Falls
    /// back to `open::that` on any failure so at minimum the platform
    /// default handler gets a crack at it. Retained for direct open
    /// paths even though the CSD settings button now opens the in-app
    /// settings overlay.
    #[allow(dead_code)]
    pub(crate) fn open_absolute_in_editor(&mut self, path: &std::path::Path) {
        use std::process::Command;

        let chain = self.config.hotspots.editor_chain.clone();
        let editor = resolve_editor_chain(&chain, which_on_path, |var| std::env::var(var).ok());

        if let Some(editor) = editor {
            match Command::new(&editor).arg(path).spawn() {
                Ok(_) => return,
                Err(e) => {
                    tracing::warn!(
                        "open_absolute_in_editor: editor {editor} failed on {}: {e}",
                        path.display()
                    );
                    self.show_toast(format!("$EDITOR ({editor}) failed to launch"));
                    // Fall through to open::that as a last resort.
                }
            }
        }

        if let Err(e) = open::that(path) {
            tracing::warn!(
                "open_absolute_in_editor: fallback open::that failed on {}: {e}",
                path.display()
            );
            self.show_toast(format!("could not open {}", path.display()));
        }
    }

    /// tn-q8ce: open a Linux-style hotspot path inside a WSL pane on
    /// native Windows.
    ///
    /// Splits the focused pane and writes a shell command into the new
    /// PTY that `cd`'s to the containing directory and `exec`'s the
    /// user's Linux `$EDITOR` (with `$VISUAL` and `nvim` fallbacks) on
    /// the file. Line / column suffixes in the hotspot string are
    /// translated to `+<line>` so vim-family editors jump to the right
    /// location.
    ///
    /// This intentionally bypasses the `editor_chain` + `plan_open_in_editor`
    /// path because that path tries to launch a **Windows** process with
    /// `Command::new(editor).spawn()`, which can't see the Linux
    /// filesystem and can't run nvim/helix/etc. from inside WSL.
    /// Running the editor inside the pane is the right UX — the user
    /// gets their actual Linux editor on the actual Linux path.
    #[cfg(windows)]
    fn open_in_wsl_pane_editor(&mut self, path_with_loc: &str, pane_cwd: Option<&str>) {
        // Split off `:line[:col]` so we can render it as `+<line>`.
        // The hotspot suffix is always plain digits separated by `:`.
        let (path, line) = match path_with_loc.find(':') {
            Some(idx) if path_with_loc[idx + 1..].starts_with(|c: char| c.is_ascii_digit()) => {
                let rest = &path_with_loc[idx + 1..];
                let line_str = rest.split(':').next().unwrap_or("1");
                (&path_with_loc[..idx], line_str)
            }
            _ => (path_with_loc, "1"),
        };

        // Build a shell one-liner that runs entirely inside the WSL
        // pane. The shell — not the Windows host — handles:
        //   - `~` expansion via the Linux `$HOME`
        //   - relative path resolution against `$PWD`
        //   - `$EDITOR` / `$VISUAL` lookup from the Linux environment
        //   - file I/O on the Linux filesystem
        //
        // We `cd` to the originating pane's cwd first so relative
        // paths (`./src/main.rs`, `../Cargo.toml`) resolve correctly.
        // `${EDITOR:-${VISUAL:-nvim}}` gives the user's configured
        // editor with a sensible default. `+<line>` is the universal
        // vi/vim/nvim jump syntax.
        //
        // The path is intentionally NOT shell-quoted with single
        // quotes — that would prevent `~` expansion. Instead we rely
        // on the hotspot text not containing shell metacharacters
        // (the file-path regex in `hotspot_detection.rs` only matches
        // `[A-Za-z0-9_\-./]` bodies, which are all shell-safe). If a
        // future regex widens the charset we'll need a smarter
        // escaper that expands tildes and quotes the rest.
        let mut cmd = String::new();
        if let Some(cwd) = pane_cwd {
            cmd.push_str("cd ");
            cmd.push_str(&shell_quote(cwd));
            cmd.push_str(" && ");
        }
        cmd.push_str("clear && exec ${EDITOR:-${VISUAL:-nvim}} ");
        cmd.push_str(path);
        cmd.push_str(" +");
        cmd.push_str(line);
        cmd.push('\n');

        // Capture original focus so we can detect split failure.
        let original_focus = self.focused_pane();
        self.split_focused_pane(crate::pane::SplitDirection::Vertical);
        let new_pane = match self.focused_pane() {
            Some(id) if Some(id) != original_focus => id,
            _ => {
                tracing::warn!("open_in_wsl_pane_editor: split did not produce a new pane");
                self.show_toast("failed to split pane for editor");
                return;
            }
        };

        tracing::info!(path = %path, line = %line, "open_in_wsl_pane_editor: spawning editor in new WSL pane");
        self.pty_write_to_pane(cmd.as_bytes(), new_pane);
    }

    /// Clear the active selection on all panes.
    pub(crate) fn clear_selection(&mut self) {
        if let Some(pane_id) = self.selection_pane.take()
            && let Some(layout) = self.get_layout_mut()
            && let Some(pane) = layout.find_pane_mut(pane_id)
            && let Some(term) = pane.backend.term()
        {
            term.lock().selection = None;
        }
        self.selection_in_progress = false;
    }
}

/// Outcome of planning an `open_in_editor` call, decoupled from the real
/// filesystem / process spawn so it can be unit tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenInEditorPlan {
    /// Spawn `editor +line path`.
    Spawn {
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

/// Pure planner for [`App::open_in_editor`]. Separates path parsing,
/// filesystem validation, and editor resolution from actual IO so each
/// failure branch can be asserted from a unit test.
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
        Some(editor) => OpenInEditorPlan::Spawn {
            editor,
            path: path.to_string(),
            line: line.to_string(),
        },
        None => OpenInEditorPlan::OpenFallback {
            path: path.to_string(),
        },
    }
}

/// Resolve an entry from the editor fallback chain to a concrete command.
///
/// Each entry is either a literal command (e.g. `"nvim"`) or an env-var
/// token (`"$EDITOR"`, `"$VISUAL"`). Literal commands are probed via the
/// supplied `which_fn`; env-var tokens are first expanded via `env_fn`
/// and the result is then probed. The first successful resolution wins.
/// Returns `None` if every entry fails.
///
/// This is factored out of [`App::open_in_editor`] so it can be unit
/// tested without touching the real filesystem or environment.
fn resolve_editor_chain<W, E>(chain: &[String], mut which_fn: W, mut env_fn: E) -> Option<String>
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
fn which_on_path(cmd: &str) -> bool {
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
pub(super) fn shell_quote(s: &str) -> String {
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
    fn plan_happy_path_yields_spawn_with_line() {
        let c = chain(&["nvim"]);
        let plan = plan_open_in_editor("/tmp/foo.rs:42:7", &c, ok_file, |_| {
            Some("nvim".to_string())
        });
        assert_eq!(
            plan,
            OpenInEditorPlan::Spawn {
                editor: "nvim".to_string(),
                path: "/tmp/foo.rs".to_string(),
                line: "42".to_string(),
            }
        );
    }

    #[test]
    fn plan_no_line_defaults_to_1() {
        let c = chain(&["vim"]);
        let plan = plan_open_in_editor("/tmp/bar.md", &c, ok_file, |_| Some("vim".to_string()));
        match plan {
            OpenInEditorPlan::Spawn { line, .. } => assert_eq!(line, "1"),
            other => panic!("expected Spawn, got {other:?}"),
        }
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
