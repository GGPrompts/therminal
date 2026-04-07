//! Pane focus borders, split separators, and per-pane header strips.

use glyphon::{Attrs, Color as GlyphColor, Family, Metrics, Resolution, TextArea, TextBounds};
use wgpu::util::DeviceExt;

use crate::grid_renderer::{ColorVertex, GridRenderer};
use crate::pane::{LayoutNode, PaneId, PaneState, SplitDirection};
use therminal_core::palette::Color as PaletteColor;

use super::colors::{
    FOCUS_BORDER_COLOR, HEADER_BG_COLOR, HEADER_BG_DIM_COLOR, HEADER_BUTTON_MARGIN,
    HEADER_BUTTON_WIDTH, SEPARATOR_COLOR, SEPARATOR_FOCUS_COLOR,
};
use super::text_cache::{cached_buf, ensure_shaped};

/// Draw a subtle border around the focused pane.
pub(crate) fn draw_pane_focus_border(
    pane: &PaneState,
    renderer: &GridRenderer,
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    let vp = pane.viewport;
    let t = 2.0_f32;
    let sw = surface_width as f32;
    let sh = surface_height as f32;

    let mut verts: Vec<ColorVertex> = Vec::new();

    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.x(),
        vp.y(),
        vp.width(),
        t,
        sw,
        sh,
        FOCUS_BORDER_COLOR,
    ));
    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.x(),
        vp.bottom() - t,
        vp.width(),
        t,
        sw,
        sh,
        FOCUS_BORDER_COLOR,
    ));
    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.x(),
        vp.y(),
        t,
        vp.height(),
        sw,
        sh,
        FOCUS_BORDER_COLOR,
    ));
    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.right() - t,
        vp.y(),
        t,
        vp.height(),
        sw,
        sh,
        FOCUS_BORDER_COLOR,
    ));

    if verts.is_empty() {
        return;
    }

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("focus_border_vbuf"),
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("focus_border_pass"),
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

/// Draw a 1px separator line in the gap between two split children.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_split_separator(
    direction: SplitDirection,
    first: &LayoutNode,
    second: &LayoutNode,
    focused: Option<PaneId>,
    renderer: &GridRenderer,
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    let first_rects = first.leaf_rects_pub();
    let second_rects = second.leaf_rects_pub();

    let (Some(f), Some(s)) = (first_rects.last(), second_rects.first()) else {
        return;
    };

    let sw = surface_width as f32;
    let sh = surface_height as f32;

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

    let verts = pixel_rect_to_ndc(px, py, pw, ph, sw, sh, color);

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("separator_vbuf"),
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("separator_pass"),
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
    pass.draw(0..6, 0..1);
}

/// Draw the pane header strip (background + text).
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_pane_header(
    pane: &PaneState,
    pane_index: usize,
    is_focused: bool,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    let vp = pane.viewport;
    let header_h = crate::pane::PANE_HEADER_HEIGHT;
    let sw = surface_width as f32;
    let sh = surface_height as f32;

    // ── Header background rect ──
    let bg_color = if is_focused {
        HEADER_BG_COLOR
    } else {
        HEADER_BG_DIM_COLOR
    };
    let bg_verts = pixel_rect_to_ndc(vp.x(), vp.y(), vp.width(), header_h, sw, sh, bg_color);

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("header_bg_vbuf"),
        contents: bytemuck::cast_slice(&bg_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("header_bg_pass"),
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
        pass.draw(0..6, 0..1);
    }

    // ── Header text ──
    let font_size = (header_h * 0.6).max(9.0);
    let line_height = header_h;
    let metrics = Metrics::new(font_size, line_height);

    let index_text = format!(" {}", pane_index + 1);
    let index_color = GlyphColor::rgba(
        PaletteColor::INK_MUTED.r,
        PaletteColor::INK_MUTED.g,
        PaletteColor::INK_MUTED.b,
        if is_focused { 255 } else { 200 },
    );

    let process_text = format!("pane {}", pane_index + 1);
    let process_color = if is_focused {
        GlyphColor::rgba(
            PaletteColor::INK.r,
            PaletteColor::INK.g,
            PaletteColor::INK.b,
            255,
        )
    } else {
        GlyphColor::rgba(
            PaletteColor::INK_MUTED.r,
            PaletteColor::INK_MUTED.g,
            PaletteColor::INK_MUTED.b,
            220,
        )
    };

    let close_color = GlyphColor::rgba(
        PaletteColor::ALERT.r,
        PaletteColor::ALERT.g,
        PaletteColor::ALERT.b,
        if is_focused { 230 } else { 160 },
    );
    let button_color = GlyphColor::rgba(
        PaletteColor::INK_MUTED.r,
        PaletteColor::INK_MUTED.g,
        PaletteColor::INK_MUTED.b,
        if is_focused { 230 } else { 170 },
    );

    let btn_x_close = vp.x() + vp.width() - HEADER_BUTTON_MARGIN - HEADER_BUTTON_WIDTH;
    let btn_x_vsplit = btn_x_close - HEADER_BUTTON_WIDTH;
    let btn_x_hsplit = btn_x_vsplit - HEADER_BUTTON_WIDTH;

    let focus_tag = if is_focused { "f" } else { "u" };
    let idx_slot = format!("hdr_idx_{pane_index}");
    let idx_key = format!("{index_text}|{:.0}|{focus_tag}", vp.width());
    let proc_slot = format!("hdr_proc_{pane_index}");
    let proc_key = format!("{process_text}|{:.0}|{focus_tag}", vp.width());
    let close_slot = format!("hdr_close_{pane_index}");
    let close_key = format!("X|{focus_tag}");
    let vsplit_slot = format!("hdr_vsplit_{pane_index}");
    let vsplit_key = format!("V|{focus_tag}");
    let hsplit_slot = format!("hdr_hsplit_{pane_index}");
    let hsplit_key = format!("H|{focus_tag}");

    // Phase 1: shape all buffers.
    let family = renderer.font_config.family.clone();
    ensure_shaped(
        &idx_slot,
        &idx_key,
        metrics,
        vp.width(),
        header_h,
        &index_text,
        Attrs::new()
            .family(Family::Name(&family))
            .color(index_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        &proc_slot,
        &proc_key,
        metrics,
        vp.width(),
        header_h,
        &process_text,
        Attrs::new()
            .family(Family::Name(&family))
            .color(process_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        &close_slot,
        &close_key,
        metrics,
        HEADER_BUTTON_WIDTH,
        header_h,
        " X",
        Attrs::new()
            .family(Family::Name(&family))
            .color(close_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        &vsplit_slot,
        &vsplit_key,
        metrics,
        HEADER_BUTTON_WIDTH,
        header_h,
        " V",
        Attrs::new()
            .family(Family::Name(&family))
            .color(button_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        &hsplit_slot,
        &hsplit_key,
        metrics,
        HEADER_BUTTON_WIDTH,
        header_h,
        " H",
        Attrs::new()
            .family(Family::Name(&family))
            .color(button_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );

    // Phase 2: immutable borrow for TextArea references.
    // If any slot is missing from the cache (shaping failure / programming error),
    // skip just that element rather than panicking on the render hot path.
    let Some(index_buf) = cached_buf(&renderer.overlay_cache, &idx_slot) else {
        tracing::warn!("pane header: index buffer slot missing; skipping header draw");
        return;
    };
    let Some(process_buf) = cached_buf(&renderer.overlay_cache, &proc_slot) else {
        tracing::warn!("pane header: process buffer slot missing; skipping header draw");
        return;
    };

    let process_text_width = process_buf
        .layout_runs()
        .next()
        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
        .unwrap_or(0.0);
    let center_offset = ((vp.width() - process_text_width) / 2.0).max(0.0);

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    let bounds = TextBounds {
        left: 0,
        top: 0,
        right: surface_width as i32,
        bottom: surface_height as i32,
    };

    let mut text_areas: Vec<TextArea<'_>> = Vec::with_capacity(5);
    text_areas.push(TextArea {
        buffer: index_buf,
        left: vp.x(),
        top: vp.y(),
        scale: 1.0,
        bounds,
        default_color: index_color,
        custom_glyphs: &[],
    });
    text_areas.push(TextArea {
        buffer: process_buf,
        left: vp.x() + center_offset,
        top: vp.y(),
        scale: 1.0,
        bounds,
        default_color: process_color,
        custom_glyphs: &[],
    });
    if let Some(buf) = cached_buf(&renderer.overlay_cache, &hsplit_slot) {
        text_areas.push(TextArea {
            buffer: buf,
            left: btn_x_hsplit,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: button_color,
            custom_glyphs: &[],
        });
    }
    if let Some(buf) = cached_buf(&renderer.overlay_cache, &vsplit_slot) {
        text_areas.push(TextArea {
            buffer: buf,
            left: btn_x_vsplit,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: button_color,
            custom_glyphs: &[],
        });
    }
    if let Some(buf) = cached_buf(&renderer.overlay_cache, &close_slot) {
        text_areas.push(TextArea {
            buffer: buf,
            left: btn_x_close,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: close_color,
            custom_glyphs: &[],
        });
    }

    if let Err(e) = renderer.overlay_text_renderer.prepare(
        device,
        queue,
        &mut renderer.font_system,
        &mut renderer.overlay_atlas,
        &renderer.viewport,
        text_areas,
        &mut renderer.swash_cache,
    ) {
        tracing::warn!("header text prepare failed: {}", e);
    }

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("header_text_pass"),
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
            tracing::warn!("header text render failed: {}", e);
        }
    }
}
