//! Keybinding help overlay: a centered semi-transparent panel listing all
//! configured keybindings grouped by section.
//!
//! Rendered as a final pass on top of all pane content, status bar, and chrome.

use std::collections::BTreeMap;

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
        _ => 3,
    }
}

// ── Draw the help overlay ─────────────────────────────────────────────

/// Draw the keybinding help overlay centered in the window.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_help_overlay(
    keybindings: &KeybindingsConfig,
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

    // ── Panel dimensions ────────────────────────────────────────────────
    let panel_w = (sw * 0.55).clamp(340.0, 600.0);
    let row_h = 24.0_f32;
    let header_row_h = 32.0_f32;
    let padding_v = 20.0_f32;
    let padding_h = 24.0_f32;

    // Group bindings by section.
    let mut sections: BTreeMap<u8, (String, Vec<(String, String)>)> = BTreeMap::new();
    for binding in &keybindings.bindings {
        let section_name = binding.action.section().to_string();
        let order = section_order(&section_name);
        let entry = sections
            .entry(order)
            .or_insert_with(|| (section_name, Vec::new()));
        let shortcut = format_shortcut(&binding.key);
        let description = binding.action.description().to_string();
        entry.1.push((shortcut, description));
    }

    // Calculate panel height.
    let section_count = sections.len();
    let binding_count: usize = sections.values().map(|(_, v)| v.len()).sum();
    let content_h = (section_count as f32 * header_row_h) + (binding_count as f32 * row_h) + row_h; // title row
    let panel_h = (content_h + padding_v * 2.0).min(sh * 0.85);

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

    let bounds = TextBounds {
        left: 0,
        top: 0,
        right: surface_width as i32,
        bottom: surface_height as i32,
    };

    let content_x = panel_x + padding_h;
    let mut current_y = panel_y + padding_v;
    let col2_x = content_x + panel_w * 0.45; // description column

    let mut text_areas: Vec<TextArea<'_>> = Vec::new();
    // We need to keep all buffers alive until after prepare() + render().
    let mut buffers: Vec<Buffer> = Vec::new();

    // Title
    let mut title_buf = Buffer::new(&mut renderer.font_system, title_metrics);
    title_buf.set_size(
        &mut renderer.font_system,
        Some(panel_w - padding_h * 2.0),
        Some(row_h + 4.0),
    );
    title_buf.set_text(
        &mut renderer.font_system,
        "Keybindings",
        &Attrs::new()
            .family(Family::Name(&renderer.font_config.family))
            .weight(Weight::BOLD)
            .color(text_color),
        Shaping::Basic,
        None,
    );
    title_buf.shape_until_scroll(&mut renderer.font_system, false);
    buffers.push(title_buf);
    // Record position for this buffer: (buffer_index, x, y, color)
    let title_idx = buffers.len() - 1;
    let _ = (title_idx, content_x, current_y);
    current_y += row_h + 8.0;

    // Build all section + binding buffers.
    struct RowInfo {
        buf_idx: usize,
        x: f32,
        y: f32,
        color: GlyphColor,
    }
    let mut rows: Vec<RowInfo> = Vec::new();

    // Title row info
    rows.push(RowInfo {
        buf_idx: 0,
        x: content_x,
        y: panel_y + padding_v,
        color: text_color,
    });

    for (section_name, bindings) in sections.values() {
        // Section header
        let mut hdr_buf = Buffer::new(&mut renderer.font_system, header_metrics);
        hdr_buf.set_size(
            &mut renderer.font_system,
            Some(panel_w - padding_h * 2.0),
            Some(header_row_h),
        );
        hdr_buf.set_text(
            &mut renderer.font_system,
            section_name,
            &Attrs::new()
                .family(Family::Name(&renderer.font_config.family))
                .weight(Weight::BOLD)
                .color(accent_color),
            Shaping::Basic,
            None,
        );
        hdr_buf.shape_until_scroll(&mut renderer.font_system, false);
        buffers.push(hdr_buf);
        rows.push(RowInfo {
            buf_idx: buffers.len() - 1,
            x: content_x,
            y: current_y,
            color: accent_color,
        });
        current_y += header_row_h;

        for (shortcut, description) in bindings {
            // Shortcut (left column)
            let mut key_buf = Buffer::new(&mut renderer.font_system, metrics);
            key_buf.set_size(&mut renderer.font_system, Some(panel_w * 0.42), Some(row_h));
            key_buf.set_text(
                &mut renderer.font_system,
                shortcut,
                &Attrs::new()
                    .family(Family::Name(&renderer.font_config.family))
                    .color(text_color),
                Shaping::Basic,
                None,
            );
            key_buf.shape_until_scroll(&mut renderer.font_system, false);
            buffers.push(key_buf);
            rows.push(RowInfo {
                buf_idx: buffers.len() - 1,
                x: content_x,
                y: current_y,
                color: text_color,
            });

            // Description (right column)
            let mut desc_buf = Buffer::new(&mut renderer.font_system, metrics);
            desc_buf.set_size(&mut renderer.font_system, Some(panel_w * 0.5), Some(row_h));
            desc_buf.set_text(
                &mut renderer.font_system,
                description,
                &Attrs::new()
                    .family(Family::Name(&renderer.font_config.family))
                    .color(muted_color),
                Shaping::Basic,
                None,
            );
            desc_buf.shape_until_scroll(&mut renderer.font_system, false);
            buffers.push(desc_buf);
            rows.push(RowInfo {
                buf_idx: buffers.len() - 1,
                x: col2_x,
                y: current_y,
                color: muted_color,
            });

            current_y += row_h;
        }
    }

    // Build TextArea references from the stored buffers and positions.
    for row in &rows {
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

// ── Helpers ───────────────────────────────────────────────────────────

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
