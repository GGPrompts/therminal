//! Chrome rendering: pane headers, status bar, separators, focus borders.
//!
//! All non-terminal-content rendering lives here -- the decorative UI elements
//! that surround the actual grid content.

use wgpu::util::DeviceExt;

use crate::grid_renderer::{ColorVertex, GridRenderer};
use crate::pane::{LayoutNode, PaneId, PaneState, SplitDirection};
use therminal_core::palette::Color as PaletteColor;

// ── Color constants ────────────────────────────────────────────────────

/// Color for focused pane border indicator (FOCUS from Codex 2031 palette).
pub(crate) const FOCUS_BORDER_COLOR: [f32; 4] = {
    let c = PaletteColor::FOCUS;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        0.8,
    ]
};

/// Color for unfocused pane separators (LINE from palette).
const SEPARATOR_COLOR: [f32; 4] = {
    let c = PaletteColor::LINE;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        1.0,
    ]
};

/// Color for separators adjacent to the focused pane (FOCUS from palette).
const SEPARATOR_FOCUS_COLOR: [f32; 4] = {
    let c = PaletteColor::FOCUS;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        0.6,
    ]
};

/// Pane header background color (PLATE from palette).
pub(crate) const HEADER_BG_COLOR: [f32; 4] = {
    let c = PaletteColor::PLATE;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        1.0,
    ]
};

/// Dimmed pane header background for unfocused panes.
pub(crate) const HEADER_BG_DIM_COLOR: [f32; 4] = {
    let c = PaletteColor::PLATE;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        0.6,
    ]
};

/// Width of each header button in pixels.
pub(crate) const HEADER_BUTTON_WIDTH: f32 = 24.0;

/// Right-side margin for header buttons.
pub(crate) const HEADER_BUTTON_MARGIN: f32 = 4.0;

/// Status bar background color (VOID_2 from palette).
const STATUS_BAR_BG_COLOR: [f32; 4] = {
    let c = PaletteColor::VOID_2;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        1.0,
    ]
};

// ── Focus border ───────────────────────────────────────────────────────

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
    let t = 2.0_f32; // border thickness
    let sw = surface_width as f32;
    let sh = surface_height as f32;

    let mut verts: Vec<ColorVertex> = Vec::new();

    // Top edge
    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.x(),
        vp.y(),
        vp.width(),
        t,
        sw,
        sh,
        FOCUS_BORDER_COLOR,
    ));
    // Bottom edge
    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.x(),
        vp.bottom() - t,
        vp.width(),
        t,
        sw,
        sh,
        FOCUS_BORDER_COLOR,
    ));
    // Left edge
    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.x(),
        vp.y(),
        t,
        vp.height(),
        sw,
        sh,
        FOCUS_BORDER_COLOR,
    ));
    // Right edge
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
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });

    pass.set_pipeline(&renderer.rect_pipeline);
    pass.set_vertex_buffer(0, vertex_buf.slice(..));
    pass.draw(0..verts.len() as u32, 0..1);
}

// ── Split separator ────────────────────────────────────────────────────

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

    // Determine if the focused pane is adjacent to this separator.
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
            // Vertical line between left and right.
            let sep_x = f.right();
            let sep_y = f.y().min(s.y());
            let sep_h = f.bottom().max(s.bottom()) - sep_y;
            (sep_x, sep_y, 1.0_f32, sep_h)
        }
        SplitDirection::Vertical => {
            // Horizontal line between top and bottom.
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
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });

    pass.set_pipeline(&renderer.rect_pipeline);
    pass.set_vertex_buffer(0, vertex_buf.slice(..));
    pass.draw(0..6, 0..1);
}

// ── Pane header ────────────────────────────────────────────────────────

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
    use glyphon::{
        Attrs, Buffer, Color as GlyphColor, Family, Metrics, Resolution, Shaping, TextArea,
        TextBounds,
    };

    let vp = pane.viewport;
    let header_h = crate::pane::PANE_HEADER_HEIGHT;
    let sw = surface_width as f32;
    let sh = surface_height as f32;

    // ── Header background rect ──────────────────────────────────────────
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
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&renderer.rect_pipeline);
        pass.set_vertex_buffer(0, vertex_buf.slice(..));
        pass.draw(0..6, 0..1);
    }

    // ── Header text ─────────────────────────────────────────────────────
    let font_size = (header_h * 0.6).max(9.0);
    let line_height = header_h;
    let metrics = Metrics::new(font_size, line_height);

    // Pane index label (left-aligned).
    let index_text = format!(" {}", pane_index + 1);
    let index_color = GlyphColor::rgba(
        PaletteColor::INK_MUTED.r,
        PaletteColor::INK_MUTED.g,
        PaletteColor::INK_MUTED.b,
        if is_focused { 255 } else { 200 },
    );

    let mut index_buf = Buffer::new(&mut renderer.font_system, metrics);
    index_buf.set_size(&mut renderer.font_system, Some(vp.width()), Some(header_h));
    index_buf.set_text(
        &mut renderer.font_system,
        &index_text,
        Attrs::new()
            .family(Family::Name(&renderer.font_config.family))
            .color(index_color),
        Shaping::Basic,
    );
    index_buf.shape_until_scroll(&mut renderer.font_system, false);

    // Process name (center-aligned via offset).
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

    let mut process_buf = Buffer::new(&mut renderer.font_system, metrics);
    process_buf.set_size(&mut renderer.font_system, Some(vp.width()), Some(header_h));
    process_buf.set_text(
        &mut renderer.font_system,
        &process_text,
        Attrs::new()
            .family(Family::Name(&renderer.font_config.family))
            .color(process_color),
        Shaping::Basic,
    );
    process_buf.shape_until_scroll(&mut renderer.font_system, false);

    // Estimate text width for centering.
    let process_text_width = process_buf
        .layout_runs()
        .next()
        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
        .unwrap_or(0.0);
    let center_offset = ((vp.width() - process_text_width) / 2.0).max(0.0);

    // ── Right-aligned header buttons: [H] [V] [X] ─────────────────────
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

    // Button layout from right edge: [X] [V] [H]
    let btn_x_close = vp.x() + vp.width() - HEADER_BUTTON_MARGIN - HEADER_BUTTON_WIDTH;
    let btn_x_vsplit = btn_x_close - HEADER_BUTTON_WIDTH;
    let btn_x_hsplit = btn_x_vsplit - HEADER_BUTTON_WIDTH;

    let mut close_buf = Buffer::new(&mut renderer.font_system, metrics);
    close_buf.set_size(
        &mut renderer.font_system,
        Some(HEADER_BUTTON_WIDTH),
        Some(header_h),
    );
    close_buf.set_text(
        &mut renderer.font_system,
        " X",
        Attrs::new()
            .family(Family::Name(&renderer.font_config.family))
            .color(close_color),
        Shaping::Basic,
    );
    close_buf.shape_until_scroll(&mut renderer.font_system, false);

    let mut vsplit_buf = Buffer::new(&mut renderer.font_system, metrics);
    vsplit_buf.set_size(
        &mut renderer.font_system,
        Some(HEADER_BUTTON_WIDTH),
        Some(header_h),
    );
    vsplit_buf.set_text(
        &mut renderer.font_system,
        " V",
        Attrs::new()
            .family(Family::Name(&renderer.font_config.family))
            .color(button_color),
        Shaping::Basic,
    );
    vsplit_buf.shape_until_scroll(&mut renderer.font_system, false);

    let mut hsplit_buf = Buffer::new(&mut renderer.font_system, metrics);
    hsplit_buf.set_size(
        &mut renderer.font_system,
        Some(HEADER_BUTTON_WIDTH),
        Some(header_h),
    );
    hsplit_buf.set_text(
        &mut renderer.font_system,
        " H",
        Attrs::new()
            .family(Family::Name(&renderer.font_config.family))
            .color(button_color),
        Shaping::Basic,
    );
    hsplit_buf.shape_until_scroll(&mut renderer.font_system, false);

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

    let text_areas = vec![
        TextArea {
            buffer: &index_buf,
            left: vp.x(),
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: index_color,
            custom_glyphs: &[],
        },
        TextArea {
            buffer: &process_buf,
            left: vp.x() + center_offset,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: process_color,
            custom_glyphs: &[],
        },
        TextArea {
            buffer: &hsplit_buf,
            left: btn_x_hsplit,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: button_color,
            custom_glyphs: &[],
        },
        TextArea {
            buffer: &vsplit_buf,
            left: btn_x_vsplit,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: button_color,
            custom_glyphs: &[],
        },
        TextArea {
            buffer: &close_buf,
            left: btn_x_close,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: close_color,
            custom_glyphs: &[],
        },
    ];

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
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
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

// ── Status bar ─────────────────────────────────────────────────────────

/// Draw the window status bar at the bottom of the screen.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_status_bar(
    pane_count: usize,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;
    use glyphon::{
        Attrs, Buffer, Color as GlyphColor, Family, Metrics, Resolution, Shaping, TextArea,
        TextBounds,
    };

    let bar_h = crate::pane::STATUS_BAR_HEIGHT;
    let sw = surface_width as f32;
    let sh = surface_height as f32;
    let bar_y = sh - bar_h;

    // ── Background rect ─────────────────────────────────────────────────
    let bg_verts = pixel_rect_to_ndc(0.0, bar_y, sw, bar_h, sw, sh, STATUS_BAR_BG_COLOR);

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("statusbar_bg_vbuf"),
        contents: bytemuck::cast_slice(&bg_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("statusbar_bg_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&renderer.rect_pipeline);
        pass.set_vertex_buffer(0, vertex_buf.slice(..));
        pass.draw(0..6, 0..1);
    }

    // ── Status bar text ─────────────────────────────────────────────────
    let font_size = (bar_h * 0.55).max(10.0);
    let line_height = bar_h;
    let metrics = Metrics::new(font_size, line_height);

    // Left: "therminal"
    let left_text = " therminal";
    let left_color = GlyphColor::rgba(
        PaletteColor::INK_MUTED.r,
        PaletteColor::INK_MUTED.g,
        PaletteColor::INK_MUTED.b,
        230,
    );

    let mut left_buf = Buffer::new(&mut renderer.font_system, metrics);
    left_buf.set_size(&mut renderer.font_system, Some(sw * 0.4), Some(bar_h));
    left_buf.set_text(
        &mut renderer.font_system,
        left_text,
        Attrs::new()
            .family(Family::Name(&renderer.font_config.family))
            .color(left_color),
        Shaping::Basic,
    );
    left_buf.shape_until_scroll(&mut renderer.font_system, false);

    // Center: pane count
    let center_text = if pane_count == 1 {
        "1 pane".to_string()
    } else {
        format!("{pane_count} panes")
    };
    let center_color = GlyphColor::rgba(
        PaletteColor::INK.r,
        PaletteColor::INK.g,
        PaletteColor::INK.b,
        200,
    );

    let mut center_buf = Buffer::new(&mut renderer.font_system, metrics);
    center_buf.set_size(&mut renderer.font_system, Some(sw), Some(bar_h));
    center_buf.set_text(
        &mut renderer.font_system,
        &center_text,
        Attrs::new()
            .family(Family::Name(&renderer.font_config.family))
            .color(center_color),
        Shaping::Basic,
    );
    center_buf.shape_until_scroll(&mut renderer.font_system, false);

    let center_text_width = center_buf
        .layout_runs()
        .next()
        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
        .unwrap_or(0.0);
    let center_offset = ((sw - center_text_width) / 2.0).max(0.0);

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    let text_areas = vec![
        TextArea {
            buffer: &left_buf,
            left: 0.0,
            top: bar_y,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: 0,
                right: surface_width as i32,
                bottom: surface_height as i32,
            },
            default_color: left_color,
            custom_glyphs: &[],
        },
        TextArea {
            buffer: &center_buf,
            left: center_offset,
            top: bar_y,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: 0,
                right: surface_width as i32,
                bottom: surface_height as i32,
            },
            default_color: center_color,
            custom_glyphs: &[],
        },
    ];

    if let Err(e) = renderer.overlay_text_renderer.prepare(
        device,
        queue,
        &mut renderer.font_system,
        &mut renderer.overlay_atlas,
        &renderer.viewport,
        text_areas,
        &mut renderer.swash_cache,
    ) {
        tracing::warn!("status bar text prepare failed: {}", e);
    }

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("statusbar_text_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        if let Err(e) = renderer.overlay_text_renderer.render(
            &renderer.overlay_atlas,
            &renderer.viewport,
            &mut pass,
        ) {
            tracing::warn!("status bar text render failed: {}", e);
        }
    }
}
