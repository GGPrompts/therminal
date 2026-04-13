//! Workspace tab bar rendering and hit-testing.

use glyphon::{
    Attrs, Buffer, Color as GlyphColor, Family, Metrics, Resolution, TextArea, TextBounds,
};
use wgpu::util::DeviceExt;

use crate::grid_renderer::{ColorVertex, GridRenderer};

use super::render_pass::with_chrome_render_pass;
use super::text_cache::{cached_buf, ensure_shaped};

/// Data collected for the workspace tab bar.
pub(crate) struct TabBarInfo {
    pub workspace_ids: Vec<usize>,
    pub active_workspace: usize,
    pub tab_labels: Vec<String>,
}

const TAB_MIN_WIDTH: f32 = 48.0;
const TAB_MAX_WIDTH: f32 = 200.0;
const TAB_PADDING: f32 = 16.0;
pub(crate) const TAB_ELLIPSIS: char = '…';

/// Draw the workspace tab bar at the top of the window.
///
/// `show_tabs` gates tab labels + active highlights: when false (single
/// workspace without CSD) this function returns before touching the GPU so
/// no background strip is reserved. With CSD enabled the bar is still drawn
/// as the title bar chrome even with a single workspace.
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
    csd_reserved: f32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    // Bar height of 0 means the whole tab-bar chrome is collapsed (single
    // workspace, no CSD). Nothing to draw — not even the background strip.
    if bar_h <= 0.0 {
        return;
    }

    let sw = surface_width as f32;
    let sh = surface_height as f32;

    // Snapshot palette colors up front so we can hand the renderer to
    // mutating helpers later in this function without re-borrowing.
    let tab_bar_bg = renderer.chrome_palette.tab_bar_bg;
    let tab_active_bg = renderer.chrome_palette.tab_active_bg;
    let tab_active_underline = renderer.chrome_palette.tab_active_underline;

    // ── Background ──
    let bg_verts = pixel_rect_to_ndc(0.0, 0.0, sw, bar_h, sw, sh, tab_bar_bg);

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("tabbar_bg_vbuf"),
        contents: bytemuck::cast_slice(&bg_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    with_chrome_render_pass(encoder, view, "tabbar_bg_pass", |pass| {
        pass.set_pipeline(&renderer.rect_pipeline);
        pass.set_vertex_buffer(0, vertex_buf.slice(..));
        pass.draw(0..6, 0..1);
    });

    if !show_tabs {
        return;
    }

    let available_w = (sw - csd_reserved).max(0.0);
    let slot_w = slot_width(available_w, info.tab_labels.len());
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
                tab_active_bg,
            ));
            tab_verts.extend_from_slice(&pixel_rect_to_ndc(
                tab_x,
                bar_h - 2.0,
                tab_w,
                2.0,
                sw,
                sh,
                tab_active_underline,
            ));
        }
    }

    if !tab_verts.is_empty() {
        let tab_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tabbar_tabs_vbuf"),
            contents: bytemuck::cast_slice(&tab_verts),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let vert_count = tab_verts.len() as u32;
        with_chrome_render_pass(encoder, view, "tabbar_tabs_pass", |pass| {
            pass.set_pipeline(&renderer.rect_pipeline);
            pass.set_vertex_buffer(0, tab_buf.slice(..));
            pass.draw(0..vert_count, 0..1);
        });
    }

    // ── Tab label text ──
    let font_size = renderer.chrome_font_size((bar_h * 0.55).max(10.0));
    let line_height = bar_h;
    let metrics = Metrics::new(font_size, line_height);

    // Theme-aware (tn-g7oo): tab text reads from chrome_fg / chrome_fg_muted
    // so a light theme that re-skins the tab background also re-skins the
    // labels in step.
    let chrome_fg = renderer.chrome_palette.chrome_fg;
    let chrome_fg_muted = renderer.chrome_palette.chrome_fg_muted;
    let active_color = GlyphColor::rgba(chrome_fg.r, chrome_fg.g, chrome_fg.b, 255);
    let inactive_color =
        GlyphColor::rgba(chrome_fg_muted.r, chrome_fg_muted.g, chrome_fg_muted.b, 200);

    let family = renderer.font_config.chrome_font_family().to_string();
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

        // Shape-measure-trim: estimate heuristics (TAB_CHAR_WIDTH) routinely
        // undershoot real glyph advance widths, producing labels that still
        // overflow their slot. Instead, shape the full label first; if the
        // measured width exceeds the available width, iteratively drop a
        // char and append an ellipsis until it fits.
        let avail = (tab_w - TAB_PADDING).max(0.0);
        let full_label = info.tab_labels[i].clone();
        let active_tag = if is_active { "a" } else { "i" };
        let slot = format!("tab_{ws_id}");

        let mut label = full_label.clone();
        loop {
            let key = format!("{label}|{active_tag}");
            ensure_shaped(
                &slot,
                &key,
                metrics,
                sw,
                bar_h,
                &label,
                Attrs::new().family(Family::Name(&family)).color(color),
                &mut renderer.font_system,
                &mut renderer.overlay_cache,
            );
            let text_w = cached_buf(&renderer.overlay_cache, &slot)
                .map(|buf| {
                    buf.layout_runs()
                        .next()
                        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
                        .unwrap_or(0.0)
                })
                .unwrap_or(0.0);
            if text_w <= avail || label.chars().count() <= 1 {
                break;
            }
            // Drop trailing chars and append ellipsis.
            let char_count = label.chars().count();
            // If the current label already ends with ellipsis, drop one more
            // real char before it; otherwise drop one and append ellipsis.
            let (without_ellipsis, has_ellipsis) = match label.strip_suffix(TAB_ELLIPSIS) {
                Some(rest) => (rest.to_string(), true),
                None => (label.clone(), false),
            };
            let take = if has_ellipsis {
                without_ellipsis.chars().count().saturating_sub(1)
            } else {
                char_count.saturating_sub(2)
            };
            if take == 0 {
                label = TAB_ELLIPSIS.to_string();
                let key = format!("{label}|{active_tag}");
                ensure_shaped(
                    &slot,
                    &key,
                    metrics,
                    sw,
                    bar_h,
                    &label,
                    Attrs::new().family(Family::Name(&family)).color(color),
                    &mut renderer.font_system,
                    &mut renderer.overlay_cache,
                );
                break;
            }
            let mut next: String = without_ellipsis.chars().take(take).collect();
            next.push(TAB_ELLIPSIS);
            label = next;
        }

        tab_slots.push((slot, tab_x, tab_w, color));
    }

    // Phase 2: immutable borrow. Missing slots are skipped.
    // (buf, centered_x, tab_x, tab_w, color) — tab_x/tab_w retained for per-tab bounds clipping.
    let tab_positions: Vec<(&Buffer, f32, f32, f32, GlyphColor)> = tab_slots
        .iter()
        .filter_map(|(slot, tab_x, tab_w, color)| {
            let buf = cached_buf(&renderer.overlay_cache, slot)?;
            let text_width = buf
                .layout_runs()
                .next()
                .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
                .unwrap_or(0.0);
            let centered_x = tab_x + ((tab_w - text_width) / 2.0).max(0.0);
            Some((buf, centered_x, *tab_x, *tab_w, *color))
        })
        .collect();

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    let text_areas: Vec<TextArea<'_>> = tab_positions
        .iter()
        .map(|(buf, x, tab_x, tab_w, color)| {
            // Clip each tab's text to its own slot rect so any residual
            // overflow never bleeds into the adjacent tab.
            let left = tab_x.floor() as i32;
            let right = (tab_x + tab_w).ceil() as i32;
            let slot_bounds = TextBounds {
                left,
                top: 0,
                right,
                bottom: bar_h as i32,
            };
            TextArea {
                buffer: buf,
                left: *x,
                top: 0.0,
                scale: 1.0,
                bounds: slot_bounds,
                default_color: *color,
                custom_glyphs: &[],
            }
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

    with_chrome_render_pass(encoder, view, "tabbar_text_pass", |pass| {
        if let Err(e) =
            renderer
                .overlay_text_renderer
                .render(&renderer.overlay_atlas, &renderer.viewport, pass)
        {
            tracing::warn!("tab bar text render failed: {}", e);
        }
    });
}

/// Return the workspace ID for a click at the given x-position in the tab bar.
pub(crate) fn tab_bar_hit_test(
    px: f32,
    workspace_ids: &[usize],
    tab_labels: &[String],
    bar_width: f32,
    csd_reserved: f32,
) -> Option<usize> {
    if workspace_ids.is_empty() {
        return None;
    }
    let available_w = (bar_width - csd_reserved).max(0.0);
    let slot_w = slot_width(available_w, tab_labels.len());
    if slot_w <= 0.0 {
        return None;
    }
    // Clicks inside the CSD reserved zone are not tab clicks.
    if px >= available_w {
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
    fn tab_bar_hit_test_empty_workspaces_returns_none() {
        assert!(tab_bar_hit_test(0.0, &[], &[], 800.0, 0.0).is_none());
        assert!(tab_bar_hit_test(100.0, &[], &labels(&["ws1"]), 800.0, 0.0).is_none());
    }

    #[test]
    fn tab_bar_hit_test_uses_clamped_slot() {
        // 4 tabs in 800 px → slot_w = 200.
        let ids = vec![1usize, 2, 3, 4];
        let ls = labels(&["a", "b", "c", "d"]);
        assert_eq!(tab_bar_hit_test(0.0, &ids, &ls, 800.0, 0.0), Some(1));
        assert_eq!(tab_bar_hit_test(199.9, &ids, &ls, 800.0, 0.0), Some(1));
        assert_eq!(tab_bar_hit_test(200.0, &ids, &ls, 800.0, 0.0), Some(2));
        assert_eq!(tab_bar_hit_test(599.9, &ids, &ls, 800.0, 0.0), Some(3));
        assert_eq!(tab_bar_hit_test(700.0, &ids, &ls, 800.0, 0.0), Some(4));
    }

    #[test]
    fn tab_bar_hit_test_beyond_last_tab_returns_none() {
        let ids = vec![1usize, 2];
        let ls = labels(&["a", "b"]);
        // 2 tabs in 800 px → slot_w capped at TAB_MAX_WIDTH = 200.
        assert!(tab_bar_hit_test(2.0 * TAB_MAX_WIDTH, &ids, &ls, 800.0, 0.0).is_none());
    }

    #[test]
    fn tab_bar_hit_test_negative_x_hits_first_tab() {
        let ids = vec![1usize];
        let ls = labels(&["hello"]);
        assert_eq!(tab_bar_hit_test(-10.0, &ids, &ls, 800.0, 0.0), Some(1));
    }

    #[test]
    fn tab_bar_hit_test_respects_csd_reserved() {
        let csd = crate::pane::CSD_BUTTONS_TOTAL_WIDTH;
        let ids = vec![1usize, 2, 3, 4];
        let ls = labels(&["a", "b", "c", "d"]);
        let available = 800.0 - csd;
        let slot_w = available / 4.0;
        assert_eq!(tab_bar_hit_test(0.0, &ids, &ls, 800.0, csd), Some(1));
        assert_eq!(
            tab_bar_hit_test(slot_w - 0.1, &ids, &ls, 800.0, csd),
            Some(1)
        );
        assert_eq!(tab_bar_hit_test(slot_w, &ids, &ls, 800.0, csd), Some(2));
        assert!(tab_bar_hit_test(available + 1.0, &ids, &ls, 800.0, csd).is_none());
    }

    #[test]
    fn tab_bar_hit_test_csd_zone_returns_none() {
        let csd = crate::pane::CSD_BUTTONS_TOTAL_WIDTH;
        let ids = vec![1usize];
        let ls = labels(&["hello"]);
        assert!(tab_bar_hit_test(800.0 - 10.0, &ids, &ls, 800.0, csd).is_none());
    }
}
