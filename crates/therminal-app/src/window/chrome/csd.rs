//! Client-side decoration (CSD) window control buttons.

use wgpu::util::DeviceExt;

use crate::grid_renderer::{ColorVertex, GridRenderer};

use super::icons::{Icon, append_icon};
use super::render_pass::with_chrome_render_pass;

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
    _queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
    bar_h: f32,
    hover_x: Option<f32>,
) {
    let sw = surface_width as f32;
    let sh = surface_height as f32;

    let layout = CsdButtonLayout::compute(sw);
    let hovered = layout.hovered_button(hover_x);

    // ── 1. Hover background tint (one render pass, no-op if no hover) ──
    draw_csd_hover_bg(
        &layout, hovered, renderer, device, encoder, view, sw, sh, bar_h,
    );

    // ── 2. Font-independent button icons ─────────────────────────────
    draw_csd_button_icons(
        &layout,
        hovered,
        renderer,
        device,
        encoder,
        view,
        surface_width,
        surface_height,
        bar_h,
    );
}

/// Right-to-left X positions of the four CSD buttons.
struct CsdButtonLayout {
    close_x: f32,
    max_x: f32,
    min_x: f32,
    settings_x: f32,
}

impl CsdButtonLayout {
    fn compute(sw: f32) -> Self {
        let close_x = sw - CSD_BTN_W;
        let max_x = close_x - CSD_BTN_W;
        let min_x = max_x - CSD_BTN_W;
        let settings_x = min_x - CSD_BTN_W;
        Self {
            close_x,
            max_x,
            min_x,
            settings_x,
        }
    }

    /// Map a hover x-position to the index of the button under the cursor.
    /// Returns `None` outside the button cluster. Indices match the order
    /// used by `draw_csd_hover_bg`: 0 = Close, 1 = Maximize, 2 = Minimize,
    /// 3 = Settings.
    fn hovered_button(&self, hover_x: Option<f32>) -> Option<usize> {
        let hx = hover_x?;
        if hx >= self.close_x {
            Some(0)
        } else if hx >= self.max_x {
            Some(1)
        } else if hx >= self.min_x {
            Some(2)
        } else if hx >= self.settings_x {
            Some(3)
        } else {
            None
        }
    }
}

/// Draw the hover background tint for the button under the cursor (if
/// any). Close uses its dedicated red color; the other buttons share the
/// generic semi-transparent white tint.
#[allow(clippy::too_many_arguments)]
fn draw_csd_hover_bg(
    layout: &CsdButtonLayout,
    hovered: Option<usize>,
    renderer: &GridRenderer,
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    sw: f32,
    sh: f32,
    bar_h: f32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    // Theme-aware (tn-g7oo): close button gets its own dedicated red, the
    // other three buttons share the generic translucent hover tint.
    let close_color = renderer.chrome_palette.csd_close;
    let hover_color = renderer.chrome_palette.csd_button_hover;

    let mut verts: Vec<ColorVertex> = Vec::new();
    match hovered {
        Some(0) => verts.extend_from_slice(&pixel_rect_to_ndc(
            layout.close_x,
            0.0,
            CSD_BTN_W,
            bar_h,
            sw,
            sh,
            close_color,
        )),
        Some(1) => verts.extend_from_slice(&pixel_rect_to_ndc(
            layout.max_x,
            0.0,
            CSD_BTN_W,
            bar_h,
            sw,
            sh,
            hover_color,
        )),
        Some(2) => verts.extend_from_slice(&pixel_rect_to_ndc(
            layout.min_x,
            0.0,
            CSD_BTN_W,
            bar_h,
            sw,
            sh,
            hover_color,
        )),
        Some(3) => verts.extend_from_slice(&pixel_rect_to_ndc(
            layout.settings_x,
            0.0,
            CSD_BTN_W,
            bar_h,
            sw,
            sh,
            hover_color,
        )),
        _ => {}
    }

    if verts.is_empty() {
        return;
    }
    let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("csd_hover_vbuf"),
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let vertex_count = verts.len() as u32;
    with_chrome_render_pass(encoder, view, "csd_hover_pass", |pass| {
        pass.set_pipeline(&renderer.rect_pipeline);
        pass.set_vertex_buffer(0, buf.slice(..));
        pass.draw(0..vertex_count, 0..1);
    });
}

/// Draw control strokes directly so missing fonts cannot turn buttons into tofu.
#[allow(clippy::too_many_arguments)]
fn draw_csd_button_icons(
    layout: &CsdButtonLayout,
    hovered: Option<usize>,
    renderer: &GridRenderer,
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
    bar_h: f32,
) {
    let mut color = renderer.chrome_palette.chrome_fg.to_f32_array();
    color[3] = 200.0 / 255.0;
    let mut vertices = Vec::new();
    for (icon, x) in [
        (Icon::Settings, layout.settings_x),
        (Icon::Minimize, layout.min_x),
        (Icon::Maximize, layout.max_x),
        (Icon::Close, layout.close_x),
    ] {
        let color = if matches!(icon, Icon::Close) && hovered == Some(0) {
            [1.0; 4]
        } else {
            color
        };
        append_icon(
            &mut vertices,
            icon,
            [x, 0.0, CSD_BTN_W, bar_h],
            [surface_width as f32, surface_height as f32],
            color,
        );
    }
    let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("csd_icons"),
        contents: bytemuck::cast_slice(&vertices),
        usage: wgpu::BufferUsages::VERTEX,
    });
    with_chrome_render_pass(encoder, view, "csd_icons", |pass| {
        pass.set_pipeline(&renderer.rect_pipeline);
        pass.set_vertex_buffer(0, buffer.slice(..));
        pass.draw(0..vertices.len() as u32, 0..1);
    });
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
