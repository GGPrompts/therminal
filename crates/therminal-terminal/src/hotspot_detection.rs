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
    pub line: usize,
    /// First column of the match (0-based, inclusive).
    pub col_start: usize,
    /// One-past-last column of the match (exclusive).
    pub col_end: usize,
}

// ── Compiled regexes ─────────────────────────────────────────────────────

/// File path with optional `:line` or `:line:col`.
/// Matches absolute (`/foo/bar.rs:42:5`), relative (`./src/main.rs:10`),
/// and bare (`src/lib.rs:7`) paths. Requires a dot-extension to avoid
/// false positives on random slash-separated text.
pub static FILE_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:\.{0,2}/)?(?:[A-Za-z0-9_\-]+/)*[A-Za-z0-9_\-]+\.[A-Za-z0-9_]+(?::\d+(?::\d+)?)?",
    )
    .unwrap()
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
    let mut hotspots = Vec::new();

    for (line, text) in rows.iter().enumerate() {
        if text.trim().is_empty() {
            continue;
        }

        // byte offset -> column index
        let char_starts: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();

        let byte_to_cols = |byte_start: usize, byte_end: usize| -> (usize, usize) {
            let start_col = char_starts
                .iter()
                .position(|&b| b == byte_start)
                .unwrap_or(0);
            let end_col = char_starts
                .iter()
                .position(|&b| b >= byte_end)
                .unwrap_or(char_starts.len());
            (start_col, end_col)
        };

        // URLs first (highest priority, least ambiguous).
        for m in URL_RE.find_iter(text) {
            let (sc, ec) = byte_to_cols(m.start(), m.end());
            hotspots.push(TextHotspot {
                kind: HotspotKind::Url,
                text: m.as_str().to_string(),
                line,
                col_start: sc,
                col_end: ec,
            });
        }

        // Rust error locations (`--> file.rs:42:5`).
        for cap in RUST_ERROR_RE.captures_iter(text) {
            if let Some(m) = cap.get(1) {
                let (sc, ec) = byte_to_cols(m.start(), m.end());
                hotspots.push(TextHotspot {
                    kind: HotspotKind::ErrorLocation,
                    text: m.as_str().to_string(),
                    line,
                    col_start: sc,
                    col_end: ec,
                });
            }
        }

        // TypeScript/C# error locations (`file.ts(42,15)`).
        for cap in TS_ERROR_RE.captures_iter(text) {
            if let Some(m) = cap.get(0) {
                let (sc, ec) = byte_to_cols(m.start(), m.end());
                hotspots.push(TextHotspot {
                    kind: HotspotKind::ErrorLocation,
                    text: m.as_str().to_string(),
                    line,
                    col_start: sc,
                    col_end: ec,
                });
            }
        }

        // File paths (skip ranges already covered by error locations).
        for m in FILE_PATH_RE.find_iter(text) {
            let (sc, ec) = byte_to_cols(m.start(), m.end());
            let dominated = hotspots.iter().any(|h| {
                h.line == line
                    && (h.kind == HotspotKind::ErrorLocation || h.kind == HotspotKind::Url)
                    && h.col_start <= sc
                    && h.col_end >= ec
            });
            if dominated {
                continue;
            }
            let txt = m.as_str();
            if !txt.contains('/') {
                continue;
            }
            hotspots.push(TextHotspot {
                kind: HotspotKind::FilePath,
                text: txt.to_string(),
                line,
                col_start: sc,
                col_end: ec,
            });
        }

        // Git branch names.
        for cap in GIT_BRANCH_RE.captures_iter(text) {
            let m = cap.get(1).or_else(|| cap.get(2));
            if let Some(m) = m {
                let (sc, ec) = byte_to_cols(m.start(), m.end());
                hotspots.push(TextHotspot {
                    kind: HotspotKind::GitRef,
                    text: m.as_str().to_string(),
                    line,
                    col_start: sc,
                    col_end: ec,
                });
            }
        }

        // Git hashes.
        for m in GIT_HASH_RE.find_iter(text) {
            let (sc, ec) = byte_to_cols(m.start(), m.end());
            let dominated = hotspots
                .iter()
                .any(|h| h.line == line && h.col_start <= sc && h.col_end >= ec);
            if dominated {
                continue;
            }
            hotspots.push(TextHotspot {
                kind: HotspotKind::GitRef,
                text: m.as_str().to_string(),
                line,
                col_start: sc,
                col_end: ec,
            });
        }

        // Issue references (`#123`, `PREFIX-456`).
        for m in ISSUE_REF_RE.find_iter(text) {
            let (sc, ec) = byte_to_cols(m.start(), m.end());
            hotspots.push(TextHotspot {
                kind: HotspotKind::IssueRef,
                text: m.as_str().to_string(),
                line,
                col_start: sc,
                col_end: ec,
            });
        }
    }

    hotspots
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
    fn multi_row_detection() {
        let hotspots = detect(&[
            "see /home/user/src/main.rs:1",
            "fixes #99 in commit abcdef1234567",
        ]);
        assert!(
            hotspots
                .iter()
                .any(|h| h.kind == HotspotKind::FilePath && h.line == 0)
        );
        assert!(
            hotspots
                .iter()
                .any(|h| h.kind == HotspotKind::IssueRef && h.line == 1)
        );
        assert!(
            hotspots
                .iter()
                .any(|h| h.kind == HotspotKind::GitRef && h.line == 1)
        );
    }

    #[test]
    fn hotspot_kind_as_str() {
        assert_eq!(HotspotKind::FilePath.as_str(), "file");
        assert_eq!(HotspotKind::ErrorLocation.as_str(), "file");
        assert_eq!(HotspotKind::GitRef.as_str(), "git_ref");
        assert_eq!(HotspotKind::IssueRef.as_str(), "issue");
        assert_eq!(HotspotKind::Url.as_str(), "url");
    }
}
