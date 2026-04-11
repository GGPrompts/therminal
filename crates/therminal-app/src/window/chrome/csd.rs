//! Client-side decoration (CSD) window control buttons.

use glyphon::{Attrs, Color as GlyphColor, Family, Metrics, Resolution, TextArea, TextBounds};
use wgpu::util::DeviceExt;

use crate::grid_renderer::{ColorVertex, GridRenderer};
use therminal_core::palette::Color as PaletteColor;

use super::render_pass::with_chrome_render_pass;
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
    let sw = surface_width as f32;
    let sh = surface_height as f32;

    let layout = CsdButtonLayout::compute(sw);
    let hovered = layout.hovered_button(hover_x);

    // ── 1. Hover background tint (one render pass, no-op if no hover) ──
    draw_csd_hover_bg(
        &layout, hovered, renderer, device, encoder, view, sw, sh, bar_h,
    );

    // ── 2. Button icons (shape + prepare + render text) ────────────────
    draw_csd_button_icons(
        &layout,
        hovered,
        renderer,
        device,
        queue,
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

    let mut verts: Vec<ColorVertex> = Vec::new();
    match hovered {
        Some(0) => verts.extend_from_slice(&pixel_rect_to_ndc(
            layout.close_x,
            0.0,
            CSD_BTN_W,
            bar_h,
            sw,
            sh,
            CSD_CLOSE_COLOR,
        )),
        Some(1) => verts.extend_from_slice(&pixel_rect_to_ndc(
            layout.max_x,
            0.0,
            CSD_BTN_W,
            bar_h,
            sw,
            sh,
            CSD_HOVER_COLOR,
        )),
        Some(2) => verts.extend_from_slice(&pixel_rect_to_ndc(
            layout.min_x,
            0.0,
            CSD_BTN_W,
            bar_h,
            sw,
            sh,
            CSD_HOVER_COLOR,
        )),
        Some(3) => verts.extend_from_slice(&pixel_rect_to_ndc(
            layout.settings_x,
            0.0,
            CSD_BTN_W,
            bar_h,
            sw,
            sh,
            CSD_HOVER_COLOR,
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

/// Slot identifiers and label glyphs for the four CSD button icons.
struct CsdButtonIcons {
    settings_slot: &'static str,
    settings_label: &'static str,
    min_slot: &'static str,
    min_label: &'static str,
    max_slot: &'static str,
    max_label: &'static str,
    close_slot: &'static str,
    close_label: &'static str,
}

impl CsdButtonIcons {
    const fn new() -> Self {
        // Settings icon: we originally used ⚙ (U+2699 gear) but it's a
        // symbol codepoint that most monospace fonts don't ship and
        // glyphon's fallback chain doesn't reliably pick up a system
        // symbol font (Segoe UI Symbol, Noto Sans Symbols, Apple Symbols)
        // without explicit wiring. Use ≡ (U+2261, identical-to / triple
        // bar) instead — it's in the Mathematical Operators block, ships
        // with every serious mono font, and visually reads as "menu /
        // settings" (the same shape Android/Material uses for the
        // hamburger menu).
        Self {
            settings_slot: "csd_settings",
            settings_label: "\u{2261}",
            min_slot: "csd_min",
            min_label: "\u{2500}",
            max_slot: "csd_max",
            max_label: "\u{25A1}",
            close_slot: "csd_close",
            close_label: "\u{2715}",
        }
    }
}

/// Shape, prepare, and render the four CSD button icon glyphs.
#[allow(clippy::too_many_arguments)]
fn draw_csd_button_icons(
    layout: &CsdButtonLayout,
    hovered: Option<usize>,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
    bar_h: f32,
) {
    let icons = CsdButtonIcons::new();
    let font_size = renderer.chrome_font_size((bar_h * 0.45).max(10.0));
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

    shape_csd_icons(
        &icons,
        hovered,
        metrics,
        bar_h,
        icon_color,
        close_icon_color,
        renderer,
    );

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    let areas = build_csd_text_areas(
        &icons,
        layout,
        bounds,
        icon_color,
        close_icon_color,
        &renderer.overlay_cache,
    );

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

    with_chrome_render_pass(encoder, view, "csd_text_pass", |pass| {
        if let Err(e) =
            renderer
                .overlay_text_renderer
                .render(&renderer.overlay_atlas, &renderer.viewport, pass)
        {
            tracing::warn!("CSD button text render failed: {e}");
        }
    });
}

/// Phase 1: shape every CSD button icon glyph into the chrome cache.
#[allow(clippy::too_many_arguments)]
fn shape_csd_icons(
    icons: &CsdButtonIcons,
    hovered: Option<usize>,
    metrics: Metrics,
    bar_h: f32,
    icon_color: GlyphColor,
    close_icon_color: GlyphColor,
    renderer: &mut GridRenderer,
) {
    let family = renderer.font_config.family.clone();
    let attrs =
        |c: GlyphColor| -> Attrs<'_> { Attrs::new().family(Family::Name(&family)).color(c) };

    ensure_shaped(
        icons.settings_slot,
        icons.settings_label,
        metrics,
        CSD_BTN_W,
        bar_h,
        icons.settings_label,
        attrs(icon_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        icons.min_slot,
        icons.min_label,
        metrics,
        CSD_BTN_W,
        bar_h,
        icons.min_label,
        attrs(icon_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        icons.max_slot,
        icons.max_label,
        metrics,
        CSD_BTN_W,
        bar_h,
        icons.max_label,
        attrs(icon_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    let close_key = format!(
        "{}|{}",
        icons.close_label,
        if hovered == Some(0) { "h" } else { "n" }
    );
    ensure_shaped(
        icons.close_slot,
        &close_key,
        metrics,
        CSD_BTN_W,
        bar_h,
        icons.close_label,
        attrs(close_icon_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
}

/// Phase 2: build TextAreas for the four CSD icons centered in their
/// respective buttons.
fn build_csd_text_areas<'cache>(
    icons: &CsdButtonIcons,
    layout: &CsdButtonLayout,
    bounds: TextBounds,
    icon_color: GlyphColor,
    close_icon_color: GlyphColor,
    cache: &'cache super::text_cache::ChromeTextCache,
) -> Vec<TextArea<'cache>> {
    let center_x = |slot: &str, btn_x: f32| -> f32 {
        let tw = cached_buf(cache, slot)
            .and_then(|b| b.layout_runs().next())
            .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
            .unwrap_or(0.0);
        btn_x + ((CSD_BTN_W - tw) / 2.0).max(0.0)
    };

    let mut areas: Vec<TextArea<'cache>> = Vec::with_capacity(4);
    push_csd_icon(
        &mut areas,
        cache,
        icons.settings_slot,
        center_x(icons.settings_slot, layout.settings_x),
        bounds,
        icon_color,
    );
    push_csd_icon(
        &mut areas,
        cache,
        icons.min_slot,
        center_x(icons.min_slot, layout.min_x),
        bounds,
        icon_color,
    );
    push_csd_icon(
        &mut areas,
        cache,
        icons.max_slot,
        center_x(icons.max_slot, layout.max_x),
        bounds,
        icon_color,
    );
    push_csd_icon(
        &mut areas,
        cache,
        icons.close_slot,
        center_x(icons.close_slot, layout.close_x),
        bounds,
        close_icon_color,
    );
    areas
}

/// Push a TextArea for one CSD icon, skipping silently if the cached
/// buffer is missing.
fn push_csd_icon<'cache>(
    areas: &mut Vec<TextArea<'cache>>,
    cache: &'cache super::text_cache::ChromeTextCache,
    slot: &str,
    left: f32,
    bounds: TextBounds,
    color: GlyphColor,
) {
    if let Some(buf) = cached_buf(cache, slot) {
        areas.push(TextArea {
            buffer: buf,
            left,
            top: 0.0,
            scale: 1.0,
            bounds,
            default_color: color,
            custom_glyphs: &[],
        });
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
