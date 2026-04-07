//! Workspace tab bar rendering and hit-testing.

use glyphon::{
    Attrs, Buffer, Color as GlyphColor, Family, Metrics, Resolution, TextArea, TextBounds,
};
use wgpu::util::DeviceExt;

use crate::grid_renderer::{ColorVertex, GridRenderer};
use therminal_core::palette::Color as PaletteColor;

use super::colors::{FOCUS_BORDER_COLOR, HEADER_BG_COLOR, STATUS_BAR_BG_COLOR};
use super::text_cache::{cached_buf, ensure_shaped};

/// Data collected for the workspace tab bar.
pub(crate) struct TabBarInfo {
    pub workspace_ids: Vec<usize>,
    pub active_workspace: usize,
    pub tab_labels: Vec<String>,
}

const TAB_MIN_WIDTH: f32 = 48.0;
const TAB_MAX_WIDTH: f32 = 200.0;
const TAB_CHAR_WIDTH: f32 = 8.0;
const TAB_PADDING: f32 = 16.0;
const TAB_ELLIPSIS: char = '…';
const TAB_BAR_BG_COLOR: [f32; 4] = STATUS_BAR_BG_COLOR;
const TAB_ACTIVE_BG_COLOR: [f32; 4] = HEADER_BG_COLOR;
const TAB_ACTIVE_UNDERLINE_COLOR: [f32; 4] = FOCUS_BORDER_COLOR;

/// Draw the workspace tab bar at the top of the window.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_tab_bar(
    info: &TabBarInfo,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
    bar_h: f32,
    show_tabs: bool,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    let sw = surface_width as f32;
    let sh = surface_height as f32;

    // ── Background ──
    let bg_verts = pixel_rect_to_ndc(0.0, 0.0, sw, bar_h, sw, sh, TAB_BAR_BG_COLOR);

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("tabbar_bg_vbuf"),
        contents: bytemuck::cast_slice(&bg_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tabbar_bg_pass"),
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
        pass.draw(0..6, 0..1);
    }

    if !show_tabs {
        return;
    }

    let slot_w = slot_width(sw, info.tab_labels.len());
    let tab_widths: Vec<f32> = info.tab_labels.iter().map(|_| slot_w).collect();

    let tab_offsets: Vec<f32> = tab_widths
        .iter()
        .scan(0.0f32, |acc, &w| {
            let x = *acc;
            *acc += w;
            Some(x)
        })
        .collect();

    // ── Active tab backgrounds ──
    let mut tab_verts: Vec<ColorVertex> = Vec::new();
    for (i, &ws_id) in info.workspace_ids.iter().enumerate() {
        let tab_x = tab_offsets[i];
        let tab_w = tab_widths[i];
        if ws_id == info.active_workspace {
            tab_verts.extend_from_slice(&pixel_rect_to_ndc(
                tab_x,
                0.0,
                tab_w,
                bar_h,
                sw,
                sh,
                TAB_ACTIVE_BG_COLOR,
            ));
            tab_verts.extend_from_slice(&pixel_rect_to_ndc(
                tab_x,
                bar_h - 2.0,
                tab_w,
                2.0,
                sw,
                sh,
                TAB_ACTIVE_UNDERLINE_COLOR,
            ));
        }
    }

    if !tab_verts.is_empty() {
        let tab_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tabbar_tabs_vbuf"),
            contents: bytemuck::cast_slice(&tab_verts),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tabbar_tabs_pass"),
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
        pass.set_vertex_buffer(0, tab_buf.slice(..));
        pass.draw(0..tab_verts.len() as u32, 0..1);
    }

    // ── Tab label text ──
    let font_size = (bar_h * 0.55).max(10.0);
    let line_height = bar_h;
    let metrics = Metrics::new(font_size, line_height);

    let bounds = TextBounds {
        left: 0,
        top: 0,
        right: surface_width as i32,
        bottom: surface_height as i32,
    };

    let active_color = GlyphColor::rgba(
        PaletteColor::INK.r,
        PaletteColor::INK.g,
        PaletteColor::INK.b,
        255,
    );
    let inactive_color = GlyphColor::rgba(
        PaletteColor::INK_MUTED.r,
        PaletteColor::INK_MUTED.g,
        PaletteColor::INK_MUTED.b,
        200,
    );

    let family = renderer.font_config.family.clone();
    let mut tab_slots: Vec<(String, f32, f32, GlyphColor)> = Vec::new();
    for (i, &ws_id) in info.workspace_ids.iter().enumerate() {
        let tab_x = tab_offsets[i];
        let tab_w = tab_widths[i];
        let is_active = ws_id == info.active_workspace;
        let color = if is_active {
            active_color
        } else {
            inactive_color
        };
        let label_owned = truncate_label(&info.tab_labels[i], tab_w);
        let label = label_owned.as_str();
        let active_tag = if is_active { "a" } else { "i" };
        let slot = format!("tab_{ws_id}");
        let key = format!("{label}|{active_tag}");

        // Pass the full surface width as the shaping constraint rather than
        // `tab_w`. With a tight per-tab width, glyphon wraps long labels onto
        // a second line and `layout_runs().next()` then only yields the first
        // visual line — which is exactly the rename bug (user sees the leading
        // workspace id but not the typed buffer or trailing cursor). Centering
        // is computed post-shape from the real text width, so a generous
        // shaping width is safe.
        ensure_shaped(
            &slot,
            &key,
            metrics,
            sw,
            bar_h,
            label,
            Attrs::new().family(Family::Name(&family)).color(color),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );

        tab_slots.push((slot, tab_x, tab_w, color));
    }

    // Phase 2: immutable borrow. Missing slots are skipped.
    let mut tab_positions: Vec<(&Buffer, f32, GlyphColor)> = Vec::new();
    for (slot, tab_x, tab_w, color) in &tab_slots {
        let Some(buf) = cached_buf(&renderer.overlay_cache, slot) else {
            continue;
        };
        let text_width = buf
            .layout_runs()
            .next()
            .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
            .unwrap_or(0.0);
        let centered_x = tab_x + ((tab_w - text_width) / 2.0).max(0.0);
        tab_positions.push((buf, centered_x, *color));
    }

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    let text_areas: Vec<TextArea<'_>> = tab_positions
        .iter()
        .map(|(buf, x, color)| TextArea {
            buffer: buf,
            left: *x,
            top: 0.0,
            scale: 1.0,
            bounds,
            default_color: *color,
            custom_glyphs: &[],
        })
        .collect();

    if let Err(e) = renderer.overlay_text_renderer.prepare(
        device,
        queue,
        &mut renderer.font_system,
        &mut renderer.overlay_atlas,
        &renderer.viewport,
        text_areas,
        &mut renderer.swash_cache,
    ) {
        tracing::warn!("tab bar text prepare failed: {}", e);
    }

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tabbar_text_pass"),
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
            tracing::warn!("tab bar text render failed: {}", e);
        }
    }
}

/// Return the workspace ID for a click at the given x-position in the tab bar.
pub(crate) fn tab_bar_hit_test(
    px: f32,
    workspace_ids: &[usize],
    tab_labels: &[String],
    bar_width: f32,
) -> Option<usize> {
    if workspace_ids.is_empty() {
        return None;
    }
    let slot_w = slot_width(bar_width, tab_labels.len());
    if slot_w <= 0.0 {
        return None;
    }
    if px < 0.0 {
        return workspace_ids.first().copied();
    }
    let idx = (px / slot_w).floor() as usize;
    workspace_ids.get(idx).copied()
}

/// Per-tab slot width: divides the bar evenly, capped at TAB_MAX_WIDTH and
/// floored at TAB_MIN_WIDTH. Tabs always share width — no overflow allowed.
fn slot_width(bar_width: f32, tab_count: usize) -> f32 {
    if tab_count == 0 {
        return 0.0;
    }
    let even = bar_width / tab_count as f32;
    even.clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH)
}

/// Truncate a label so its rendered width fits within `slot_w` (accounting
/// for TAB_PADDING). Appends an ellipsis when characters are dropped.
fn truncate_label(label: &str, slot_w: f32) -> String {
    let avail = (slot_w - TAB_PADDING).max(0.0);
    let max_chars = (avail / TAB_CHAR_WIDTH).floor() as usize;
    let char_count = label.chars().count();
    if char_count <= max_chars {
        return label.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return TAB_ELLIPSIS.to_string();
    }
    let keep = max_chars - 1;
    let mut out: String = label.chars().take(keep).collect();
    out.push(TAB_ELLIPSIS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn slot_width_divides_evenly_under_max() {
        // 4 tabs in 600 px → 150 each, below TAB_MAX_WIDTH.
        assert_eq!(slot_width(600.0, 4), 150.0);
    }

    #[test]
    fn slot_width_caps_at_max() {
        // 1 tab in 1000 px → would be 1000, capped to TAB_MAX_WIDTH.
        assert_eq!(slot_width(1000.0, 1), TAB_MAX_WIDTH);
    }

    #[test]
    fn slot_width_floors_at_min() {
        // 100 tabs in 100 px → 1 each, raised to TAB_MIN_WIDTH.
        assert_eq!(slot_width(100.0, 100), TAB_MIN_WIDTH);
    }

    #[test]
    fn truncate_label_short_unchanged() {
        // slot 200 → avail 184 → 23 chars max → "hello" passes through.
        assert_eq!(truncate_label("hello", 200.0), "hello");
    }

    #[test]
    fn truncate_label_long_gets_ellipsis() {
        // slot 48 → avail 32 → 4 chars max → 3 chars + ellipsis.
        let out = truncate_label("verylonglabel", 48.0);
        assert!(out.ends_with(TAB_ELLIPSIS));
        assert_eq!(out.chars().count(), 4);
    }

    #[test]
    fn tab_bar_hit_test_empty_workspaces_returns_none() {
        assert!(tab_bar_hit_test(0.0, &[], &[], 800.0).is_none());
        assert!(tab_bar_hit_test(100.0, &[], &labels(&["ws1"]), 800.0).is_none());
    }

    #[test]
    fn tab_bar_hit_test_uses_clamped_slot() {
        // 4 tabs in 800 px → slot_w = 200.
        let ids = vec![1usize, 2, 3, 4];
        let ls = labels(&["a", "b", "c", "d"]);
        assert_eq!(tab_bar_hit_test(0.0, &ids, &ls, 800.0), Some(1));
        assert_eq!(tab_bar_hit_test(199.9, &ids, &ls, 800.0), Some(1));
        assert_eq!(tab_bar_hit_test(200.0, &ids, &ls, 800.0), Some(2));
        assert_eq!(tab_bar_hit_test(599.9, &ids, &ls, 800.0), Some(3));
        assert_eq!(tab_bar_hit_test(700.0, &ids, &ls, 800.0), Some(4));
    }

    #[test]
    fn tab_bar_hit_test_beyond_last_tab_returns_none() {
        let ids = vec![1usize, 2];
        let ls = labels(&["a", "b"]);
        // 2 tabs in 800 px → slot_w capped at TAB_MAX_WIDTH = 200.
        assert!(tab_bar_hit_test(2.0 * TAB_MAX_WIDTH, &ids, &ls, 800.0).is_none());
    }

    #[test]
    fn tab_bar_hit_test_negative_x_hits_first_tab() {
        let ids = vec![1usize];
        let ls = labels(&["hello"]);
        assert_eq!(tab_bar_hit_test(-10.0, &ids, &ls, 800.0), Some(1));
    }
}
