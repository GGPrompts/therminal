//! Overlay layer: two-pass GPU rendering with depth-ordered compositing.
//!
//! The terminal renderer uses two passes:
//!   1. **Grid pass** — terminal cell content (backgrounds, glyphs, cursor)
//!   2. **Overlay pass** — semi-transparent chrome, widgets, and modals
//!
//! `OverlayLayer` collects geometry (colored quads) and text areas across
//! three depth tiers, then composites them in a single batched render pass
//! with alpha blending on top of the grid content.
//!
//! ## Depth tiers (rendered bottom-to-top)
//!   - **Chrome** (0) — status bar, tab bar, pane headers, separators, focus borders
//!   - **Widget** (1) — Phase 6 overlay widgets (gauges, cards, indicators)
//!   - **Modal**  (2) — help overlay, context menus, visual bell, toast notifications

use crate::grid_renderer::{ColorVertex, GridRenderer};
use wgpu::util::DeviceExt;

// ── Depth tiers ──────────────────────────────────────────────────────

/// Depth tier for overlay elements. Lower tiers render first (behind higher tiers).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(dead_code)]
pub enum OverlayTier {
    /// Chrome: status bar, tab bar, pane headers, separators, focus borders.
    Chrome = 0,
    /// Widgets: Phase 6 overlay widgets (gauges, cards, indicators).
    Widget = 1,
    /// Modals: help overlay, context menus, visual bell, notifications.
    Modal = 2,
}

// ── Overlay quad ─────────────────────────────────────────────────────

/// A colored quad to be rendered in the overlay pass.
///
/// Quads are axis-aligned rectangles specified in pixel coordinates.
/// They are converted to NDC and batched per tier before rendering.
#[derive(Clone, Debug)]
pub struct OverlayQuad {
    /// Pixel x of the top-left corner.
    pub x: f32,
    /// Pixel y of the top-left corner.
    pub y: f32,
    /// Width in pixels.
    pub width: f32,
    /// Height in pixels.
    pub height: f32,
    /// RGBA color with premultiplied alpha.
    pub color: [f32; 4],
    /// Which depth tier this quad belongs to.
    pub tier: OverlayTier,
}

// ── OverlayLayer ─────────────────────────────────────────────────────

/// Collects overlay geometry for a single frame, then renders it all
/// in one compositing pass after the grid pass completes.
///
/// Usage:
/// ```ignore
/// let mut overlay = OverlayLayer::new();
/// overlay.push_quad(OverlayQuad { ... });
/// // ... collect all chrome/widget/modal quads ...
/// overlay.render(renderer, device, queue, &view, width, height);
/// ```
pub struct OverlayLayer {
    /// Accumulated quads, unsorted. Sorted by tier at render time.
    quads: Vec<OverlayQuad>,
}

impl OverlayLayer {
    /// Create a new empty overlay layer for this frame.
    pub fn new() -> Self {
        Self {
            quads: Vec::with_capacity(64),
        }
    }

    /// Add a single colored quad to the overlay.
    #[allow(dead_code)]
    pub fn push_quad(&mut self, quad: OverlayQuad) {
        self.quads.push(quad);
    }

    /// Add a pixel-space rectangle with the given color and tier.
    pub fn push_rect(
        &mut self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: [f32; 4],
        tier: OverlayTier,
    ) {
        self.quads.push(OverlayQuad {
            x,
            y,
            width,
            height,
            color,
            tier,
        });
    }

    /// Returns true if no quads have been submitted.
    pub fn is_empty(&self) -> bool {
        self.quads.is_empty()
    }

    /// Number of quads submitted.
    #[allow(dead_code)]
    pub fn quad_count(&self) -> usize {
        self.quads.len()
    }

    /// Render all collected overlay quads in a single batched pass.
    ///
    /// Quads are sorted by tier (Chrome < Widget < Modal) so that higher
    /// tiers composite on top of lower tiers. Within a tier, quads render
    /// in submission order.
    ///
    /// The render pass uses `LoadOp::Load` to preserve the grid content
    /// underneath, and alpha blending for semi-transparency.
    pub fn render(
        &mut self,
        renderer: &GridRenderer,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        if self.quads.is_empty() {
            return;
        }

        // Sort by tier so lower tiers render first.
        self.quads.sort_by_key(|q| q.tier);

        let sw = surface_width as f32;
        let sh = surface_height as f32;

        // Convert all quads to NDC vertices.
        let mut vertices: Vec<ColorVertex> = Vec::with_capacity(self.quads.len() * 6);
        for quad in &self.quads {
            let verts = pixel_rect_to_ndc_overlay(
                quad.x,
                quad.y,
                quad.width,
                quad.height,
                sw,
                sh,
                quad.color,
            );
            vertices.extend_from_slice(&verts);
        }

        if vertices.is_empty() {
            return;
        }

        let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("overlay_vbuf"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("overlay_pass_encoder"),
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay_composite_pass"),
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
            pass.draw(0..vertices.len() as u32, 0..1);
        }

        queue.submit(std::iter::once(encoder.finish()));
    }

    /// Clear all quads for reuse next frame.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.quads.clear();
    }
}

impl Default for OverlayLayer {
    fn default() -> Self {
        Self::new()
    }
}

// ── NDC conversion ───────────────────────────────────────────────────

/// Convert a pixel-space rectangle to 6 NDC vertices (two triangles).
///
/// This is the overlay module's own conversion to avoid coupling to
/// `color_mapping::pixel_rect_to_ndc` (which serves the grid pass).
fn pixel_rect_to_ndc_overlay(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    surface_w: f32,
    surface_h: f32,
    color: [f32; 4],
) -> [ColorVertex; 6] {
    let x0 = 2.0 * x / surface_w - 1.0;
    let y0 = 1.0 - 2.0 * y / surface_h;
    let x1 = 2.0 * (x + w) / surface_w - 1.0;
    let y1 = 1.0 - 2.0 * (y + h) / surface_h;

    [
        ColorVertex {
            position: [x0, y0],
            color,
        },
        ColorVertex {
            position: [x1, y0],
            color,
        },
        ColorVertex {
            position: [x0, y1],
            color,
        },
        ColorVertex {
            position: [x1, y0],
            color,
        },
        ColorVertex {
            position: [x1, y1],
            color,
        },
        ColorVertex {
            position: [x0, y1],
            color,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_layer_empty_by_default() {
        let layer = OverlayLayer::new();
        assert!(layer.is_empty());
        assert_eq!(layer.quad_count(), 0);
    }

    #[test]
    fn overlay_layer_push_and_count() {
        let mut layer = OverlayLayer::new();
        layer.push_rect(
            0.0,
            0.0,
            100.0,
            20.0,
            [1.0, 0.0, 0.0, 0.5],
            OverlayTier::Chrome,
        );
        layer.push_rect(
            0.0,
            0.0,
            200.0,
            40.0,
            [0.0, 1.0, 0.0, 0.8],
            OverlayTier::Modal,
        );
        assert!(!layer.is_empty());
        assert_eq!(layer.quad_count(), 2);
    }

    #[test]
    fn overlay_layer_clear() {
        let mut layer = OverlayLayer::new();
        layer.push_rect(10.0, 10.0, 50.0, 50.0, [1.0; 4], OverlayTier::Widget);
        layer.clear();
        assert!(layer.is_empty());
    }

    #[test]
    fn overlay_tier_ordering() {
        assert!(OverlayTier::Chrome < OverlayTier::Widget);
        assert!(OverlayTier::Widget < OverlayTier::Modal);
    }

    #[test]
    fn ndc_conversion_full_screen() {
        // A quad covering the full surface should produce NDC corners at (-1,1) to (1,-1).
        let verts = pixel_rect_to_ndc_overlay(0.0, 0.0, 800.0, 600.0, 800.0, 600.0, [1.0; 4]);
        // Top-left vertex: (-1, 1)
        assert!((verts[0].position[0] - (-1.0)).abs() < 1e-5);
        assert!((verts[0].position[1] - 1.0).abs() < 1e-5);
        // Bottom-right vertex: (1, -1)
        assert!((verts[4].position[0] - 1.0).abs() < 1e-5);
        assert!((verts[4].position[1] - (-1.0)).abs() < 1e-5);
    }

    #[test]
    fn sort_by_tier() {
        let mut layer = OverlayLayer::new();
        layer.push_rect(0.0, 0.0, 10.0, 10.0, [1.0; 4], OverlayTier::Modal);
        layer.push_rect(0.0, 0.0, 10.0, 10.0, [1.0; 4], OverlayTier::Chrome);
        layer.push_rect(0.0, 0.0, 10.0, 10.0, [1.0; 4], OverlayTier::Widget);

        // Sort (same as render does internally).
        layer.quads.sort_by_key(|q| q.tier);

        assert_eq!(layer.quads[0].tier, OverlayTier::Chrome);
        assert_eq!(layer.quads[1].tier, OverlayTier::Widget);
        assert_eq!(layer.quads[2].tier, OverlayTier::Modal);
    }
}
