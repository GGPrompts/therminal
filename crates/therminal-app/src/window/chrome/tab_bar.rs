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
const TAB_CHAR_WIDTH: f32 = 8.0;
const TAB_PADDING: f32 = 16.0;
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

    let tab_widths: Vec<f32> = info
        .tab_labels
        .iter()
        .map(|label| (label.len() as f32 * TAB_CHAR_WIDTH + TAB_PADDING).max(TAB_MIN_WIDTH))
        .collect();

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
        let label = &info.tab_labels[i];
        let active_tag = if is_active { "a" } else { "i" };
        let slot = format!("tab_{ws_id}");
        let key = format!("{label}|{active_tag}");

        ensure_shaped(
            &slot,
            &key,
            metrics,
            tab_w,
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
) -> Option<usize> {
    if workspace_ids.is_empty() {
        return None;
    }
    let mut cumulative_x = 0.0f32;
    for (i, label) in tab_labels.iter().enumerate() {
        let tab_w = (label.len() as f32 * TAB_CHAR_WIDTH + TAB_PADDING).max(TAB_MIN_WIDTH);
        if px < cumulative_x + tab_w {
            return workspace_ids.get(i).copied();
        }
        cumulative_x += tab_w;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// Compute the expected tab width for a label, mirroring `tab_bar_hit_test`.
    fn tab_w(label: &str) -> f32 {
        (label.len() as f32 * TAB_CHAR_WIDTH + TAB_PADDING).max(TAB_MIN_WIDTH)
    }

    #[test]
    fn tab_bar_hit_test_empty_workspaces_returns_none() {
        assert!(tab_bar_hit_test(0.0, &[], &[]).is_none());
        assert!(tab_bar_hit_test(100.0, &[], &labels(&["ws1"])).is_none());
    }

    #[test]
    fn tab_bar_hit_test_single_tab_at_origin() {
        let ids = vec![1usize];
        let ls = labels(&["alpha"]);
        // Any x in [0, tab_w("alpha")) must return workspace 1.
        assert_eq!(tab_bar_hit_test(0.0, &ids, &ls), Some(1));
        assert_eq!(tab_bar_hit_test(tab_w("alpha") - 0.001, &ids, &ls), Some(1));
    }

    #[test]
    fn tab_bar_hit_test_single_tab_beyond_width_returns_none() {
        let ids = vec![1usize];
        let ls = labels(&["alpha"]);
        assert!(tab_bar_hit_test(tab_w("alpha"), &ids, &ls).is_none());
        assert!(tab_bar_hit_test(tab_w("alpha") + 100.0, &ids, &ls).is_none());
    }

    #[test]
    fn tab_bar_hit_test_two_tabs_correct_workspace() {
        let ids = vec![1usize, 2];
        let ls = labels(&["one", "two"]);
        let w1 = tab_w("one");
        let w2 = tab_w("two");

        // Click in the first tab.
        assert_eq!(tab_bar_hit_test(0.0, &ids, &ls), Some(1));
        assert_eq!(tab_bar_hit_test(w1 - 1.0, &ids, &ls), Some(1));

        // Click exactly at the start of the second tab.
        assert_eq!(tab_bar_hit_test(w1, &ids, &ls), Some(2));
        assert_eq!(tab_bar_hit_test(w1 + w2 - 1.0, &ids, &ls), Some(2));

        // Click beyond both tabs.
        assert!(tab_bar_hit_test(w1 + w2, &ids, &ls).is_none());
    }

    #[test]
    fn tab_bar_hit_test_short_labels_respect_min_width() {
        // A single-character label must still produce a tab of at least TAB_MIN_WIDTH.
        let ids = vec![7usize];
        let ls = labels(&["X"]);
        let expected_w = TAB_MIN_WIDTH; // "X" * 8 + 16 = 24 < 48, so clamped to 48.
        assert_eq!(tab_bar_hit_test(0.0, &ids, &ls), Some(7));
        assert_eq!(tab_bar_hit_test(expected_w - 0.001, &ids, &ls), Some(7));
        assert!(tab_bar_hit_test(expected_w, &ids, &ls).is_none());
    }

    #[test]
    fn tab_bar_hit_test_three_tabs_middle_selected() {
        let ids = vec![10usize, 20, 30];
        let ls = labels(&["first", "second", "third"]);
        let w0 = tab_w("first");
        let w1 = tab_w("second");

        // Middle tab.
        let mid_px = w0 + w1 / 2.0;
        assert_eq!(tab_bar_hit_test(mid_px, &ids, &ls), Some(20));
    }

    #[test]
    fn tab_bar_hit_test_negative_x_hits_first_tab() {
        // Negative x is less than cumulative_x (0) + tab_w, so it hits the first tab.
        let ids = vec![1usize];
        let ls = labels(&["hello"]);
        assert_eq!(tab_bar_hit_test(-10.0, &ids, &ls), Some(1));
    }
}
