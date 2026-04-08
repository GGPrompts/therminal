//! Transient on-screen toast notifications.
//!
//! A [`Toast`] is a small text message rendered in the lower-right of the
//! window for a few seconds. Used to surface failures that would otherwise
//! be silent to the user (e.g. `open_in_editor` failures from hotspot
//! clicks).
//!
//! The state machine (`Toast::new`, `is_expired`) is GPU-free and unit
//! tested. The actual rendering is handled by [`draw_toast`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use glyphon::{
    Attrs, Buffer, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping, TextArea,
    TextBounds,
};
use therminal_core::palette::Color as PaletteColor;

use crate::grid_renderer::GridRenderer;
use crate::overlay::{OverlayLayer, OverlayTier};

/// Default on-screen lifetime for a toast.
pub(crate) const TOAST_TTL: Duration = Duration::from_millis(2500);

/// A transient message shown in the lower-right corner of the window.
#[derive(Debug, Clone)]
pub(crate) struct Toast {
    pub text: String,
    pub expires_at: Instant,
}

impl Toast {
    /// Create a new toast that expires `ttl` from `now`.
    pub fn new(text: impl Into<String>, now: Instant, ttl: Duration) -> Self {
        Self {
            text: text.into(),
            expires_at: now + ttl,
        }
    }

    /// Returns true if `now` is past this toast's expiry.
    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.expires_at
    }
}

/// Cache slot key for the shaped toast text. Kept private so nothing
/// else in the overlay cache collides with it.
const TOAST_CACHE_SLOT: &str = "toast_text";

#[allow(clippy::too_many_arguments)]
fn ensure_shaped(
    cache_key: &str,
    metrics: Metrics,
    width: f32,
    height: f32,
    text: &str,
    attrs: Attrs<'_>,
    font_system: &mut FontSystem,
    cache: &mut HashMap<String, (String, Buffer)>,
) {
    let needs_reshape = cache
        .get(TOAST_CACHE_SLOT)
        .map(|(k, _)| k.as_str() != cache_key)
        .unwrap_or(true);

    if needs_reshape {
        let mut buf = Buffer::new(font_system, metrics);
        buf.set_size(font_system, Some(width), Some(height));
        buf.set_text(font_system, text, &attrs, Shaping::Basic, None);
        buf.shape_until_scroll(font_system, false);
        cache.insert(TOAST_CACHE_SLOT.to_string(), (cache_key.to_string(), buf));
    }
}

/// Draw a toast in the lower-right corner of the window.
///
/// Pushes a semi-opaque background rect into `overlay` (Modal tier) and
/// renders the text via `renderer`'s overlay text pipeline in its own
/// encoder/pass so the caller can still submit other work afterwards.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_toast(
    toast: &Toast,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
    overlay: &mut OverlayLayer,
) {
    let sw = surface_width as f32;
    let sh = surface_height as f32;
    if sw <= 0.0 || sh <= 0.0 {
        return;
    }

    let pad_x: f32 = 12.0;
    let pad_y: f32 = 6.0;
    let bar_h = crate::pane::STATUS_BAR_HEIGHT;
    let font_size = (bar_h * 0.55).max(11.0);
    let line_height = font_size + 4.0;
    let metrics = Metrics::new(font_size, line_height);

    let text_color = GlyphColor::rgba(
        PaletteColor::INK.r,
        PaletteColor::INK.g,
        PaletteColor::INK.b,
        240,
    );
    let family = renderer.font_config.family.clone();
    let key = format!("{}|{:.0}", toast.text, sw);
    ensure_shaped(
        &key,
        metrics,
        sw * 0.6,
        line_height,
        &toast.text,
        Attrs::new().family(Family::Name(&family)).color(text_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );

    let buf = match renderer.overlay_cache.get(TOAST_CACHE_SLOT) {
        Some((_, b)) => b,
        None => return,
    };

    let text_w = buf
        .layout_runs()
        .next()
        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
        .unwrap_or(0.0);

    let box_w = text_w + pad_x * 2.0;
    let box_h = line_height + pad_y * 2.0;
    let margin: f32 = 12.0;

    // Anchor above the status bar so it doesn't get clipped by chrome.
    let anchor_bottom = sh - bar_h - margin;
    let box_x = (sw - box_w - margin).max(0.0);
    let box_y = (anchor_bottom - box_h).max(0.0);

    // Background quad (Modal tier, on top of chrome/widgets).
    overlay.push_rect(
        box_x,
        box_y,
        box_w,
        box_h,
        [0.08, 0.08, 0.10, 0.88],
        OverlayTier::Modal,
    );

    let bounds = TextBounds {
        left: box_x as i32,
        top: box_y as i32,
        right: (box_x + box_w) as i32,
        bottom: (box_y + box_h) as i32,
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
        tracing::warn!("toast text prepare failed: {e}");
        return;
    }

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("toast_text_encoder"),
    });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("toast_text_pass"),
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
            tracing::warn!("toast text render failed: {e}");
        }
    }
    queue.submit(std::iter::once(encoder.finish()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_not_expired_before_ttl() {
        let now = Instant::now();
        let t = Toast::new("hello", now, Duration::from_millis(2500));
        assert!(!t.is_expired(now));
        assert!(!t.is_expired(now + Duration::from_millis(2499)));
    }

    #[test]
    fn toast_expired_at_ttl() {
        let now = Instant::now();
        let t = Toast::new("hello", now, Duration::from_millis(2500));
        assert!(t.is_expired(now + Duration::from_millis(2500)));
        assert!(t.is_expired(now + Duration::from_secs(10)));
    }

    #[test]
    fn toast_stores_text() {
        let now = Instant::now();
        let t = Toast::new("file not found: foo.rs", now, TOAST_TTL);
        assert_eq!(t.text, "file not found: foo.rs");
    }

    #[test]
    fn toast_default_ttl_is_2500ms() {
        assert_eq!(TOAST_TTL, Duration::from_millis(2500));
    }

    #[test]
    fn later_push_overwrites_earlier() {
        // Emulate App::show_toast semantics: newer toast replaces older.
        let now = Instant::now();
        let slot: Option<Toast> = Some(Toast::new("first", now, TOAST_TTL));
        assert_eq!(slot.as_ref().map(|t| t.text.as_str()), Some("first"));
        // A subsequent show_toast call replaces the slot wholesale; emulate
        // that by re-binding instead of mutating.
        let slot: Option<Toast> = Some(Toast::new("second", now, TOAST_TTL));
        assert_eq!(slot.as_ref().map(|t| t.text.as_str()), Some("second"));
    }
}
