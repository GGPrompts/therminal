//! Hotspot detection for actionable patterns in terminal text.
//!
//! Scans text rows for file paths (with optional line:col), error locations
//! (Rust, TypeScript), git refs, and issue references. This module operates
//! on plain text rows so it can be used by both the app (GPU renderer) and
//! the daemon (MCP server) without depending on rendering types.

use std::sync::LazyLock;

use regex::Regex;

// ── Types ────────────────────────────────────────────────────────────────

/// The category of an actionable hotspot.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum HotspotKind {
    /// A file path, optionally with `:line` or `:line:col` suffix.
    FilePath,
    /// A compiler/linter error location (e.g. Rust `error[E0123]: ... --> src/file.rs:42:5`).
    ErrorLocation,
    /// A git commit hash or branch name in git-context output.
    GitRef,
    /// An issue/PR reference like `#1234` or `PREFIX-123`.
    IssueRef,
    /// A detected URL.
    Url,
}

impl HotspotKind {
    /// Return the string name used in MCP results.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FilePath => "file",
            Self::ErrorLocation => "file",
            Self::GitRef => "git_ref",
            Self::IssueRef => "issue",
            Self::Url => "url",
        }
    }
}

/// A single actionable hotspot detected in terminal text.
#[derive(Clone, Debug)]
pub struct TextHotspot {
    /// What kind of actionable item this is.
    pub kind: HotspotKind,
    /// The matched text.
    pub text: String,
    /// Row index (0-based).
    pub row: usize,
    /// First column of the match (0-based, inclusive).
    pub start_col: usize,
    /// One-past-last column of the match (exclusive).
    pub end_col: usize,
    /// True when the hotspot is a `FilePath` whose target stat'd as a
    /// directory at detection time. Other hotspot kinds always set this
    /// to `false`. Populated by [`promote_directory_hotspots`] — text-only
    /// detection (the regex pass) does not touch the filesystem and
    /// always emits `is_dir = false`.
    pub is_dir: bool,
}

// ── Compiled regexes ─────────────────────────────────────────────────────

/// File path with optional `:line` or `:line:col`.
/// Matches EITHER paths with a leading prefix (`./`, `../`, `/`) OR
/// any path whose final segment ends in a known source/config/doc
/// extension from a curated whitelist. This avoids false positives on
/// plain `word.word` tokens (e.g. `2.0`, `1.5x`, `Loaded.files`).
pub static FILE_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Keep extension list in sync with the module docs / tests.
    const EXT: &str = r"rs|ts|tsx|jsx|js|mjs|cjs|py|md|toml|json|yaml|yml|html|css|scss|sh|bash|zsh|fish|ps1|go|java|kt|swift|c|h|cpp|hpp|rb|php|lua|sql|xml|lock|txt|log|env|gitignore|dockerfile";
    let pat = format!(
        r"(?:(?:\.{{1,2}}/|/)(?:[A-Za-z0-9_\-.]+/)*[A-Za-z0-9_\-.]+(?:\.(?:{ext}))?|(?:[A-Za-z0-9_\-.]+/)*[A-Za-z0-9_\-]+\.(?:{ext}))(?::\d+(?::\d+)?)?",
        ext = EXT
    );
    Regex::new(&pat).unwrap()
});

/// Rust-style error location: `  --> src/file.rs:42:5`
pub static RUST_ERROR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-->\s+((?:[A-Za-z0-9_\-]+/)*[A-Za-z0-9_\-]+\.[A-Za-z0-9_]+:\d+:\d+)").unwrap()
});

/// TypeScript/C#-style error location: `file.ts(42,15)`
pub static TS_ERROR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([A-Za-z0-9_\-]+(?:/[A-Za-z0-9_\-]+)*\.[A-Za-z0-9_]+)\((\d+),(\d+)\)").unwrap()
});

/// Git short/long hash: 7-40 hex characters, word-bounded.
pub static GIT_HASH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9a-f]{7,40}\b").unwrap());

/// Git branch name in `git branch` output: current branch line starts with `* `.
/// Requires `* ` prefix (current branch indicator) or that the name contains
/// `/` or `-` (which real branch names almost always do) to avoid false
/// positives on indented prose.
pub static GIT_BRANCH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:\* ([A-Za-z0-9_/\-\.]+)|  ([A-Za-z0-9_\.][-A-Za-z0-9_/\.]*[/\-][A-Za-z0-9_/\-\.]*))",
    )
    .unwrap()
});

/// Issue/PR reference: `#1234` or `PREFIX-123` (2-8 uppercase prefix).
pub static ISSUE_REF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\b[A-Z]{2,8}-\d+\b|#\d+\b)").unwrap());

/// HTTP(S) URL pattern.
pub static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s<>\)\]\}"'`,;]+"#).unwrap());

// ── Text-based detection ─────────────────────────────────────────────────

/// Scan text rows for actionable hotspots.
///
/// Each row is a string of characters. Returns hotspots with row/column
/// coordinates. This function is designed to work with plain text (e.g.
/// from a `PaneSnapshot` grid) without requiring GPU render types.
pub fn detect_hotspots_from_text(rows: &[String]) -> Vec<TextHotspot> {
    detect_hotspots_from_text_with_wrap(rows, &[])
}

/// Like [`detect_hotspots_from_text`], but accepts per-row wrap state.
///
/// `is_continuation[row]` should be `true` when the previous physical row
/// hard-wrapped into this row (alacritty's `WRAPLINE` flag on the last cell
/// of `row - 1`). Physical rows that share a logical (hard-wrapped) line are
/// joined into a single string before regex scanning, then matches are mapped
/// back to per-row column spans. A single match crossing N physical rows
/// produces N `TextHotspot`s — one per row — all sharing the same `text`
/// (the full joined match), so click-to-open on any row resolves to the
/// same target.
pub fn detect_hotspots_from_text_with_wrap(
    rows: &[String],
    is_continuation: &[bool],
) -> Vec<TextHotspot> {
    let mut hotspots = Vec::new();

    // Walk rows, grouping consecutive rows linked by `is_continuation` into
    // logical lines. For each logical line we build a joined String plus an
    // origin map giving (row, col_in_row) per char.
    let mut i = 0;
    while i < rows.len() {
        let start = i;
        let mut end = i + 1;
        while end < rows.len() && is_continuation.get(end).copied().unwrap_or(false) {
            end += 1;
        }
        i = end;

        if rows[start..end].iter().all(|r| r.trim().is_empty()) {
            continue;
        }

        // Build joined text + per-char origin and per-char byte start.
        let mut joined = String::new();
        let mut origins_by_char: Vec<(usize, usize)> = Vec::new();
        let mut char_byte_starts: Vec<usize> = Vec::new();
        for (offset, row_text) in rows[start..end].iter().enumerate() {
            let row_index = start + offset;
            for (col, ch) in row_text.chars().enumerate() {
                char_byte_starts.push(joined.len());
                joined.push(ch);
                origins_by_char.push((row_index, col));
            }
        }
        if joined.trim().is_empty() {
            continue;
        }

        // Translate a (byte_start, byte_end) match in `joined` into per-row
        // spans (row, start_col, end_col). A single regex match that crosses
        // a row boundary produces multiple consecutive spans.
        let byte_range_to_spans =
            |byte_start: usize, byte_end: usize| -> Vec<(usize, usize, usize)> {
                let start_char = char_byte_starts
                    .iter()
                    .position(|&b| b == byte_start)
                    .unwrap_or(0);
                let end_char = char_byte_starts
                    .iter()
                    .position(|&b| b >= byte_end)
                    .unwrap_or(char_byte_starts.len());
                if end_char <= start_char {
                    return Vec::new();
                }
                let mut spans: Vec<(usize, usize, usize)> = Vec::new();
                let mut k = start_char;
                while k < end_char {
                    let (row, col) = origins_by_char[k];
                    let mut last_col = col;
                    let mut k2 = k + 1;
                    while k2 < end_char {
                        let (r2, c2) = origins_by_char[k2];
                        if r2 != row || c2 != last_col + 1 {
                            break;
                        }
                        last_col = c2;
                        k2 += 1;
                    }
                    spans.push((row, col, last_col + 1));
                    k = k2;
                }
                spans
            };

        let push_match = |hotspots: &mut Vec<TextHotspot>,
                          kind: HotspotKind,
                          full_text: &str,
                          spans: Vec<(usize, usize, usize)>| {
            for (row, sc, ec) in spans {
                hotspots.push(TextHotspot {
                    kind: kind.clone(),
                    text: full_text.to_string(),
                    row,
                    start_col: sc,
                    end_col: ec,
                    is_dir: false,
                });
            }
        };

        // Track claimed (row, col) ranges for dominance suppression.
        let mut claimed: Vec<(HotspotKind, usize, usize, usize)> = Vec::new();
        let is_dominated = |claimed: &[(HotspotKind, usize, usize, usize)],
                            spans: &[(usize, usize, usize)],
                            only_higher: bool|
         -> bool {
            spans.iter().any(|(row, sc, ec)| {
                claimed.iter().any(|(k, r, s, e)| {
                    r == row
                        && *s <= *sc
                        && *e >= *ec
                        && (!only_higher
                            || matches!(k, HotspotKind::Url | HotspotKind::ErrorLocation))
                })
            })
        };

        // URLs first.
        for m in URL_RE.find_iter(&joined) {
            let spans = byte_range_to_spans(m.start(), m.end());
            if spans.is_empty() {
                continue;
            }
            for s in &spans {
                claimed.push((HotspotKind::Url, s.0, s.1, s.2));
            }
            push_match(&mut hotspots, HotspotKind::Url, m.as_str(), spans);
        }

        // Rust error locations (`--> file.rs:42:5`).
        for cap in RUST_ERROR_RE.captures_iter(&joined) {
            if let Some(m) = cap.get(1) {
                let spans = byte_range_to_spans(m.start(), m.end());
                if spans.is_empty() {
                    continue;
                }
                for s in &spans {
                    claimed.push((HotspotKind::ErrorLocation, s.0, s.1, s.2));
                }
                push_match(&mut hotspots, HotspotKind::ErrorLocation, m.as_str(), spans);
            }
        }

        // TypeScript/C# error locations (`file.ts(42,15)`).
        for cap in TS_ERROR_RE.captures_iter(&joined) {
            if let Some(m) = cap.get(0) {
                let spans = byte_range_to_spans(m.start(), m.end());
                if spans.is_empty() {
                    continue;
                }
                for s in &spans {
                    claimed.push((HotspotKind::ErrorLocation, s.0, s.1, s.2));
                }
                push_match(&mut hotspots, HotspotKind::ErrorLocation, m.as_str(), spans);
            }
        }

        // File paths.
        for m in FILE_PATH_RE.find_iter(&joined) {
            let spans = byte_range_to_spans(m.start(), m.end());
            if spans.is_empty() || is_dominated(&claimed, &spans, true) {
                continue;
            }
            for s in &spans {
                claimed.push((HotspotKind::FilePath, s.0, s.1, s.2));
            }
            push_match(&mut hotspots, HotspotKind::FilePath, m.as_str(), spans);
        }

        // Git branch names — applied per physical row (anchored regex).
        for (offset, row_text) in rows[start..end].iter().enumerate() {
            let row_index = start + offset;
            for cap in GIT_BRANCH_RE.captures_iter(row_text) {
                let m = cap.get(1).or_else(|| cap.get(2));
                if let Some(m) = m {
                    let char_starts: Vec<usize> = row_text.char_indices().map(|(b, _)| b).collect();
                    let sc = char_starts
                        .iter()
                        .position(|&b| b == m.start())
                        .unwrap_or(0);
                    let ec = char_starts
                        .iter()
                        .position(|&b| b >= m.end())
                        .unwrap_or(char_starts.len());
                    hotspots.push(TextHotspot {
                        kind: HotspotKind::GitRef,
                        text: m.as_str().to_string(),
                        row: row_index,
                        start_col: sc,
                        end_col: ec,
                        is_dir: false,
                    });
                }
            }
        }

        // Git hashes.
        for m in GIT_HASH_RE.find_iter(&joined) {
            let spans = byte_range_to_spans(m.start(), m.end());
            if spans.is_empty() || is_dominated(&claimed, &spans, false) {
                continue;
            }
            for s in &spans {
                claimed.push((HotspotKind::GitRef, s.0, s.1, s.2));
            }
            push_match(&mut hotspots, HotspotKind::GitRef, m.as_str(), spans);
        }

        // Issue references (`#123`, `PREFIX-456`).
        for m in ISSUE_REF_RE.find_iter(&joined) {
            let spans = byte_range_to_spans(m.start(), m.end());
            if spans.is_empty() {
                continue;
            }
            push_match(&mut hotspots, HotspotKind::IssueRef, m.as_str(), spans);
        }
    }

    hotspots
}

// ── Directory promotion ─────────────────────────────────────────────────

/// Mark `FilePath` hotspots whose target stat'd as a directory.
///
/// Walks `hotspots` and, for each `FilePath` entry, calls `is_dir_fn(path)`
/// on the path portion (the part before any `:line[:col]` suffix). If the
/// callback returns `Ok(true)`, the entry's `is_dir` flag is set so the
/// click handler can route the hotspot through the folder-open path
/// instead of the editor fallback chain.
///
/// `Ok(false)` is treated as definitive ("the filesystem says this is not
/// a directory") and the entry is left alone. `Err(_)` — including the
/// common cross-filesystem case where stat fails because the path doesn't
/// exist on the host running therminal (e.g. WSL paths on a Windows
/// build) — falls through to [`classify_path_as_directory_heuristic`],
/// which is a pure, filesystem-free guess. Without that fallback the
/// right-click context menu would never offer the folder actions for
/// any directory whose stat fails, which is the entire problem on the
/// Windows build of therminal driving WSL2 panes (tn-fqvx).
///
/// The callback is generic so tests can stub it without touching the
/// real filesystem. Production callers pass a closure that wraps
/// `std::fs::metadata(p).map(|m| m.is_dir())`.
pub fn promote_directory_hotspots<F>(hotspots: &mut [TextHotspot], mut is_dir_fn: F)
where
    F: FnMut(&str) -> std::io::Result<bool>,
{
    for h in hotspots.iter_mut() {
        if h.kind != HotspotKind::FilePath {
            continue;
        }
        let path = strip_line_col_suffix(&h.text);
        match is_dir_fn(path) {
            Ok(true) => h.is_dir = true,
            Ok(false) => {} // filesystem says no — trust it
            Err(_) => {
                // stat failed (most often: path lives on a filesystem the
                // host can't see, e.g. WSL paths on a Windows therminal
                // build). Fall through to a pure heuristic so the user can
                // still get the folder context menu.
                if classify_path_as_directory_heuristic(path) {
                    h.is_dir = true;
                }
            }
        }
    }
}

/// Pure, filesystem-free heuristic for "does this path look like a directory?"
///
/// Used as a fallback in [`promote_directory_hotspots`] when `std::fs::metadata`
/// fails — most commonly because the path lives on a filesystem the host
/// process can't see (e.g. a `/home/marci/...` POSIX path opened on the
/// Windows build of therminal pointing into WSL2). The rules, applied in
/// order, are:
///
/// 1. Trailing `/` ⇒ directory. The user can always add an explicit slash
///    to force the folder context menu.
/// 2. Empty / root `/` ⇒ directory. The root is always a directory.
/// 3. Bare token (no `/` separator at all) ⇒ NOT a directory. Things like
///    `Makefile`, `Cargo.toml`, `README` are written as bare names in shell
///    output and are overwhelmingly files, not directories. Without this
///    rule, extensionless bare names like `Makefile` would be misclassified.
/// 4. The last path component contains no `.` ⇒ directory. This catches
///    the overwhelming majority of directory references in shell output
///    (`/home/marci/projects/therminal`, `/usr/local/bin`, …) without
///    misclassifying dotfiles like `.bashrc` (rule applied to the *last*
///    component, and a leading dot is still a `.`).
///
/// Returning `true` here is best-effort: the click handler still validates
/// the target before launching anything, and the worst case is that the
/// user sees a "folder" menu on a path that turns out not to exist.
pub fn classify_path_as_directory_heuristic(path: &str) -> bool {
    // Rule 1: explicit trailing slash.
    if path.ends_with('/') {
        return true;
    }
    // Rule 2: empty / root.
    if path.is_empty() || path == "/" {
        return true;
    }
    // Rule 3: bare tokens (no separator) are treated as files. Catches
    // extensionless names like `Makefile`, `README`, `Dockerfile` that
    // would otherwise pass rule 4.
    if !path.contains('/') {
        return false;
    }
    // Rule 4: extensionless last component.
    let last = path.rsplit('/').next().unwrap_or(path);
    if last.is_empty() {
        return true;
    }
    !last.contains('.')
}

/// Strip an optional `:line[:col]` suffix from a path-like string.
///
/// `src/main.rs:42:5` -> `src/main.rs`. Strings without a numeric colon
/// suffix are returned unchanged.
pub fn strip_line_col_suffix(text: &str) -> &str {
    if let Some(idx) = text.find(':')
        && text[idx + 1..].starts_with(|c: char| c.is_ascii_digit())
    {
        &text[..idx]
    } else {
        text
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(lines: &[&str]) -> Vec<TextHotspot> {
        let rows: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        detect_hotspots_from_text(&rows)
    }

    #[test]
    fn detects_absolute_file_path_with_line_col() {
        let hotspots = detect(&["error at /home/user/src/main.rs:42:5 here"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1);
        assert_eq!(fp[0].text, "/home/user/src/main.rs:42:5");
    }

    #[test]
    fn detects_relative_file_path() {
        let hotspots = detect(&["see ./src/lib.rs:10 for details"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1);
        assert_eq!(fp[0].text, "./src/lib.rs:10");
    }

    #[test]
    fn detects_rust_error_location() {
        let hotspots = detect(&["  --> crates/therminal-app/src/main.rs:42:5"]);
        let el: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::ErrorLocation)
            .collect();
        assert_eq!(el.len(), 1);
        assert_eq!(el[0].text, "crates/therminal-app/src/main.rs:42:5");
    }

    #[test]
    fn detects_git_hash() {
        let hotspots = detect(&["commit a1b2c3d4e5f6789 Author: someone"]);
        let gr: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::GitRef)
            .collect();
        assert_eq!(gr.len(), 1);
        assert_eq!(gr[0].text, "a1b2c3d4e5f6789");
    }

    #[test]
    fn detects_issue_ref() {
        let hotspots = detect(&["fixes #1234 in the code"]);
        let ir: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::IssueRef)
            .collect();
        assert_eq!(ir.len(), 1);
        assert_eq!(ir[0].text, "#1234");
    }

    #[test]
    fn detects_url() {
        let hotspots = detect(&["visit https://example.com/path?q=1 for info"]);
        let urls: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::Url)
            .collect();
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].text, "https://example.com/path?q=1");
    }

    #[test]
    fn rust_error_suppresses_duplicate_file_path() {
        let hotspots = detect(&["  --> src/main.rs:42:5"]);
        let el = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::ErrorLocation)
            .count();
        let fp = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .count();
        assert_eq!(el, 1);
        assert_eq!(fp, 0);
    }

    #[test]
    fn detects_ts_error_location() {
        let hotspots = detect(&["Error in src/app.ts(42,15): something failed"]);
        let el: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::ErrorLocation)
            .collect();
        assert_eq!(el.len(), 1);
        assert_eq!(el[0].text, "src/app.ts(42,15)");
    }

    #[test]
    fn detects_git_branch_current() {
        let hotspots = detect(&["* main"]);
        let gr: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::GitRef)
            .collect();
        assert_eq!(gr.len(), 1);
        assert_eq!(gr[0].text, "main");
    }

    #[test]
    fn detects_git_branch_with_separator() {
        let hotspots = detect(&["  feature/my-branch"]);
        let gr: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::GitRef)
            .collect();
        assert_eq!(gr.len(), 1);
        assert_eq!(gr[0].text, "feature/my-branch");
    }

    #[test]
    fn no_false_positive_on_indented_prose() {
        let hotspots = detect(&["  This is a normal paragraph line"]);
        let gr: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::GitRef)
            .collect();
        assert_eq!(gr.len(), 0, "indented prose should not match as git branch");
    }

    #[test]
    fn detects_issue_ref_prefix() {
        let hotspots = detect(&["see JIRA-456 for context"]);
        let ir: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::IssueRef)
            .collect();
        assert_eq!(ir.len(), 1);
        assert_eq!(ir[0].text, "JIRA-456");
    }

    #[test]
    fn multi_row_detection() {
        let hotspots = detect(&[
            "see /home/user/src/main.rs:1",
            "fixes #99 in commit abcdef1234567",
        ]);
        assert!(
            hotspots
                .iter()
                .any(|h| h.kind == HotspotKind::FilePath && h.row == 0)
        );
        assert!(
            hotspots
                .iter()
                .any(|h| h.kind == HotspotKind::IssueRef && h.row == 1)
        );
        assert!(
            hotspots
                .iter()
                .any(|h| h.kind == HotspotKind::GitRef && h.row == 1)
        );
    }

    #[test]
    fn no_false_positive_on_word_dot_word() {
        for line in &[
            "Searching for matches",
            "42 files updated",
            "Loaded config successfully",
            "Channeling energy",
            "version 2.0 released",
            "running at 1.5x speed",
        ] {
            let hotspots = detect(&[line]);
            let fp: Vec<_> = hotspots
                .iter()
                .filter(|h| h.kind == HotspotKind::FilePath)
                .collect();
            assert!(
                fp.is_empty(),
                "expected no file-path match in {line:?}, got {fp:?}"
            );
        }
    }

    #[test]
    fn detects_bare_filename_with_known_extension() {
        let hotspots = detect(&["edit Cargo.toml now"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1);
        assert_eq!(fp[0].text, "Cargo.toml");
    }

    #[test]
    fn detects_relative_path_with_line_col() {
        let hotspots = detect(&["see ./src/main.rs:42 here"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1);
        assert_eq!(fp[0].text, "./src/main.rs:42");
    }

    #[test]
    fn detects_absolute_python_path() {
        let hotspots = detect(&["open /home/user/foo.py please"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1);
        assert_eq!(fp[0].text, "/home/user/foo.py");
    }

    #[test]
    fn detects_nested_path_with_line_col() {
        let hotspots = detect(&["at crates/therminal-app/src/window/mod.rs:120:5 boom"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1);
        assert_eq!(fp[0].text, "crates/therminal-app/src/window/mod.rs:120:5");
    }

    #[test]
    fn wrapped_file_path_joins_across_rows() {
        // The exact tn-4mi0 / tn-a2qd fixture: a long path that hard-wraps
        // between two physical rows. After joining, the FULL path matches
        // and the underline spans both rows.
        let rows = vec![
            "see crates/therminal-app/src/window/eve".to_string(),
            "nt_handler.rs:42 boom".to_string(),
        ];
        let cont = vec![false, true];
        let hotspots = detect_hotspots_from_text_with_wrap(&rows, &cont);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 2, "expected 2 spans, got {fp:?}");
        let full = "crates/therminal-app/src/window/event_handler.rs:42";
        assert_eq!(fp[0].text, full);
        assert_eq!(fp[1].text, full);
        assert_eq!(fp[0].row, 0);
        assert_eq!(fp[0].start_col, 4);
        assert_eq!(fp[0].end_col, rows[0].chars().count());
        assert_eq!(fp[1].row, 1);
        assert_eq!(fp[1].start_col, 0);
        assert_eq!(fp[1].end_col, "nt_handler.rs:42".chars().count());
    }

    #[test]
    fn wrapped_url_joins_across_rows() {
        let rows = vec![
            "visit https://example.com/very/long/".to_string(),
            "path/to/file?query=foo done".to_string(),
        ];
        let cont = vec![false, true];
        let hotspots = detect_hotspots_from_text_with_wrap(&rows, &cont);
        let urls: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::Url)
            .collect();
        assert_eq!(urls.len(), 2, "expected 2 url spans, got {urls:?}");
        let full = "https://example.com/very/long/path/to/file?query=foo";
        assert_eq!(urls[0].text, full);
        assert_eq!(urls[1].text, full);
        assert_eq!(urls[0].row, 0);
        assert_eq!(urls[1].row, 1);
        assert_eq!(urls[1].start_col, 0);
    }

    #[test]
    fn non_continuation_rows_do_not_join() {
        let rows = vec![
            "edit ./src/main.rs:10 now".to_string(),
            "another line entirely".to_string(),
        ];
        let cont = vec![false, false];
        let hotspots = detect_hotspots_from_text_with_wrap(&rows, &cont);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1);
        assert_eq!(fp[0].text, "./src/main.rs:10");
        assert_eq!(fp[0].row, 0);
    }

    #[test]
    fn one_line_path_still_matches_with_wrap_array() {
        // Negative regression: passing wrap state must not break normal detection.
        let rows = vec!["edit ./src/main.rs:10 now".to_string()];
        let cont = vec![false];
        let hotspots = detect_hotspots_from_text_with_wrap(&rows, &cont);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1);
        assert_eq!(fp[0].text, "./src/main.rs:10");
    }

    #[test]
    fn hotspot_kind_as_str() {
        assert_eq!(HotspotKind::FilePath.as_str(), "file");
        assert_eq!(HotspotKind::ErrorLocation.as_str(), "file");
        assert_eq!(HotspotKind::GitRef.as_str(), "git_ref");
        assert_eq!(HotspotKind::IssueRef.as_str(), "issue");
        assert_eq!(HotspotKind::Url.as_str(), "url");
    }

    #[test]
    fn detect_defaults_is_dir_false() {
        // The text-only detection pass never touches the filesystem, so
        // freshly produced hotspots must always have is_dir = false.
        let hotspots = detect(&["see /home/user/src/main.rs:1 for context"]);
        assert!(hotspots.iter().all(|h| !h.is_dir));
    }

    #[test]
    fn strip_line_col_suffix_handles_all_shapes() {
        assert_eq!(strip_line_col_suffix("src/main.rs:42:5"), "src/main.rs");
        assert_eq!(strip_line_col_suffix("src/main.rs:42"), "src/main.rs");
        assert_eq!(strip_line_col_suffix("src/main.rs"), "src/main.rs");
        assert_eq!(strip_line_col_suffix("/tmp"), "/tmp");
        // Non-numeric colon suffix left alone.
        assert_eq!(strip_line_col_suffix("foo:bar"), "foo:bar");
    }

    #[test]
    fn promote_directory_hotspots_marks_only_filepath_dirs() {
        let mut hotspots = vec![
            TextHotspot {
                kind: HotspotKind::FilePath,
                text: "/tmp/some-dir".to_string(),
                row: 0,
                start_col: 0,
                end_col: 13,
                is_dir: false,
            },
            TextHotspot {
                kind: HotspotKind::FilePath,
                text: "/tmp/some-file.rs".to_string(),
                row: 1,
                start_col: 0,
                end_col: 17,
                is_dir: false,
            },
            TextHotspot {
                kind: HotspotKind::Url,
                text: "https://example.com".to_string(),
                row: 2,
                start_col: 0,
                end_col: 19,
                is_dir: false,
            },
        ];
        promote_directory_hotspots(&mut hotspots, |p| Ok(p == "/tmp/some-dir"));
        assert!(hotspots[0].is_dir, "directory should be promoted");
        assert!(!hotspots[1].is_dir, "file should not be promoted");
        assert!(!hotspots[2].is_dir, "URL must never be promoted");
    }

    #[test]
    fn promote_directory_hotspots_strips_line_col_before_stat() {
        // The path part of `src/dir:42:5` (a degenerate but possible
        // hotspot text) should be the only thing handed to the stat fn.
        let mut hotspots = vec![TextHotspot {
            kind: HotspotKind::FilePath,
            text: "src/dir:42:5".to_string(),
            row: 0,
            start_col: 0,
            end_col: 12,
            is_dir: false,
        }];
        let mut seen = String::new();
        promote_directory_hotspots(&mut hotspots, |p| {
            seen.clear();
            seen.push_str(p);
            Ok(true)
        });
        assert_eq!(seen, "src/dir");
        assert!(hotspots[0].is_dir);
    }

    #[test]
    fn promote_directory_hotspots_falls_through_to_heuristic_on_io_error() {
        // When stat fails, we fall through to the pure heuristic. A path
        // whose last component has an extension (`missing.rs`) must NOT be
        // promoted, because the heuristic correctly classifies it as a file.
        let mut hotspots = vec![TextHotspot {
            kind: HotspotKind::FilePath,
            text: "/nope/missing.rs".to_string(),
            row: 0,
            start_col: 0,
            end_col: 16,
            is_dir: false,
        }];
        promote_directory_hotspots(&mut hotspots, |_| Err(std::io::Error::other("nope")));
        assert!(!hotspots[0].is_dir);
    }

    #[test]
    fn promote_directory_hotspots_heuristic_promotes_extensionless_path_on_io_error() {
        // The whole point of tn-fqvx: a POSIX directory path on the Windows
        // build of therminal stat-fails, but the heuristic catches it.
        let mut hotspots = vec![TextHotspot {
            kind: HotspotKind::FilePath,
            text: "/home/marci/projects/therminal".to_string(),
            row: 0,
            start_col: 0,
            end_col: 30,
            is_dir: false,
        }];
        promote_directory_hotspots(&mut hotspots, |_| {
            Err(std::io::Error::other("cross-fs stat failure"))
        });
        assert!(
            hotspots[0].is_dir,
            "extensionless POSIX path must be heuristically promoted when stat fails"
        );
    }

    #[test]
    fn promote_directory_hotspots_trusts_ok_false() {
        // Definitive Ok(false) from the filesystem must NOT be overridden
        // by the heuristic.
        let mut hotspots = vec![TextHotspot {
            kind: HotspotKind::FilePath,
            text: "/tmp/extensionless-file".to_string(),
            row: 0,
            start_col: 0,
            end_col: 23,
            is_dir: false,
        }];
        promote_directory_hotspots(&mut hotspots, |_| Ok(false));
        assert!(
            !hotspots[0].is_dir,
            "Ok(false) from stat must be trusted over the heuristic"
        );
    }

    #[test]
    fn classify_path_as_directory_heuristic_trailing_slash() {
        assert!(classify_path_as_directory_heuristic("/home/marci/"));
        assert!(classify_path_as_directory_heuristic("src/"));
        assert!(classify_path_as_directory_heuristic("./foo.rs/"));
    }

    #[test]
    fn classify_path_as_directory_heuristic_extensionless_component() {
        assert!(classify_path_as_directory_heuristic(
            "/home/marci/projects/therminal"
        ));
        assert!(classify_path_as_directory_heuristic("/usr/bin"));
        // Bare tokens without a separator are treated as files (rule 3)
        // even when extensionless — `crates` and `src` alone could be
        // either, but in shell output they tend to appear with a path.
        assert!(!classify_path_as_directory_heuristic("crates"));
        assert!(!classify_path_as_directory_heuristic("src"));
        // ...but the same names with a separator stay directories.
        assert!(classify_path_as_directory_heuristic("./crates"));
        assert!(classify_path_as_directory_heuristic("foo/src"));
    }

    #[test]
    fn classify_path_as_directory_heuristic_file_with_extension() {
        assert!(!classify_path_as_directory_heuristic("/home/marci/foo.rs"));
        assert!(!classify_path_as_directory_heuristic("Cargo.toml"));
        assert!(!classify_path_as_directory_heuristic("src/main.rs"));
        assert!(!classify_path_as_directory_heuristic("/etc/hosts.allow"));
    }

    #[test]
    fn classify_path_as_directory_heuristic_dotfile_is_not_directory() {
        // Dotfiles like .bashrc have a `.` in the last component and must
        // be treated as files, not directories.
        assert!(!classify_path_as_directory_heuristic(".bashrc"));
        assert!(!classify_path_as_directory_heuristic("/home/marci/.bashrc"));
        assert!(!classify_path_as_directory_heuristic(".gitignore"));
    }

    #[test]
    fn classify_path_as_directory_heuristic_root_and_empty() {
        assert!(classify_path_as_directory_heuristic("/"));
        assert!(classify_path_as_directory_heuristic(""));
    }

    #[test]
    fn classify_path_as_directory_heuristic_makefile_like_no_extension() {
        // Bare tokens (no `/` separator) are treated as files even when
        // they have no extension. This catches `Makefile`, `README`,
        // `Dockerfile`, etc., which are overwhelmingly files in shell
        // output.
        assert!(!classify_path_as_directory_heuristic("Makefile"));
        assert!(!classify_path_as_directory_heuristic("README"));
        assert!(!classify_path_as_directory_heuristic("Dockerfile"));
    }
}
