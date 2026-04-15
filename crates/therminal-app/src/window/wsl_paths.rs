//! WSL↔Windows path translation (tn-q8ce).
//!
//! When `therminal-app` runs as a **native Windows build** but the pane's
//! shell is `wsl.exe`, the shell emits Linux-style paths via OSC 7 and in
//! its own output (e.g. `/home/marci/projects/therminal/src/main.rs`).
//! Click handlers like [`crate::window::pane_ops::App::open_in_editor`]
//! then `std::fs::metadata()` that literal string as a Windows path,
//! which resolves to `C:\home\marci\…` — a path that does not exist on
//! the Windows filesystem. Every hotspot click inside a WSL pane fails
//! on native Windows as a result.
//!
//! The fix is the `\\wsl.localhost\<distro>\…` UNC form, which Windows
//! (10 21H2+ and 11) resolves through the WSL virtual filesystem
//! provider back onto the Linux namespace. On older builds the
//! equivalent prefix was `\\wsl$\<distro>\…`; the translator here
//! prefers `wsl.localhost` because Microsoft made it the canonical
//! form in Windows 11 and the old `wsl$` prefix still works as a
//! symlink on systems that have it.
//!
//! # Scope
//!
//! This module is the **reverse** of [`crate::window::chrome::abbreviate_path`],
//! which handles "therminal inside WSL, showing a Windows path". See
//! the top of `CLAUDE.md` (WSL2 section) and the
//! `ref_wsl2_environment.md` memory for the full topology matrix.
//!
//! # Pure vs impure
//!
//! - [`translate_linux_to_unc`] is a pure function — no filesystem, no
//!   environment, no process spawn. Fully unit-tested on Linux.
//! - [`detect_default_distro`] / [`detect_wsl_home`] are re-exported from
//!   `therminal_runtime::wsl` and share the process-wide `OnceLock` cache
//!   (tn-9ixz). On Linux/macOS builds they short-circuit to `None` without
//!   ever touching the process table.
//! - [`translate_if_wsl_windows`] is the top-level click-handler hook:
//!   no-op on non-Windows, no-op when the distro can't be detected,
//!   otherwise it delegates to `translate_linux_to_unc`.

use std::borrow::Cow;

// Re-export the shared detection helpers so the rest of the app module keeps
// calling `wsl_paths::detect_default_distro()` and `wsl_paths::detect_wsl_home()`
// without change — both now share the single OnceLock in therminal-runtime.
pub use therminal_runtime::wsl::{detect_default_distro, detect_wsl_home};

/// Return true when the click handler should route `path` into a new
/// WSL pane instead of handing it to the Windows host.
///
/// The decision is based on the **focused pane's cwd**, not the path
/// alone. If the cwd is a POSIX absolute path (`/home/marci/…`) the
/// pane is unambiguously a WSL shell — the daemon's OSC 7 reader
/// populated it from the Linux shell, Windows cmd/pwsh never emit
/// forward-slash roots. All path shapes then flow to WSL: absolute
/// (`/foo`), tilde (`~/foo`), relative (`./foo`, `../foo`, `foo`).
/// The only exception is Windows-absolute paths (`C:\…`, `\\…`),
/// which represent the rare "echo'd Windows path inside a WSL pane"
/// case and stay on the host side.
///
/// On non-Windows builds always returns `false` — there is nothing
/// to route, and the caller's existing path handles the shell-local
/// case correctly.
///
/// Returns `false` when we can't determine the cwd (no focused pane,
/// OSC 7 never fired) so the caller falls through to the existing
/// host-side path. Worst case: a click fails as before instead of
/// doing something surprising.
pub fn is_wsl_pane_path(cwd: Option<&str>, path: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }
    // Require a POSIX-absolute cwd as the WSL shell fingerprint. A
    // Windows cwd (cmd, pwsh) starts with a drive letter or UNC
    // prefix and must not be treated as WSL.
    let Some(cwd) = cwd else {
        return false;
    };
    if !cwd.starts_with('/') {
        return false;
    }
    // Windows-absolute paths emitted inside a WSL pane (e.g. the user
    // pastes a `C:\…` path, or a cross-compile tool logs one) stay
    // on the host — the Linux shell has no better claim to them than
    // the Windows FS does.
    if therminal_terminal::hotspot_detection::is_windows_absolute(path) {
        return false;
    }
    true
}

/// Return true if `path` is an absolute Linux-style path (starts with `/`)
/// that should be translated when the host is Windows + WSL.
///
/// Returning false for relative paths, `~`-prefixed paths, and already-
/// Windows-shaped paths (`C:\…`, `\\server\share\…`) means callers can
/// blindly pipe every hotspot through the translator without a
/// pre-filter.
pub fn is_translatable_linux_path(path: &str) -> bool {
    // Fast-path: nothing to do for empty / relative / tilde / Windows
    // absolute.
    if path.is_empty() || !path.starts_with('/') {
        return false;
    }
    // UNC paths start with `\\` so they don't hit the `/` check above.
    // Leading `//` is uncommon but could appear in rendered URLs; we
    // refuse to translate it because it's ambiguous (pseudo-UNC, Cygwin,
    // or just a typo).
    if path.starts_with("//") {
        return false;
    }
    true
}

/// Convert a Linux path like `/home/marci/foo.rs` to the WSL UNC form
/// `\\wsl.localhost\<distro>\home\marci\foo.rs`.
///
/// Pure — no I/O. Preserves any trailing `:line[:col]` hotspot suffix
/// verbatim (the editor fallback chain needs to see it).
///
/// - Non-translatable paths (per [`is_translatable_linux_path`]) are
///   returned unchanged.
/// - Forward slashes are flipped to backslashes in the body so Windows
///   tooling receives a clean native path.
/// - The `:line:col` suffix (if any) is NOT touched — line numbers are
///   colon-delimited, never backslash-delimited, and `open_in_editor`
///   parses that suffix independently.
pub fn translate_linux_to_unc<'a>(path: &'a str, distro: &str) -> Cow<'a, str> {
    if !is_translatable_linux_path(path) {
        return Cow::Borrowed(path);
    }
    if distro.is_empty() {
        // Caller should have filtered this out, but be defensive — an
        // empty distro would produce `\\wsl.localhost\\home\…` which
        // Windows rejects. Leave the path unchanged so the existing
        // "file not found" toast still fires.
        return Cow::Borrowed(path);
    }

    // Split off the trailing `:line[:col]` suffix (digits only, up to
    // two colon-separated groups from the right). We don't want to
    // backslash-ify colons that belong to the hotspot suffix.
    let (body, suffix) = split_line_col_suffix(path);

    // Strip the leading `/` (we're going to slot the body in after
    // `\\wsl.localhost\<distro>\`). The remaining forward slashes
    // become backslashes.
    let body_no_leading = &body[1..];
    let body_back = body_no_leading.replace('/', "\\");

    let mut out = String::with_capacity(body_back.len() + distro.len() + 20);
    out.push_str(r"\\wsl.localhost\");
    out.push_str(distro);
    out.push('\\');
    out.push_str(&body_back);
    out.push_str(suffix);
    Cow::Owned(out)
}

/// Split a path-plus-suffix like `/foo/bar.rs:42:5` into `("/foo/bar.rs", ":42:5")`.
///
/// Only strips suffixes that match `:<digits>` or `:<digits>:<digits>`
/// from the right. If the tail doesn't match, returns `(path, "")`.
fn split_line_col_suffix(path: &str) -> (&str, &str) {
    // Walk backwards: at most two colon-delimited all-digit groups.
    let bytes = path.as_bytes();
    // Find the rightmost colon.
    let Some(last_colon) = path.rfind(':') else {
        return (path, "");
    };
    let after = &path[last_colon + 1..];
    if after.is_empty() || !after.bytes().all(|b| b.is_ascii_digit()) {
        return (path, "");
    }
    // Maybe there's a second (line) colon before the col one.
    let before_last = &path[..last_colon];
    if let Some(second) = before_last.rfind(':') {
        let between = &path[second + 1..last_colon];
        if !between.is_empty() && between.bytes().all(|b| b.is_ascii_digit()) {
            // Guard against turning a Windows drive letter (`C:`) into a
            // bogus `line:col` suffix. Only happens if the original path
            // was already Windows-shaped, which this function is never
            // called with — but be safe.
            if second >= 1 && bytes[second - 1].is_ascii_alphabetic() && second == 1 {
                return (&path[..last_colon], &path[last_colon..]);
            }
            return (&path[..second], &path[second..]);
        }
    }
    (&path[..last_colon], &path[last_colon..])
}

/// Expand a leading `~/` in a Linux-style path using the WSL `$HOME`
/// (not the Windows host's `$HOME`).
///
/// On Windows, consults [`detect_wsl_home`] and substitutes `~` with
/// the Linux HOME. `~` alone becomes `$HOME`. Paths without a leading
/// `~` pass through unchanged. On non-Windows builds this is a no-op
/// (callers should use the regular [`therminal_terminal::hotspot_detection::expand_tilde`]).
pub fn expand_tilde_wsl(path: &str) -> Cow<'_, str> {
    if !cfg!(windows) {
        return Cow::Borrowed(path);
    }
    if path != "~" && !path.starts_with("~/") {
        return Cow::Borrowed(path);
    }
    let Some(home) = detect_wsl_home() else {
        return Cow::Borrowed(path);
    };
    if path == "~" {
        return Cow::Owned(home);
    }
    let rest = &path[2..]; // strip "~/"
    let trimmed = home.trim_end_matches('/');
    Cow::Owned(format!("{trimmed}/{rest}"))
}

/// Top-level click-handler hook: translate a Linux path to a WSL UNC
/// path if we're on Windows and a default WSL distro is detected.
///
/// On non-Windows builds this is a no-op. On Windows builds it only
/// translates paths that pass [`is_translatable_linux_path`] and only
/// when a distro was detected at startup.
pub fn translate_if_wsl_windows(path: &str) -> Cow<'_, str> {
    if !cfg!(windows) || !is_translatable_linux_path(path) {
        return Cow::Borrowed(path);
    }
    match detect_default_distro() {
        Some(distro) => translate_linux_to_unc(path, &distro),
        None => Cow::Borrowed(path),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translatable_checks() {
        assert!(is_translatable_linux_path("/home/marci/foo.rs"));
        assert!(is_translatable_linux_path("/"));
        assert!(!is_translatable_linux_path(""));
        assert!(!is_translatable_linux_path("foo.rs"));
        assert!(!is_translatable_linux_path("./foo.rs"));
        assert!(!is_translatable_linux_path("../foo.rs"));
        assert!(!is_translatable_linux_path("~/foo.rs"));
        assert!(!is_translatable_linux_path(r"C:\Users\x"));
        assert!(!is_translatable_linux_path(r"\\server\share"));
        // Double-slash guarded explicitly.
        assert!(!is_translatable_linux_path("//foo/bar"));
    }

    #[test]
    fn simple_translation() {
        let out = translate_linux_to_unc("/home/marci/foo.rs", "Ubuntu");
        assert_eq!(out, r"\\wsl.localhost\Ubuntu\home\marci\foo.rs");
    }

    #[test]
    fn translation_preserves_line_col_suffix() {
        let out = translate_linux_to_unc("/home/marci/foo.rs:42", "Ubuntu");
        assert_eq!(out, r"\\wsl.localhost\Ubuntu\home\marci\foo.rs:42");

        let out = translate_linux_to_unc("/home/marci/foo.rs:42:5", "Ubuntu");
        assert_eq!(out, r"\\wsl.localhost\Ubuntu\home\marci\foo.rs:42:5");
    }

    #[test]
    fn translation_leaves_non_translatable_alone() {
        assert_eq!(translate_linux_to_unc("foo.rs", "Ubuntu"), "foo.rs");
        assert_eq!(
            translate_linux_to_unc("./src/main.rs", "Ubuntu"),
            "./src/main.rs"
        );
        assert_eq!(translate_linux_to_unc("~/foo", "Ubuntu"), "~/foo");
        assert_eq!(
            translate_linux_to_unc(r"C:\Users\x", "Ubuntu"),
            r"C:\Users\x"
        );
    }

    #[test]
    fn translation_with_empty_distro_is_noop() {
        assert_eq!(
            translate_linux_to_unc("/home/marci/foo.rs", ""),
            "/home/marci/foo.rs"
        );
    }

    #[test]
    fn translation_handles_root_path() {
        let out = translate_linux_to_unc("/", "Ubuntu");
        assert_eq!(out, r"\\wsl.localhost\Ubuntu\");
    }

    #[test]
    fn translation_handles_nested_path() {
        let out = translate_linux_to_unc(
            "/home/marci/projects/therminal/crates/therminal-app/src/main.rs",
            "Ubuntu",
        );
        assert_eq!(
            out,
            r"\\wsl.localhost\Ubuntu\home\marci\projects\therminal\crates\therminal-app\src\main.rs"
        );
    }

    #[test]
    fn translation_handles_kali_distro() {
        let out = translate_linux_to_unc("/etc/hosts", "kali-linux");
        assert_eq!(out, r"\\wsl.localhost\kali-linux\etc\hosts");
    }

    #[test]
    fn split_suffix_extracts_line_col() {
        assert_eq!(
            split_line_col_suffix("/foo/bar.rs:42:5"),
            ("/foo/bar.rs", ":42:5")
        );
        assert_eq!(
            split_line_col_suffix("/foo/bar.rs:42"),
            ("/foo/bar.rs", ":42")
        );
        assert_eq!(split_line_col_suffix("/foo/bar.rs"), ("/foo/bar.rs", ""));
        // Non-numeric tail: not a suffix.
        assert_eq!(
            split_line_col_suffix("/foo/bar.rs:abc"),
            ("/foo/bar.rs:abc", "")
        );
    }

    #[test]
    fn translate_if_wsl_windows_is_noop_on_linux() {
        #[cfg(not(windows))]
        {
            assert_eq!(
                translate_if_wsl_windows("/home/marci/foo.rs"),
                "/home/marci/foo.rs"
            );
            assert_eq!(translate_if_wsl_windows("./relative.rs"), "./relative.rs");
        }
    }

    // Safety check is now in therminal-runtime — verify it's accessible from here.
    #[test]
    fn is_safe_distro_name_via_runtime() {
        assert!(therminal_runtime::wsl::is_safe_distro_name("Ubuntu-24.04"));
        assert!(!therminal_runtime::wsl::is_safe_distro_name("../escape"));
    }
}
