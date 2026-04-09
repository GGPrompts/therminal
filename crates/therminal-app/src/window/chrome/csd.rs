//! Client-side decoration (CSD) window control buttons.

use glyphon::{Attrs, Color as GlyphColor, Family, Metrics, Resolution, TextArea, TextBounds};
use wgpu::util::DeviceExt;

use crate::grid_renderer::{ColorVertex, GridRenderer};
use therminal_core::palette::Color as PaletteColor;

use super::text_cache::{cached_buf, ensure_shaped};

/// Actions triggered by CSD window control buttons.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CsdAction {
    Settings,
    Minimize,
    Maximize,
    Close,
}

/// Width of each CSD window control button.
const CSD_BTN_W: f32 = crate::pane::CSD_BUTTON_WIDTH;

const CSD_CLOSE_COLOR: [f32; 4] = [0.85, 0.25, 0.25, 1.0];
const CSD_HOVER_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.1];

/// Hit-test CSD window control buttons (right-aligned in the tab bar).
pub(crate) fn csd_button_hit_test(px: f32, bar_h: f32, surface_width: f32) -> Option<CsdAction> {
    if bar_h <= 0.0 {
        return None;
    }
    let close_x = surface_width - CSD_BTN_W;
    let max_x = close_x - CSD_BTN_W;
    let min_x = max_x - CSD_BTN_W;
    let settings_x = min_x - CSD_BTN_W;

    if px >= close_x {
        Some(CsdAction::Close)
    } else if px >= max_x {
        Some(CsdAction::Maximize)
    } else if px >= min_x {
        Some(CsdAction::Minimize)
    } else if px >= settings_x {
        Some(CsdAction::Settings)
    } else {
        None
    }
}

/// Draw CSD window control buttons on the right side of the tab bar.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_csd_buttons(
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
    bar_h: f32,
    hover_x: Option<f32>,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    let sw = surface_width as f32;
    let sh = surface_height as f32;

    let close_x = sw - CSD_BTN_W;
    let max_x = close_x - CSD_BTN_W;
    let min_x = max_x - CSD_BTN_W;
    let settings_x = min_x - CSD_BTN_W;

    let mut verts: Vec<ColorVertex> = Vec::new();

    let hovered = hover_x.and_then(|hx| {
        if hx >= close_x {
            Some(0)
        } else if hx >= max_x {
            Some(1)
        } else if hx >= min_x {
            Some(2)
        } else if hx >= settings_x {
            Some(3)
        } else {
            None
        }
    });

    if hovered == Some(0) {
        verts.extend_from_slice(&pixel_rect_to_ndc(
            close_x,
            0.0,
            CSD_BTN_W,
            bar_h,
            sw,
            sh,
            CSD_CLOSE_COLOR,
        ));
    }
    if hovered == Some(1) {
        verts.extend_from_slice(&pixel_rect_to_ndc(
            max_x,
            0.0,
            CSD_BTN_W,
            bar_h,
            sw,
            sh,
            CSD_HOVER_COLOR,
        ));
    }
    if hovered == Some(2) {
        verts.extend_from_slice(&pixel_rect_to_ndc(
            min_x,
            0.0,
            CSD_BTN_W,
            bar_h,
            sw,
            sh,
            CSD_HOVER_COLOR,
        ));
    }
    if hovered == Some(3) {
        verts.extend_from_slice(&pixel_rect_to_ndc(
            settings_x,
            0.0,
            CSD_BTN_W,
            bar_h,
            sw,
            sh,
            CSD_HOVER_COLOR,
        ));
    }

    if !verts.is_empty() {
        let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("csd_hover_vbuf"),
            contents: bytemuck::cast_slice(&verts),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("csd_hover_pass"),
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
        pass.set_vertex_buffer(0, buf.slice(..));
        pass.draw(0..verts.len() as u32, 0..1);
    }

    // ── Button icons ──
    let font_size = (bar_h * 0.45).max(10.0);
    let metrics = Metrics::new(font_size, bar_h);
    let bounds = TextBounds {
        left: 0,
        top: 0,
        right: surface_width as i32,
        bottom: surface_height as i32,
    };

    let icon_color = GlyphColor::rgba(
        PaletteColor::INK.r,
        PaletteColor::INK.g,
        PaletteColor::INK.b,
        200,
    );
    let close_icon_color = if hovered == Some(0) {
        GlyphColor::rgba(255, 255, 255, 255)
    } else {
        icon_color
    };

    let family = renderer.font_config.family.clone();

    // Settings icon: we originally used ⚙ (U+2699 gear) but it's a
    // symbol codepoint that most monospace fonts don't ship and
    // glyphon's fallback chain doesn't reliably pick up a system
    // symbol font (Segoe UI Symbol, Noto Sans Symbols, Apple Symbols)
    // without explicit wiring. Use ≡ (U+2261, identical-to / triple
    // bar) instead — it's in the Mathematical Operators block, ships
    // with every serious mono font (cascadia, firacode, jetbrains
    // mono, Source Code Pro, etc.), and visually reads as "menu /
    // settings" (the same shape Android/Material uses for the
    // hamburger menu). Stays pinned to the user's mono family for
    // visual consistency with the other CSD glyphs.
    let settings_label = "\u{2261}";
    let settings_slot = "csd_settings";
    ensure_shaped(
        settings_slot,
        settings_label,
        metrics,
        CSD_BTN_W,
        bar_h,
        settings_label,
        Attrs::new().family(Family::Name(&family)).color(icon_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );

    let min_label = "\u{2500}";
    let min_slot = "csd_min";
    ensure_shaped(
        min_slot,
        min_label,
        metrics,
        CSD_BTN_W,
        bar_h,
        min_label,
        Attrs::new().family(Family::Name(&family)).color(icon_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );

    let max_label = "\u{25A1}";
    let max_slot = "csd_max";
    ensure_shaped(
        max_slot,
        max_label,
        metrics,
        CSD_BTN_W,
        bar_h,
        max_label,
        Attrs::new().family(Family::Name(&family)).color(icon_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );

    let close_label = "\u{2715}";
    let close_slot = "csd_close";
    let close_key = format!(
        "{close_label}|{}",
        if hovered == Some(0) { "h" } else { "n" }
    );
    ensure_shaped(
        close_slot,
        &close_key,
        metrics,
        CSD_BTN_W,
        bar_h,
        close_label,
        Attrs::new()
            .family(Family::Name(&family))
            .color(close_icon_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );

    // Center each icon in its button. Missing slot falls back to button origin.
    let center_x = |slot: &str, btn_x: f32| -> f32 {
        let tw = cached_buf(&renderer.overlay_cache, slot)
            .and_then(|b| b.layout_runs().next())
            .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
            .unwrap_or(0.0);
        btn_x + ((CSD_BTN_W - tw) / 2.0).max(0.0)
    };

    let settings_cx = center_x(settings_slot, settings_x);
    let min_cx = center_x(min_slot, min_x);
    let max_cx = center_x(max_slot, max_x);
    let close_cx = center_x(close_slot, close_x);

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    let mut areas: Vec<TextArea<'_>> = Vec::with_capacity(4);
    if let Some(buf) = cached_buf(&renderer.overlay_cache, settings_slot) {
        areas.push(TextArea {
            buffer: buf,
            left: settings_cx,
            top: 0.0,
            scale: 1.0,
            bounds,
            default_color: icon_color,
            custom_glyphs: &[],
        });
    }
    if let Some(buf) = cached_buf(&renderer.overlay_cache, min_slot) {
        areas.push(TextArea {
            buffer: buf,
            left: min_cx,
            top: 0.0,
            scale: 1.0,
            bounds,
            default_color: icon_color,
            custom_glyphs: &[],
        });
    }
    if let Some(buf) = cached_buf(&renderer.overlay_cache, max_slot) {
        areas.push(TextArea {
            buffer: buf,
            left: max_cx,
            top: 0.0,
            scale: 1.0,
            bounds,
            default_color: icon_color,
            custom_glyphs: &[],
        });
    }
    if let Some(buf) = cached_buf(&renderer.overlay_cache, close_slot) {
        areas.push(TextArea {
            buffer: buf,
            left: close_cx,
            top: 0.0,
            scale: 1.0,
            bounds,
            default_color: close_icon_color,
            custom_glyphs: &[],
        });
    }

    if let Err(e) = renderer.overlay_text_renderer.prepare(
        device,
        queue,
        &mut renderer.font_system,
        &mut renderer.overlay_atlas,
        &renderer.viewport,
        areas,
        &mut renderer.swash_cache,
    ) {
        tracing::warn!("CSD button text prepare failed: {e}");
    }

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("csd_text_pass"),
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
            tracing::warn!("CSD button text render failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // CSD_BTN_W = 46.0 (CSD_BUTTON_WIDTH from pane::geometry)
    // Four buttons right-to-left: Close, Maximize, Minimize, Settings
    // surface_width = 600
    //   close_x     = 600 - 46 = 554
    //   max_x       = 554 - 46 = 508
    //   min_x       = 508 - 46 = 462
    //   settings_x  = 462 - 46 = 416

    const SW: f32 = 600.0;
    const BTN_W: f32 = crate::pane::CSD_BUTTON_WIDTH;

    fn close_x() -> f32 {
        SW - BTN_W
    }
    fn max_x() -> f32 {
        close_x() - BTN_W
    }
    fn min_x() -> f32 {
        max_x() - BTN_W
    }
    fn settings_x() -> f32 {
        min_x() - BTN_W
    }

    #[test]
    fn csd_hit_test_returns_none_when_bar_h_zero() {
        let result = csd_button_hit_test(SW - 1.0, 0.0, SW);
        assert!(result.is_none());
    }

    #[test]
    fn csd_hit_test_returns_none_when_bar_h_negative() {
        let result = csd_button_hit_test(SW - 1.0, -1.0, SW);
        assert!(result.is_none());
    }

    #[test]
    fn csd_hit_test_close_at_right_edge() {
        // Any px >= close_x is Close.
        assert!(matches!(
            csd_button_hit_test(close_x(), 30.0, SW),
            Some(CsdAction::Close)
        ));
        assert!(matches!(
            csd_button_hit_test(SW - 1.0, 30.0, SW),
            Some(CsdAction::Close)
        ));
    }

    #[test]
    fn csd_hit_test_maximize_in_middle_button() {
        let px = max_x() + BTN_W / 2.0; // center of the maximize button
        assert!(matches!(
            csd_button_hit_test(px, 30.0, SW),
            Some(CsdAction::Maximize)
        ));
    }

    #[test]
    fn csd_hit_test_minimize_in_leftmost_button() {
        let px = min_x() + 1.0;
        assert!(matches!(
            csd_button_hit_test(px, 30.0, SW),
            Some(CsdAction::Minimize)
        ));
    }

    #[test]
    fn csd_hit_test_settings_in_leftmost_button() {
        let px = settings_x() + BTN_W / 2.0; // center of the settings button
        assert!(matches!(
            csd_button_hit_test(px, 30.0, SW),
            Some(CsdAction::Settings)
        ));
    }

    #[test]
    fn csd_hit_test_settings_at_exact_left_edge() {
        // Exactly at settings_x → Settings.
        assert!(matches!(
            csd_button_hit_test(settings_x(), 30.0, SW),
            Some(CsdAction::Settings)
        ));
    }

    #[test]
    fn csd_hit_test_left_of_settings_is_none() {
        let px = settings_x() - 1.0;
        assert!(csd_button_hit_test(px, 30.0, SW).is_none());
    }

    #[test]
    fn csd_hit_test_at_exact_boundary_minimize_vs_settings() {
        // Exactly at min_x → Minimize.
        assert!(matches!(
            csd_button_hit_test(min_x(), 30.0, SW),
            Some(CsdAction::Minimize)
        ));
        // One pixel left → Settings.
        assert!(matches!(
            csd_button_hit_test(min_x() - 1.0, 30.0, SW),
            Some(CsdAction::Settings)
        ));
    }

    #[test]
    fn csd_hit_test_at_exact_boundary_close_vs_maximize() {
        // Exactly at close_x → Close.
        assert!(matches!(
            csd_button_hit_test(close_x(), 30.0, SW),
            Some(CsdAction::Close)
        ));
        // One pixel left → Maximize.
        assert!(matches!(
            csd_button_hit_test(close_x() - 1.0, 30.0, SW),
            Some(CsdAction::Maximize)
        ));
    }

    #[test]
    fn csd_hit_test_at_exact_boundary_maximize_vs_minimize() {
        assert!(matches!(
            csd_button_hit_test(max_x(), 30.0, SW),
            Some(CsdAction::Maximize)
        ));
        assert!(matches!(
            csd_button_hit_test(max_x() - 1.0, 30.0, SW),
            Some(CsdAction::Minimize)
        ));
    }

    #[test]
    fn csd_hit_test_at_exact_min_x_boundary() {
        // Exactly at min_x → Minimize.
        assert!(matches!(
            csd_button_hit_test(min_x(), 30.0, SW),
            Some(CsdAction::Minimize)
        ));
    }

    #[test]
    fn csd_hit_test_four_buttons_at_narrow_width() {
        // At a narrow surface width, all four buttons still resolve right-to-left.
        let sw = 400.0;
        let close_x = sw - BTN_W;
        let max_x = close_x - BTN_W;
        let min_x = max_x - BTN_W;
        let settings_x = min_x - BTN_W;

        assert!(matches!(
            csd_button_hit_test(close_x + 1.0, 30.0, sw),
            Some(CsdAction::Close)
        ));
        assert!(matches!(
            csd_button_hit_test(max_x + 1.0, 30.0, sw),
            Some(CsdAction::Maximize)
        ));
        assert!(matches!(
            csd_button_hit_test(min_x + 1.0, 30.0, sw),
            Some(CsdAction::Minimize)
        ));
        assert!(matches!(
            csd_button_hit_test(settings_x + 1.0, 30.0, sw),
            Some(CsdAction::Settings)
        ));
        assert!(csd_button_hit_test(settings_x - 1.0, 30.0, sw).is_none());
    }
}
