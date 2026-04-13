//! Settings overlay renderer: `draw_settings_overlay` orchestration.
//!
//! Two-pass GPU draw — split across submodules:
//! - [`layout`] — `PanelLayout` describing every screen-space rect the
//!   overlay needs (panel, nav strip, control row positions, value column
//!   x, etc.). Pure geometry math, no wgpu.
//! - [`rects`] — `build_rect_vertices`: scrim / panel / nav / focus ring
//!   / per-control rect geometry into a `Vec<ColorVertex>`.
//! - [`text`] — `build_text_areas`: glyphon `Buffer`s + `TextArea`
//!   placements for the title, hint, nav labels, and per-control text.
//!   Also owns `truncate_for_width`.

mod layout;
mod rects;
mod text;

use wgpu::util::DeviceExt;

use crate::grid_renderer::GridRenderer;

use super::state::SettingsOverlayState;

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_settings_overlay(
    state: &mut SettingsOverlayState,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    let layout = layout::PanelLayout::compute(surface_width, surface_height);
    state.set_panel_rect(
        layout.panel_x,
        layout.panel_y,
        layout.panel_w,
        layout.panel_h,
    );

    // Snapshot the chrome palette (`Copy`) so both passes can reference it
    // without conflicting with the `&mut renderer` borrow in pass 2.
    let chrome_palette = renderer.chrome_palette;

    // ── Pass 1: rect pipeline (scrim + panel + nav + focus + control bgs) ──
    let verts = rects::build_rect_vertices(state, &layout, &chrome_palette);
    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("settings_overlay_rects"),
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let mut rect_encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("settings_overlay_rect_encoder"),
    });
    {
        let mut pass = rect_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("settings_overlay_rect_pass"),
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
    queue.submit(std::iter::once(rect_encoder.finish()));

    // ── Pass 2: glyphon text (title, hint, nav, per-control labels) ──
    use glyphon::Resolution;
    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );
    let (buffers, placements) = text::build_text_buffers(state, &layout, renderer, &chrome_palette);
    let text_areas: Vec<glyphon::TextArea<'_>> = placements
        .iter()
        .map(|p| glyphon::TextArea {
            buffer: &buffers[p.buffer_idx],
            left: p.left,
            top: p.top,
            scale: 1.0,
            bounds: p.bounds,
            default_color: p.color,
            custom_glyphs: &[],
        })
        .collect();
    let mut text_encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("settings_overlay_text_encoder"),
    });
    if let Err(e) = renderer.overlay_text_renderer.prepare(
        device,
        queue,
        &mut renderer.font_system,
        &mut renderer.overlay_atlas,
        &renderer.viewport,
        text_areas,
        &mut renderer.swash_cache,
    ) {
        tracing::warn!("settings overlay text prepare failed: {}", e);
    }
    {
        let mut pass = text_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("settings_overlay_text_pass"),
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
            tracing::warn!("settings overlay text render failed: {}", e);
        }
    }
    queue.submit(std::iter::once(text_encoder.finish()));
}
