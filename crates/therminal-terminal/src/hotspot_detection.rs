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
    /// Optional provenance tag for pattern-pack-sourced hotspots (tn-yrjd).
    ///
    /// `None` for built-in hotspots emitted by the regex detectors above.
    /// `Some(PatternHotspotSource)` when the hotspot was synthesised from
    /// a `PatternEngine` match via [`hotspots_from_pattern_matches`]. The
    /// click handler uses this to short-circuit the standard pipeline
    /// (resolve-against-cwd plus editor-fallback chain) when the pattern
    /// already resolved a target URL.
    pub pattern_source: Option<PatternHotspotSource>,
}

/// Provenance data attached to a pattern-pack-sourced hotspot.
///
/// Carries everything the click handler needs to route the action without
/// doing its own filesystem dance: pack+rule identity (for debug logs and
/// emit_event routing), the raw on_click kind, the resolved target (URL
/// or file path), and the tooltip label.
#[derive(Clone, Debug)]
pub struct PatternHotspotSource {
    /// Originating pack — matches `PatternMatch::pack_name`.
    pub pack_name: String,
    /// Originating pattern rule name.
    pub rule_name: String,
    /// `"open_editor"`, `"open_url"`, or `"emit_event"`.
    pub on_click: String,
    /// Resolved `target` template (tilde- and capture-expanded).
    pub target: Option<String>,
    /// Resolved `label` template. Used as the tooltip on hover.
    pub label: Option<String>,
    /// The `kind` field declared on the pattern (defaults to `"pattern"`).
    pub declared_kind: String,
}

// ── Compiled regexes ─────────────────────────────────────────────────────

/// File path with optional `:line` or `:line:col`.
///
/// Matches EITHER paths with a leading prefix (`./`, `../`, `/`, `~/`, `~user/`)
/// OR any path whose final segment ends in a known source/config/doc
/// extension from a curated whitelist. This avoids false positives on
/// plain `word.word` tokens (e.g. `2.0`, `1.5x`, `Loaded.files`).
///
/// The regex itself deliberately over-matches — the real work happens in
/// [`is_plausible_file_path_match`], which checks the characters immediately
/// before and after the match to reject:
///
/// - Multi-segment identifiers with no anchor and no extension (`a/b/c`,
///   `foo/bar/baz`, `path-like/identifier`, `HTTP/1.1`).
/// - Matches that live inside markdown emphasis (`*foo.rs*`, `_bar.py_`,
///   `**baz.md**`).
/// - Matches that are glued to surrounding word characters (`_under.rs_`,
///   `foo.rs.backup`, `github.c` after `@`).
///
/// Extensions are listed longest-first so the regex engine's leftmost-first
/// alternation semantics pick the longest match — without this, `package.json`
/// would match as `package.js` (the `js` alternative would win, consuming
/// only `.js` and leaving `on`). Single-letter extensions (`c`, `h`) stay at
/// the tail because they can only match inside a properly-bounded candidate.
///
/// `~` is kept in the match (it's later expanded to `$HOME` by
/// [`expand_tilde`] when the click handler resolves the path). The
/// `~user/` form is included because POSIX tools print it in shell
/// output; the `user` segment matches the same `[A-Za-z0-9_-]+` charset
/// as a path segment and must be followed by `/`.
pub static FILE_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Extensions sorted longest-first so the alternation picks the longest
    // match under regex's leftmost-first semantics. Multi-char extensions
    // are applied to any stem with at least one path-word char; single-char
    // extensions (`c`, `h`) additionally require the stem to contain at
    // least one ASCII letter, so `see section 2.h for details` doesn't
    // match `2.h` as a path. Keep the lists in sync with the module docs
    // and tests.
    //
    // Extensions that look like common filenames (`dockerfile`,
    // `gitignore`) are accepted as a suffix (`Foo.dockerfile`) but the
    // bare-filename matcher only gets to see them as post-`.` text —
    // `Dockerfile` alone is not a path for the purposes of this regex.
    const EXT_MULTI: &str = concat!(
        // 10-9 chars
        r"dockerfile|gitignore|",
        // 5 chars
        r"swift|",
        // 4 chars
        r"yaml|toml|bash|fish|json|html|scss|lock|java|",
        // 3 chars
        r"tsx|jsx|mjs|cjs|yml|css|zsh|ps1|hpp|cpp|php|lua|sql|xml|log|env|txt|",
        // 2 chars
        r"rs|ts|js|py|md|sh|go|kt|rb",
    );
    const EXT_SINGLE: &str = r"c|h";
    // Anchored branch: any of the known extensions may appear. Ordered
    // multi-first so that `package.json` wins over `package.js`.
    let ext_any = format!("{EXT_MULTI}|{EXT_SINGLE}");
    let pat = format!(
        concat!(
            // Branch A: anchored (./, ../, /, ~/, ~user/) — any ext OK.
            r"(?:",
            r"(?:\.{{1,2}}/|/|~(?:[A-Za-z0-9_\-]+)?/)",
            r"(?:[A-Za-z0-9_\-.]+/)*",
            r"[A-Za-z0-9_\-.]+",
            r"(?:\.(?:{ext_any}))?",
            r"|",
            // Branch B: unanchored + multi-char extension. Any stem is fine.
            r"(?:[A-Za-z0-9_\-.]+/)*[A-Za-z0-9_\-]+\.(?:{ext_multi})",
            r"|",
            // Branch C: unanchored + single-char extension. The stem must
            // contain at least one ASCII letter, so `2.h`, `1.c`, `9.h`
            // (common in prose like "see section 2.h") are rejected by
            // the regex itself instead of relying on post-filtering.
            r"(?:[A-Za-z0-9_\-.]+/)*[A-Za-z0-9_\-]*[A-Za-z][A-Za-z0-9_\-]*\.(?:{ext_single})",
            r"|",
            // Branch D: Windows drive-letter absolute paths — C:\foo\bar.rs,
            // C:/foo/bar.rs. Both forward-slash and backslash separators are
            // accepted. A known file extension is required (same whitelist as
            // branches B/C) to avoid false positives on `C:\` alone.
            r"[A-Za-z]:[\\/]",
            r"(?:[A-Za-z0-9_\-.]+[\\/])*",
            r"[A-Za-z0-9_\-.]+",
            r"(?:\.(?:{ext_any}))?",
            r")",
            // Optional :line[:col] suffix applies to all four branches.
            r"(?::\d+(?::\d+)?)?",
        ),
        ext_any = ext_any,
        ext_multi = EXT_MULTI,
        ext_single = EXT_SINGLE,
    );
    Regex::new(&pat).unwrap()
});

/// Validate a candidate `FILE_PATH_RE` match against its surrounding context.
///
/// The regex itself over-matches — it will happily grab `/b/c` out of
/// `a/b/c`, `foo.rs` out of `*foo.rs*`, and `/identifier` out of
/// `path-like/identifier`. This function inspects the byte immediately
/// before `start` and the byte immediately after `end` (using the
/// `joined` text the match was found in) and returns `true` only when:
///
/// 1. The candidate has a leading anchor (`./`, `../`, `/`, `~/`, `~user/`),
///    OR the last segment ends in a known file extension. This is enforced
///    by the regex itself — anything that reaches this function already
///    satisfies at least one of those branches.
/// 2. The preceding character (if any) is a plausible left boundary —
///    start of string, whitespace, or one of the opener/punctuation
///    characters listed in [`is_left_boundary`]. This rejects matches
///    glued to a preceding word character (`foo/bar` → reject `/bar`),
///    preceded by `@` (`git@github.c` → reject), or preceded by `.`
///    (`a.b.c` → reject the inner `b.c`).
/// 3. The trailing character (if any) is a plausible right boundary —
///    end of string, whitespace, or one of the closer/punctuation
///    characters listed in [`is_right_boundary`]. This rejects markdown
///    emphasis (`_foo.rs_`), words that continue past the match
///    (`foo.rs.backup` → reject because the `.` is followed by a word
///    char), and general word-char run-on.
///
/// The function operates on the raw joined text used by
/// [`detect_hotspots_from_text_with_wrap`] so it can be applied uniformly
/// to multi-row wrapped matches.
fn is_plausible_file_path_match(joined: &str, start: usize, end: usize) -> bool {
    let left_ok = match joined[..start].chars().next_back() {
        None => true,
        Some(c) => is_left_boundary(c),
    };
    if !left_ok {
        return false;
    }
    let tail = &joined[end..];
    let mut tail_chars = tail.chars();
    let right_ok = match tail_chars.next() {
        None => true,
        Some('.') => {
            // A trailing `.` is only allowed if it's end-of-sentence
            // punctuation — i.e. NOT followed by a word char. This lets
            // `see src/main.rs.` match while rejecting `foo.rs.bak`
            // (where the `.` is followed by more path content).
            match tail_chars.next() {
                None => true,
                Some(next) => !is_path_word_char(next),
            }
        }
        Some(c) => is_right_boundary(c),
    };
    if !right_ok {
        return false;
    }
    true
}

/// Characters allowed immediately before a file-path match.
///
/// Start of string is also allowed (handled by the caller). This list
/// deliberately excludes:
///
/// - Word characters (`A-Za-z0-9_`) — to prevent `foo/bar` matching
///   `/bar` and `_under.rs` matching as a path.
/// - `.` — to prevent `a.b.c` matching the inner `b.c`.
/// - `/`, `-`, `~` — which would indicate the candidate is a continuation
///   of a longer token the regex didn't consume.
/// - `@` — SCP-style URIs (`git@github.com:...`) and email addresses.
/// - `*` — markdown emphasis (`*foo.rs*`).
/// - `:` — to reject the right-hand side of SCP-style URIs
///   (`user@host:/tmp/file.txt`) without also rejecting shell-prompt
///   punctuation (`modified:   src/lib.rs` has a space between `:` and
///   the path, so the boundary char is the space).
fn is_left_boundary(c: char) -> bool {
    if c.is_whitespace() {
        return true;
    }
    matches!(
        c,
        '(' | '[' | '{' | '<' | '"' | '\'' | '`' | '=' | ',' | ';' | '!' | '?'
    )
}

/// Characters allowed immediately after a file-path match.
///
/// End of string is also allowed (handled by the caller). A trailing `.`
/// is handled specially by the caller (allowed only if followed by a
/// non-word char). This list deliberately excludes:
///
/// - Word characters (`A-Za-z0-9_`) — to prevent `foo.rs.backup` matching
///   only `foo.rs` when the user meant a longer path, and to prevent
///   markdown `_foo.rs_` from matching.
/// - `/`, `-`, `~` — continuations of a longer token.
/// - `*` — markdown emphasis (`*foo.rs*`).
/// - `@` — ambiguous, usually means "not a simple path".
fn is_right_boundary(c: char) -> bool {
    if c.is_whitespace() {
        return true;
    }
    matches!(
        c,
        ')' | ']' | '}' | '>' | '"' | '\'' | '`' | ',' | ';' | '!' | '?' | ':'
    )
}

/// Is `c` a word char in the "part of a path segment" sense?
///
/// Used by [`is_plausible_file_path_match`] when deciding whether a
/// trailing `.` counts as end-of-sentence punctuation or as a continuation
/// into another segment.
fn is_path_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

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
                    pattern_source: None,
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

        // File paths. The regex over-matches on purpose; the real
        // filtering happens in `is_plausible_file_path_match`, which
        // inspects the characters on either side of the candidate to
        // reject `a/b/c`, markdown emphasis, SCP URIs, and similar
        // non-path strings.
        for m in FILE_PATH_RE.find_iter(&joined) {
            if !is_plausible_file_path_match(&joined, m.start(), m.end()) {
                continue;
            }
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
                        pattern_source: None,
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
    // Resolve `$HOME` once per call so tilde-prefixed hotspots stat
    // against the real filesystem instead of a literal `~/...`. The
    // `~user/...` form is left un-expanded here and falls through to
    // the heuristic (see [`expand_tilde`]).
    //
    // On Windows we intentionally pass `None` for `home`: the host
    // process's `$HOME` points at the Windows user profile
    // (`C:\Users\<user>`), but the hotspots in a WSL pane refer to the
    // Linux `$HOME` (`/home/<user>`) which the Windows process cannot
    // see. Expanding `~` against the wrong home just produces a
    // Windows path that the stat below can't classify and the
    // heuristic rejects (rule 3 — no forward-slash separator). Leave
    // the literal `~/...` untouched and let the heuristic handle it.
    let home = if cfg!(windows) {
        None
    } else {
        std::env::var("HOME").ok()
    };
    for h in hotspots.iter_mut() {
        if h.kind != HotspotKind::FilePath {
            continue;
        }
        let path = strip_line_col_suffix(&h.text);
        let expanded = expand_tilde(path, home.as_deref());
        match is_dir_fn(expanded.as_ref()) {
            Ok(true) => h.is_dir = true,
            Ok(false) => {} // filesystem says no — trust it
            Err(_) => {
                // stat failed (most often: path lives on a filesystem the
                // host can't see, e.g. WSL paths on a Windows therminal
                // build). Fall through to a pure heuristic so the user can
                // still get the folder context menu.
                if classify_path_as_directory_heuristic(expanded.as_ref()) {
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

/// Expand a leading `~/` or `~user/` prefix into an absolute path.
///
/// Pure: `home_dir` is passed in (typically `std::env::var("HOME")`)
/// so tests can stub it without touching the environment. Only the
/// leading prefix is inspected, so callers can pass a raw hotspot
/// string with or without a trailing `:line[:col]` suffix.
///
/// - `~/foo/bar` with home=`/home/marci` → `/home/marci/foo/bar`
/// - `~` with home=`/home/marci` → `/home/marci`
/// - `~alice/foo` → unchanged. We don't call `getpwnam`, and shell
///   output showing `~alice/foo` is overwhelmingly informational;
///   the click handler will still attempt the literal path and
///   toast on failure.
/// - anything without a leading `~` → unchanged
///
/// Returns a `Cow` so the no-op case doesn't allocate.
/// Return true if `path` is a Windows-absolute path that should be
/// treated as already-absolute by [`resolve_relative_to_cwd`].
///
/// Matches three shapes:
/// - Drive-letter with either separator: `C:\foo`, `D:/bar`
/// - UNC server paths: `\\server\share\…`
/// - Forward-slash UNC variant: `//server/share/…`
///
/// A bare `C:` (no separator) is intentionally not matched because it
/// is Windows-relative (the cwd on drive C), not absolute. A stray
/// `\\` without a following name is rejected for the same reason.
///
/// Pure, zero-allocation, works on any build. Used so Windows-absolute
/// paths pulled out of shell output don't get incorrectly joined
/// against a Linux-style cwd when therminal is running on native
/// Windows with a WSL shell. See tn-jci5.
pub fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    // Drive-letter form: `[A-Za-z]:[\\/]…`
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    // UNC form: `\\…` or `//…` with at least one non-separator char
    // after the double separator.
    if bytes.len() >= 3
        && ((bytes[0] == b'\\' && bytes[1] == b'\\') || (bytes[0] == b'/' && bytes[1] == b'/'))
        && bytes[2] != b'\\'
        && bytes[2] != b'/'
    {
        return true;
    }
    false
}

pub fn expand_tilde<'a>(path: &'a str, home_dir: Option<&str>) -> std::borrow::Cow<'a, str> {
    use std::borrow::Cow;
    if path == "~" {
        if let Some(home) = home_dir {
            return Cow::Owned(home.to_string());
        }
        return Cow::Borrowed(path);
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir
    {
        let trimmed = home.trim_end_matches('/');
        return Cow::Owned(format!("{trimmed}/{rest}"));
    }
    Cow::Borrowed(path)
}

/// Join a relative `path` against `cwd` into an absolute path.
///
/// Leaves absolute, `~`-prefixed, and `cwd.is_none()` paths unchanged.
/// Handles `./`, `../`, and bare relative prefixes. Normalizes leading
/// `../` segments against `cwd` but does NOT touch the filesystem.
/// Preserves any trailing `:line[:col]` suffix verbatim.
///
/// Used by the click handlers to resolve shell-relative hotspots (like
/// `./src/main.rs:42`) against the focused pane's OSC 7 cwd, since
/// therminal's own process cwd is almost never what the user's shell
/// has `cd`'d to.
///
/// - `/foo/bar` → unchanged (already absolute)
/// - `~/foo` → unchanged (caller should call [`expand_tilde`] first)
/// - `cwd.is_none()` → unchanged (no basis to resolve against)
/// - `./foo` + cwd `/home/x` → `/home/x/foo`
/// - `../foo` + cwd `/home/x/y` → `/home/x/foo`
/// - `foo/bar.rs` + cwd `/home/x` → `/home/x/foo/bar.rs`
///
/// Returns a `Cow` so no-op branches are allocation-free.
pub fn resolve_relative_to_cwd<'a>(path: &'a str, cwd: Option<&str>) -> std::borrow::Cow<'a, str> {
    use std::borrow::Cow;

    // Already absolute (Unix) — nothing to do.
    if path.starts_with('/') {
        return Cow::Borrowed(path);
    }
    // Windows-absolute paths — `C:\…`, `C:/…`, and UNC `\\server\…`
    // or `\\wsl.localhost\…`. These are absolute in Windows terms even
    // though they don't start with `/`, so joining them against a
    // Unix-style `cwd` (e.g. a WSL pane's OSC 7) produces nonsense like
    // `/home/marci/C:\Users\…`. Treat them as already-absolute (tn-jci5,
    // partial — full platform-aware join is still tracked under that
    // issue).
    if is_windows_absolute(path) {
        return Cow::Borrowed(path);
    }
    // Caller is responsible for `~` expansion first; we leave it alone.
    if path.starts_with('~') {
        return Cow::Borrowed(path);
    }
    // Empty path is a no-op.
    if path.is_empty() {
        return Cow::Borrowed(path);
    }
    // Without a cwd we can't resolve anything.
    let Some(cwd) = cwd else {
        return Cow::Borrowed(path);
    };

    // Split off a trailing `:line[:col]` suffix so we only operate on
    // the path prefix, then re-attach it verbatim at the end.
    let prefix = strip_line_col_suffix(path);
    let suffix = &path[prefix.len()..];

    // Strip a leading `./` — it's noise for the join.
    let mut rel = prefix.strip_prefix("./").unwrap_or(prefix);

    // Pop one `cwd` segment per leading `../`. We trim the trailing
    // slash first so `/home/x/y/` and `/home/x/y` behave identically.
    // When `base` runs out of segments we pin at the empty string and
    // the final join re-adds a single leading `/`, so `../../` above
    // root stays at root instead of emitting `//foo`.
    let mut base: &str = cwd.trim_end_matches('/');
    while let Some(rest) = rel.strip_prefix("../") {
        base = match base.rsplit_once('/') {
            Some((parent, _)) => parent,
            None => "",
        };
        rel = rest;
    }
    // A bare `..` (no trailing slash) pops one more segment from base.
    if rel == ".." {
        base = match base.rsplit_once('/') {
            Some((parent, _)) => parent,
            None => "",
        };
        rel = "";
    } else if rel == "." {
        // A bare `.` resolves to `base` itself.
        rel = "";
    }

    // Glue. Handle the root edge case so we don't emit `//foo` or a
    // trailing slash for an empty relative part.
    let joined = match (base.is_empty(), rel.is_empty()) {
        (true, true) => "/".to_string() + suffix,
        (true, false) => format!("/{rel}{suffix}"),
        (false, true) => format!("{base}{suffix}"),
        (false, false) => format!("{base}/{rel}{suffix}"),
    };
    Cow::Owned(joined)
}

// ── Pattern-engine bridge ───────────────────────────────────────────────
//
// Convert `PatternEngine` hotspot-action matches into `TextHotspot`s so
// pattern-sourced hotspots coexist with the built-in file/URL/error
// detectors in the same registry. The caller supplies the row index and
// a UTF-8-safe function for mapping byte offsets within `line` to
// character columns (the GUI already has one of these for its grid —
// see `grid_renderer::byte_to_col`). For callers that don't need grid-
// accurate columns (the daemon / MCP `semantic.get_hotspots` tool), we
// also provide `hotspots_from_pattern_matches_simple` which approximates
// columns as byte offsets — fine for ASCII-only outputs.

use crate::semantic_patterns::{PatternMatch, ResolvedAction};

/// Produce a `TextHotspot` per pattern match whose action is
/// `Hotspot(...)`. Non-hotspot actions are skipped (the caller routes
/// them separately). `row` is the absolute row index of the line the
/// matches came from, and `byte_to_col` translates match byte offsets to
/// character columns (usually via a grid-aware mapping).
///
/// The resulting hotspot's `kind` mirrors the closest built-in variant
/// the pattern's `on_click` maps to, so the existing click dispatcher
/// can route it without caring about the pattern-pack origin:
///
/// - `open_editor` → [`HotspotKind::FilePath`]
/// - `open_url` → [`HotspotKind::Url`]
/// - `emit_event` → [`HotspotKind::FilePath`] (neutral default — the
///   `pattern_source` field is what the handler branches on)
pub fn hotspots_from_pattern_matches<F>(
    matches: &[PatternMatch],
    row: usize,
    mut byte_to_col: F,
) -> Vec<TextHotspot>
where
    F: FnMut(usize) -> usize,
{
    use crate::semantic_patterns::HotspotOnClick;

    let mut out = Vec::new();
    for m in matches {
        let ResolvedAction::Hotspot(ref h) = m.action else {
            continue;
        };
        let kind = match h.on_click {
            HotspotOnClick::OpenUrl => HotspotKind::Url,
            HotspotOnClick::OpenEditor | HotspotOnClick::EmitEvent => HotspotKind::FilePath,
        };
        let start_col = byte_to_col(m.byte_start);
        let end_col = byte_to_col(m.byte_end);
        out.push(TextHotspot {
            kind,
            text: m.matched_text.clone(),
            row,
            start_col,
            end_col,
            is_dir: false,
            pattern_source: Some(PatternHotspotSource {
                pack_name: m.pack_name.clone(),
                rule_name: m.pattern_name.clone(),
                on_click: h.on_click.as_str().to_string(),
                target: h.target.clone(),
                label: h.label.clone(),
                declared_kind: h.kind.clone(),
            }),
        });
    }
    out
}

/// Byte-offset-as-column convenience wrapper for ASCII-only callers.
/// Equivalent to `hotspots_from_pattern_matches(matches, row, |b| b)`.
pub fn hotspots_from_pattern_matches_simple(
    matches: &[PatternMatch],
    row: usize,
) -> Vec<TextHotspot> {
    hotspots_from_pattern_matches(matches, row, |b| b)
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
                pattern_source: None,
            },
            TextHotspot {
                kind: HotspotKind::FilePath,
                text: "/tmp/some-file.rs".to_string(),
                row: 1,
                start_col: 0,
                end_col: 17,
                is_dir: false,
                pattern_source: None,
            },
            TextHotspot {
                kind: HotspotKind::Url,
                text: "https://example.com".to_string(),
                row: 2,
                start_col: 0,
                end_col: 19,
                is_dir: false,
                pattern_source: None,
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
            pattern_source: None,
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
            pattern_source: None,
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
            pattern_source: None,
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
            pattern_source: None,
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

    // ── Tilde prefix ──────────────────────────────────────────────────

    #[test]
    fn detects_tilde_directory_path() {
        // The full-issue fixture: `~/projects/therminal` printed by a
        // shell prompt. Before the tilde fix, the regex would match only
        // `/projects/therminal`, so the click handler would `cd` into a
        // nonexistent absolute path.
        let hotspots = detect(&["cwd: ~/projects/therminal right now"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1, "expected exactly one path match, got {fp:?}");
        assert_eq!(fp[0].text, "~/projects/therminal");
    }

    #[test]
    fn detects_tilde_file_path_with_line_col() {
        let hotspots = detect(&["edit ~/.config/therminal/therminal.toml:12:4 please"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1);
        assert_eq!(fp[0].text, "~/.config/therminal/therminal.toml:12:4");
    }

    #[test]
    fn detects_tilde_user_path() {
        // POSIX `~user/…` form.
        let hotspots = detect(&["see ~alice/src/main.rs here"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1);
        assert_eq!(fp[0].text, "~alice/src/main.rs");
    }

    #[test]
    fn tilde_without_slash_is_not_a_path() {
        // A bare `~` in prose must not match (e.g. "approx ~5 seconds").
        // The regex requires `~/` or `~user/` — so "~5" never matches.
        let hotspots = detect(&["roughly ~5 seconds later"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert!(fp.is_empty(), "bare `~5` should not match, got {fp:?}");
    }

    // ── Corpus-based false-positive suite (tn-2qzi) ──────────────────
    //
    // Helper that asserts NO FilePath hotspot is produced for any line
    // in the corpus. Error messages include the offending line so
    // regressions are easy to localise.
    fn assert_no_file_path(corpus: &[&str]) {
        for line in corpus {
            let hs = detect(&[line]);
            let fp: Vec<_> = hs
                .iter()
                .filter(|h| h.kind == HotspotKind::FilePath)
                .collect();
            assert!(
                fp.is_empty(),
                "expected no file-path match in {line:?}, got {:?}",
                fp.iter().map(|h| &h.text).collect::<Vec<_>>()
            );
        }
    }

    /// Helper that asserts exactly one FilePath hotspot is produced with
    /// the given text. Keeps the positive corpus test readable.
    fn assert_file_path_match(line: &str, expected: &str) {
        let hs = detect(&[line]);
        let fp: Vec<_> = hs
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(
            fp.len(),
            1,
            "expected exactly one file-path match in {line:?}, got {:?}",
            fp.iter().map(|h| &h.text).collect::<Vec<_>>()
        );
        assert_eq!(fp[0].text, expected, "wrong match text for {line:?}");
    }

    #[test]
    fn corpus_rejects_bare_multi_segment_identifiers() {
        // Pure slash-separated identifiers with no leading anchor and no
        // known extension on the final segment. The regex's unanchored
        // branch requires a known extension, so these must never match.
        assert_no_file_path(&[
            "a/b/c",
            "foo/bar/baz",
            "module/submodule/name",
            "path-like/identifier",
            "HTTP/1.1 200 OK",
            "application/json Content-Type",
            "audio/mpeg, video/mp4",
            "key=value/other=thing",
        ]);
    }

    #[test]
    fn corpus_rejects_markdown_emphasis() {
        // The regex would happily match the bare filename *inside* the
        // markers — boundary validation rejects it because the
        // surrounding char is `*` or `_`.
        assert_no_file_path(&[
            "this is *foo.rs* in markdown",
            "the *bar.py* is here",
            "see **bold.md** text",
            "text with _under.rs_ emphasis",
            "combining *a.rs* and *b.py* in one line",
        ]);
        // Sanity: backticks (inline-code style) ARE allowed as boundaries,
        // so paths inside backticks still match.
        assert_file_path_match("backtick `foo.rs` works fine", "foo.rs");
    }

    #[test]
    fn corpus_rejects_log_fragments() {
        // Real log-tail output: timestamps, module paths, numeric ratios,
        // version strings. None of these should produce a file-path
        // hotspot.
        assert_no_file_path(&[
            "INFO 2024-01-15 10:30:45 server started",
            "[2024-01-15T10:30:45Z INFO app::module] handled request",
            "loaded 2.0.1 config",
            "version v1.2.3 released",
            "size 1.5MB compressed",
            "ratio 3:4 aspect",
            "time 12:34:56 UTC",
            "42 files updated, 3 deleted",
            "Channeling energy at 100% efficiency",
            "elapsed: 1.234s",
            "running at 1.5x speed",
        ]);
    }

    #[test]
    fn corpus_rejects_scp_style_and_email_uris() {
        // SCP-style `user@host:path` should not spawn a FilePath hotspot
        // for the remote side. The `@` kills the left side and the `:`
        // kills the right side.
        assert_no_file_path(&[
            "git@github.com:user/repo.git",
            "user@host:/tmp/file.txt",
            "alice@example.com sent a message",
            "contact: ops@company.org",
        ]);
    }

    #[test]
    fn corpus_rejects_word_dot_word_prose() {
        // `word.word` tokens that the v1 regex avoided, plus a few new
        // shapes that the tightened regex must also reject: tokens where
        // the `.ext` tail happens to be a known extension but the whole
        // thing is clearly not a path.
        assert_no_file_path(&[
            "Searching for matches",
            "Loaded config successfully",
            "a.b.c is a common identifier",
            "see section 2.h for details", // `2.h` — ext `h` bounded by digit, preceded by space, followed by ` for`
            "foo.md5 checksum",            // `md5` not an ext; nothing should match
            "debug: name.rs.backup is old", // trailing `.backup` continues past `rs`
            "list: first.item second.thing", // no known ext on either
        ]);
    }

    #[test]
    fn corpus_rejects_github_dot_c_and_single_char_ext_traps() {
        // Single-letter extensions (`c`, `h`) are the most dangerous
        // entries in the whitelist. They may ONLY fire when both
        // boundaries are satisfied. These strings must NOT produce
        // hotspots.
        assert_no_file_path(&[
            "git@github.com wrote",
            "host.c.example.net domain",
            "prefix.c.suffix token",
            "fn.h.method() in prose",
        ]);
    }

    #[test]
    fn corpus_positive_cargo_build_errors() {
        // Rust errors: the actual file path with `:line:col` must match.
        // The `-->` prefix triggers a separate ErrorLocation hotspot, so
        // for cargo output we verify both shapes independently.
        assert_file_path_match(
            "error at /home/user/src/main.rs:42:5 here",
            "/home/user/src/main.rs:42:5",
        );
        assert_file_path_match(
            "at crates/therminal-app/src/window/mod.rs:120:5 boom",
            "crates/therminal-app/src/window/mod.rs:120:5",
        );
    }

    #[test]
    fn corpus_positive_git_status_output() {
        // `git status` paths are space-separated after a `modified:` /
        // `new file:` label.
        assert_file_path_match("modified:   src/lib.rs", "src/lib.rs");
        assert_file_path_match(
            "new file:   crates/therminal-terminal/src/hotspot_detection.rs",
            "crates/therminal-terminal/src/hotspot_detection.rs",
        );
        // A relative path sitting inside a directory diff summary.
        assert_file_path_match(
            " M  crates/therminal-core/Cargo.toml",
            "crates/therminal-core/Cargo.toml",
        );
    }

    #[test]
    fn corpus_positive_python_traceback() {
        // Python error lines quote the path with `"…"`. Both quoting
        // styles must be boundary-accepted.
        assert_file_path_match(
            "File \"/usr/lib/python3.11/foo.py\", line 42",
            "/usr/lib/python3.11/foo.py",
        );
        assert_file_path_match("  File \"./app.py\", line 10, in main", "./app.py");
    }

    #[test]
    fn corpus_positive_ls_like_output() {
        // `ls -la` style paths — absolute, bare directory names with
        // anchors.
        assert_file_path_match("ls -la /etc/hosts", "/etc/hosts");
        assert_file_path_match(
            "-rw-r--r-- 1 marci users 1024 Jan 15 10:30 /home/marci/src/main.rs",
            "/home/marci/src/main.rs",
        );
    }

    #[test]
    fn corpus_positive_two_paths_on_one_line() {
        // The `package.json, tsconfig.json` case: previously the first
        // match would snap to `.js` (wrong alternation ordering) and
        // the boundary check would reject the truncated token, so
        // neither path was reported. With `json` ordered before `js`
        // AND the boundary check, both paths now match cleanly.
        let hotspots = detect(&["package.json, tsconfig.json"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .map(|h| h.text.clone())
            .collect();
        assert_eq!(
            fp,
            vec!["package.json".to_string(), "tsconfig.json".to_string()]
        );
    }

    #[test]
    fn corpus_positive_parenthesized_and_quoted_paths() {
        // Paths wrapped in common opener/closer pairs must still match.
        assert_file_path_match("(src/main.rs)", "src/main.rs");
        assert_file_path_match("\"src/main.rs\"", "src/main.rs");
        assert_file_path_match("'src/main.rs'", "src/main.rs");
        assert_file_path_match("`src/main.rs`", "src/main.rs");
        assert_file_path_match("[src/main.rs]", "src/main.rs");
        assert_file_path_match("<src/main.rs>", "src/main.rs");
    }

    #[test]
    fn corpus_positive_sentence_punctuation() {
        // A trailing `.`, `,`, `;`, `:`, `!`, `?` counts as
        // end-of-sentence punctuation and must not swallow the final
        // extension character. The trailing `.` rule is special-cased
        // to avoid collision with `foo.rs.backup` (which has a word
        // char after the `.`).
        assert_file_path_match("edit src/main.rs.", "src/main.rs");
        assert_file_path_match("edit src/main.rs,", "src/main.rs");
        assert_file_path_match("edit src/main.rs;", "src/main.rs");
        assert_file_path_match("see src/main.rs!", "src/main.rs");
        assert_file_path_match("is src/main.rs?", "src/main.rs");
    }

    #[test]
    fn corpus_positive_path_followed_by_extension_run_on() {
        // `foo.rs.backup` should NOT match only `foo.rs` — the trailing
        // `.backup` continues into word chars, so the boundary check
        // rejects the truncated candidate. The whole token lacks a
        // known-extension tail so nothing matches at all.
        assert_no_file_path(&[
            "old: src/main.rs.backup here",
            "cached: config.toml.bak file",
        ]);
    }

    #[test]
    fn corpus_positive_short_extension_with_proper_boundary() {
        // Single-letter extensions DO match when the boundary is clean.
        // `main.c`, `stdio.h` are the canonical C-source examples.
        assert_file_path_match("edit main.c now", "main.c");
        assert_file_path_match("include stdio.h for printf", "stdio.h");
        assert_file_path_match("see ./src/vendor.h:12 details", "./src/vendor.h:12");
    }

    #[test]
    fn corpus_positive_json_ordering_fix() {
        // Regression lock-in for the alternation ordering fix: longer
        // extensions must beat their shorter prefixes.
        assert_file_path_match("open package.json", "package.json");
        assert_file_path_match("look at app.jsx", "app.jsx");
        assert_file_path_match("build src/app.tsx", "src/app.tsx");
        assert_file_path_match("load config.yaml", "config.yaml");
        assert_file_path_match("see Cargo.lock", "Cargo.lock");
    }

    #[test]
    fn corpus_rejects_url_path_portion() {
        // URLs are handled by URL_RE and dominate over FilePath hotspots.
        // Even without URL dominance, the boundary check would reject
        // the path portion because it's preceded by `:` (as in `http://`).
        let hotspots = detect(&[
            "visit https://example.com/foo.rs for info",
            "see http://example.com/path/bar.toml now",
        ]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert!(
            fp.is_empty(),
            "URL path portion must not produce a file-path hotspot, got {fp:?}"
        );
    }

    #[test]
    fn boundary_helpers_are_consistent() {
        // Unit tests for the boundary predicates so regressions in the
        // tables are caught immediately rather than only showing up as
        // surprising corpus failures.
        assert!(is_left_boundary(' '));
        assert!(is_left_boundary('\t'));
        assert!(is_left_boundary('('));
        assert!(is_left_boundary('"'));
        assert!(!is_left_boundary('a'));
        assert!(!is_left_boundary('_'));
        assert!(!is_left_boundary('.'));
        assert!(!is_left_boundary('/'));
        assert!(!is_left_boundary('@'));
        assert!(!is_left_boundary('*'));
        assert!(!is_left_boundary(':'));

        assert!(is_right_boundary(' '));
        assert!(is_right_boundary(','));
        assert!(is_right_boundary(')'));
        assert!(is_right_boundary(':'));
        assert!(!is_right_boundary('a'));
        assert!(!is_right_boundary('_'));
        assert!(!is_right_boundary('/'));
        assert!(!is_right_boundary('*'));
        assert!(!is_right_boundary('@'));

        assert!(is_path_word_char('a'));
        assert!(is_path_word_char('Z'));
        assert!(is_path_word_char('0'));
        assert!(is_path_word_char('_'));
        assert!(is_path_word_char('-'));
        assert!(!is_path_word_char(' '));
        assert!(!is_path_word_char('.'));
        assert!(!is_path_word_char('/'));
    }

    #[test]
    fn expand_tilde_home_slash() {
        let out = expand_tilde("~/projects/therminal", Some("/home/marci"));
        assert_eq!(out, "/home/marci/projects/therminal");
    }

    #[test]
    fn expand_tilde_home_slash_trailing_slash_in_home() {
        // `$HOME=/home/marci/` (with trailing slash) must not double up.
        let out = expand_tilde("~/foo", Some("/home/marci/"));
        assert_eq!(out, "/home/marci/foo");
    }

    #[test]
    fn expand_tilde_bare_tilde() {
        let out = expand_tilde("~", Some("/home/marci"));
        assert_eq!(out, "/home/marci");
    }

    #[test]
    fn expand_tilde_preserves_line_col_suffix() {
        // `expand_tilde` only touches the leading prefix, so a
        // `:line:col` suffix passes through unchanged.
        let out = expand_tilde("~/foo.rs:42:5", Some("/home/marci"));
        assert_eq!(out, "/home/marci/foo.rs:42:5");
    }

    #[test]
    fn expand_tilde_leaves_user_form_alone() {
        // `~alice/foo` needs `getpwnam` to resolve, which we don't do.
        // The function returns the path unchanged so the caller can try
        // the literal path and toast on failure.
        let out = expand_tilde("~alice/foo", Some("/home/marci"));
        assert_eq!(out, "~alice/foo");
    }

    #[test]
    fn expand_tilde_without_home_is_noop() {
        // No `$HOME` set → unchanged, rather than producing `/foo`
        // which would be wrong.
        let out = expand_tilde("~/foo", None);
        assert_eq!(out, "~/foo");
    }

    #[test]
    fn expand_tilde_ignores_non_tilde() {
        let out = expand_tilde("/usr/local/bin", Some("/home/marci"));
        assert_eq!(out, "/usr/local/bin");
        let out2 = expand_tilde("./src/main.rs", Some("/home/marci"));
        assert_eq!(out2, "./src/main.rs");
    }

    // ── resolve_relative_to_cwd ────────────────────────────────────────

    #[test]
    fn resolve_relative_leaves_absolute_path_alone() {
        // Rule: path starts with `/` → unchanged.
        let out = resolve_relative_to_cwd("/usr/local/bin", Some("/home/x"));
        assert_eq!(out, "/usr/local/bin");
    }

    #[test]
    fn resolve_relative_leaves_tilde_path_alone() {
        // Rule: path starts with `~` → unchanged (caller must
        // `expand_tilde` first).
        let out = resolve_relative_to_cwd("~/foo", Some("/home/x"));
        assert_eq!(out, "~/foo");
    }

    // ── tn-jci5: Windows-absolute paths must not be joined against a
    //    Unix-style cwd (the therminal-on-Windows + WSL shell topology).

    #[test]
    fn is_windows_absolute_drive_letter() {
        assert!(is_windows_absolute(r"C:\Users\marci"));
        assert!(is_windows_absolute("C:/Users/marci"));
        assert!(is_windows_absolute(r"d:\foo"));
        assert!(is_windows_absolute("Z:/tmp"));
    }

    #[test]
    fn is_windows_absolute_unc() {
        assert!(is_windows_absolute(r"\\server\share\file"));
        assert!(is_windows_absolute(r"\\wsl.localhost\Ubuntu\home\marci"));
        assert!(is_windows_absolute(r"\\wsl$\Ubuntu\home\marci"));
    }

    #[test]
    fn is_windows_absolute_rejects_ambiguous() {
        // Bare drive letter (no separator) is Windows-relative, not absolute.
        assert!(!is_windows_absolute("C:"));
        assert!(!is_windows_absolute("C:foo"));
        // Single backslash is not UNC.
        assert!(!is_windows_absolute(r"\foo"));
        // Double separator with no name is not UNC.
        assert!(!is_windows_absolute(r"\\"));
        assert!(!is_windows_absolute("//"));
        // Linux absolute still isn't "Windows absolute".
        assert!(!is_windows_absolute("/home/marci"));
        // Relative.
        assert!(!is_windows_absolute("foo.rs"));
        assert!(!is_windows_absolute("./foo.rs"));
        // Empty.
        assert!(!is_windows_absolute(""));
    }

    #[test]
    fn resolve_relative_leaves_windows_absolute_alone_under_unix_cwd() {
        // Regression test for the settings-gear bug: on Windows with a
        // WSL focused pane, the config path is Windows-absolute
        // (`C:\Users\…`) but the cwd is Linux-style. Joining them
        // produced `/home/marci/.../C:\Users\...\therminal.toml`.
        let out = resolve_relative_to_cwd(
            r"C:\Users\marci\AppData\Roaming\therminal\therminal.toml",
            Some("/home/marci/projects/therminal"),
        );
        assert_eq!(
            out,
            r"C:\Users\marci\AppData\Roaming\therminal\therminal.toml"
        );
    }

    #[test]
    fn resolve_relative_leaves_unc_alone_under_unix_cwd() {
        let out = resolve_relative_to_cwd(
            r"\\wsl.localhost\Ubuntu\home\marci\foo.rs",
            Some("/home/marci"),
        );
        assert_eq!(out, r"\\wsl.localhost\Ubuntu\home\marci\foo.rs");
    }

    #[test]
    fn resolve_relative_no_cwd_is_noop() {
        // Rule: `cwd.is_none()` → unchanged (no basis to resolve).
        let out = resolve_relative_to_cwd("./foo", None);
        assert_eq!(out, "./foo");
    }

    #[test]
    fn resolve_relative_dot_slash_prefix() {
        // Rule: `./foo` + cwd `/home/x` → `/home/x/foo`.
        let out = resolve_relative_to_cwd("./foo", Some("/home/x"));
        assert_eq!(out, "/home/x/foo");
    }

    #[test]
    fn resolve_relative_preserves_line_col_suffix() {
        // Rule: `./foo/bar.rs:42` + cwd `/home/x` →
        // `/home/x/foo/bar.rs:42`.
        let out = resolve_relative_to_cwd("./foo/bar.rs:42", Some("/home/x"));
        assert_eq!(out, "/home/x/foo/bar.rs:42");
    }

    #[test]
    fn resolve_relative_preserves_line_col_col_suffix() {
        // `:line:col` shape also passes through unchanged.
        let out = resolve_relative_to_cwd("./src/main.rs:42:5", Some("/home/x"));
        assert_eq!(out, "/home/x/src/main.rs:42:5");
    }

    #[test]
    fn resolve_relative_single_dotdot() {
        // Rule: `../foo` + cwd `/home/x/y` → `/home/x/foo`.
        let out = resolve_relative_to_cwd("../foo", Some("/home/x/y"));
        assert_eq!(out, "/home/x/foo");
    }

    #[test]
    fn resolve_relative_double_dotdot() {
        // Rule: `../../foo` + cwd `/home/x/y/z` → `/home/x/foo`.
        let out = resolve_relative_to_cwd("../../foo", Some("/home/x/y/z"));
        assert_eq!(out, "/home/x/foo");
    }

    #[test]
    fn resolve_relative_bare_path() {
        // Rule: `foo/bar.rs` + cwd `/home/x` → `/home/x/foo/bar.rs`.
        let out = resolve_relative_to_cwd("foo/bar.rs", Some("/home/x"));
        assert_eq!(out, "/home/x/foo/bar.rs");
    }

    #[test]
    fn resolve_relative_bare_path_with_line_col() {
        let out = resolve_relative_to_cwd("foo/bar.rs:42", Some("/home/x"));
        assert_eq!(out, "/home/x/foo/bar.rs:42");
    }

    #[test]
    fn resolve_relative_cwd_trailing_slash_not_doubled() {
        // Rule: cwd with trailing slash (`/home/x/`) → don't double-slash.
        let out = resolve_relative_to_cwd("./foo", Some("/home/x/"));
        assert_eq!(out, "/home/x/foo");
        // Same rule via the bare-relative branch.
        let out2 = resolve_relative_to_cwd("foo", Some("/home/x/"));
        assert_eq!(out2, "/home/x/foo");
    }

    #[test]
    fn resolve_relative_dotdot_above_root_stays_at_root() {
        // Defensive: `../foo` from cwd `/` should not produce `//foo`.
        let out = resolve_relative_to_cwd("../foo", Some("/"));
        assert_eq!(out, "/foo");
    }

    #[test]
    fn resolve_relative_bare_dot() {
        // Bare `.` resolves to `cwd` itself.
        let out = resolve_relative_to_cwd(".", Some("/home/x"));
        assert_eq!(out, "/home/x");
    }

    #[test]
    fn resolve_relative_bare_dotdot() {
        // Bare `..` pops one segment.
        let out = resolve_relative_to_cwd("..", Some("/home/x/y"));
        assert_eq!(out, "/home/x");
    }

    // ── Pattern-engine bridge tests ─────────────────────────────────────

    #[test]
    fn pattern_matches_convert_to_hotspots() {
        use crate::semantic_patterns::{
            HotspotOnClick, PatternEngine, PatternEngineConfig, ResolvedAction,
        };
        use std::path::PathBuf;

        // Build a tiny engine with one hotspot pattern.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("t.toml"),
            r#"
[[pattern]]
name = "go-to-src"
match = '(?P<f>src/[a-z]+\.rs):(?P<l>\d+)'
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "open_editor"
target = "{f}"
label = "open {f}:{l}"
kind = "pattern-demo"
"#,
        )
        .unwrap();

        let engine = PatternEngine::new(PatternEngineConfig {
            enabled: true,
            user_pattern_dir: Some(tmp.path().to_path_buf()),
            shipped_pattern_dir: Some(PathBuf::new()),
            ..PatternEngineConfig::new_default()
        });

        let line = "see src/lib.rs:42 here";
        let matches = engine.process_finalized_line(1, line, None, None);
        assert_eq!(matches.len(), 1);
        // Sanity: action shape.
        match &matches[0].action {
            ResolvedAction::Hotspot(h) => assert_eq!(h.on_click, HotspotOnClick::OpenEditor),
            _ => panic!(),
        }

        let hotspots = hotspots_from_pattern_matches_simple(&matches, 7);
        assert_eq!(hotspots.len(), 1);
        let h = &hotspots[0];
        assert_eq!(h.kind, HotspotKind::FilePath);
        assert_eq!(h.text, "src/lib.rs:42");
        assert_eq!(h.row, 7);
        // byte_to_col = identity for ASCII input.
        assert_eq!(h.start_col, 4);
        assert_eq!(h.end_col, 17);
        let ps = h.pattern_source.as_ref().expect("pattern source");
        assert_eq!(ps.pack_name, "t");
        assert_eq!(ps.rule_name, "go-to-src");
        assert_eq!(ps.on_click, "open_editor");
        assert_eq!(ps.target.as_deref(), Some("src/lib.rs"));
        assert_eq!(ps.label.as_deref(), Some("open src/lib.rs:42"));
        assert_eq!(ps.declared_kind, "pattern-demo");
    }

    // ── Windows drive-letter absolute paths (tn-v974) ─────────────────

    #[test]
    fn detects_windows_backslash_path() {
        let hotspots = detect(&[r"error at C:\foo\bar.rs here"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1, "expected one match, got {fp:?}");
        assert_eq!(fp[0].text, r"C:\foo\bar.rs");
    }

    #[test]
    fn detects_windows_forward_slash_path_with_line() {
        let hotspots = detect(&["error at C:/foo/bar.rs:42 here"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1, "expected one match, got {fp:?}");
        assert_eq!(fp[0].text, "C:/foo/bar.rs:42");
    }

    #[test]
    fn detects_windows_path_with_line_and_col() {
        let hotspots = detect(&[r"error at D:\src\main.rs:12:3 here"]);
        let fp: Vec<_> = hotspots
            .iter()
            .filter(|h| h.kind == HotspotKind::FilePath)
            .collect();
        assert_eq!(fp.len(), 1, "expected one match, got {fp:?}");
        assert_eq!(fp[0].text, r"D:\src\main.rs:12:3");
    }

    #[test]
    fn pattern_url_click_produces_url_hotspot() {
        use crate::semantic_patterns::{PatternEngine, PatternEngineConfig};
        use std::path::PathBuf;

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("t.toml"),
            r#"
[[pattern]]
name = "gh-issue"
match = 'GH-(?P<n>\d+)'
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "open_url"
target = "https://github.com/example/repo/issues/{n}"
label = "GH-{n}"
"#,
        )
        .unwrap();

        let engine = PatternEngine::new(PatternEngineConfig {
            enabled: true,
            user_pattern_dir: Some(tmp.path().to_path_buf()),
            shipped_pattern_dir: Some(PathBuf::new()),
            ..PatternEngineConfig::new_default()
        });

        let matches = engine.process_finalized_line(1, "closes GH-42", None, None);
        let hotspots = hotspots_from_pattern_matches_simple(&matches, 0);
        assert_eq!(hotspots.len(), 1);
        assert_eq!(hotspots[0].kind, HotspotKind::Url);
        assert_eq!(
            hotspots[0]
                .pattern_source
                .as_ref()
                .unwrap()
                .target
                .as_deref(),
            Some("https://github.com/example/repo/issues/42")
        );
    }
}
