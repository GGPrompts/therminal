//! Helpers that push chrome geometry through `OverlayLayer` instead of
//! issuing their own render passes.

use wgpu::util::DeviceExt;

use therminal_core::palette::ChromePalette;

use crate::grid_renderer::GridRenderer;
use crate::overlay::{OverlayLayer, OverlayTier};
use crate::pane::{LayoutNode, PaneId, PaneState, SplitDirection};

/// Draw a full-screen semi-transparent white overlay for the visual bell.
///
/// Legacy direct-draw path. The active code path routes the visual bell
/// through `OverlayLayer` via `push_visual_bell_overlay`. Kept for tests
/// and as a fallback.
#[allow(dead_code)]
pub(crate) fn draw_visual_bell_overlay(
    intensity: f32,
    renderer: &GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    if intensity <= 0.0 {
        return;
    }

    let alpha = intensity * 0.3;
    let color = [1.0_f32, 1.0, 1.0, alpha];
    let sw = surface_width as f32;
    let sh = surface_height as f32;

    let verts = pixel_rect_to_ndc(0.0, 0.0, sw, sh, sw, sh, color);

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("visual_bell_vbuf"),
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("visual_bell_encoder"),
    });

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("visual_bell_pass"),
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
        pass.draw(0..verts.len() as u32, 0..1);
    }

    queue.submit(std::iter::once(encoder.finish()));
}

/// Push the focus border quads for a pane into the overlay layer.
#[allow(dead_code)]
pub(crate) fn push_focus_border_overlay(
    pane: &PaneState,
    palette: &ChromePalette,
    overlay: &mut OverlayLayer,
) {
    let vp = pane.viewport;
    let t = 2.0_f32;
    let color = palette.focus_border;

    overlay.push_rect(vp.x(), vp.y(), vp.width(), t, color, OverlayTier::Chrome);
    overlay.push_rect(
        vp.x(),
        vp.bottom() - t,
        vp.width(),
        t,
        color,
        OverlayTier::Chrome,
    );
    overlay.push_rect(vp.x(), vp.y(), t, vp.height(), color, OverlayTier::Chrome);
    overlay.push_rect(
        vp.right() - t,
        vp.y(),
        t,
        vp.height(),
        color,
        OverlayTier::Chrome,
    );
}

/// Push the split separator quad into the overlay layer.
#[allow(dead_code)]
pub(crate) fn push_separator_overlay(
    direction: SplitDirection,
    first: &LayoutNode,
    second: &LayoutNode,
    focused: Option<PaneId>,
    palette: &ChromePalette,
    overlay: &mut OverlayLayer,
) {
    let first_rects = first.leaf_rects_pub();
    let second_rects = second.leaf_rects_pub();

    let (Some(f), Some(s)) = (first_rects.last(), second_rects.first()) else {
        return;
    };

    let first_ids = first.pane_ids();
    let second_ids = second.pane_ids();
    let is_focused_adjacent = focused
        .map(|fid| first_ids.contains(&fid) || second_ids.contains(&fid))
        .unwrap_or(false);
    let color = if is_focused_adjacent {
        palette.separator_focus
    } else {
        palette.separator
    };

    let (px, py, pw, ph) = match direction {
        SplitDirection::Horizontal => {
            let sep_x = f.right();
            let sep_y = f.y().min(s.y());
            let sep_h = f.bottom().max(s.bottom()) - sep_y;
            (sep_x, sep_y, 1.0_f32, sep_h)
        }
        SplitDirection::Vertical => {
            let sep_x = f.x().min(s.x());
            let sep_y = f.bottom();
            let sep_w = f.right().max(s.right()) - sep_x;
            (sep_x, sep_y, sep_w, 1.0_f32)
        }
    };

    overlay.push_rect(px, py, pw, ph, color, OverlayTier::Chrome);
}

/// Push the pane header background quad into the overlay layer.
#[allow(dead_code)]
pub(crate) fn push_header_bg_overlay(
    pane: &PaneState,
    is_focused: bool,
    palette: &ChromePalette,
    overlay: &mut OverlayLayer,
) -> f32 {
    let vp = pane.viewport;
    let header_h = crate::pane::PANE_HEADER_HEIGHT;

    let bg_color = if is_focused {
        palette.header_bg
    } else {
        palette.header_bg_dim
    };

    overlay.push_rect(
        vp.x(),
        vp.y(),
        vp.width(),
        header_h,
        bg_color,
        OverlayTier::Chrome,
    );
    header_h
}

/// Push the visual bell overlay quad into the overlay layer.
pub(crate) fn push_visual_bell_overlay(
    intensity: f32,
    surface_width: u32,
    surface_height: u32,
    overlay: &mut OverlayLayer,
) {
    if intensity <= 0.0 {
        return;
    }

    let alpha = intensity * 0.3;
    let color = [1.0_f32, 1.0, 1.0, alpha];
    let sw = surface_width as f32;
    let sh = surface_height as f32;

    overlay.push_rect(0.0, 0.0, sw, sh, color, OverlayTier::Modal);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_visual_bell_overlay_zero_intensity_pushes_nothing() {
        let mut overlay = OverlayLayer::new();
        push_visual_bell_overlay(0.0, 800, 600, &mut overlay);
        assert!(overlay.is_empty());
        assert_eq!(overlay.quad_count(), 0);
    }

    #[test]
    fn push_visual_bell_overlay_negative_intensity_pushes_nothing() {
        let mut overlay = OverlayLayer::new();
        push_visual_bell_overlay(-1.0, 800, 600, &mut overlay);
        assert!(overlay.is_empty());
    }

    #[test]
    fn push_visual_bell_overlay_positive_intensity_pushes_one_quad() {
        let mut overlay = OverlayLayer::new();
        push_visual_bell_overlay(0.5, 800, 600, &mut overlay);
        assert!(!overlay.is_empty());
        assert_eq!(overlay.quad_count(), 1);
    }

    #[test]
    fn push_visual_bell_overlay_full_intensity_pushes_one_quad() {
        let mut overlay = OverlayLayer::new();
        push_visual_bell_overlay(1.0, 1920, 1080, &mut overlay);
        assert_eq!(overlay.quad_count(), 1);
    }

    #[test]
    fn push_visual_bell_overlay_small_positive_intensity_still_pushes() {
        // Even the tiniest positive value above 0.0 should produce a quad.
        let mut overlay = OverlayLayer::new();
        push_visual_bell_overlay(f32::MIN_POSITIVE, 640, 480, &mut overlay);
        assert_eq!(overlay.quad_count(), 1);
    }

    #[test]
    fn push_visual_bell_overlay_exactly_zero_intensity_is_skipped() {
        let mut overlay = OverlayLayer::new();
        push_visual_bell_overlay(0.0, 100, 100, &mut overlay);
        assert_eq!(overlay.quad_count(), 0);
    }

    #[test]
    fn push_visual_bell_overlay_multiple_calls_accumulate() {
        // Each positive-intensity call should add one quad.
        let mut overlay = OverlayLayer::new();
        push_visual_bell_overlay(0.2, 800, 600, &mut overlay);
        push_visual_bell_overlay(0.5, 800, 600, &mut overlay);
        assert_eq!(overlay.quad_count(), 2);
    }

    #[test]
    fn push_visual_bell_overlay_mixed_calls_only_positive_count() {
        let mut overlay = OverlayLayer::new();
        push_visual_bell_overlay(-0.5, 800, 600, &mut overlay); // skipped
        push_visual_bell_overlay(0.0, 800, 600, &mut overlay); // skipped
        push_visual_bell_overlay(0.3, 800, 600, &mut overlay); // added
        assert_eq!(overlay.quad_count(), 1);
    }
}
