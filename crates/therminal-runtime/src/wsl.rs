//! Shared WSL detection helpers (tn-9ixz).
//!
//! This module centralises the machinery that both `therminal-harness-claude`
//! and `therminal-app` need when the daemon or GUI runs as a **native Windows
//! process** while the user's shell (and AI harnesses) live inside WSL2.
//!
//! # What lives here
//!
//! - [`detect_default_distro`] — runs `wsl.exe -l -q` **once per process** via
//!   a `OnceLock` cache and returns the first (default) distribution name.
//! - [`detect_wsl_home`] — runs `wsl.exe -e sh -c 'printf %s "$HOME"'` **once
//!   per process** and returns the Linux `$HOME`.
//! - [`linux_to_unc`] — pure path builder: `/tmp/foo` → `\\wsl.localhost\<distro>\tmp\foo`.
//! - [`is_safe_distro_name`] — allowlist check: only alnum + `-` + `.` + `_`.
//!   Used internally by [`detect_default_distro`] **and** exported so callers
//!   can validate externally-supplied names.
//! - [`is_wsl_unc_path`] — returns `true` for `\\wsl.localhost\…` and `\\wsl$\…`.
//!
//! # One probe per process
//!
//! Before this module existed, `therminal-harness-claude` and `therminal-app`
//! each maintained their own `OnceLock<Option<String>>` statics and each
//! shelled out to `wsl.exe` separately — two probes on the first click / first
//! harness tick. The shared statics here eliminate the duplicate probes.
//!
//! # Pure vs impure
//!
//! - [`linux_to_unc`] and [`is_safe_distro_name`] are pure — no I/O, no env, no
//!   process spawn. Fully unit-testable on any platform.
//! - [`detect_default_distro`] and [`detect_wsl_home`] shell out to `wsl.exe`
//!   on the first call, then cache. On non-Windows builds they are compile-time
//!   `None` (the `#[cfg(windows)]` statics never exist).

use std::path::{Path, PathBuf};

// ── Windows-only statics ───────────────────────────────────────────────────

/// Cached default WSL distribution name (Windows-only).
///
/// Populated on first call to [`detect_default_distro`]. The outer `Option`
/// distinguishes "not yet probed" from "probed-and-absent". On non-Windows
/// builds this static is never instantiated and the function returns a
/// compile-time `None`.
#[cfg(windows)]
static DEFAULT_DISTRO: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Cached Linux `$HOME` from the default WSL distribution (Windows-only).
///
/// Populated on first call to [`detect_wsl_home`].
#[cfg(windows)]
static WSL_HOME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

// ── Public API ─────────────────────────────────────────────────────────────

/// Return the name of the user's default WSL distribution, or `None`
/// if WSL is not installed / not detectable.
///
/// Caches the first result for the lifetime of the process. Compile-time
/// `None` on non-Windows targets — no `wsl.exe` probe ever happens.
pub fn detect_default_distro() -> Option<String> {
    #[cfg(windows)]
    {
        DEFAULT_DISTRO
            .get_or_init(|| {
                use std::process::Command;
                // `wsl.exe -l -q` lists distro names (quiet, one per line).
                // The first line is the default. Output is UTF-16 LE on
                // Windows; strip the BOM (0xFF 0xFE) before removing the
                // interleaved NUL bytes so the ASCII distro name decodes
                // cleanly without pulling in a full UTF-16 dependency.
                let output = Command::new("wsl.exe").args(["-l", "-q"]).output().ok()?;
                if !output.status.success() {
                    tracing::debug!(
                        status = ?output.status,
                        stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                        "wsl: `wsl.exe -l -q` failed, no distro detected"
                    );
                    return None;
                }
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
                    // installer-enforced distro name charset before splicing
                    // it into a UNC path string. A tampered `wsl.exe` or
                    // malformed output could otherwise inject a path like
                    // `..\..\Windows\System32` and escape the UNC root.
                    tracing::warn!(
                        distro = %first,
                        "wsl: rejecting unsafe distro name from `wsl.exe -l -q`"
                    );
                    None
                } else {
                    tracing::info!(distro = %first, "wsl: detected default WSL distro");
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
                        "wsl: `wsl.exe -e sh -c printf $HOME` failed"
                    );
                    return None;
                }
                let cleaned: Vec<u8> = output.stdout.into_iter().filter(|&b| b != 0).collect();
                let s = String::from_utf8_lossy(&cleaned).trim().to_string();
                if s.is_empty() || !s.starts_with('/') {
                    None
                } else {
                    tracing::info!(home = %s, "wsl: detected WSL $HOME");
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

/// Build a UNC path that points at a Linux absolute path inside the named
/// WSL distribution.
///
/// Returns `None` for inputs that aren't a Linux-shaped absolute path, or
/// when `distro` is empty. Pure — no filesystem, no env probing.
///
/// ```rust,ignore
/// use therminal_runtime::wsl::linux_to_unc;
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

    // Strip the leading `/` and flip remaining slashes to backslashes so the
    // produced PathBuf is a clean Windows-shaped UNC path.
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

/// Return `true` when `name` only contains characters allowed in a WSL
/// distribution name.
///
/// Real-world WSL distro names are constrained by the installer to
/// alphanumeric + hyphen + dot + underscore. This allowlist is a
/// defense-in-depth check against a tampered `wsl.exe` returning a
/// path-traversal payload that would escape the `\\wsl.localhost\<distro>\…`
/// UNC root when spliced into [`linux_to_unc`].
pub fn is_safe_distro_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_')
}

/// Return `true` when `path` looks like a UNC path into the WSL virtual
/// filesystem (`\\wsl.localhost\…` or the legacy `\\wsl$\…` prefix).
pub fn is_wsl_unc_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.starts_with(r"\\wsl.localhost\") || s.starts_with(r"\\wsl$\")
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── linux_to_unc ────────────────────────────────────────────────────────

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

    // ── is_safe_distro_name ─────────────────────────────────────────────────

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

    // ── is_wsl_unc_path ─────────────────────────────────────────────────────

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

    // ── BOM-stripping regression (mirrors harness test) ─────────────────────

    /// `wsl.exe -l -q` on Windows outputs UTF-16 LE with a BOM (0xFF 0xFE).
    /// Without stripping the BOM first, the NUL-filter leaves `\xFF\xFE` in
    /// the byte stream, `from_utf8_lossy` turns them into U+FFFD replacement
    /// characters, and `is_safe_distro_name` rejects the otherwise valid name.
    #[test]
    fn utf16_le_bom_stripped_before_distro_parse() {
        let raw: Vec<u8> = {
            let mut v = vec![0xFF, 0xFE]; // BOM
            for ch in "Ubuntu-24.04\r\n".encode_utf16() {
                v.push(ch as u8);
                v.push((ch >> 8) as u8);
            }
            v
        };
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

    /// Verify the old (broken) behavior: without BOM stripping the distro name
    /// contains replacement characters and is rejected by `is_safe_distro_name`.
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
        let cleaned: Vec<u8> = raw.iter().copied().filter(|&b| b != 0).collect();
        let s = String::from_utf8_lossy(&cleaned);
        let first = s.lines().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap();
        assert!(
            first.starts_with('\u{FFFD}'),
            "expected replacement chars from BOM bytes"
        );
        assert!(
            !is_safe_distro_name(first),
            "distro with BOM remnants must be rejected"
        );
    }

    // ── Non-Windows short-circuit ────────────────────────────────────────────

    #[cfg(not(windows))]
    #[test]
    fn detection_helpers_are_noops_on_unix() {
        assert!(detect_default_distro().is_none());
        assert!(detect_wsl_home().is_none());
    }
}
