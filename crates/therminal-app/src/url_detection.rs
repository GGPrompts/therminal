//! Regex-based URL detection for terminal cells.
//!
//! Scans visible cells for http(s) URLs that don't already have an OSC 8
//! hyperlink and annotates them so the renderer can underline/color them.

use std::sync::LazyLock;

use crate::grid_renderer::RenderCell;

/// Compiled regex for detecting URLs in visible terminal text.
/// Matches http:// and https:// URLs, stopping at common terminal delimiters.
pub(crate) static URL_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"https?://[^\s<>\x00-\x1f\x7f)\]}>\"'`]+"#).unwrap());

/// Detect URLs via regex in the visible cell grid and annotate cells that
/// don't already have an OSC 8 hyperlink. Groups cells by row, reconstructs
/// the row text, finds URL matches, and tags matching columns.
pub(crate) fn detect_urls_in_cells(cells: &mut [RenderCell], screen_lines: usize) {
    // Group cell indices by row for efficient per-row scanning.
    let mut row_indices: Vec<Vec<usize>> = vec![Vec::new(); screen_lines];
    for (i, cell) in cells.iter().enumerate() {
        if cell.row < screen_lines {
            row_indices[cell.row].push(i);
        }
    }

    for row_idxs in &row_indices {
        if row_idxs.is_empty() {
            continue;
        }

        // Find max column to size the row text buffer.
        let max_col = row_idxs.iter().map(|&i| cells[i].col).max().unwrap_or(0);

        // Build the row text string and a byte-offset-to-column mapping.
        // Each char in row_text corresponds to one terminal column.
        let mut row_text: Vec<char> = vec![' '; max_col + 1];
        for &idx in row_idxs {
            row_text[cells[idx].col] = cells[idx].c;
        }
        let text: String = row_text.iter().collect();

        // Build byte-offset → column-index mapping.
        // char_starts[col] = byte offset where column `col` starts in `text`.
        let char_starts: Vec<usize> = text.char_indices().map(|(byte, _)| byte).collect();

        // Find URL matches in this row's text.
        for mat in URL_RE.find_iter(&text) {
            let match_byte_start = mat.start();
            let url = mat.as_str();

            // Strip common trailing punctuation that's not part of the URL.
            let url =
                url.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'));
            if url.len() < 10 {
                // Too short to be a real URL (at minimum "http://x.y")
                continue;
            }
            let url_byte_end = match_byte_start + url.len();

            // Convert byte offsets to column indices.
            let start_col = char_starts
                .iter()
                .position(|&b| b == match_byte_start)
                .unwrap_or(0);
            let end_col = char_starts
                .iter()
                .position(|&b| b >= url_byte_end)
                .unwrap_or(char_starts.len());

            // Tag each cell in the URL column range that doesn't already have a hyperlink.
            for &idx in row_idxs {
                let col = cells[idx].col;
                if col >= start_col && col < end_col && cells[idx].hyperlink.is_none() {
                    cells[idx].hyperlink = Some(url.to_owned());
                }
            }
        }
    }
}
