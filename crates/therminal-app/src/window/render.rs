//! Per-pane render orchestration: bridges the layout tree to GridRenderer.
//!
//! Contains the recursive pane traversal that renders each pane's terminal
//! content, headers, and separators.

use std::sync::Arc;

use alacritty_terminal::term::TermDamage;
use alacritty_terminal::term::cell::Flags;

use crate::grid_renderer::{GridRenderer, HyperlinkSource, RenderCell, cell_display_text};
use crate::hotspot_detection::detect_hotspots;
use crate::pane::{LayoutNode, PaneId, PaneState};
use crate::url_detection::detect_urls_in_cells;
use alacritty_terminal::grid::Dimensions;

use super::chrome::{draw_pane_focus_border, draw_pane_header, draw_split_separator};

// ── Recursive pane rendering ───────────────────────────────────────────

/// Recursively render all panes in the layout tree.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_panes_recursive(
    node: &LayoutNode,
    focused: Option<PaneId>,
    show_focus: bool,
    pane_count: usize,
    pane_counter: &mut usize,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    match node {
        LayoutNode::Leaf(pane) => {
            let idx = *pane_counter;
            *pane_counter += 1;
            // Each pane gets its own encoder so that glyphon prepare()/render()
            // for one pane doesn't overwrite another pane's glyph data in the
            // shared atlas before the GPU executes the draw commands.
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("pane_encoder"),
            });
            render_single_pane(
                pane,
                idx,
                focused == Some(pane.id) && show_focus,
                pane_count,
                renderer,
                device,
                queue,
                &mut encoder,
                view,
                surface_width,
                surface_height,
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
                pane_counter,
                renderer,
                device,
                queue,
                view,
                surface_width,
                surface_height,
            );
            render_panes_recursive(
                second,
                focused,
                show_focus,
                pane_count,
                pane_counter,
                renderer,
                device,
                queue,
                view,
                surface_width,
                surface_height,
            );

            // Draw separator line between the two children.
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
    pane_index: usize,
    draw_focus_border: bool,
    pane_count: usize,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    let vp = pane.viewport;
    let term = match pane.backend.term() {
        Some(t) => t,
        None => return, // Non-terminal panes don't render via this path.
    };
    let mut term_guard = term.lock();

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

    // ── Skip undamaged frames entirely ──────────────────────────────────
    // When partial damage reports an empty set, nothing changed — use
    // the existing cached state without collecting cells or running
    // URL/hotspot detection.
    if let Some(ref damaged) = damaged_rows
        && !damaged.iter().any(|&d| d)
    {
        term_guard.reset_damage();
        drop(term_guard);

        // Draw pane header (multi-pane) even when content is unchanged.
        let header_h = crate::pane::effective_header_height(pane_count);
        if pane_count > 1 {
            draw_pane_header(
                pane,
                pane_index,
                draw_focus_border,
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

    // ── Damage-aware URL/hotspot detection ───────────────────────────────
    // When partial damage is available, only run detection on damaged rows
    // to avoid re-scanning the entire visible area every frame.
    if let Some(ref damage_vec) = damaged_rows {
        // Filter to only damaged-row cells for detection.
        let mut damaged_cells: Vec<RenderCell> = cells
            .iter()
            .filter(|c| damage_vec.get(c.row).copied().unwrap_or(false))
            .cloned()
            .collect();

        detect_urls_in_cells(&mut damaged_cells, screen_lines);
        let hotspots = detect_hotspots(&damaged_cells, screen_lines);

        // Apply detected URLs back to the main cells vec.
        for dc in &damaged_cells {
            if dc.hyperlink.is_some()
                && let Some(cell) = cells
                    .iter_mut()
                    .find(|c| c.row == dc.row && c.col == dc.col)
            {
                cell.hyperlink.clone_from(&dc.hyperlink);
                cell.hyperlink_source = dc.hyperlink_source;
            }
        }

        // Apply detected hotspots to the main cells vec.
        for hotspot in &hotspots {
            for cell in cells.iter_mut() {
                if cell.row == hotspot.row
                    && cell.col >= hotspot.start_col
                    && cell.col < hotspot.end_col
                {
                    cell.hotspot = Some((hotspot.kind.clone(), Arc::clone(&hotspot.text)));
                }
            }
        }
    } else {
        // Full damage — detect on all cells.
        detect_urls_in_cells(&mut cells, screen_lines);
        let hotspots = detect_hotspots(&cells, screen_lines);
        for hotspot in &hotspots {
            for cell in cells.iter_mut() {
                if cell.row == hotspot.row
                    && cell.col >= hotspot.start_col
                    && cell.col < hotspot.end_col
                {
                    cell.hotspot = Some((hotspot.kind.clone(), Arc::clone(&hotspot.text)));
                }
            }
        }
    }

    // ── Draw pane header strip (only when multiple panes) ────────────────
    let header_h = crate::pane::effective_header_height(pane_count);
    if pane_count > 1 {
        draw_pane_header(
            pane,
            pane_index,
            draw_focus_border,
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
        screen_lines,
        selection_range.as_ref(),
        display_offset,
        damaged_rows.as_deref(),
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
