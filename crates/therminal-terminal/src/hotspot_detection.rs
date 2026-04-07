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
}
