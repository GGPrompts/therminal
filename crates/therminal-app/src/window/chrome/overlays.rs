//! Helpers that push chrome geometry through `OverlayLayer` instead of
//! issuing their own render passes.

use wgpu::util::DeviceExt;

use crate::grid_renderer::GridRenderer;
use crate::overlay::{OverlayLayer, OverlayTier};
use crate::pane::{LayoutNode, PaneId, PaneState, SplitDirection};

use super::colors::{
    FOCUS_BORDER_COLOR, HEADER_BG_COLOR, HEADER_BG_DIM_COLOR, SEPARATOR_COLOR,
    SEPARATOR_FOCUS_COLOR,
};

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
pub(crate) fn push_focus_border_overlay(pane: &PaneState, overlay: &mut OverlayLayer) {
    let vp = pane.viewport;
    let t = 2.0_f32;

    overlay.push_rect(
        vp.x(),
        vp.y(),
        vp.width(),
        t,
        FOCUS_BORDER_COLOR,
        OverlayTier::Chrome,
    );
    overlay.push_rect(
        vp.x(),
        vp.bottom() - t,
        vp.width(),
        t,
        FOCUS_BORDER_COLOR,
        OverlayTier::Chrome,
    );
    overlay.push_rect(
        vp.x(),
        vp.y(),
        t,
        vp.height(),
        FOCUS_BORDER_COLOR,
        OverlayTier::Chrome,
    );
    overlay.push_rect(
        vp.right() - t,
        vp.y(),
        t,
        vp.height(),
        FOCUS_BORDER_COLOR,
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
        SEPARATOR_FOCUS_COLOR
    } else {
        SEPARATOR_COLOR
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
    overlay: &mut OverlayLayer,
) -> f32 {
    let vp = pane.viewport;
    let header_h = crate::pane::PANE_HEADER_HEIGHT;

    let bg_color = if is_focused {
        HEADER_BG_COLOR
    } else {
        HEADER_BG_DIM_COLOR
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
