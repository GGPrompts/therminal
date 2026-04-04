//! Thin wrapper around glyphon 0.7 + cosmic-text 0.12 for consistent text
//! rendering across all therminal crates.
//!
//! # Glyphon 0.7 API notes
//! In glyphon 0.7 a [`Cache`] holds shared pipeline/shader state and must be
//! created first.  [`TextAtlas`] and [`Viewport`] each borrow that `Cache` at
//! construction time.  The old pattern of constructing `TextAtlas` directly
//! from a `Device` no longer exists.
//!
//! The workspace uses wgpu 23, which matches glyphon 0.7's dependency.

use glyphon::{
    Attrs, Buffer, Cache, Color, ColorMode, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextAtlas, TextRenderer, Viewport,
};
use wgpu::{Device, MultisampleState, Queue, TextureFormat};

/// All glyphon state bundled into a single, reusable struct.
///
/// Create one of these per rendering surface and share it across draw calls.
/// Call [`resize`](TherminalTextRenderer::resize) whenever the surface dimensions
/// change, and [`make_buffer`](TherminalTextRenderer::make_buffer) to produce a
/// shaped [`Buffer`] ready for submission to `TextRenderer::prepare`.
pub struct TherminalTextRenderer {
    /// cosmic-text font system — owns loaded fonts and shaping caches.
    pub font_system: FontSystem,
    /// Swash rasterisation cache used by glyphon during `prepare`.
    pub swash_cache: SwashCache,
    /// Shared glyphon pipeline / shader cache (wgpu 23).
    pub cache: Cache,
    /// GPU glyph atlas.
    pub atlas: TextAtlas,
    /// Per-surface viewport (holds the screen-resolution uniform buffer).
    pub viewport: Viewport,
    /// The actual text renderer that submits draw calls.
    pub renderer: TextRenderer,
}

impl TherminalTextRenderer {
    /// Initialise all glyphon state for a given surface.
    ///
    /// All wgpu types here come from the **wgpu 23** family (the version
    /// glyphon 0.7 depends on).
    ///
    /// # Arguments
    /// * `device`  — wgpu 23 `Device`
    /// * `queue`   — wgpu 23 `Queue`
    /// * `format`  — `TextureFormat` of the render target
    /// * `width`   — surface width in physical pixels
    /// * `height`  — surface height in physical pixels
    pub fn new(
        device: &Device,
        queue: &Queue,
        format: TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        let font_system = FontSystem::new();
        let swash_cache = SwashCache::new();

        // glyphon 0.7: Cache must be created before TextAtlas / Viewport.
        let cache = Cache::new(device);

        let mut atlas = TextAtlas::with_color_mode(
            device,
            queue,
            &cache,
            format,
            glyphon_color_mode_for_surface(format),
        );

        let viewport = {
            let mut vp = Viewport::new(device, &cache);
            vp.update(queue, Resolution { width, height });
            vp
        };

        // No multisampling, no depth/stencil — matches the therminal compositor
        // default.  Callers that need MSAA can build their own renderer.
        let renderer = TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);

        Self {
            font_system,
            swash_cache,
            cache,
            atlas,
            viewport,
            renderer,
        }
    }

    /// Update the viewport when the surface is resized.
    pub fn resize(&mut self, queue: &Queue, width: u32, height: u32) {
        self.viewport.update(queue, Resolution { width, height });
    }

    /// Create a shaped, laid-out [`Buffer`] for the given text.
    ///
    /// The buffer uses the system default font family.  Callers that need
    /// specific typefaces (Rajdhani, Share Tech Mono, …) should set attributes
    /// on the returned buffer directly via `Buffer::set_rich_text`.
    ///
    /// # Arguments
    /// * `text`        — UTF-8 string to render
    /// * `font_size`   — font size in points
    /// * `line_height` — line height in points (typically `font_size * 1.2`)
    /// * `color`       — glyphon [`Color`] (RGBA)
    pub fn make_buffer(
        &mut self,
        text: &str,
        font_size: f32,
        line_height: f32,
        color: Color,
    ) -> Buffer {
        let metrics = Metrics::new(font_size, line_height);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);

        // Default to no explicit size constraint — callers can call
        // `buffer.set_size` to wrap at a specific width.
        buffer.set_size(&mut self.font_system, None, None);

        let attrs = Attrs::new().color(color).family(Family::SansSerif);
        buffer.set_text(&mut self.font_system, text, attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut self.font_system, false);

        buffer
    }
}

/// Choose the glyphon color mode that matches the surface color space.
///
/// glyphon expects `Accurate` for sRGB targets and `Web` for linear targets
/// that still receive colors specified in the sRGB color space.
pub fn glyphon_color_mode_for_surface(format: TextureFormat) -> ColorMode {
    match format {
        TextureFormat::Rgba8UnormSrgb
        | TextureFormat::Bgra8UnormSrgb
        | TextureFormat::Bc1RgbaUnormSrgb
        | TextureFormat::Bc2RgbaUnormSrgb
        | TextureFormat::Bc3RgbaUnormSrgb
        | TextureFormat::Bc7RgbaUnormSrgb
        | TextureFormat::Etc2Rgb8UnormSrgb
        | TextureFormat::Etc2Rgb8A1UnormSrgb
        | TextureFormat::Etc2Rgba8UnormSrgb
        | TextureFormat::Astc {
            channel: wgpu::AstcChannel::UnormSrgb,
            ..
        } => ColorMode::Accurate,
        _ => ColorMode::Web,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_surfaces_use_accurate_mode() {
        assert_eq!(
            glyphon_color_mode_for_surface(TextureFormat::Bgra8UnormSrgb),
            ColorMode::Accurate
        );
        assert_eq!(
            glyphon_color_mode_for_surface(TextureFormat::Rgba8UnormSrgb),
            ColorMode::Accurate
        );
    }

    #[test]
    fn linear_surfaces_use_web_mode() {
        assert_eq!(
            glyphon_color_mode_for_surface(TextureFormat::Bgra8Unorm),
            ColorMode::Web
        );
        assert_eq!(
            glyphon_color_mode_for_surface(TextureFormat::Rgba16Float),
            ColorMode::Web
        );
    }
}
