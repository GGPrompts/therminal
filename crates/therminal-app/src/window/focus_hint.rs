//! Hover-reveal hint bar for focus mode (tn-sfn9).
//!
//! When focus mode is active and the mouse hovers within 4 px of the top
//! edge, a small translucent bar appears with "F11 to exit focus mode".
//! The hint uses the same OverlayLayer + glyphon pattern as the toast
//! module.

use glyphon::{
    Attrs, Color as GlyphColor, Family, Metrics, Resolution, Shaping, TextArea, TextBounds,
};
use therminal_core::palette::Color as PaletteColor;

use crate::grid_renderer::GridRenderer;
use crate::overlay::{OverlayLayer, OverlayTier};

const HINT_TEXT: &str = "F11 to exit focus mode";
const HINT_CACHE_SLOT: &str = "focus_hint_text";

/// Draw the focus-mode hover-reveal hint bar at the top of the window.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_focus_mode_hint(
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
    overlay: &mut OverlayLayer,
) {
    let sw = surface_width as f32;
    if sw <= 0.0 || (surface_height as f32) <= 0.0 {
        return;
    }

    let bar_h: f32 = 24.0;
    let font_size = (bar_h * 0.55).max(11.0);
    let line_height = font_size + 4.0;
    let metrics = Metrics::new(font_size, line_height);
    let pad_x: f32 = 12.0;
    let pad_y = (bar_h - line_height) / 2.0;

    let text_color = GlyphColor::rgba(
        PaletteColor::INK.r,
        PaletteColor::INK.g,
        PaletteColor::INK.b,
        220,
    );
    let family = renderer.font_config.chrome_font_family().to_string();

    // Shape the hint text (cached via overlay_cache).
    let cache_key = format!("focus_hint|{:.0}", sw);
    let needs_reshape = renderer
        .overlay_cache
        .get(HINT_CACHE_SLOT)
        .map(|(k, _)| k.as_str() != cache_key)
        .unwrap_or(true);
    if needs_reshape {
        let mut buf = glyphon::Buffer::new(&mut renderer.font_system, metrics);
        buf.set_size(&mut renderer.font_system, Some(sw), Some(line_height));
        buf.set_text(
            &mut renderer.font_system,
            HINT_TEXT,
            &Attrs::new().family(Family::Name(&family)).color(text_color),
            Shaping::Basic,
            None,
        );
        buf.shape_until_scroll(&mut renderer.font_system, false);
        renderer
            .overlay_cache
            .insert(HINT_CACHE_SLOT.to_string(), (cache_key, buf));
    }

    let buf = match renderer.overlay_cache.get(HINT_CACHE_SLOT) {
        Some((_, b)) => b,
        None => return,
    };

    let text_w = buf
        .layout_runs()
        .next()
        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
        .unwrap_or(0.0);

    // Center the hint bar at the top of the window.
    let box_w = text_w + pad_x * 2.0;
    let box_x = (sw - box_w) / 2.0;
    let box_y: f32 = 0.0;

    // Translucent dark background.
    overlay.push_rect(
        box_x,
        box_y,
        box_w,
        bar_h,
        [0.06, 0.06, 0.08, 0.80],
        OverlayTier::Modal,
    );

    // Text rendering -- follows the exact same pattern as toast.rs.
    let bounds = TextBounds {
        left: box_x as i32,
        top: box_y as i32,
        right: (box_x + box_w) as i32,
        bottom: (box_y + bar_h) as i32,
    };

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    let text_areas = vec![TextArea {
        buffer: buf,
        left: box_x + pad_x,
        top: box_y + pad_y,
        scale: 1.0,
        bounds,
        default_color: text_color,
        custom_glyphs: &[],
    }];

    if let Err(e) = renderer.overlay_text_renderer.prepare(
        device,
        queue,
        &mut renderer.font_system,
        &mut renderer.overlay_atlas,
        &renderer.viewport,
        text_areas,
        &mut renderer.swash_cache,
    ) {
        tracing::warn!("focus hint text prepare failed: {e}");
        return;
    }

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("focus_hint_encoder"),
    });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("focus_hint_text_pass"),
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
            tracing::warn!("focus hint text render failed: {e}");
        }
    }
    queue.submit(std::iter::once(encoder.finish()));
}
