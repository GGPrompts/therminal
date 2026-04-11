//! Shared render-pass helper for chrome rendering.
//!
//! Every chrome submodule (pane_header, status_bar, csd, tab_bar) opens
//! `LoadOp::Load + StoreOp::Store` color-only render passes against the
//! same swapchain view. The verbose `RenderPassDescriptor` literal ends up
//! repeated 9+ times across the four files. `with_chrome_render_pass`
//! centralises that boilerplate so each call site only specifies the label
//! and the actual draw commands.

/// Begin a chrome render pass with `LoadOp::Load + StoreOp::Store` against
/// the given swapchain view, run the supplied closure on it, and end the
/// pass when the closure returns.
///
/// All chrome passes share the same descriptor shape, so this helper hides
/// the noisy literal and keeps the per-call site focused on what the pass
/// actually draws.
pub(super) fn with_chrome_render_pass<F>(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    label: &'static str,
    draw: F,
) where
    F: FnOnce(&mut wgpu::RenderPass<'_>),
{
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
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
    draw(&mut pass);
}
