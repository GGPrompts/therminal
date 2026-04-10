//! Per-pane render orchestration: bridges the layout tree to GridRenderer.
//!
//! Contains the recursive pane traversal that renders each pane's terminal
//! content, headers, and separators.

use std::collections::HashMap;
use std::sync::Arc;

use alacritty_terminal::term::TermDamage;
use alacritty_terminal::term::cell::Flags;

use crate::grid_renderer::{GridRenderer, HyperlinkSource, RenderCell, cell_display_text};
use crate::pane::{LayoutNode, PaneBackendKind, PaneId, PaneState};
use crate::url_detection::detect_urls_in_cells;
use alacritty_terminal::grid::Dimensions;
use therminal_harness_claude::tool_call_hotspots::detect_claude_tool_call_hotspots;
use therminal_terminal::agent_registry::AgentRegistry;
use therminal_terminal::hotspot_detection::{
    TextHotspot, detect_hotspots_from_text_with_wrap, hotspots_from_pattern_matches,
    promote_directory_hotspots,
};
use therminal_terminal::semantic_patterns::PatternEngine;

use crate::claude_cwd::ClaudeCwdTracker;
use crate::widgets::pattern_widget::PatternWidgetMatch;

use super::chrome::{draw_pane_focus_border, draw_pane_header, draw_split_separator};

// ── Helpers ───────────────────────────────────────────────────────────

/// Apply detected hotspots to cells using a row-indexed lookup.
///
/// Complexity: O(H + C) where H = number of hotspots, C = number of cells.
/// Builds a `HashMap<row, Vec<&Hotspot>>` so each cell only checks hotspots
/// on its own row, avoiding the previous O(H * C) nested loop.
fn apply_hotspots_to_cells(cells: &mut [RenderCell], hotspots: &[TextHotspot]) {
    if hotspots.is_empty() {
        return;
    }
    // Group hotspots by row for O(1) row lookup.
    let mut row_hotspots: HashMap<usize, Vec<&TextHotspot>> = HashMap::new();
    for h in hotspots {
        row_hotspots.entry(h.row).or_default().push(h);
    }
    // Single pass over cells: check only hotspots on the same row.
    for cell in cells.iter_mut() {
        if let Some(row_hs) = row_hotspots.get(&cell.row) {
            for h in row_hs {
                if cell.col >= h.start_col && cell.col < h.end_col {
                    cell.hotspot = Some((
                        h.kind.clone(),
                        Arc::from(h.text.as_str()),
                        h.is_dir,
                        h.resolved_text.as_deref().map(Arc::from),
                    ));
                    break; // first matching hotspot wins
                }
            }
        }
    }
}

#[cfg(test)]
fn damaged_rows_any(damaged_rows: Option<&[bool]>) -> bool {
    damaged_rows.is_some_and(|rows| rows.iter().any(|&damaged| damaged))
}

fn damage_rows_empty(damaged_rows: Option<&[bool]>) -> bool {
    matches!(damaged_rows, Some(rows) if !rows.iter().any(|&damaged| damaged))
}

/// Force a full rebuild when the row cache doesn't match the current viewport.
///
/// When the cache is invalid (different dimensions, display offset, or freshly
/// cleared after a theme/font change), partial damage cannot reconstruct the
/// full screen — non-cached rows would remain None and disappear. A full rebuild
/// is always safe and correct; the `damaged_rows` partial optimisation only
/// applies when the existing cache is still valid.
fn should_force_full_rebuild(cache_matches_viewport: bool, _damaged_rows: Option<&[bool]>) -> bool {
    !cache_matches_viewport
}

/// Promote `FilePath` hotspots whose target stat'd as a directory using
/// the real filesystem (tn-zqwg). Wraps `std::fs::metadata` so the click
/// handler can route directory hotspots through `folder_pane_command`
/// instead of the editor fallback chain.
///
/// tn-q8ce: on native Windows with a WSL pane, the hotspot path is a
/// Linux path (e.g. `/home/marci/projects`) which `std::fs::metadata`
/// cannot stat directly — it resolves to `C:\home\marci\…` and returns
/// NotFound, so `is_dir` stays `false` and the click incorrectly lands
/// in the editor path. We transparently stat via the `\\wsl.localhost\…`
/// UNC form instead, which Windows can resolve through the WSL virtual
/// filesystem provider. On Linux/macOS the translator is a no-op.
fn promote_directory_hotspots_from_fs(hotspots: &mut [TextHotspot]) {
    promote_directory_hotspots(hotspots, |p| {
        let translated = crate::window::wsl_paths::translate_if_wsl_windows(p);
        std::fs::metadata(translated.as_ref()).map(|m| m.is_dir())
    });
}

/// Compute per-row "is continuation" flags from cell wrap state.
///
/// Row `r` is a continuation when row `r - 1` had its last cell flagged
/// `WRAPLINE` (alacritty's hard-wrap marker). Used to suppress hotspot
/// matches anchored at column 0 of a wrapped line — see
/// `detect_hotspots_from_text_with_wrap`.
fn compute_wrap_continuation(cells: &[RenderCell], screen_lines: usize) -> Vec<bool> {
    // Track the max column with a WRAPLINE flag per row, then convert to
    // "row r+1 is continuation" by shifting.
    let mut row_wraps = vec![false; screen_lines];
    for cell in cells {
        if cell.row < screen_lines && cell.flags.contains(Flags::WRAPLINE) {
            row_wraps[cell.row] = true;
        }
    }
    let mut is_cont = vec![false; screen_lines];
    if screen_lines > 1 {
        is_cont[1..screen_lines].copy_from_slice(&row_wraps[..(screen_lines - 1)]);
    }
    is_cont
}

/// Extract row text strings from a cell grid for text-based hotspot detection.
fn extract_row_text_from_cells(cells: &[RenderCell], screen_lines: usize) -> Vec<String> {
    let mut rows: Vec<Vec<char>> = vec![Vec::new(); screen_lines];
    for cell in cells {
        if cell.row < screen_lines {
            let row_chars = &mut rows[cell.row];
            if cell.col >= row_chars.len() {
                row_chars.resize(cell.col + 1, ' ');
            }
            row_chars[cell.col] = cell.c;
        }
    }
    rows.into_iter()
        .map(|chars| chars.into_iter().collect())
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn extend_hotspots_from_patterns(
    engine: &PatternEngine,
    pane_id: u64,
    row_texts: &[String],
    hotspots: &mut Vec<TextHotspot>,
    widget_sink: &mut Vec<PatternWidgetMatch>,
    pane_vp_x: f32,
    pane_vp_y: f32,
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

        // Collect Widget-action matches for the widget rendering pass (tn-068b).
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
                    pane_vp_x,
                    pane_vp_y,
                    widget: w.clone(),
                });
            }
        }
    }
}

// ── Recursive pane rendering ───────────────────────────────────────────

/// Recursively render all panes in the layout tree.
///
/// Pass 1 of the two-pass renderer: terminal grid cells. Pane headers,
/// separators, and focus borders are still drawn directly here as part
/// of the per-pane sequence (they require glyphon text rendering which
/// needs its own prepare/render cycle). The semi-transparent overlay
/// pass for chrome backgrounds and modal widgets is composited in
/// `OverlayLayer::render()` after all pane content has been submitted.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_panes_recursive(
    node: &LayoutNode,
    focused: Option<PaneId>,
    show_focus: bool,
    pane_count: usize,
    show_pane_headers: bool,
    pane_counter: &mut usize,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
    agent_registry: &std::sync::Mutex<AgentRegistry>,
    claude_cwd: &ClaudeCwdTracker,
    pattern_engine: Option<&PatternEngine>,
) {
    match node {
        LayoutNode::Leaf(pane) => {
            *pane_counter += 1;
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("pane_encoder"),
            });
            render_single_pane(
                pane,
                focused == Some(pane.id) && show_focus,
                pane_count,
                show_pane_headers,
                renderer,
                device,
                queue,
                &mut encoder,
                view,
                surface_width,
                surface_height,
                agent_registry,
                claude_cwd,
                pattern_engine,
            );
            queue.submit(std::iter::once(encoder.finish()));
        }
        LayoutNode::Split {
            direction,
            first,
            second,
            ..
        } => {
            render_panes_recursive(
                first,
                focused,
                show_focus,
                pane_count,
                show_pane_headers,
                pane_counter,
                renderer,
                device,
                queue,
                view,
                surface_width,
                surface_height,
                agent_registry,
                claude_cwd,
                pattern_engine,
            );
            render_panes_recursive(
                second,
                focused,
                show_focus,
                pane_count,
                show_pane_headers,
                pane_counter,
                renderer,
                device,
                queue,
                view,
                surface_width,
                surface_height,
                agent_registry,
                claude_cwd,
                pattern_engine,
            );

            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("separator_encoder"),
            });
            draw_split_separator(
                *direction,
                first,
                second,
                focused,
                renderer,
                device,
                &mut encoder,
                view,
                surface_width,
                surface_height,
            );
            queue.submit(std::iter::once(encoder.finish()));
        }
        LayoutNode::Empty => {}
    }
}

// ── Single pane rendering ──────────────────────────────────────────────

/// Render a single pane within its viewport rect.
#[allow(clippy::too_many_arguments)]
fn render_single_pane(
    pane: &PaneState,
    draw_focus_border: bool,
    pane_count: usize,
    show_pane_headers: bool,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
    agent_registry: &std::sync::Mutex<AgentRegistry>,
    claude_cwd: &ClaudeCwdTracker,
    pattern_engine: Option<&PatternEngine>,
) {
    // Look up the Claude agent cwd and header title for this pane once
    // per frame (tn-ykxb, tn-5wrx). Two layers of Option because the
    // pane might not have an agent at all, and an agent might not be a
    // Claude process with a live state file.
    let agent_pid: Option<u32> = {
        let reg = agent_registry.lock().unwrap_or_else(|e| e.into_inner());
        reg.get(pane.id).and_then(|entry| entry.pid)
    };
    let claude_meta = agent_pid.and_then(|pid| claude_cwd.chrome_meta_for_pid(pid));
    let claude_agent_cwd: Option<std::path::PathBuf> =
        claude_meta.as_ref().and_then(|meta| meta.cwd.clone());
    let claude_header_title: Option<String> =
        claude_meta.as_ref().and_then(|meta| meta.header_title());
    let vp = pane.viewport;
    let term = match pane.backend.term() {
        Some(t) => t,
        None => return, // Non-terminal panes don't render via this path.
    };
    let mut term_guard = term.lock();

    let columns = term_guard.columns();
    let screen_lines = term_guard.screen_lines();
    let damaged_rows = match term_guard.damage() {
        TermDamage::Full => None,
        TermDamage::Partial(iter) => {
            let mut damaged = vec![false; screen_lines];
            for bounds in iter {
                if bounds.is_damaged() && bounds.line < screen_lines {
                    damaged[bounds.line] = true;
                }
            }
            Some(damaged)
        }
    };

    // Tell the renderer which pane we're about to render. Per-pane caches
    // are keyed by pane ID, so this must happen before any render call.
    renderer.set_current_pane(pane.id);

    let content = term_guard.renderable_content();
    let display_offset = content.display_offset;
    let cursor = content.cursor;
    let selection_range = content.selection;
    let is_remote_pane = matches!(pane.backend, PaneBackendKind::RemotePty { .. });

    // ── Skip undamaged frames entirely ──────────────────────────────────
    // When partial damage reports an empty set, nothing changed — use
    // the existing cached state without collecting cells or running
    // URL/hotspot detection.
    let cache_matches_viewport =
        renderer.pane_cache_matches_viewport(pane.id, columns, screen_lines, display_offset);
    let force_full_rebuild =
        should_force_full_rebuild(cache_matches_viewport, damaged_rows.as_deref())
            || (is_remote_pane && damage_rows_empty(damaged_rows.as_deref()));
    let damaged_rows_for_render = if force_full_rebuild {
        None
    } else {
        damaged_rows.as_deref()
    };

    if damage_rows_empty(damaged_rows.as_deref()) && cache_matches_viewport {
        term_guard.reset_damage();
        drop(term_guard);

        // Draw pane header (multi-pane) even when content is unchanged.
        let header_h = crate::pane::effective_header_height(pane_count, show_pane_headers);
        if pane_count > 1 && show_pane_headers {
            draw_pane_header(
                pane,
                draw_focus_border,
                claude_header_title.as_deref(),
                renderer,
                device,
                queue,
                encoder,
                view,
                surface_width,
                surface_height,
            );
        }

        let internal_pad_x = renderer.padding_x();
        let internal_pad_y = renderer.padding_y();
        renderer.set_viewport_offset(vp.x() + internal_pad_x, vp.y() + internal_pad_y + header_h);

        renderer.render_cached(
            &cursor,
            columns,
            screen_lines,
            selection_range.as_ref(),
            display_offset,
            device,
            queue,
            encoder,
            view,
            surface_width,
            surface_height,
        );
        renderer.restore_padding();

        if draw_focus_border {
            draw_pane_focus_border(
                pane,
                renderer,
                device,
                encoder,
                view,
                surface_width,
                surface_height,
            );
        }
        return;
    }

    let mut cells: Vec<RenderCell> = content
        .display_iter
        .filter_map(|indexed| {
            let point = indexed.point;
            let cell = indexed.cell;

            let viewport_line = point.line.0 + display_offset as i32;
            let row = usize::try_from(viewport_line).ok()?;
            if row >= screen_lines {
                return None;
            }

            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                return None;
            }

            let hyperlink = cell.hyperlink().map(|h| Arc::from(h.uri()));
            let hyperlink_source = if hyperlink.is_some() {
                Some(HyperlinkSource::Osc8)
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

    term_guard.reset_damage();
    drop(term_guard);

    // Pre-compute pane viewport pixel origin for widget placement (tn-068b).
    let header_h_early = crate::pane::effective_header_height(pane_count, show_pane_headers);
    let pane_vp_x = vp.x() + renderer.padding_x();
    let pane_vp_y = vp.y() + renderer.padding_y() + header_h_early;

    // ── Damage-aware URL/hotspot detection ───────────────────────────────
    // When partial damage is available, only run detection on damaged rows
    // to avoid re-scanning the entire visible area every frame.
    if let Some(damage_vec) = damaged_rows_for_render {
        // Collect indices of damaged-row cells to avoid cloning entire cells.
        // We build a separate vec of references for detection, then write
        // results back via a HashMap for O(1) lookup.  Complexity: O(D)
        // where D = number of damaged-row cells, instead of O(N*M).
        let mut damaged_cells: Vec<RenderCell> = cells
            .iter()
            .filter(|c| damage_vec.get(c.row).copied().unwrap_or(false))
            .cloned()
            .collect();

        detect_urls_in_cells(&mut damaged_cells, screen_lines);
        let row_texts = extract_row_text_from_cells(&damaged_cells, screen_lines);
        // Use the full cell set for wrap state — damaged cells alone may
        // not include the previous row's last cell (where WRAPLINE lives).
        let wrap_cont = compute_wrap_continuation(&cells, screen_lines);
        let mut hotspots = detect_hotspots_from_text_with_wrap(&row_texts, &wrap_cont);
        promote_directory_hotspots_from_fs(&mut hotspots);
        if let Some(ref cwd) = claude_agent_cwd {
            hotspots.extend(detect_claude_tool_call_hotspots(&row_texts, cwd));
        }
        if let Some(engine) = pattern_engine {
            extend_hotspots_from_patterns(
                engine,
                pane.id,
                &row_texts,
                &mut hotspots,
                &mut renderer.pattern_widget_sink,
                pane_vp_x,
                pane_vp_y,
            );
        }

        // O(1) copy-back: build a HashMap from detected URLs keyed by (row, col),
        // then single-pass over `cells` to apply. Avoids O(N*M) linear search.
        let url_map: HashMap<(usize, usize), (Arc<str>, HyperlinkSource)> = damaged_cells
            .into_iter()
            .filter_map(|dc| {
                dc.hyperlink.map(|link| {
                    (
                        (dc.row, dc.col),
                        (link, dc.hyperlink_source.unwrap_or(HyperlinkSource::Regex)),
                    )
                })
            })
            .collect();

        if !url_map.is_empty() {
            for cell in cells.iter_mut() {
                if let Some((link, source)) = url_map.get(&(cell.row, cell.col)) {
                    cell.hyperlink = Some(Arc::clone(link));
                    cell.hyperlink_source = Some(*source);
                }
            }
        }

        // Row-indexed hotspot application: O(C + H) instead of O(H*C).
        // Build a map of row -> hotspots, then single-pass over cells checking
        // only hotspots on the matching row.
        apply_hotspots_to_cells(&mut cells, &hotspots);
    } else {
        // Full damage — detect on all cells.
        detect_urls_in_cells(&mut cells, screen_lines);
        let row_texts = extract_row_text_from_cells(&cells, screen_lines);
        let wrap_cont = compute_wrap_continuation(&cells, screen_lines);
        let mut hotspots = detect_hotspots_from_text_with_wrap(&row_texts, &wrap_cont);
        promote_directory_hotspots_from_fs(&mut hotspots);
        if let Some(ref cwd) = claude_agent_cwd {
            hotspots.extend(detect_claude_tool_call_hotspots(&row_texts, cwd));
        }
        if let Some(engine) = pattern_engine {
            extend_hotspots_from_patterns(
                engine,
                pane.id,
                &row_texts,
                &mut hotspots,
                &mut renderer.pattern_widget_sink,
                pane_vp_x,
                pane_vp_y,
            );
        }

        // Row-indexed hotspot application: O(C + H) instead of O(H*C).
        apply_hotspots_to_cells(&mut cells, &hotspots);
    }

    // ── Draw pane header strip (only when multiple panes and enabled) ──
    let header_h = crate::pane::effective_header_height(pane_count, show_pane_headers);
    if pane_count > 1 && show_pane_headers {
        draw_pane_header(
            pane,
            draw_focus_border,
            claude_header_title.as_deref(),
            renderer,
            device,
            queue,
            encoder,
            view,
            surface_width,
            surface_height,
        );
    }

    // Temporarily adjust renderer's padding to offset by the pane's viewport origin,
    // plus the pane header height.
    let internal_pad_x = renderer.padding_x();
    let internal_pad_y = renderer.padding_y();
    renderer.set_viewport_offset(vp.x() + internal_pad_x, vp.y() + internal_pad_y + header_h);

    renderer.render(
        &cells,
        &cursor,
        columns,
        screen_lines,
        selection_range.as_ref(),
        display_offset,
        damaged_rows_for_render,
        device,
        queue,
        encoder,
        view,
        surface_width,
        surface_height,
    );

    renderer.restore_padding();

    // Draw focus indicator border for the focused pane.
    if draw_focus_border {
        draw_pane_focus_border(
            pane,
            renderer,
            device,
            encoder,
            view,
            surface_width,
            surface_height,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{damage_rows_empty, damaged_rows_any, should_force_full_rebuild};

    #[test]
    fn damaged_rows_any_detects_nonempty_damage() {
        assert!(!damaged_rows_any(None));
        assert!(!damaged_rows_any(Some(&[])));
        assert!(!damaged_rows_any(Some(&[false, false])));
        assert!(damaged_rows_any(Some(&[false, true, false])));
    }

    #[test]
    fn damage_rows_empty_detects_all_false_vectors() {
        assert!(!damage_rows_empty(None));
        assert!(damage_rows_empty(Some(&[])));
        assert!(damage_rows_empty(Some(&[false, false])));
        assert!(!damage_rows_empty(Some(&[false, true, false])));
    }

    #[test]
    fn force_full_rebuild_whenever_cache_invalid() {
        // Cache invalid → always force full rebuild regardless of damage state.
        assert!(should_force_full_rebuild(false, Some(&[])));
        assert!(should_force_full_rebuild(false, Some(&[false, false])));
        // Cache invalid with partial damage → still force full rebuild (theme-change fix).
        assert!(should_force_full_rebuild(false, Some(&[false, true])));
        assert!(should_force_full_rebuild(false, None));
        // Cache valid → do not force full rebuild.
        assert!(!should_force_full_rebuild(true, Some(&[false, false])));
        assert!(!should_force_full_rebuild(true, Some(&[false, true])));
        assert!(!should_force_full_rebuild(true, None));
    }
}
