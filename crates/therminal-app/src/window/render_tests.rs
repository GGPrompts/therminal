//! End-to-end render pipeline integration tests (tn-75kq).
//!
//! These tests exercise the full chain from terminal bytes to `RenderCell`
//! hotspot annotations without requiring a GPU surface:
//!
//!   alacritty `Term` --> byte feed --> cell extraction --> row text -->
//!   hotspot detection --> pattern engine --> `apply_hotspots_to_cells` -->
//!   assert on `RenderCell.hotspot`
//!
//! This catches cross-crate data bugs (like tn-mnlo) where
//! `PatternMatch.byte_end` -> `TextHotspot.end_col` -> `RenderCell.hotspot`
//! silently propagate incorrect spans.

use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi;

use therminal_terminal::hotspot_detection::{
    HotspotKind, TextHotspot, detect_hotspots_from_text_with_wrap, hotspots_from_pattern_matches,
};
use therminal_terminal::semantic_patterns::{PatternEngine, PatternEngineConfig};

use crate::grid_renderer::{RenderCell, cell_display_text};
use crate::pane::PaneListener;
use crate::pane::state::PaneTermSize;
use crate::widgets::pattern_widget::PatternWidgetMatch;

use super::render::{apply_hotspots_to_cells, compute_wrap_continuation, extract_row_text_from_cells};

// ── Test helper ─────────────────────────────────────────────────────────

/// Feed raw bytes into a fresh `Term` and extract `RenderCell`s.
///
/// Returns `(cells, screen_lines, columns)` so callers can run hotspot
/// detection and apply results.
fn feed_and_extract(
    cols: usize,
    rows: usize,
    input: &[u8],
) -> (Vec<RenderCell>, usize, usize) {
    let listener = PaneListener::new();
    let size = PaneTermSize {
        columns: cols,
        screen_lines: rows,
    };
    let mut term: Term<PaneListener> = Term::new(TermConfig::default(), &size, listener);
    let mut proc = ansi::Processor::<ansi::StdSyncHandler>::new();
    proc.advance(&mut term, input);

    let screen_lines = term.screen_lines();
    let columns = term.columns();

    // Walk the viewport and build RenderCells, mirroring the extraction
    // in window/render.rs `render_single_pane`.
    let content = term.renderable_content();
    let cells: Vec<RenderCell> = content
        .display_iter
        .filter_map(|indexed| {
            let point = indexed.point;
            let cell = indexed.cell;
            let row = usize::try_from(point.line.0).ok()?;
            if row >= screen_lines {
                return None;
            }
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                return None;
            }
            let hyperlink = cell.hyperlink().map(|h| Arc::from(h.uri()));
            let hyperlink_source = if hyperlink.is_some() {
                Some(crate::grid_renderer::HyperlinkSource::Osc8)
            } else {
                None
            };
            Some(RenderCell {
                row,
                col: point.column.0,
                c: cell.c,
                text: cell_display_text(cell.c, cell.zerowidth()),
                fg: cell.fg,
                bg: cell.bg,
                flags: cell.flags,
                hyperlink,
                hyperlink_source,
                hotspot: None,
            })
        })
        .collect();

    (cells, screen_lines, columns)
}

/// Full pipeline: feed bytes, detect hotspots (built-in + pattern engine),
/// apply to cells, return the annotated cell grid.
fn pipeline(
    cols: usize,
    rows: usize,
    input: &[u8],
    engine: Option<&PatternEngine>,
) -> Vec<RenderCell> {
    let (mut cells, screen_lines, _columns) = feed_and_extract(cols, rows, input);

    let row_texts = extract_row_text_from_cells(&cells, screen_lines);
    let wrap_cont = compute_wrap_continuation(&cells, screen_lines);
    let mut hotspots = detect_hotspots_from_text_with_wrap(&row_texts, &wrap_cont);

    if let Some(engine) = engine {
        let mut widget_sink: Vec<PatternWidgetMatch> = Vec::new();
        extend_hotspots_from_patterns_test(
            engine,
            1, // pane_id
            &row_texts,
            &mut hotspots,
            &mut widget_sink,
        );
    }

    apply_hotspots_to_cells(&mut cells, &hotspots);
    cells
}

/// Simplified version of `extend_hotspots_from_patterns` for tests
/// (no pane viewport coordinates needed).
fn extend_hotspots_from_patterns_test(
    engine: &PatternEngine,
    pane_id: u64,
    row_texts: &[String],
    hotspots: &mut Vec<TextHotspot>,
    widget_sink: &mut Vec<PatternWidgetMatch>,
) {
    for (row_idx, text) in row_texts.iter().enumerate() {
        if text.is_empty() {
            continue;
        }
        let matches = engine.process_finalized_line(pane_id, text, None, None);
        if matches.is_empty() {
            continue;
        }
        let byte_to_col_map: Vec<usize> = {
            let mut map = vec![0usize; text.len() + 1];
            let mut col = 0usize;
            for (byte_idx, ch) in text.char_indices() {
                map[byte_idx] = col;
                col += 1;
                for item in map
                    .iter_mut()
                    .take(byte_idx + ch.len_utf8())
                    .skip(byte_idx + 1)
                {
                    *item = col;
                }
            }
            map[text.len()] = col;
            map
        };
        let pattern_hotspots = hotspots_from_pattern_matches(&matches, row_idx, |byte| {
            byte_to_col_map.get(byte).copied().unwrap_or(byte)
        });
        hotspots.extend(pattern_hotspots);

        // Collect Widget-action matches.
        for m in &matches {
            if let therminal_terminal::semantic_patterns::ResolvedAction::Widget(ref w) = m.action {
                let start_col = byte_to_col_map
                    .get(m.byte_start)
                    .copied()
                    .unwrap_or(m.byte_start);
                let end_col = byte_to_col_map
                    .get(m.byte_end)
                    .copied()
                    .unwrap_or(m.byte_end);
                widget_sink.push(PatternWidgetMatch {
                    pane_id,
                    row: row_idx,
                    start_col,
                    end_col,
                    pane_vp_x: 0.0,
                    pane_vp_y: 0.0,
                    widget: w.clone(),
                });
            }
        }
    }
}

/// Helper to build a `PatternEngine` from a TOML pack string.
fn engine_from_toml(toml: &str) -> PatternEngine {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("test-pack.toml"), toml).unwrap();
    // Leak the tempdir so it outlives the engine (patterns are loaded at
    // construction time, so the dir is only needed during `new()`).
    #[allow(deprecated)]
    let dir = tmp.into_path();
    PatternEngine::new(PatternEngineConfig {
        enabled: true,
        user_pattern_dir: Some(dir),
        shipped_pattern_dir: Some(PathBuf::new()),
        ..PatternEngineConfig::new_default()
    })
}

// ── Scenario 1: Pattern hotspot with highlight group ────────────────────
//
// A glossary-style pattern matches `\bPTY\b` with trailing context in
// `(?P<ctx>...)` but uses `highlight = "term"` to restrict the visual
// underline to just the 3-character keyword. Only the "PTY" cells
// should carry `hotspot = Some(...)`.

#[test]
fn pattern_hotspot_highlight_group_narrows_to_keyword() {
    let engine = engine_from_toml(
        r#"
pack_name = "glossary"
[[pattern]]
name = "pty-explain"
match = '\b(?P<term>PTY)\b(?P<ctx>.{0,200})'
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "emit_event"
label = "Explain: {term}"
kind = "hotspot.explain"
highlight = "term"
"#,
    );

    let line = b"daemon PTY/session flows that are still ignored\r\n";
    let cells = pipeline(80, 24, line, Some(&engine));

    // The keyword "PTY" sits at columns 7..10 on row 0.
    let hotspot_cells: Vec<&RenderCell> = cells.iter().filter(|c| c.hotspot.is_some()).collect();
    assert!(
        !hotspot_cells.is_empty(),
        "expected at least one cell with a hotspot"
    );

    // All hotspot cells must be on row 0 and within columns 7..10.
    for cell in &hotspot_cells {
        assert_eq!(cell.row, 0, "hotspot cell on unexpected row {}", cell.row);
        assert!(
            cell.col >= 7 && cell.col < 10,
            "hotspot cell col {} outside expected range 7..10 (char='{}'); highlight group should narrow the span",
            cell.col,
            cell.c,
        );
    }

    // Exactly 3 cells: P, T, Y.
    assert_eq!(
        hotspot_cells.len(),
        3,
        "expected 3 hotspot cells for 'PTY', got {} (cols: {:?})",
        hotspot_cells.len(),
        hotspot_cells.iter().map(|c| c.col).collect::<Vec<_>>(),
    );

    // Trailing context cells should NOT have a hotspot.
    let trailing: Vec<&RenderCell> = cells
        .iter()
        .filter(|c| c.row == 0 && c.col >= 10 && c.c != ' ')
        .collect();
    for cell in &trailing {
        assert!(
            cell.hotspot.is_none(),
            "trailing cell col {} ('{}') should not have a hotspot",
            cell.col,
            cell.c,
        );
    }
}

// ── Scenario 2: Overlapping hotspots (URL suppresses file path) ─────────
//
// A line contains a URL like `https://example.com/src/main.rs:42`. The
// built-in URL hotspot detector should match the URL, and the file-path
// detector would also match `src/main.rs:42`. The URL hotspot should
// suppress (take precedence over) the file-path hotspot because URLs
// are higher priority in `detect_hotspots_from_text_with_wrap`.

#[test]
fn url_hotspot_suppresses_overlapping_file_path() {
    // The URL contains a file-path-like suffix, so both detectors fire.
    // The URL should win for the overlapping region.
    let line = b"see https://example.com/src/main.rs for details\r\n";
    let cells = pipeline(80, 24, line, None);

    let hotspot_cells: Vec<&RenderCell> = cells.iter().filter(|c| c.hotspot.is_some()).collect();
    assert!(
        !hotspot_cells.is_empty(),
        "expected hotspot cells for the URL"
    );

    // All hotspot cells covering the URL region should be of kind Url.
    // "https://example.com/src/main.rs" starts at col 4.
    let url_start = 4;
    let url_text = "https://example.com/src/main.rs";
    let url_end = url_start + url_text.len();

    for cell in &hotspot_cells {
        if cell.col >= url_start && cell.col < url_end {
            let (kind, _, _, _) = cell.hotspot.as_ref().unwrap();
            assert_eq!(
                *kind,
                HotspotKind::Url,
                "cell col {} should be Url hotspot, not {:?}",
                cell.col,
                kind,
            );
        }
    }

    // There should be no FilePath hotspot cells that overlap the URL range.
    let overlapping_filepath: Vec<&RenderCell> = hotspot_cells
        .iter()
        .filter(|c| {
            c.col >= url_start
                && c.col < url_end
                && c.hotspot
                    .as_ref()
                    .is_some_and(|(k, _, _, _)| *k == HotspotKind::FilePath)
        })
        .copied()
        .collect();
    assert!(
        overlapping_filepath.is_empty(),
        "file path hotspot should not overlap URL region; found {} cells",
        overlapping_filepath.len(),
    );
}

// ── Scenario 3: Wrapped-line hotspot ────────────────────────────────────
//
// Feed a file path that wraps across two rows (terminal width < path
// length). The hotspot should span both rows with correct
// start_col/end_col per row.

#[test]
fn wrapped_line_hotspot_spans_two_rows() {
    // Use a narrow terminal (40 cols). The path starts near the end of
    // row 0 and wraps onto row 1.
    let cols = 40;
    let prefix = "see ";
    // Path that is long enough to wrap: must exceed (cols - prefix.len()).
    let path = "/home/user/projects/therminal/src/main.rs:42:5";
    let line = format!("{}{}\r\n", prefix, path);

    let cells = pipeline(cols, 24, line.as_bytes(), None);

    let hotspot_cells: Vec<&RenderCell> = cells.iter().filter(|c| c.hotspot.is_some()).collect();
    assert!(
        !hotspot_cells.is_empty(),
        "expected hotspot cells for the wrapped file path"
    );

    // Hotspot should span row 0 and row 1.
    let rows_with_hotspots: std::collections::BTreeSet<usize> =
        hotspot_cells.iter().map(|c| c.row).collect();
    assert!(
        rows_with_hotspots.contains(&0) && rows_with_hotspots.contains(&1),
        "hotspot should span rows 0 and 1, but found rows: {:?}",
        rows_with_hotspots,
    );

    // Row 0 hotspot should start at the prefix offset.
    let row0_hotspots: Vec<&RenderCell> = hotspot_cells
        .iter()
        .filter(|c| c.row == 0)
        .copied()
        .collect();
    if let Some(first) = row0_hotspots.first() {
        assert_eq!(
            first.col,
            prefix.len(),
            "row 0 hotspot should start at column {}",
            prefix.len(),
        );
    }

    // Row 1 hotspot should start at column 0 (continuation).
    let row1_hotspots: Vec<&RenderCell> = hotspot_cells
        .iter()
        .filter(|c| c.row == 1)
        .copied()
        .collect();
    if let Some(first) = row1_hotspots.first() {
        assert_eq!(first.col, 0, "row 1 hotspot should start at column 0");
    }

    // Total hotspot cells should equal path length.
    assert_eq!(
        hotspot_cells.len(),
        path.len(),
        "total hotspot cells ({}) should equal path length ({})",
        hotspot_cells.len(),
        path.len(),
    );
}

// ── Scenario 4: Alt-screen round-trip ───────────────────────────────────
//
// Feed content on the primary screen, switch to alt screen
// (ESC[?1049h), feed different content, switch back (ESC[?1049l),
// and verify the primary screen content is restored.

#[test]
fn alt_screen_round_trip_preserves_primary_content() {
    let cols = 80;
    let rows = 24;

    // Write "PRIMARY" on the primary screen.
    let mut input = Vec::new();
    input.extend_from_slice(b"PRIMARY content here\r\n");

    // Enter alt screen.
    input.extend_from_slice(b"\x1b[?1049h");

    // Write "ALTSCREEN" on the alt screen.
    input.extend_from_slice(b"ALTSCREEN content\r\n");

    // Exit alt screen (should restore primary).
    input.extend_from_slice(b"\x1b[?1049l");

    let (cells, screen_lines, _) = feed_and_extract(cols, rows, &input);
    let row_texts = extract_row_text_from_cells(&cells, screen_lines);

    // The primary screen content should be visible again.
    let joined = row_texts.join("");
    assert!(
        joined.contains("PRIMARY content here"),
        "primary screen content should be restored after alt screen exit; got: {:?}",
        &row_texts[..3],
    );

    // The alt screen content should NOT be visible.
    assert!(
        !joined.contains("ALTSCREEN"),
        "alt screen content should not be visible after switching back to primary; got: {:?}",
        &row_texts[..3],
    );
}
