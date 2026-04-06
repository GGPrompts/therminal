//! Per-pane render orchestration: bridges the layout tree to GridRenderer.
//!
//! Contains the recursive pane traversal that renders each pane's terminal
//! content, headers, and separators.

use std::collections::HashSet;

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::TermDamage;

use crate::grid_renderer::{cell_display_text, GridRenderer, RenderCell};
use crate::hotspot_detection::detect_hotspots;
use crate::pane::{LayoutNode, PaneId, PaneState};
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
    let mut term_guard = pane.term.lock();

    let damaged_rows = match term_guard.damage() {
        TermDamage::Full => None,
        TermDamage::Partial(iter) => {
            let set: HashSet<usize> = iter
                .filter_map(|bounds| {
                    if bounds.is_damaged() {
                        Some(bounds.line)
                    } else {
                        None
                    }
                })
                .collect();
            Some(set)
        }
    };

    let content = term_guard.renderable_content();
    let screen_lines = term_guard.screen_lines();
    let display_offset = content.display_offset;
    let cursor = content.cursor;
    let selection_range = content.selection;

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

            let hyperlink = cell.hyperlink().map(|h| h.uri().to_owned());

            Some(RenderCell {
                row,
                col: point.column.0,
                c: cell.c,
                text: cell_display_text(cell.c, cell.zerowidth()),
                fg: cell.fg,
                bg: cell.bg,
                flags: cell.flags,
                hyperlink,
                hotspot: None,
            })
        })
        .collect();

    term_guard.reset_damage();
    drop(term_guard);

    // Annotate cells with detected hotspots (file paths, errors, git refs, etc.).
    let hotspots = detect_hotspots(&cells, screen_lines);
    for hotspot in &hotspots {
        for cell in cells.iter_mut() {
            if cell.row == hotspot.row
                && cell.col >= hotspot.start_col
                && cell.col < hotspot.end_col
            {
                cell.hotspot = Some(hotspot.kind.clone());
            }
        }
    }

    // In multi-pane mode, clear per-pane caches so stale state from a previous pane
    // doesn't bleed through. This forces a full rebuild (damaged_rows = None) since
    // the cache was just wiped. In single-pane mode, keep the cache for incremental
    // rendering -- without this, undamaged rows disappear after the cache clear.
    let damaged_rows = if pane_count > 1 {
        renderer.reset_pane_caches();
        None // force full rebuild after cache clear
    } else {
        damaged_rows
    };

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
        damaged_rows.as_ref(),
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
