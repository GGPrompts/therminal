//! Editor invocation: open_in_editor, open_absolute_in_editor,
//! open_in_wsl_pane_editor.

use crate::window::App;

#[cfg(windows)]
use super::planner::shell_quote;
use super::planner::{
    OpenInEditorPlan, plan_open_in_editor, resolve_editor_chain, which_on_path,
};

impl App {
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

        tracing::info!(path = %path, line = %line, "open_in_wsl_pane_editor: spawning editor in new WSL pane");

        // In daemon mode the split is async — carry the command bytes in the
        // completion callback so they're written after the PTY is live.
        if self.is_daemon_mode() {
            use crate::window::pane_ops::DaemonSplitOnComplete;
            self.split_focused_pane_auto_with(DaemonSplitOnComplete::WriteBytesAndFocus {
                bytes: cmd.into_bytes(),
                toast: None,
            });
            return;
        }

        // Local mode: split is synchronous — write immediately.
        let original_focus = self.focused_pane();
        self.split_focused_pane_auto();
        let new_pane = match self.focused_pane() {
            Some(id) if Some(id) != original_focus => id,
            _ => {
                tracing::warn!("open_in_wsl_pane_editor: split did not produce a new pane");
                self.show_toast("failed to split pane for editor");
                return;
            }
        };

        self.pty_write_to_pane(cmd.as_bytes(), new_pane);
    }
}
