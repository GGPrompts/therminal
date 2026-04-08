//! Directory hotspot click handlers (tn-zqwg).
//!
//! Routes directory hotspots through a dedicated folder-open path instead
//! of the file `editor_chain` fallback. The primary action spawns the
//! configured `folder_pane_command` (default `tfe`) in a NEW pane whose
//! cwd is the clicked directory; the secondary action invokes the
//! `folder_opener` chain to reveal the directory in an external file
//! manager (Nautilus, Dolphin, Finder, Explorer, etc).
//!
//! The pane-spawn path is intentionally layered on top of
//! [`App::split_focused_pane`] + the existing `pty_write_to_pane` helper:
//!
//! 1. The focused pane is split (cwd inheritance is irrelevant — we
//!    immediately `cd` to the target directory).
//! 2. We pre-check that the first token of `folder_pane_command` resolves
//!    on `PATH`. If yes, we send `cd '/path' && exec <cmd> '/path' …\r`
//!    so the command replaces the shell and exiting the command closes
//!    the pane. If the binary is missing, we toast and send only `cd`
//!    so the user lands in a working shell at the right directory.
//!
//! This keeps `pane_ops.rs` and the existing split machinery untouched
//! while still satisfying "reuse the pane-split + cwd plumbing".

use std::process::Command;

use tracing::{debug, info, warn};

use crate::pane::SplitDirection;

use super::App;

// ── Pure planning ────────────────────────────────────────────────────────

/// Outcome of planning a folder-pane open. Decoupled from real IO so it
/// can be exhaustively unit tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FolderPaneOpenPlan {
    /// The first token of `folder_pane_command` resolved on `PATH`. The
    /// pane should be split, then `bytes` should be written to the new
    /// pane's PTY. `bytes` includes a leading `cd` to the target dir
    /// followed by `exec <cmd> [args…]\n`.
    SpawnCommand { cmd_display: String, bytes: Vec<u8> },
    /// The configured command's binary is missing on `PATH`. The pane
    /// should still be split and `bytes` (just the `cd`) should be
    /// written; the caller is also responsible for showing a "tfe not
    /// found — falling back to shell in folder" toast using
    /// `missing_binary` for the error message.
    FallbackShell {
        missing_binary: String,
        bytes: Vec<u8>,
    },
    /// The configuration disabled the in-pane spawn entirely
    /// (`folder_pane_command = []`). The caller should fall straight
    /// through to the file-manager chain.
    NoCommand,
}

/// Substitute the literal token `{path}` in each argument with `path`.
///
/// Tokens that don't appear are left untouched. Multiple `{path}`
/// tokens in a single argument are all replaced. Empty arguments are
/// preserved verbatim. Returns a fresh owned vec.
pub(crate) fn substitute_path_token(args: &[String], path: &str) -> Vec<String> {
    args.iter().map(|a| a.replace("{path}", path)).collect()
}

/// Plan the bytes to send into the freshly-split pane.
///
/// `command` is the configured `folder_pane_command` argv (the first
/// element is the binary, the rest are args, with `{path}` tokens
/// substituted later). `which_fn` is a `PATH` probe so callers can stub
/// the filesystem in tests.
pub(crate) fn plan_folder_pane_open<W>(
    path: &str,
    command: &[String],
    mut which_fn: W,
) -> FolderPaneOpenPlan
where
    W: FnMut(&str) -> bool,
{
    if command.is_empty() {
        return FolderPaneOpenPlan::NoCommand;
    }

    let head = command.first().map(String::as_str).unwrap_or("");
    if head.is_empty() {
        return FolderPaneOpenPlan::NoCommand;
    }

    let substituted = substitute_path_token(command, path);

    // Always start with `cd '/path' && clear`. Even on the fallback
    // branch this puts the user's shell in the right directory.
    let cd_clear = format!("cd {} && clear", shell_quote(path));

    if which_fn(head) {
        // Build `exec <head> [args…]` with shell-quoted arguments so
        // paths with spaces / special characters are passed verbatim.
        let mut exec_line = String::from("exec ");
        for (i, arg) in substituted.iter().enumerate() {
            if i > 0 {
                exec_line.push(' ');
            }
            exec_line.push_str(&shell_quote(arg));
        }
        let bytes = format!("{cd_clear} && {exec_line}\n").into_bytes();
        FolderPaneOpenPlan::SpawnCommand {
            cmd_display: substituted.join(" "),
            bytes,
        }
    } else {
        let bytes = format!("{cd_clear}\n").into_bytes();
        FolderPaneOpenPlan::FallbackShell {
            missing_binary: head.to_string(),
            bytes,
        }
    }
}

/// Outcome of planning a folder-opener (file manager) call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FolderOpenerPlan {
    /// Run `cmd` with `[path]` as the final argument.
    Spawn { cmd: String, args: Vec<String> },
    /// Nothing in the chain resolved — fall back to `open::that(path)`.
    OpenFallback,
}

/// Pick the first command from `chain` whose head token resolves on
/// `PATH`. `$VAR` tokens are expanded via `env_fn`.
pub(crate) fn plan_folder_opener<W, E>(
    chain: &[String],
    mut which_fn: W,
    mut env_fn: E,
) -> FolderOpenerPlan
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
        // Allow user to embed args in a single string ("nautilus --new-window").
        let head = cmd.split_whitespace().next().unwrap_or("");
        if head.is_empty() {
            continue;
        }
        if which_fn(head) {
            let parts: Vec<String> = cmd.split_whitespace().map(String::from).collect();
            let (cmd_token, extra_args) = parts.split_first().unwrap();
            return FolderOpenerPlan::Spawn {
                cmd: cmd_token.to_string(),
                args: extra_args.to_vec(),
            };
        }
    }
    FolderOpenerPlan::OpenFallback
}

// ── App-side dispatchers ────────────────────────────────────────────────

impl App {
    /// Spawn the configured `folder_pane_command` in a new pane rooted at
    /// `path`. Falls back to a plain shell in the directory (with a toast)
    /// when the binary is missing. Falls all the way through to the
    /// file-manager chain when `folder_pane_command` is empty.
    pub(crate) fn open_folder_in_pane(&mut self, path: &str) {
        let command = self.config.hotspots.folder_pane_command.clone();
        let plan = plan_folder_pane_open(path, &command, which_on_path);

        let (bytes, fallback_msg) = match plan {
            FolderPaneOpenPlan::SpawnCommand { cmd_display, bytes } => {
                info!(%path, %cmd_display, "open_folder_in_pane: spawning command");
                (bytes, None)
            }
            FolderPaneOpenPlan::FallbackShell {
                missing_binary,
                bytes,
            } => {
                let msg = format!("{missing_binary} not found — falling back to shell in folder");
                warn!(%path, %missing_binary, "open_folder_in_pane: command missing on PATH");
                (bytes, Some(msg))
            }
            FolderPaneOpenPlan::NoCommand => {
                debug!(
                    "open_folder_in_pane: folder_pane_command is empty, deferring to file manager"
                );
                self.open_folder_in_file_manager(path);
                return;
            }
        };

        // Capture the originally focused pane id so we can detect that the
        // split actually produced a new one.
        let original_focus = self.focused_pane();
        self.split_focused_pane(SplitDirection::Vertical);
        let new_pane = match self.focused_pane() {
            Some(id) if Some(id) != original_focus => id,
            _ => {
                warn!("open_folder_in_pane: split did not produce a new pane");
                return;
            }
        };

        if let Some(msg) = fallback_msg {
            self.show_toast(msg);
        }

        self.pty_write_to_pane(&bytes, new_pane);
    }

    /// Reveal `path` in an external file manager via the
    /// `folder_opener` chain. Final fallback is `open::that(path)` so
    /// the platform default (xdg-open / open / explorer) wins.
    pub(crate) fn open_folder_in_file_manager(&mut self, path: &str) {
        let chain = self.config.hotspots.folder_opener.clone();
        let plan = plan_folder_opener(&chain, which_on_path, |var| {
            std::env::var(var).ok().filter(|s| !s.is_empty())
        });

        match plan {
            FolderOpenerPlan::Spawn { cmd, args } => {
                let mut command = Command::new(&cmd);
                for arg in &args {
                    command.arg(arg);
                }
                command.arg(path);
                match command.spawn() {
                    Ok(_) => {
                        info!(%path, %cmd, "open_folder_in_file_manager: spawned");
                    }
                    Err(e) => {
                        warn!(%path, %cmd, "file manager spawn failed: {e}");
                        self.show_toast(format!("{cmd} failed to launch"));
                        // Last-ditch attempt via the platform `open` crate.
                        if let Err(e2) = open::that(path) {
                            warn!(%path, "open::that fallback also failed: {e2}");
                        }
                    }
                }
            }
            FolderOpenerPlan::OpenFallback => {
                if let Err(e) = open::that(path) {
                    warn!(%path, "no folder_opener entry resolved and open::that failed: {e}");
                    self.show_toast("no folder opener available");
                }
            }
        }
    }
}

// ── PATH probe + shell quoting ──────────────────────────────────────────

/// Cross-platform `PATH` probe. Mirrors the helper in `pane_ops.rs` but
/// kept private here so this module is self-contained.
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

/// Minimal POSIX shell single-quote escape. Wraps in `'…'` and escapes
/// embedded single quotes. Used for the `cd` and `exec` lines we send
/// into the freshly-spawned shell.
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

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── substitute_path_token ──

    #[test]
    fn substitute_replaces_single_token() {
        let result = substitute_path_token(&argv(&["tfe", "{path}"]), "/home/me");
        assert_eq!(result, vec!["tfe".to_string(), "/home/me".to_string()]);
    }

    #[test]
    fn substitute_leaves_other_args_alone() {
        let result = substitute_path_token(
            &argv(&["yazi", "--cwd", "{path}", "--theme", "dark"]),
            "/srv",
        );
        assert_eq!(
            result,
            vec![
                "yazi".to_string(),
                "--cwd".to_string(),
                "/srv".to_string(),
                "--theme".to_string(),
                "dark".to_string(),
            ]
        );
    }

    #[test]
    fn substitute_replaces_multiple_tokens_in_one_arg() {
        let result = substitute_path_token(&argv(&["echo", "{path}/sub:{path}"]), "/a");
        assert_eq!(result, vec!["echo".to_string(), "/a/sub:/a".to_string()]);
    }

    #[test]
    fn substitute_handles_no_token() {
        let result = substitute_path_token(&argv(&["broot"]), "/tmp");
        assert_eq!(result, vec!["broot".to_string()]);
    }

    #[test]
    fn substitute_handles_paths_with_spaces() {
        let result = substitute_path_token(&argv(&["tfe", "{path}"]), "/home/me/My Projects/foo");
        assert_eq!(
            result,
            vec!["tfe".to_string(), "/home/me/My Projects/foo".to_string()]
        );
    }

    // ── plan_folder_pane_open ──

    #[test]
    fn plan_empty_command_yields_no_command() {
        let plan = plan_folder_pane_open("/tmp", &[], |_| true);
        assert_eq!(plan, FolderPaneOpenPlan::NoCommand);
    }

    #[test]
    fn plan_empty_head_yields_no_command() {
        let plan = plan_folder_pane_open("/tmp", &argv(&["", "{path}"]), |_| true);
        assert_eq!(plan, FolderPaneOpenPlan::NoCommand);
    }

    #[test]
    fn plan_resolved_binary_emits_exec_line() {
        let plan =
            plan_folder_pane_open("/srv/data", &argv(&["tfe", "{path}"]), |head| head == "tfe");
        match plan {
            FolderPaneOpenPlan::SpawnCommand { cmd_display, bytes } => {
                assert_eq!(cmd_display, "tfe /srv/data");
                let s = String::from_utf8(bytes).unwrap();
                assert!(s.starts_with("cd '/srv/data' && clear && exec 'tfe' '/srv/data'"));
                assert!(s.ends_with('\n'));
            }
            other => panic!("expected SpawnCommand, got {other:?}"),
        }
    }

    #[test]
    fn plan_missing_binary_emits_cd_only_with_message() {
        let plan = plan_folder_pane_open("/srv/data", &argv(&["tfe", "{path}"]), |_| false);
        match plan {
            FolderPaneOpenPlan::FallbackShell {
                missing_binary,
                bytes,
            } => {
                assert_eq!(missing_binary, "tfe");
                let s = String::from_utf8(bytes).unwrap();
                assert_eq!(s, "cd '/srv/data' && clear\n");
            }
            other => panic!("expected FallbackShell, got {other:?}"),
        }
    }

    #[test]
    fn plan_quotes_path_with_spaces_correctly() {
        let plan = plan_folder_pane_open("/home/me/My Stuff", &argv(&["yazi", "{path}"]), |_| true);
        match plan {
            FolderPaneOpenPlan::SpawnCommand { bytes, .. } => {
                let s = String::from_utf8(bytes).unwrap();
                assert!(s.contains("cd '/home/me/My Stuff'"));
                assert!(s.contains("'yazi' '/home/me/My Stuff'"));
            }
            other => panic!("expected SpawnCommand, got {other:?}"),
        }
    }

    #[test]
    fn plan_escapes_embedded_single_quote_in_path() {
        let plan = plan_folder_pane_open("/tmp/it's-fine", &argv(&["tfe", "{path}"]), |_| true);
        match plan {
            FolderPaneOpenPlan::SpawnCommand { bytes, .. } => {
                let s = String::from_utf8(bytes).unwrap();
                // POSIX-safe escape: '...'\''...' (close, escaped quote, reopen).
                assert!(s.contains(r#"'/tmp/it'\''s-fine'"#));
            }
            other => panic!("expected SpawnCommand, got {other:?}"),
        }
    }

    #[test]
    fn plan_extra_args_passed_through() {
        let plan = plan_folder_pane_open(
            "/srv",
            &argv(&["yazi", "--theme", "dark", "{path}"]),
            |_| true,
        );
        match plan {
            FolderPaneOpenPlan::SpawnCommand { cmd_display, bytes } => {
                assert_eq!(cmd_display, "yazi --theme dark /srv");
                let s = String::from_utf8(bytes).unwrap();
                assert!(s.contains("'yazi' '--theme' 'dark' '/srv'"));
            }
            other => panic!("expected SpawnCommand, got {other:?}"),
        }
    }

    // ── plan_folder_opener ──

    #[test]
    fn opener_picks_first_resolved_entry() {
        let chain = argv(&["nautilus", "dolphin", "xdg-open"]);
        let plan = plan_folder_opener(&chain, |c| c == "dolphin", |_| None);
        assert_eq!(
            plan,
            FolderOpenerPlan::Spawn {
                cmd: "dolphin".to_string(),
                args: vec![]
            }
        );
    }

    #[test]
    fn opener_falls_back_when_chain_misses() {
        let chain = argv(&["nautilus", "dolphin"]);
        let plan = plan_folder_opener(&chain, |_| false, |_| None);
        assert_eq!(plan, FolderOpenerPlan::OpenFallback);
    }

    #[test]
    fn opener_expands_env_token() {
        let chain = argv(&["$FILE_MANAGER", "xdg-open"]);
        let plan = plan_folder_opener(
            &chain,
            |c| c == "ranger",
            |var| {
                if var == "FILE_MANAGER" {
                    Some("ranger".to_string())
                } else {
                    None
                }
            },
        );
        assert_eq!(
            plan,
            FolderOpenerPlan::Spawn {
                cmd: "ranger".to_string(),
                args: vec![]
            }
        );
    }

    #[test]
    fn opener_skips_unset_env_token() {
        let chain = argv(&["$FILE_MANAGER", "nautilus"]);
        let plan = plan_folder_opener(&chain, |c| c == "nautilus", |_| None);
        assert_eq!(
            plan,
            FolderOpenerPlan::Spawn {
                cmd: "nautilus".to_string(),
                args: vec![]
            }
        );
    }

    #[test]
    fn opener_preserves_inline_args() {
        let chain = argv(&["nautilus --new-window"]);
        let plan = plan_folder_opener(&chain, |c| c == "nautilus", |_| None);
        assert_eq!(
            plan,
            FolderOpenerPlan::Spawn {
                cmd: "nautilus".to_string(),
                args: vec!["--new-window".to_string()],
            }
        );
    }

    #[test]
    fn opener_skips_empty_entries() {
        let chain = argv(&["", "xdg-open"]);
        let plan = plan_folder_opener(&chain, |c| c == "xdg-open", |_| None);
        assert_eq!(
            plan,
            FolderOpenerPlan::Spawn {
                cmd: "xdg-open".to_string(),
                args: vec![],
            }
        );
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
