//! Keybinding help overlay: a centered semi-transparent panel listing all
//! configured keybindings grouped by section.
//!
//! Rendered as a final pass on top of all pane content, status bar, and chrome.
//!
//! Layout strategy:
//!   1. Auto-size the panel to fit content height, capped at ~80% of window height.
//!   2. If content exceeds the cap, switch to a two-column layout with the split
//!      placed at a category boundary (categories are never broken).
//!   3. If even two columns at 80% height overflow, the inner panel acts as a
//!      scissor (via glyphon `TextBounds`) so nothing bleeds outside the box,
//!      and a "more bindings hidden" hint is drawn at the bottom.

use std::collections::{BTreeMap, HashSet};

use wgpu::util::DeviceExt;

use crate::grid_renderer::{ColorVertex, GridRenderer};
use therminal_core::config::KeybindingsConfig;
use therminal_core::palette::Color as PaletteColor;

// ── Overlay colors ────────────────────────────────────────────────────

/// Semi-transparent dark background for the overlay scrim.
const SCRIM_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.6];

/// Panel background (PLATE from palette with high alpha).
const PANEL_BG_COLOR: [f32; 4] = {
    let c = PaletteColor::PLATE;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        0.95,
    ]
};

// ── Section order ─────────────────────────────────────────────────────

/// Fixed ordering for help sections.
fn section_order(name: &str) -> u8 {
    match name {
        "Pane Management" => 0,
        "Font" => 1,
        "General" => 2,
        "WebView" => 3,
        "Navigation" => 4,
        "Widgets" => 5,
        "Mouse" => 6,
        _ => 7,
    }
}

// ── Layout planner (pure, testable) ───────────────────────────────────

/// Result of laying out the help overlay's category list.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HelpLayoutPlan {
    /// All categories fit in a single column. `content_h` is the actual
    /// height needed (not the cap).
    SingleColumn { content_h: f32 },
    /// Categories were split across two columns. `split_index` is the index
    /// of the first category that goes into column 1 (so column 0 has
    /// `[0..split_index)`). `content_h` is the height of the taller column.
    /// `clipped` is true when even the two-column layout overflows the cap.
    TwoColumn {
        split_index: usize,
        content_h: f32,
        clipped: bool,
    },
}

/// Decide single- vs two-column layout for the help overlay given each
/// category's height (header + binding rows) and the maximum allowed
/// inner content height.
///
/// The split is chosen to minimise the taller of the two columns while
/// respecting category boundaries. If `cap` is non-positive, returns a
/// clipped two-column layout with `split_index` at the midpoint.
pub(crate) fn plan_help_layout(cat_heights: &[f32], cap: f32) -> HelpLayoutPlan {
    let total: f32 = cat_heights.iter().sum();
    if cap <= 0.0 {
        // No room at all — return a clipped two-column plan as a sentinel.
        let mid = cat_heights.len() / 2;
        return HelpLayoutPlan::TwoColumn {
            split_index: mid,
            content_h: total,
            clipped: true,
        };
    }
    if total <= cap || cat_heights.len() < 2 {
        return HelpLayoutPlan::SingleColumn { content_h: total };
    }

    // Find the split that minimises max(col0, col1).
    let mut best_split = 1usize;
    let mut best_max = f32::INFINITY;
    let mut prefix = 0.0_f32;
    for i in 1..cat_heights.len() {
        prefix += cat_heights[i - 1];
        let col0 = prefix;
        let col1 = total - prefix;
        let m = col0.max(col1);
        if m < best_max {
            best_max = m;
            best_split = i;
        }
    }

    let clipped = best_max > cap;
    HelpLayoutPlan::TwoColumn {
        split_index: best_split,
        content_h: best_max,
        clipped,
    }
}

// ── Draw the help overlay ─────────────────────────────────────────────

/// Draw the keybinding help overlay centered in the window.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_help_overlay(
    keybindings: &KeybindingsConfig,
    scroll_rows: u32,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;
    use glyphon::{
        Attrs, Buffer, Color as GlyphColor, Family, Metrics, Resolution, Shaping, TextArea,
        TextBounds, Weight,
    };

    let sw = surface_width as f32;
    let sh = surface_height as f32;

    // ── Full-screen scrim ───────────────────────────────────────────────
    let scrim_verts = pixel_rect_to_ndc(0.0, 0.0, sw, sh, sw, sh, SCRIM_COLOR);

    // ── Layout constants ────────────────────────────────────────────────
    let row_h = 24.0_f32;
    let header_row_h = 32.0_f32;
    let padding_v = 20.0_f32;
    let padding_h = 24.0_f32;
    let title_h = row_h + 4.0 + 8.0; // title metric line + spacing below
    let column_gap = 32.0_f32;
    let hint_h = row_h;

    // Group bindings into sections, collapsing numeric families and
    // deduplicating rows that render identically.
    let categories: Vec<(String, Vec<(String, String)>)> = build_help_categories(keybindings);
    let cat_heights: Vec<f32> = categories
        .iter()
        .map(|(_, b)| header_row_h + b.len() as f32 * row_h)
        .collect();

    // ── Layout planning ─────────────────────────────────────────────────
    let panel_w_one = (sw * 0.55).clamp(340.0, 600.0).min(sw - 20.0);
    let panel_w_two = (sw * 0.85).clamp(680.0, 1100.0).min(sw - 20.0);
    let max_panel_h = sh * 0.80;
    let inner_max_h = (max_panel_h - padding_v * 2.0 - title_h - hint_h).max(0.0);

    let plan = plan_help_layout(&cat_heights, inner_max_h);
    let (panel_w, columns_count, content_inner_h, clipped, split_index) = match &plan {
        HelpLayoutPlan::SingleColumn { content_h } => {
            (panel_w_one, 1usize, *content_h, false, categories.len())
        }
        HelpLayoutPlan::TwoColumn {
            content_h,
            clipped,
            split_index,
        } => (panel_w_two, 2usize, *content_h, *clipped, *split_index),
    };

    let extra_h = if clipped { hint_h } else { 0.0 };
    let panel_h = (title_h + content_inner_h + extra_h + padding_v * 2.0).min(max_panel_h);

    let max_scroll_rows = max_scroll_rows(keybindings, surface_width, surface_height);
    let scroll_rows = scroll_rows.min(max_scroll_rows);
    let scroll_offset = scroll_rows as f32 * row_h;

    let panel_x = (sw - panel_w) / 2.0;
    let panel_y = (sh - panel_h) / 2.0;

    let panel_verts = pixel_rect_to_ndc(panel_x, panel_y, panel_w, panel_h, sw, sh, PANEL_BG_COLOR);

    // ── Draw background rects ───────────────────────────────────────────
    let mut all_verts: Vec<ColorVertex> = Vec::new();
    all_verts.extend_from_slice(&scrim_verts);
    all_verts.extend_from_slice(&panel_verts);

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("help_overlay_bg_vbuf"),
        contents: bytemuck::cast_slice(&all_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("help_overlay_bg_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&renderer.rect_pipeline);
        pass.set_vertex_buffer(0, vertex_buf.slice(..));
        pass.draw(0..all_verts.len() as u32, 0..1);
    }

    // ── Text content ────────────────────────────────────────────────────
    let font_size = 14.0_f32;
    let metrics = Metrics::new(font_size, row_h);
    let header_metrics = Metrics::new(font_size + 2.0, header_row_h);
    let title_metrics = Metrics::new(font_size + 4.0, row_h + 4.0);

    let text_color = GlyphColor::rgba(
        PaletteColor::INK.r,
        PaletteColor::INK.g,
        PaletteColor::INK.b,
        240,
    );
    let muted_color = GlyphColor::rgba(
        PaletteColor::INK_MUTED.r,
        PaletteColor::INK_MUTED.g,
        PaletteColor::INK_MUTED.b,
        220,
    );
    let accent_color = GlyphColor::rgba(
        PaletteColor::FOCUS.r,
        PaletteColor::FOCUS.g,
        PaletteColor::FOCUS.b,
        255,
    );

    // Inner panel bounds for static text (title/footer), plus a dedicated
    // scroll viewport for binding rows so the list cannot overlap header/footer.
    let inner_left = panel_x + padding_h;
    let inner_top = panel_y + padding_v;
    let inner_right = panel_x + panel_w - padding_h;
    let inner_bottom = panel_y + panel_h - padding_v;
    let static_bounds = TextBounds {
        left: inner_left as i32,
        top: inner_top as i32,
        right: inner_right as i32,
        bottom: inner_bottom as i32,
    };

    let content_x = inner_left;
    let mut current_y = inner_top;

    let col_inner_w = if columns_count == 2 {
        ((panel_w - padding_h * 2.0 - column_gap) / 2.0).max(0.0)
    } else {
        panel_w - padding_h * 2.0
    };
    let col2_x_offset = col_inner_w * 0.45; // shortcut→description split inside a column

    let mut text_areas: Vec<TextArea<'_>> = Vec::new();
    let mut buffers: Vec<Buffer> = Vec::new();

    // Title (spans full panel width)
    let title_inner_w = panel_w - padding_h * 2.0;
    let mut title_buf = Buffer::new(&mut renderer.font_system, title_metrics);
    title_buf.set_size(
        &mut renderer.font_system,
        Some(title_inner_w),
        Some(row_h + 4.0),
    );
    title_buf.set_text(
        &mut renderer.font_system,
        "Keybindings",
        &Attrs::new()
            .family(Family::Name(renderer.font_config.chrome_font_family()))
            .weight(Weight::BOLD)
            .color(text_color),
        Shaping::Basic,
        None,
    );
    title_buf.shape_until_scroll(&mut renderer.font_system, false);
    buffers.push(title_buf);
    current_y += row_h + 8.0;

    // Build all section + binding buffers.
    struct RowInfo {
        buf_idx: usize,
        x: f32,
        y: f32,
        color: GlyphColor,
        scrollable: bool,
    }
    let mut rows: Vec<RowInfo> = Vec::new();

    rows.push(RowInfo {
        buf_idx: 0,
        x: content_x,
        y: panel_y + padding_v,
        color: text_color,
        scrollable: false,
    });

    let title_baseline_y = current_y;
    let content_bottom = if max_scroll_rows > 0 {
        inner_bottom - hint_h
    } else {
        inner_bottom
    };
    let content_bounds = TextBounds {
        left: inner_left as i32,
        top: title_baseline_y as i32,
        right: inner_right as i32,
        bottom: content_bottom as i32,
    };
    let col0_x = content_x;
    let col1_x = content_x + col_inner_w + column_gap;

    for (cat_idx, (section_name, bindings)) in categories.iter().enumerate() {
        // Switch to the second column at the planned split boundary.
        if columns_count == 2 && cat_idx == split_index {
            current_y = title_baseline_y;
        }
        let col_x = if columns_count == 2 && cat_idx >= split_index {
            col1_x
        } else {
            col0_x
        };
        let col_x_desc = col_x + col2_x_offset;

        // Section header
        let mut hdr_buf = Buffer::new(&mut renderer.font_system, header_metrics);
        hdr_buf.set_size(
            &mut renderer.font_system,
            Some(col_inner_w),
            Some(header_row_h),
        );
        hdr_buf.set_text(
            &mut renderer.font_system,
            section_name,
            &Attrs::new()
                .family(Family::Name(renderer.font_config.chrome_font_family()))
                .weight(Weight::BOLD)
                .color(accent_color),
            Shaping::Basic,
            None,
        );
        hdr_buf.shape_until_scroll(&mut renderer.font_system, false);
        buffers.push(hdr_buf);
        rows.push(RowInfo {
            buf_idx: buffers.len() - 1,
            x: col_x,
            y: current_y - scroll_offset,
            color: accent_color,
            scrollable: true,
        });
        current_y += header_row_h;

        for (shortcut, description) in bindings {
            // Shortcut (left within column)
            let mut key_buf = Buffer::new(&mut renderer.font_system, metrics);
            key_buf.set_size(
                &mut renderer.font_system,
                Some(col_inner_w * 0.42),
                Some(row_h),
            );
            key_buf.set_text(
                &mut renderer.font_system,
                shortcut,
                &Attrs::new()
                    .family(Family::Name(renderer.font_config.chrome_font_family()))
                    .color(text_color),
                Shaping::Basic,
                None,
            );
            key_buf.shape_until_scroll(&mut renderer.font_system, false);
            buffers.push(key_buf);
            rows.push(RowInfo {
                buf_idx: buffers.len() - 1,
                x: col_x,
                y: current_y - scroll_offset,
                color: text_color,
                scrollable: true,
            });

            // Description (right within column)
            let mut desc_buf = Buffer::new(&mut renderer.font_system, metrics);
            desc_buf.set_size(
                &mut renderer.font_system,
                Some(col_inner_w * 0.55),
                Some(row_h),
            );
            desc_buf.set_text(
                &mut renderer.font_system,
                description,
                &Attrs::new()
                    .family(Family::Name(renderer.font_config.chrome_font_family()))
                    .color(muted_color),
                Shaping::Basic,
                None,
            );
            desc_buf.shape_until_scroll(&mut renderer.font_system, false);
            buffers.push(desc_buf);
            rows.push(RowInfo {
                buf_idx: buffers.len() - 1,
                x: col_x_desc,
                y: current_y - scroll_offset,
                color: muted_color,
                scrollable: true,
            });

            current_y += row_h;
        }
    }

    if max_scroll_rows > 0 {
        let hint_y = panel_y + panel_h - padding_v - hint_h;
        let mut hint_buf = Buffer::new(&mut renderer.font_system, metrics);
        hint_buf.set_size(
            &mut renderer.font_system,
            Some(panel_w - padding_h * 2.0),
            Some(row_h),
        );
        let hint = format!(
            "scroll: wheel / PgUp PgDn / arrows ({}/{})",
            scroll_rows, max_scroll_rows
        );
        hint_buf.set_text(
            &mut renderer.font_system,
            &hint,
            &Attrs::new()
                .family(Family::Name(renderer.font_config.chrome_font_family()))
                .color(muted_color),
            Shaping::Basic,
            None,
        );
        hint_buf.shape_until_scroll(&mut renderer.font_system, false);
        buffers.push(hint_buf);
        rows.push(RowInfo {
            buf_idx: buffers.len() - 1,
            x: content_x,
            y: hint_y,
            color: muted_color,
            scrollable: false,
        });
    }

    // Build TextArea references from the stored buffers and positions.
    for row in &rows {
        let bounds = if row.scrollable {
            content_bounds
        } else {
            static_bounds
        };
        text_areas.push(TextArea {
            buffer: &buffers[row.buf_idx],
            left: row.x,
            top: row.y,
            scale: 1.0,
            bounds,
            default_color: row.color,
            custom_glyphs: &[],
        });
    }

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    if let Err(e) = renderer.overlay_text_renderer.prepare(
        device,
        queue,
        &mut renderer.font_system,
        &mut renderer.overlay_atlas,
        &renderer.viewport,
        text_areas,
        &mut renderer.swash_cache,
    ) {
        tracing::warn!("help overlay text prepare failed: {}", e);
    }

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("help_overlay_text_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });

        if let Err(e) = renderer.overlay_text_renderer.render(
            &renderer.overlay_atlas,
            &renderer.viewport,
            &mut pass,
        ) {
            tracing::warn!("help overlay text render failed: {}", e);
        }
    }
}

/// Return the maximum vertical scroll offset for the help overlay body,
/// measured in whole row steps.
pub(crate) fn max_scroll_rows(
    keybindings: &KeybindingsConfig,
    _surface_width: u32,
    surface_height: u32,
) -> u32 {
    let sh = surface_height as f32;

    let row_h = 24.0_f32;
    let header_row_h = 32.0_f32;
    let padding_v = 20.0_f32;
    let title_h = row_h + 4.0 + 8.0;
    let hint_h = row_h;

    let categories: Vec<(String, Vec<(String, String)>)> = build_help_categories(keybindings);
    let cat_heights: Vec<f32> = categories
        .iter()
        .map(|(_, b)| header_row_h + b.len() as f32 * row_h)
        .collect();

    let max_panel_h = sh * 0.80;
    let inner_max_h = (max_panel_h - padding_v * 2.0 - title_h - hint_h).max(0.0);
    let plan = plan_help_layout(&cat_heights, inner_max_h);
    let content_inner_h = match plan {
        HelpLayoutPlan::SingleColumn { content_h } => content_h,
        HelpLayoutPlan::TwoColumn { content_h, .. } => content_h,
    };

    let overflow_px = (content_inner_h - inner_max_h).max(0.0);
    let rows = (overflow_px / row_h).ceil();
    if rows.is_finite() && rows > 0.0 {
        rows as u32
    } else {
        0
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Build the ordered category list shown in the help overlay.
///
/// Responsibilities:
///   1. Group bindings by section in fixed order (`section_order`).
///   2. Collapse contiguous numeric families (e.g. `Alt+1..Alt+9` for
///      `SwitchWorkspace`) into a single `Alt+(1-9)` placeholder row.
///   3. Deduplicate rows that render identically — some actions are
///      intentionally bound to two physical keys for cross-platform layout
///      reasons (e.g. `ctrl+shift+/` and `ctrl+shift+?` both map to
///      `ShowHelp`, and `format_shortcut` normalizes `/` → `?` so both
///      produce the same label). See tn-ftlo.
///
/// Pure + testable: does not touch the renderer, GPU, or any global state.
pub(crate) fn build_help_categories(
    keybindings: &KeybindingsConfig,
) -> Vec<(String, Vec<(String, String)>)> {
    let mut sections: BTreeMap<u8, (String, Vec<(String, String)>)> = BTreeMap::new();
    let mut seen_numeric_groups: HashSet<(u8, String, String)> = HashSet::new();
    // Per-section set of (formatted_shortcut, description) rows we've
    // already emitted, so a dual binding like ctrl+shift+/ and ctrl+shift+?
    // does not produce two identical lines.
    let mut seen_rows: HashSet<(u8, String, String)> = HashSet::new();

    for binding in &keybindings.bindings {
        let section_name = binding.action.section().to_string();
        let order = section_order(&section_name);
        let entry = sections
            .entry(order)
            .or_insert_with(|| (section_name, Vec::new()));
        let shortcut = format_shortcut(&binding.key);
        let description = binding.action.description().to_string();

        let (row_shortcut, row_description) =
            if let Some(sig) = numeric_group_signature(&shortcut, &binding.action) {
                let key = (order, sig.0.clone(), sig.1);
                if !seen_numeric_groups.insert(key) {
                    continue;
                }
                (format!("{}(1-9)", sig.0), description)
            } else {
                (shortcut, description)
            };

        let row_key = (order, row_shortcut.clone(), row_description.clone());
        if !seen_rows.insert(row_key) {
            continue;
        }
        entry.1.push((row_shortcut, row_description));
    }

    // ── Hardcoded "Mouse" section for non-keybinding mouse conventions ──
    // These are implicit modifier behaviours, not configurable keybindings,
    // but users need to discover them (tn-vevc).
    let mouse_order = section_order("Mouse");
    let mouse_section = sections
        .entry(mouse_order)
        .or_insert_with(|| ("Mouse".to_string(), Vec::new()));
    mouse_section.1.push((
        "Shift+Click".to_string(),
        "Bypass TUI mouse capture (select text / open hotspot)".to_string(),
    ));
    mouse_section.1.push((
        "Shift+RightClick".to_string(),
        "Force context menu (TUI mouse / WebView)".to_string(),
    ));
    mouse_section.1.push((
        "Shift+Scroll".to_string(),
        "Scrollback in TUI mouse mode".to_string(),
    ));

    sections.values().cloned().collect()
}

/// If this binding belongs to a numeric key family (shortcut ends in a
/// single digit, action Debug repr has a trailing digit component), return
/// `(modifier_prefix, action_family)` where:
///
/// - `modifier_prefix` is the formatted shortcut up to and including the
///   final `+`, e.g. `"Alt+"` for `Alt+3`, or `""` for `3`.
/// - `action_family` is the action's `Debug` representation with the
///   trailing digit group stripped, e.g. `"SwitchWorkspace"` for
///   `SwitchWorkspace(3)` or `"Tab"` for `Tab7`.
///
/// This is general enough that future `Ctrl+Tab1..9` style groups collapse
/// automatically without extra code.
fn numeric_group_signature(
    shortcut: &str,
    action: &therminal_core::config::KeyAction,
) -> Option<(String, String)> {
    let last = shortcut.chars().next_back()?;
    if !last.is_ascii_digit() {
        return None;
    }
    let prefix_len = shortcut.len() - last.len_utf8();
    let prefix = &shortcut[..prefix_len];
    if !prefix.is_empty() && !prefix.ends_with('+') {
        return None;
    }

    let dbg = format!("{action:?}");
    let family = strip_trailing_digit_group(&dbg)?;

    Some((prefix.to_string(), family))
}

/// Strip a trailing digit group from a Debug repr.
fn strip_trailing_digit_group(dbg: &str) -> Option<String> {
    if let Some(open) = dbg.rfind('(')
        && dbg.ends_with(')')
    {
        let inner = &dbg[open + 1..dbg.len() - 1];
        if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()) {
            return Some(dbg[..open].to_string());
        }
    }
    let trimmed = dbg.trim_end_matches(|c: char| c.is_ascii_digit());
    if trimmed.len() < dbg.len() && !trimmed.is_empty() {
        return Some(trimmed.to_string());
    }
    None
}

/// Format a binding key string for display (e.g. "ctrl+shift+h" -> "Ctrl+Shift+H").
fn format_shortcut(key: &str) -> String {
    key.split('+')
        .map(|part| {
            let trimmed = part.trim();
            match trimmed.to_lowercase().as_str() {
                "ctrl" | "control" => "Ctrl".to_string(),
                "shift" => "Shift".to_string(),
                "alt" | "option" => "Alt".to_string(),
                "super" | "meta" | "cmd" | "win" => "Super".to_string(),
                "enter" | "return" => "Enter".to_string(),
                "escape" | "esc" => "Esc".to_string(),
                "plus" => "+".to_string(),
                "minus" => "-".to_string(),
                "space" => "Space".to_string(),
                "up" | "arrowup" => "Up".to_string(),
                "down" | "arrowdown" => "Down".to_string(),
                "left" | "arrowleft" => "Left".to_string(),
                "right" | "arrowright" => "Right".to_string(),
                "pageup" | "page_up" => "PgUp".to_string(),
                "pagedown" | "page_down" => "PgDn".to_string(),
                "backspace" => "Backspace".to_string(),
                "delete" | "del" => "Delete".to_string(),
                "insert" | "ins" => "Insert".to_string(),
                "home" => "Home".to_string(),
                "end" => "End".to_string(),
                "tab" => "Tab".to_string(),
                s if s.starts_with('f') && s[1..].parse::<u8>().is_ok() => s.to_uppercase(),
                "/" => "?".to_string(), // Ctrl+Shift+/ produces ?
                other => {
                    let mut c = other.chars();
                    match c.next() {
                        Some(first) => first.to_uppercase().to_string() + c.as_str(),
                        None => other.to_string(),
                    }
                }
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cats(sizes: &[usize]) -> Vec<f32> {
        // Match production formula: header_row_h (32) + N * row_h (24)
        sizes.iter().map(|n| 32.0 + *n as f32 * 24.0).collect()
    }

    #[test]
    fn small_content_tall_window_single_column() {
        // Three small categories, plenty of vertical room.
        let heights = cats(&[3, 4, 2]);
        let plan = plan_help_layout(&heights, 1000.0);
        match plan {
            HelpLayoutPlan::SingleColumn { content_h } => {
                let total: f32 = heights.iter().sum();
                assert!((content_h - total).abs() < 1e-3);
            }
            other => panic!("expected SingleColumn, got {other:?}"),
        }
    }

    #[test]
    fn large_content_tall_window_single_column_at_content_height() {
        // Total just barely fits.
        let heights = cats(&[10, 10, 10]);
        let total: f32 = heights.iter().sum();
        let plan = plan_help_layout(&heights, total + 1.0);
        match plan {
            HelpLayoutPlan::SingleColumn { content_h } => {
                assert!((content_h - total).abs() < 1e-3);
            }
            other => panic!("expected SingleColumn, got {other:?}"),
        }
    }

    #[test]
    fn large_content_short_window_switches_to_two_columns() {
        let heights = cats(&[8, 8, 8, 8]);
        let total: f32 = heights.iter().sum();
        // Cap below total but big enough that the best split fits.
        let plan = plan_help_layout(&heights, total / 2.0 + 50.0);
        match plan {
            HelpLayoutPlan::TwoColumn {
                split_index,
                content_h,
                clipped,
            } => {
                assert!(!clipped, "should fit in two columns without clipping");
                assert!(split_index >= 1 && split_index < heights.len());
                // Column heights are at most ~half + some.
                assert!(content_h <= total / 2.0 + 50.0 + 1.0);
                // The best split for [8,8,8,8] equal cats is at index 2.
                assert_eq!(split_index, 2);
            }
            other => panic!("expected TwoColumn, got {other:?}"),
        }
    }

    #[test]
    fn split_respects_category_boundaries() {
        // Uneven categories: [big, small, small, small]. Best split should
        // place the big one alone in column 0.
        let heights = cats(&[20, 2, 2, 2]);
        let plan = plan_help_layout(&heights, 100.0);
        match plan {
            HelpLayoutPlan::TwoColumn { split_index, .. } => {
                assert_eq!(split_index, 1);
            }
            other => panic!("expected TwoColumn, got {other:?}"),
        }
    }

    #[test]
    fn huge_content_tiny_window_clipped() {
        let heights = cats(&[20, 20, 20, 20, 20]);
        // Cap way too small for even two columns.
        let plan = plan_help_layout(&heights, 100.0);
        match plan {
            HelpLayoutPlan::TwoColumn { clipped, .. } => {
                assert!(clipped, "expected clipped two-column layout");
            }
            other => panic!("expected TwoColumn(clipped), got {other:?}"),
        }
    }

    #[test]
    fn zero_or_negative_cap_returns_clipped() {
        let heights = cats(&[3, 3]);
        let plan = plan_help_layout(&heights, 0.0);
        assert!(matches!(
            plan,
            HelpLayoutPlan::TwoColumn { clipped: true, .. }
        ));
    }

    #[test]
    fn single_category_never_splits() {
        let heights = cats(&[50]);
        // Even with a tiny cap, we can't split a single category.
        let plan = plan_help_layout(&heights, 10.0);
        assert!(matches!(plan, HelpLayoutPlan::SingleColumn { .. }));
    }

    /// Regression for tn-ftlo: the default keybindings intentionally
    /// register both `ctrl+shift+/` and `ctrl+shift+?` for `ShowHelp` so the
    /// same physical key works across keyboard layouts. Both normalize to
    /// the same displayed label ("Ctrl+Shift+?"), so the help overlay used
    /// to render two identical rows. `build_help_categories` now dedupes on
    /// (formatted_shortcut, description) per section.
    #[test]
    fn dual_binding_same_label_renders_once() {
        use therminal_core::config::{KeyAction, Keybinding, KeybindingsConfig};
        let kb = KeybindingsConfig {
            bindings: vec![
                Keybinding {
                    key: "ctrl+shift+/".to_string(),
                    action: KeyAction::ShowHelp,
                },
                Keybinding {
                    key: "ctrl+shift+?".to_string(),
                    action: KeyAction::ShowHelp,
                },
            ],
        };
        let cats = build_help_categories(&kb);
        // Two sections: "General" (keybindings) + "Mouse" (hardcoded).
        assert_eq!(cats.len(), 2, "expected two sections, got {cats:?}");
        let (section, rows) = &cats[0];
        assert_eq!(section, "General");
        assert_eq!(
            rows.len(),
            1,
            "ctrl+shift+/ and ctrl+shift+? should collapse to one row, got {rows:?}"
        );
        assert_eq!(rows[0].0, "Ctrl+Shift+?");
        assert_eq!(cats[1].0, "Mouse");
    }

    /// Two bindings that render with the same shortcut but different
    /// descriptions must still produce two rows — dedup is on the full
    /// (shortcut, description) pair, not on shortcut alone.
    #[test]
    fn same_shortcut_different_actions_both_shown() {
        use therminal_core::config::{KeyAction, Keybinding, KeybindingsConfig};
        // Both ShowHelp and Copy happen to live in the "General" section;
        // binding them to the same key would be unusual but we want to
        // guarantee the dedup does not hide one of them if it happens.
        let kb = KeybindingsConfig {
            bindings: vec![
                Keybinding {
                    key: "ctrl+shift+/".to_string(),
                    action: KeyAction::ShowHelp,
                },
                Keybinding {
                    key: "ctrl+shift+/".to_string(),
                    action: KeyAction::Copy,
                },
            ],
        };
        let cats = build_help_categories(&kb);
        // Two sections: "General" (keybindings) + "Mouse" (hardcoded).
        assert_eq!(cats.len(), 2);
        let (_, rows) = &cats[0];
        assert_eq!(rows.len(), 2, "distinct actions must not be collapsed");
    }

    /// Navigation and Widgets sections must be separate, not merged.
    #[test]
    fn navigation_and_widgets_are_separate_sections() {
        use therminal_core::config::{KeyAction, Keybinding, KeybindingsConfig};
        let kb = KeybindingsConfig {
            bindings: vec![
                Keybinding {
                    key: "ctrl+alt+t".to_string(),
                    action: KeyAction::ToggleAgentTimeline,
                },
                Keybinding {
                    key: "ctrl+alt+up".to_string(),
                    action: KeyAction::JumpRegionPrev,
                },
            ],
        };
        let cats = build_help_categories(&kb);
        let section_names: Vec<&str> = cats.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            section_names.contains(&"Navigation"),
            "expected Navigation section, got {section_names:?}"
        );
        assert!(
            section_names.contains(&"Widgets"),
            "expected Widgets section, got {section_names:?}"
        );
        // They must be distinct entries.
        assert_ne!(
            section_names.iter().position(|&s| s == "Navigation"),
            section_names.iter().position(|&s| s == "Widgets"),
        );
    }

    /// Default keybindings produce all expected sections.
    #[test]
    fn default_keybindings_produce_all_sections() {
        let kb = KeybindingsConfig::default();
        let cats = build_help_categories(&kb);
        let section_names: Vec<&str> = cats.iter().map(|(n, _)| n.as_str()).collect();
        for expected in &[
            "Pane Management",
            "Font",
            "General",
            "Navigation",
            "Widgets",
            "Mouse",
        ] {
            assert!(
                section_names.contains(expected),
                "missing section {expected:?}, got {section_names:?}"
            );
        }
    }

    /// Regression for tn-22l4 (extended for tn-eq9g): the WebView
    /// keybindings must surface in the `Ctrl+Shift+?` help overlay under
    /// a dedicated "WebView" section so users discover them as a family.
    /// Dual bindings (`Ctrl+L`/`Alt+L` for NavigateWebView, `Ctrl+Shift+B`/
    /// `Alt+Enter` for SpawnWebViewPane) all render as distinct rows.
    #[test]
    fn webview_bindings_appear_in_webview_section() {
        let kb = KeybindingsConfig::default();
        let cats = build_help_categories(&kb);
        let section_names: Vec<&str> = cats.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            section_names.contains(&"WebView"),
            "expected WebView section, got {section_names:?}"
        );
        let webview_rows: &Vec<(String, String)> = &cats
            .iter()
            .find(|(n, _)| n == "WebView")
            .expect("WebView section must exist")
            .1;
        let expected = [
            ("Ctrl+L", "Navigate WebView pane to URL"),
            ("Alt+L", "Navigate WebView pane to URL"),
            ("Ctrl+Shift+B", "Spawn new WebView pane from URL"),
            ("Alt+Enter", "Spawn new WebView pane from URL"),
            ("Alt+Home", "Return WebView to spawn URL"),
        ];
        for (k, d) in expected {
            assert!(
                webview_rows.iter().any(|(rk, rd)| rk == k && rd == d),
                "expected {k:?} / {d:?} row in WebView, got {webview_rows:?}"
            );
        }
    }

    /// Sections appear in the documented order.
    #[test]
    fn default_keybindings_section_order() {
        let kb = KeybindingsConfig::default();
        let cats = build_help_categories(&kb);
        let section_names: Vec<&str> = cats.iter().map(|(n, _)| n.as_str()).collect();
        let expected_order = vec![
            "Pane Management",
            "Font",
            "General",
            "WebView",
            "Navigation",
            "Widgets",
            "Mouse",
        ];
        assert_eq!(section_names, expected_order);
    }

    /// `max_scroll_rows` returns 0 when the window is tall enough to fit all
    /// content, and a positive value when it overflows.
    #[test]
    fn max_scroll_rows_tall_window_returns_zero() {
        let kb = KeybindingsConfig::default();
        // A very tall window should not need scrolling.
        assert_eq!(max_scroll_rows(&kb, 800, 2000), 0);
    }

    #[test]
    fn max_scroll_rows_tiny_window_returns_positive() {
        let kb = KeybindingsConfig::default();
        // A tiny window must require scrolling.
        let rows = max_scroll_rows(&kb, 400, 200);
        assert!(rows > 0, "expected scrollable overflow, got {rows}");
    }

    #[test]
    fn format_shortcut_named_keys() {
        assert_eq!(format_shortcut("ctrl+alt+pageup"), "Ctrl+Alt+PgUp");
        assert_eq!(format_shortcut("ctrl+alt+pagedown"), "Ctrl+Alt+PgDn");
        assert_eq!(format_shortcut("f11"), "F11");
        assert_eq!(format_shortcut("f1"), "F1");
        assert_eq!(format_shortcut("ctrl+alt+up"), "Ctrl+Alt+Up");
        assert_eq!(format_shortcut("ctrl+shift+enter"), "Ctrl+Shift+Enter");
        assert_eq!(format_shortcut("ctrl+,"), "Ctrl+,");
    }
}
