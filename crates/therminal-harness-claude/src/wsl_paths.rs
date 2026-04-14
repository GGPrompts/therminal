//! WSL path resolution for the Claude harness on Windows native (tn-966s).
//!
//! When `therminal-daemon` runs as a **native Windows** process but Claude
//! Code runs inside WSL2, the harness's two filesystem inputs live on a
//! filesystem the Windows host normally cannot reach by their Linux paths:
//!
//! - State files at `/tmp/claude-code-state/*.json`
//! - JSONL transcripts under `~/.claude/projects/{hash}/...`
//!
//! Windows 10 21H2+ and Windows 11 expose the WSL filesystem through the
//! `\\wsl.localhost\<distro>\...` UNC namespace (older builds use the
//! `\\wsl$\<distro>\...` prefix as a symlink for compat). The `notify`
//! crate's recommended OS-native watcher works over this UNC path because
//! the WSL provider surfaces it as a regular Windows filesystem; the
//! ChangeNotification handles attached to a `\\wsl.localhost\Ubuntu\tmp`
//! directory fire whenever the underlying inotify event lands.
//!
//! This module provides the **Windows-only** machinery the harness needs
//! to discover the active distro, the Linux user's `$HOME`, and to
//! resolve `/tmp/claude-code-state` and `~/.claude/projects` to UNC
//! paths the daemon can hand to `ClaudeStatePoller::with_dirs` and
//! `ClaudeJsonlRegistry`.
//!
//! # Topology matrix
//!
//! | Daemon host          | Claude host | Source path                          | Used path                                          |
//! |----------------------|-------------|--------------------------------------|----------------------------------------------------|
//! | Linux native         | Linux       | `/tmp/claude-code-state`             | `/tmp/claude-code-state`                           |
//! | WSL2                 | WSL2        | `/tmp/claude-code-state`             | `/tmp/claude-code-state`                           |
//! | Windows native       | WSL2        | `/tmp/claude-code-state`             | `\\wsl.localhost\<distro>\tmp\claude-code-state`   |
//! | Windows native       | Windows     | `%TEMP%\claude-code-state`           | `%TEMP%\claude-code-state` (existing fallback)     |
//!
//! Only the third row is new. Rows 1 and 2 use the regular Linux path;
//! row 4 keeps the existing Windows-temp fallback in `state.rs`.
//!
//! # Pure vs impure
//!
//! - [`linux_to_unc`] is a pure path function — no I/O, no env, no probe.
//!   Fully unit-tested on every platform.
//! - [`expand_home_to_unc`] is pure (takes the WSL `$HOME` as input).
//! - [`detect_default_distro`] and [`detect_wsl_home`] shell out to
//!   `wsl.exe` exactly **once per process** via `OnceLock` caches. They
//!   are no-ops on non-Windows builds (compile-time `None`).
//! - [`claude_state_dir_unc`], [`codex_state_dir_unc`],
//!   [`copilot_state_dir_unc`], [`claude_projects_dir_unc`] are the
//!   top-level entry points the rest of the crate uses. They return
//!   `None` whenever WSL probing fails so the caller can fall back to
//!   its existing path.
//!
//! # Distinct from `therminal-app::window::wsl_paths`
//!
//! `therminal-app/src/window/wsl_paths.rs` solves a related-but-distinct
//! problem: translating a Linux path that **the user clicked** in a WSL
//! pane into a UNC path so a Windows file manager can open it. That
//! module lives next to the click handler and depends on per-pane cwd
//! signal. This module is the **daemon-side** equivalent: it answers
//! "where do I point the file watcher and JSONL tailer when the daemon
//! is on Windows but Claude is in WSL?" and depends on no per-pane
//! state. The two modules deliberately do not share code so the harness
//! crate stays free of an `therminal-app` dependency.

use std::path::{Path, PathBuf};

/// Build a UNC path that points at a Linux absolute path inside the
/// named WSL distribution.
///
/// Returns `None` for inputs that aren't a Linux-shaped absolute path,
/// or when `distro` is empty. Pure — no filesystem, no env probing.
///
/// ```rust,ignore
/// use therminal_harness_claude::wsl_paths::linux_to_unc;
/// let p = linux_to_unc("Ubuntu", "/tmp/claude-code-state").unwrap();
/// assert_eq!(p.to_string_lossy(), r"\\wsl.localhost\Ubuntu\tmp\claude-code-state");
/// ```
pub fn linux_to_unc(distro: &str, linux_path: &str) -> Option<PathBuf> {
    if distro.is_empty() {
        return None;
    }
    if linux_path.is_empty() || !linux_path.starts_with('/') {
        return None;
    }
    // Reject leading `//` ambiguities (pseudo-UNC, Cygwin, typo).
    if linux_path.starts_with("//") {
        return None;
    }

    // Strip the leading `/` and flip remaining slashes to backslashes
    // so the produced PathBuf is a clean Windows-shaped UNC.
    let body = &linux_path[1..];
    let body_back = body.replace('/', "\\");

    let mut s = String::with_capacity(body_back.len() + distro.len() + 20);
    s.push_str(r"\\wsl.localhost\");
    s.push_str(distro);
    if !body_back.is_empty() {
        s.push('\\');
        s.push_str(&body_back);
    }
    Some(PathBuf::from(s))
}

/// Resolve `~/<rest>` against a Linux `$HOME` and return the
/// corresponding UNC path inside `distro`. Pure.
///
/// `home` must be a Linux absolute path (typically the result of
/// `wsl.exe -e sh -c 'printf %s "$HOME"'`). If `path` does not start
/// with `~` it is treated as already-absolute and forwarded to
/// [`linux_to_unc`].
pub fn expand_home_to_unc(distro: &str, home: &str, path: &str) -> Option<PathBuf> {
    if distro.is_empty() || home.is_empty() {
        return None;
    }
    let resolved = if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        let trimmed = home.trim_end_matches('/');
        format!("{trimmed}/{rest}")
    } else {
        path.to_string()
    };
    linux_to_unc(distro, &resolved)
}

// ── Windows-only: distro + WSL $HOME detection ─────────────────────────────

/// Cached default WSL distribution name.
///
/// Populated on first call to [`detect_default_distro`] via
/// `wsl.exe -l -q`. The outer `Option` distinguishes "not yet probed"
/// from "probed and absent". On non-Windows builds the cache is unused
/// and the function returns a compile-time `None`.
#[cfg(windows)]
static DEFAULT_DISTRO: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Cached Linux `$HOME` from the default WSL distribution.
///
/// Populated on first call to [`detect_wsl_home`] via
/// `wsl.exe -e sh -c 'printf %s "$HOME"'`. `None` if the probe failed.
#[cfg(windows)]
static WSL_HOME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Return the name of the user's default WSL distribution, or `None`
/// if WSL is not installed / not detectable.
///
/// Caches the first result for the lifetime of the process. Compile-
/// time `None` on non-Windows targets.
pub fn detect_default_distro() -> Option<String> {
    #[cfg(windows)]
    {
        DEFAULT_DISTRO
            .get_or_init(|| {
                use std::process::Command;
                // `wsl.exe -l -q` lists distro names (quiet). The first
                // line is the default. Output is UTF-16 LE on Windows;
                // strip embedded NULs before UTF-8 decoding to handle
                // ASCII-named distros (Ubuntu, Debian, kali-linux,
                // openSUSE-Tumbleweed) without pulling in a UTF-16
                // dependency. Non-ASCII distro names are rare in
                // practice and would round-trip through any
                // installation-specific charset; we accept the simple
                // path here and revisit if a real user hits it.
                let output = Command::new("wsl.exe").args(["-l", "-q"]).output().ok()?;
                if !output.status.success() {
                    tracing::debug!(
                        status = ?output.status,
                        stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                        "wsl_paths: `wsl.exe -l -q` failed, no distro detected"
                    );
                    return None;
                }
                // Strip the UTF-16 LE BOM (0xFF 0xFE) if present before
                // NUL-stripping. `wsl.exe -l -q` on Windows always
                // outputs UTF-16 LE with a BOM when captured via pipe;
                // the BOM bytes survive the NUL filter and end up as
                // U+FFFD replacement characters after `from_utf8_lossy`,
                // which causes `is_safe_distro_name` to reject an
                // otherwise valid distro name like "Ubuntu-24.04".
                let raw = &output.stdout;
                let raw = if raw.starts_with(&[0xFF, 0xFE]) {
                    &raw[2..]
                } else {
                    raw
                };
                let cleaned: Vec<u8> = raw.iter().copied().filter(|&b| b != 0).collect();
                let s = String::from_utf8_lossy(&cleaned);
                let first = s.lines().map(|l| l.trim()).find(|l| !l.is_empty())?;
                if first.is_empty() {
                    None
                } else if !is_safe_distro_name(first) {
                    // Defense-in-depth: reject anything outside the
                    // installer-enforced distro name charset before
                    // splicing it into a UNC path string. A tampered
                    // `wsl.exe` or malformed output could otherwise
                    // return something like `..\..\Windows\System32`
                    // which would escape the UNC root.
                    tracing::warn!(
                        distro = %first,
                        "wsl_paths: rejecting unsafe distro name from `wsl.exe -l -q`"
                    );
                    None
                } else {
                    tracing::info!(distro = %first, "wsl_paths: detected default WSL distro");
                    Some(first.to_string())
                }
            })
            .clone()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Return `true` when `name` only contains characters allowed in a
/// WSL distribution name. Real-world WSL distro names are constrained
/// by the installer to alphanumeric + hyphen + dot + underscore; this
/// allowlist is a defense-in-depth check against a tampered `wsl.exe`
/// returning a path-traversal payload that would escape the
/// `\\wsl.localhost\<distro>\...` UNC root when spliced into
/// [`linux_to_unc`].
#[cfg(any(windows, test))]
fn is_safe_distro_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_')
}

/// Return the Linux `$HOME` of the default WSL distribution, or `None`
/// when we can't detect it.
///
/// Cached for the lifetime of the process. Compile-time `None` on
/// non-Windows targets.
pub fn detect_wsl_home() -> Option<String> {
    #[cfg(windows)]
    {
        WSL_HOME
            .get_or_init(|| {
                use std::process::Command;
                let output = Command::new("wsl.exe")
                    .args(["-e", "sh", "-c", r#"printf %s "$HOME""#])
                    .output()
                    .ok()?;
                if !output.status.success() {
                    tracing::debug!(
                        status = ?output.status,
                        stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                        "wsl_paths: `wsl.exe -e sh -c printf $HOME` failed"
                    );
                    return None;
                }
                let cleaned: Vec<u8> = output.stdout.into_iter().filter(|&b| b != 0).collect();
                let s = String::from_utf8_lossy(&cleaned).trim().to_string();
                if s.is_empty() || !s.starts_with('/') {
                    None
                } else {
                    tracing::info!(home = %s, "wsl_paths: detected WSL $HOME");
                    Some(s)
                }
            })
            .clone()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

// ── Top-level path resolvers ───────────────────────────────────────────────

/// Return the UNC path for `/tmp/claude-code-state` inside the user's
/// default WSL distribution, or `None` on non-Windows / WSL absent /
/// distro detection failure.
///
/// The caller (`state.rs::default_state_dir`) treats `None` as
/// "fall back to the existing platform default" so this function is
/// safe to call unconditionally.
pub fn claude_state_dir_unc() -> Option<PathBuf> {
    state_dir_unc("claude-code-state")
}

/// UNC path for `/tmp/codex-state` inside the default WSL distro.
pub fn codex_state_dir_unc() -> Option<PathBuf> {
    state_dir_unc("codex-state")
}

/// UNC path for `/tmp/copilot-state` inside the default WSL distro.
pub fn copilot_state_dir_unc() -> Option<PathBuf> {
    state_dir_unc("copilot-state")
}

/// Return the UNC path for `~/.claude/projects` inside the default
/// WSL distro. Used by the JSONL tailer to discover session
/// transcripts and subagent sidechain files.
pub fn claude_projects_dir_unc() -> Option<PathBuf> {
    let distro = detect_default_distro()?;
    let home = detect_wsl_home()?;
    expand_home_to_unc(&distro, &home, "~/.claude/projects")
}

pub(crate) fn state_dir_unc(name: &str) -> Option<PathBuf> {
    let distro = detect_default_distro()?;
    let linux_path = format!("/tmp/{name}");
    linux_to_unc(&distro, &linux_path)
}

/// Return `true` when `path` looks like a UNC path that should be
/// treated as the WSL virtual filesystem (used by tracing breadcrumbs
/// and to gate WSL-specific best-effort behavior).
pub fn is_wsl_unc_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.starts_with(r"\\wsl.localhost\") || s.starts_with(r"\\wsl$\")
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_to_unc_simple_path() {
        let p = linux_to_unc("Ubuntu", "/tmp/claude-code-state").unwrap();
        assert_eq!(
            p.to_string_lossy(),
            r"\\wsl.localhost\Ubuntu\tmp\claude-code-state"
        );
    }

    #[test]
    fn linux_to_unc_nested_path() {
        let p = linux_to_unc("Ubuntu", "/home/marci/.claude/projects").unwrap();
        assert_eq!(
            p.to_string_lossy(),
            r"\\wsl.localhost\Ubuntu\home\marci\.claude\projects"
        );
    }

    #[test]
    fn linux_to_unc_root_only() {
        let p = linux_to_unc("Ubuntu", "/").unwrap();
        assert_eq!(p.to_string_lossy(), r"\\wsl.localhost\Ubuntu");
    }

    #[test]
    fn linux_to_unc_distro_with_dash() {
        let p = linux_to_unc("kali-linux", "/etc/hosts").unwrap();
        assert_eq!(p.to_string_lossy(), r"\\wsl.localhost\kali-linux\etc\hosts");
    }

    #[test]
    fn linux_to_unc_rejects_relative() {
        assert!(linux_to_unc("Ubuntu", "tmp/foo").is_none());
        assert!(linux_to_unc("Ubuntu", "./foo").is_none());
        assert!(linux_to_unc("Ubuntu", "../foo").is_none());
        assert!(linux_to_unc("Ubuntu", "~/foo").is_none());
    }

    #[test]
    fn linux_to_unc_rejects_empty_distro() {
        assert!(linux_to_unc("", "/tmp/foo").is_none());
    }

    #[test]
    fn linux_to_unc_rejects_double_slash() {
        assert!(linux_to_unc("Ubuntu", "//foo/bar").is_none());
    }

    #[test]
    fn linux_to_unc_rejects_empty_path() {
        assert!(linux_to_unc("Ubuntu", "").is_none());
    }

    #[test]
    fn expand_home_to_unc_with_tilde_slash() {
        let p = expand_home_to_unc("Ubuntu", "/home/marci", "~/.claude/projects").unwrap();
        assert_eq!(
            p.to_string_lossy(),
            r"\\wsl.localhost\Ubuntu\home\marci\.claude\projects"
        );
    }

    #[test]
    fn expand_home_to_unc_bare_tilde() {
        let p = expand_home_to_unc("Ubuntu", "/home/marci", "~").unwrap();
        assert_eq!(p.to_string_lossy(), r"\\wsl.localhost\Ubuntu\home\marci");
    }

    #[test]
    fn expand_home_to_unc_already_absolute() {
        // Non-tilde-prefixed paths flow straight through linux_to_unc.
        let p = expand_home_to_unc("Ubuntu", "/home/marci", "/tmp/foo").unwrap();
        assert_eq!(p.to_string_lossy(), r"\\wsl.localhost\Ubuntu\tmp\foo");
    }

    #[test]
    fn expand_home_to_unc_trailing_slash_in_home() {
        // A `$HOME` like `/home/marci/` should not produce a double slash
        // when joined with the `~/foo` rest.
        let p = expand_home_to_unc("Ubuntu", "/home/marci/", "~/foo").unwrap();
        assert_eq!(
            p.to_string_lossy(),
            r"\\wsl.localhost\Ubuntu\home\marci\foo"
        );
    }

    #[test]
    fn expand_home_to_unc_rejects_empty_inputs() {
        assert!(expand_home_to_unc("", "/home/marci", "~/foo").is_none());
        assert!(expand_home_to_unc("Ubuntu", "", "~/foo").is_none());
    }

    #[test]
    fn is_wsl_unc_path_recognises_canonical() {
        assert!(is_wsl_unc_path(Path::new(
            r"\\wsl.localhost\Ubuntu\tmp\claude-code-state"
        )));
    }

    #[test]
    fn is_wsl_unc_path_recognises_legacy_dollar() {
        assert!(is_wsl_unc_path(Path::new(r"\\wsl$\Ubuntu\tmp")));
    }

    #[test]
    fn is_wsl_unc_path_rejects_other_unc() {
        assert!(!is_wsl_unc_path(Path::new(r"\\server\share\file")));
        assert!(!is_wsl_unc_path(Path::new("/tmp/foo")));
    }

    #[test]
    fn is_safe_distro_name_accepts_real_world_names() {
        assert!(is_safe_distro_name("Ubuntu"));
        assert!(is_safe_distro_name("Ubuntu-24.04"));
        assert!(is_safe_distro_name("Debian_12"));
        assert!(is_safe_distro_name("kali-linux"));
        assert!(is_safe_distro_name("openSUSE-Tumbleweed"));
    }

    #[test]
    fn is_safe_distro_name_rejects_path_traversal_and_garbage() {
        assert!(!is_safe_distro_name(r"..\Windows"));
        assert!(!is_safe_distro_name("../etc"));
        assert!(!is_safe_distro_name("foo;bar"));
        assert!(!is_safe_distro_name("foo bar"));
        assert!(!is_safe_distro_name(""));
        assert!(!is_safe_distro_name(r"foo\bar"));
        assert!(!is_safe_distro_name("foo/bar"));
    }

    /// Regression: `wsl.exe -l -q` on Windows outputs UTF-16 LE with a
    /// BOM (0xFF 0xFE). Without stripping the BOM, the NUL-filter leaves
    /// `\xFF\xFE` in the byte stream → `from_utf8_lossy` converts them to
    /// U+FFFD → `is_safe_distro_name` rejects the distro name → the WSL
    /// probe never activates → no agent detection on Windows+WSL panes.
    #[test]
    fn utf16_le_bom_stripped_before_distro_parse() {
        // Simulate `wsl.exe -l -q` raw UTF-16 LE output for "Ubuntu-24.04\r\n"
        // with a BOM prefix.
        let raw: Vec<u8> = {
            let mut v = vec![0xFF, 0xFE]; // BOM
            for ch in "Ubuntu-24.04\r\n".encode_utf16() {
                v.push(ch as u8);
                v.push((ch >> 8) as u8);
            }
            v
        };
        // Replicate the fix: strip BOM, then strip NULs, then parse.
        let raw = if raw.starts_with(&[0xFF, 0xFE]) {
            &raw[2..]
        } else {
            &raw[..]
        };
        let cleaned: Vec<u8> = raw.iter().copied().filter(|&b| b != 0).collect();
        let s = String::from_utf8_lossy(&cleaned);
        let first = s.lines().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap();
        assert_eq!(first, "Ubuntu-24.04");
        assert!(is_safe_distro_name(first));
    }

    /// Verify the old (broken) behavior: without BOM stripping, the distro
    /// name would contain replacement characters and be rejected.
    #[test]
    fn utf16_le_bom_without_stripping_produces_invalid_name() {
        let raw: Vec<u8> = {
            let mut v = vec![0xFF, 0xFE]; // BOM
            for ch in "Ubuntu\r\n".encode_utf16() {
                v.push(ch as u8);
                v.push((ch >> 8) as u8);
            }
            v
        };
        // Old behavior: NUL-strip without BOM removal.
        let cleaned: Vec<u8> = raw.iter().copied().filter(|&b| b != 0).collect();
        let s = String::from_utf8_lossy(&cleaned);
        let first = s.lines().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap();
        // The BOM bytes become replacement characters:
        assert!(
            first.starts_with('\u{FFFD}'),
            "expected replacement chars from BOM bytes"
        );
        assert!(
            !is_safe_distro_name(first),
            "distro with BOM remnants must be rejected"
        );
    }

    // On non-Windows builds the detection helpers must short-circuit so
    // that the rest of the harness keeps using the regular Linux paths.
    #[cfg(not(windows))]
    #[test]
    fn detection_helpers_are_noops_on_unix() {
        assert!(detect_default_distro().is_none());
        assert!(detect_wsl_home().is_none());
        assert!(claude_state_dir_unc().is_none());
        assert!(codex_state_dir_unc().is_none());
        assert!(copilot_state_dir_unc().is_none());
        assert!(claude_projects_dir_unc().is_none());
    }

    // ── Table-driven WSL state path rewriting (tn-cntx) ──────────────────

    /// Table-driven: `state_dir_unc` resolves all three state directory
    /// names through `linux_to_unc` when given a valid distro.
    #[test]
    fn state_dir_unc_table_driven() {
        struct Case {
            distro: &'static str,
            state_name: &'static str,
            expected: &'static str,
        }
        let cases = [
            Case {
                distro: "Ubuntu",
                state_name: "claude-code-state",
                expected: r"\\wsl.localhost\Ubuntu\tmp\claude-code-state",
            },
            Case {
                distro: "Ubuntu",
                state_name: "codex-state",
                expected: r"\\wsl.localhost\Ubuntu\tmp\codex-state",
            },
            Case {
                distro: "Ubuntu",
                state_name: "copilot-state",
                expected: r"\\wsl.localhost\Ubuntu\tmp\copilot-state",
            },
            Case {
                distro: "Debian_12",
                state_name: "claude-code-state",
                expected: r"\\wsl.localhost\Debian_12\tmp\claude-code-state",
            },
            Case {
                distro: "kali-linux",
                state_name: "codex-state",
                expected: r"\\wsl.localhost\kali-linux\tmp\codex-state",
            },
            Case {
                distro: "openSUSE-Tumbleweed",
                state_name: "copilot-state",
                expected: r"\\wsl.localhost\openSUSE-Tumbleweed\tmp\copilot-state",
            },
            Case {
                distro: "Ubuntu-24.04",
                state_name: "claude-code-state",
                expected: r"\\wsl.localhost\Ubuntu-24.04\tmp\claude-code-state",
            },
        ];
        for (i, c) in cases.iter().enumerate() {
            // Inline the logic of `state_dir_unc` so we can inject `distro`.
            let linux_path = format!("/tmp/{}", c.state_name);
            let result = linux_to_unc(c.distro, &linux_path);
            assert_eq!(
                result.as_ref().map(|p| p.to_string_lossy().into_owned()),
                Some(c.expected.to_string()),
                "case {i}: distro={} name={}",
                c.distro,
                c.state_name
            );
        }
    }

    /// Missing distro (empty string) causes all state dir lookups to
    /// return `None`, exercising the fallback-to-non-WSL-path branch.
    #[test]
    fn state_dir_unc_missing_distro_returns_none() {
        for name in &["claude-code-state", "codex-state", "copilot-state"] {
            let linux_path = format!("/tmp/{name}");
            assert!(
                linux_to_unc("", &linux_path).is_none(),
                "empty distro should return None for {name}"
            );
        }
    }

    /// Distro names with spaces are rejected by `is_safe_distro_name`,
    /// which the real detect_default_distro checks. Verify `linux_to_unc`
    /// still produces a path (it does not check safety — that is the
    /// caller's job) but `is_safe_distro_name` rejects the name.
    #[test]
    fn distro_with_spaces_rejected_by_safety_check() {
        let distro = "My WSL Distro";
        // The name itself is unsafe:
        assert!(
            !is_safe_distro_name(distro),
            "distro name with spaces must be rejected"
        );
        // But linux_to_unc is a pure path builder — it does not safety-check:
        assert!(linux_to_unc(distro, "/tmp/claude-code-state").is_some());
    }

    // ── Table-driven WSL home resolution (tn-cntx) ───────────────────────

    #[test]
    fn expand_home_table_driven() {
        struct Case {
            label: &'static str,
            distro: &'static str,
            home: &'static str,
            path: &'static str,
            expected: Option<&'static str>,
        }
        let cases = [
            Case {
                label: "standard /home/user",
                distro: "Ubuntu",
                home: "/home/alice",
                path: "~/.claude/projects",
                expected: Some(r"\\wsl.localhost\Ubuntu\home\alice\.claude\projects"),
            },
            Case {
                label: "root user /root",
                distro: "Ubuntu",
                home: "/root",
                path: "~/.claude/projects",
                expected: Some(r"\\wsl.localhost\Ubuntu\root\.claude\projects"),
            },
            Case {
                label: "user with hyphens",
                distro: "Ubuntu",
                home: "/home/dev-user",
                path: "~/.claude/projects",
                expected: Some(r"\\wsl.localhost\Ubuntu\home\dev-user\.claude\projects"),
            },
            Case {
                label: "user with underscores",
                distro: "Ubuntu",
                home: "/home/dev_user",
                path: "~/.claude/projects",
                expected: Some(r"\\wsl.localhost\Ubuntu\home\dev_user\.claude\projects"),
            },
            Case {
                label: "home resolution failure (empty home)",
                distro: "Ubuntu",
                home: "",
                path: "~/.claude/projects",
                expected: None,
            },
            Case {
                label: "home resolution failure (empty distro)",
                distro: "",
                home: "/home/alice",
                path: "~/.claude/projects",
                expected: None,
            },
            Case {
                label: "custom distro with nested home",
                distro: "kali-linux",
                home: "/home/kali",
                path: "~/.claude/projects",
                expected: Some(r"\\wsl.localhost\kali-linux\home\kali\.claude\projects"),
            },
            Case {
                label: "trailing slash on home",
                distro: "Ubuntu",
                home: "/home/alice/",
                path: "~/.config/something",
                expected: Some(r"\\wsl.localhost\Ubuntu\home\alice\.config\something"),
            },
            Case {
                label: "absolute path (no tilde) passes through",
                distro: "Ubuntu",
                home: "/home/alice",
                path: "/var/log/messages",
                expected: Some(r"\\wsl.localhost\Ubuntu\var\log\messages"),
            },
        ];
        for (i, c) in cases.iter().enumerate() {
            let result = expand_home_to_unc(c.distro, c.home, c.path);
            assert_eq!(
                result.as_ref().map(|p| p.to_string_lossy().into_owned()),
                c.expected.map(String::from),
                "case {i} ({}): distro={} home={} path={}",
                c.label,
                c.distro,
                c.home,
                c.path
            );
        }
    }
}
