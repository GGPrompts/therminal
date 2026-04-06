#![allow(dead_code)]

//! Hotspot detection for actionable patterns in terminal output.
//!
//! Scans visible cells for file paths (with optional line:col), error locations
//! (Rust, TypeScript), git refs, and issue references. Follows the same
//! row-scanning pattern as `url_detection.rs`.

use std::sync::LazyLock;

use regex::Regex;

use crate::grid_renderer::RenderCell;

// ── Types ────────────────────────────────────────────────────────────────

/// The category of an actionable hotspot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotspotKind {
    /// A detected URL (delegated from url_detection, included for completeness).
    Url,
    /// A file path, optionally with `:line` or `:line:col` suffix.
    FilePath,
    /// A compiler/linter error location (e.g. Rust `error[E0123]: ... --> src/file.rs:42:5`).
    ErrorLocation,
    /// A git commit hash or branch name in git-context output.
    GitRef,
    /// An issue/PR reference like `#1234` or `PREFIX-123`.
    IssueRef,
}

/// A single actionable hotspot detected in the visible terminal grid.
#[derive(Clone, Debug)]
pub struct Hotspot {
    /// What kind of actionable item this is.
    pub kind: HotspotKind,
    /// The matched text.
    pub text: String,
    /// Viewport row (0-based).
    pub row: usize,
    /// First column of the match (0-based, inclusive).
    pub start_col: usize,
    /// One-past-last column of the match (exclusive).
    pub end_col: usize,
}

// ── Compiled regexes ─────────────────────────────────────────────────────

/// File path with optional `:line` or `:line:col`.
/// Matches absolute (`/foo/bar.rs:42:5`), relative (`./src/main.rs:10`),
/// and bare (`src/lib.rs:7`) paths. Requires a dot-extension to avoid
/// false positives on random slash-separated text.
static FILE_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:\.{0,2}/)?(?:[A-Za-z0-9_\-]+/)*[A-Za-z0-9_\-]+\.[A-Za-z0-9_]+(?::\d+(?::\d+)?)?",
    )
    .unwrap()
});

/// Rust-style error location: `  --> src/file.rs:42:5`
static RUST_ERROR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-->\s+((?:[A-Za-z0-9_\-]+/)*[A-Za-z0-9_\-]+\.[A-Za-z0-9_]+:\d+:\d+)").unwrap()
});

/// TypeScript/C#-style error location: `file.ts(42,15)`
static TS_ERROR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([A-Za-z0-9_\-]+(?:/[A-Za-z0-9_\-]+)*\.[A-Za-z0-9_]+)\((\d+),(\d+)\)").unwrap()
});

/// Git short/long hash: 7–40 hex characters, word-bounded.
static GIT_HASH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9a-f]{7,40}\b").unwrap());

/// Git branch name in `git branch` output: lines starting with `* ` or `  `.
static GIT_BRANCH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[* ] {1,2}([A-Za-z0-9_/\-\.]+)").unwrap());

/// Issue/PR reference: `#1234` or `PREFIX-123` (2–8 uppercase prefix).
static ISSUE_REF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\b[A-Z]{2,8}-\d+\b|#\d+\b)").unwrap());

// ── Detection entry point ────────────────────────────────────────────────

/// Scan visible cells for actionable hotspots.
///
/// Groups cells by row, reconstructs row text, runs each detection regex,
/// and returns all matches with their grid coordinates.
pub(crate) fn detect_hotspots(cells: &[RenderCell], screen_lines: usize) -> Vec<Hotspot> {
    let mut hotspots = Vec::new();

    // Group cell indices by row.
    let mut row_indices: Vec<Vec<usize>> = vec![Vec::new(); screen_lines];
    for (i, cell) in cells.iter().enumerate() {
        if cell.row < screen_lines {
            row_indices[cell.row].push(i);
        }
    }

    for (row, row_idxs) in row_indices.iter().enumerate() {
        if row_idxs.is_empty() {
            continue;
        }

        let max_col = row_idxs.iter().map(|&i| cells[i].col).max().unwrap_or(0);

        // Build row text and byte-offset-to-column mapping.
        let mut row_chars: Vec<char> = vec![' '; max_col + 1];
        for &idx in row_idxs {
            row_chars[cells[idx].col] = cells[idx].c;
        }
        let text: String = row_chars.iter().collect();

        // byte offset -> column index
        let char_starts: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();

        // Helper: convert byte range to column range.
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

        // ── Rust error locations (`--> file.rs:42:5`) ────────────────
        for cap in RUST_ERROR_RE.captures_iter(&text) {
            if let Some(m) = cap.get(1) {
                let (sc, ec) = byte_to_cols(m.start(), m.end());
                hotspots.push(Hotspot {
                    kind: HotspotKind::ErrorLocation,
                    text: m.as_str().to_owned(),
                    row,
                    start_col: sc,
                    end_col: ec,
                });
            }
        }

        // ── TypeScript/C# error locations (`file.ts(42,15)`) ─────────
        for cap in TS_ERROR_RE.captures_iter(&text) {
            if let Some(m) = cap.get(0) {
                let (sc, ec) = byte_to_cols(m.start(), m.end());
                hotspots.push(Hotspot {
                    kind: HotspotKind::ErrorLocation,
                    text: m.as_str().to_owned(),
                    row,
                    start_col: sc,
                    end_col: ec,
                });
            }
        }

        // ── File paths (skip ranges already covered by error locations) ──
        for m in FILE_PATH_RE.find_iter(&text) {
            let (sc, ec) = byte_to_cols(m.start(), m.end());
            // Skip if this range overlaps with an already-detected error location.
            let dominated = hotspots.iter().any(|h| {
                h.row == row
                    && h.kind == HotspotKind::ErrorLocation
                    && h.start_col <= sc
                    && h.end_col >= ec
            });
            if dominated {
                continue;
            }
            // Require at least one `/` or `.\` to be a plausible path (not just `foo.bar`).
            let txt = m.as_str();
            if !txt.contains('/') {
                continue;
            }
            hotspots.push(Hotspot {
                kind: HotspotKind::FilePath,
                text: txt.to_owned(),
                row,
                start_col: sc,
                end_col: ec,
            });
        }

        // ── Git branch names ─────────────────────────────────────────
        for cap in GIT_BRANCH_RE.captures_iter(&text) {
            if let Some(m) = cap.get(1) {
                let (sc, ec) = byte_to_cols(m.start(), m.end());
                hotspots.push(Hotspot {
                    kind: HotspotKind::GitRef,
                    text: m.as_str().to_owned(),
                    row,
                    start_col: sc,
                    end_col: ec,
                });
            }
        }

        // ── Git hashes ──────────────────────────────────────────────
        for m in GIT_HASH_RE.find_iter(&text) {
            let (sc, ec) = byte_to_cols(m.start(), m.end());
            // Skip if overlapping with a branch name already detected.
            let dominated = hotspots
                .iter()
                .any(|h| h.row == row && h.start_col <= sc && h.end_col >= ec);
            if dominated {
                continue;
            }
            hotspots.push(Hotspot {
                kind: HotspotKind::GitRef,
                text: m.as_str().to_owned(),
                row,
                start_col: sc,
                end_col: ec,
            });
        }

        // ── Issue references (`#123`, `PREFIX-456`) ──────────────────
        for m in ISSUE_REF_RE.find_iter(&text) {
            let (sc, ec) = byte_to_cols(m.start(), m.end());
            hotspots.push(Hotspot {
                kind: HotspotKind::IssueRef,
                text: m.as_str().to_owned(),
                row,
                start_col: sc,
                end_col: ec,
            });
        }
    }

    hotspots
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::vte::ansi::Color as AnsiColor;

    /// Helper: build a row of RenderCells from a string at the given row index.
    fn cells_from_str(row: usize, s: &str) -> Vec<RenderCell> {
        s.chars()
            .enumerate()
            .map(|(col, c)| RenderCell {
                row,
                col,
                c,
                text: c.to_string(),
                fg: AnsiColor::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
                bg: AnsiColor::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
                flags: Flags::empty(),
                hyperlink: None,
                hyperlink_source: None,
                hotspot: None,
            })
            .collect()
    }

    #[test]
    fn detects_absolute_file_path_with_line_col() {
        let cells = cells_from_str(0, "error at /home/user/src/main.rs:42:5 here");
        let hotspots = detect_hotspots(&cells, 1);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1);
        assert_eq!(fp[0].text, "/home/user/src/main.rs:42:5");
    }

    #[test]
    fn detects_relative_file_path() {
        let cells = cells_from_str(0, "see ./src/lib.rs:10 for details");
        let hotspots = detect_hotspots(&cells, 1);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1);
        assert_eq!(fp[0].text, "./src/lib.rs:10");
    }

    #[test]
    fn detects_rust_error_location() {
        let cells = cells_from_str(0, "  --> crates/therminal-app/src/main.rs:42:5");
        let hotspots = detect_hotspots(&cells, 1);
        let el: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::ErrorLocation)
            .collect();
        assert_eq!(el.len(), 1);
        assert_eq!(el[0].text, "crates/therminal-app/src/main.rs:42:5");
    }

    #[test]
    fn detects_ts_error_location() {
        let cells = cells_from_str(0, "Error in src/app.ts(42,15): something failed");
        let hotspots = detect_hotspots(&cells, 1);
        let el: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::ErrorLocation)
            .collect();
        assert_eq!(el.len(), 1);
        assert_eq!(el[0].text, "src/app.ts(42,15)");
    }

    #[test]
    fn detects_git_hash() {
        let cells = cells_from_str(0, "commit a1b2c3d4e5f6789 Author: someone");
        let hotspots = detect_hotspots(&cells, 1);
        let gr: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::GitRef)
            .collect();
        assert_eq!(gr.len(), 1);
        assert_eq!(gr[0].text, "a1b2c3d4e5f6789");
    }

    #[test]
    fn detects_git_branch() {
        let cells = cells_from_str(0, "* main");
        let hotspots = detect_hotspots(&cells, 1);
        let gr: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::GitRef)
            .collect();
        assert_eq!(gr.len(), 1);
        assert_eq!(gr[0].text, "main");
    }

    #[test]
    fn detects_issue_ref_hash() {
        let cells = cells_from_str(0, "fixes #1234 in the code");
        let hotspots = detect_hotspots(&cells, 1);
        let ir: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::IssueRef)
            .collect();
        assert_eq!(ir.len(), 1);
        assert_eq!(ir[0].text, "#1234");
    }

    #[test]
    fn detects_issue_ref_prefix() {
        let cells = cells_from_str(0, "see JIRA-456 for context");
        let hotspots = detect_hotspots(&cells, 1);
        let ir: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::IssueRef)
            .collect();
        assert_eq!(ir.len(), 1);
        assert_eq!(ir[0].text, "JIRA-456");
    }

    #[test]
    fn rust_error_suppresses_duplicate_file_path() {
        let cells = cells_from_str(0, "  --> src/main.rs:42:5");
        let hotspots = detect_hotspots(&cells, 1);
        // Should get ErrorLocation but NOT a duplicate FilePath.
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
        let mut cells = cells_from_str(0, "see /home/user/src/main.rs:1");
        cells.extend(cells_from_str(1, "fixes #99 in commit abcdef1234567"));
        let hotspots = detect_hotspots(&cells, 2);
        assert!(hotspots
            .iter()
            .any(|h| h.kind == HotspotKind::FilePath && h.row == 0));
        assert!(hotspots
            .iter()
            .any(|h| h.kind == HotspotKind::IssueRef && h.row == 1));
        assert!(hotspots
            .iter()
            .any(|h| h.kind == HotspotKind::GitRef && h.row == 1));
    }
}
