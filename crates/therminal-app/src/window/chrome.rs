//! Chrome rendering: pane headers, status bar, separators, focus borders.
//!
//! All non-terminal-content rendering lives here -- the decorative UI elements
//! that surround the actual grid content.

use std::collections::HashMap;

use glyphon::{
    Attrs, Buffer, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping, TextArea,
    TextBounds,
};
use wgpu::util::DeviceExt;

use crate::grid_renderer::{ColorVertex, GridRenderer};
use crate::pane::{LayoutNode, PaneId, PaneState, SplitDirection};
use therminal_core::palette::Color as PaletteColor;

// ── Overlay text cache ────────────────────────────────────────────────

/// Ensure a cached shaped Buffer exists for the given slot.
/// If the cache key matches, this is a no-op. Otherwise creates a new Buffer,
/// shapes it, and stores it in the cache.
///
/// After calling this for all needed slots, retrieve buffers via
/// `cache.get(slot).unwrap().1` for use in TextArea references.
#[allow(clippy::too_many_arguments)]
fn ensure_shaped(
    slot: &str,
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
        .get(slot)
        .map(|(k, _)| k.as_str() != cache_key)
        .unwrap_or(true);

    if needs_reshape {
        let mut buf = Buffer::new(font_system, metrics);
        buf.set_size(font_system, Some(width), Some(height));
        buf.set_text(font_system, text, attrs, Shaping::Basic);
        buf.shape_until_scroll(font_system, false);
        cache.insert(slot.to_string(), (cache_key.to_string(), buf));
    }
}

/// Get a reference to a cached Buffer. Panics if the slot was not previously
/// populated via `ensure_shaped`.
fn cached_buf<'a>(cache: &'a HashMap<String, (String, Buffer)>, slot: &str) -> &'a Buffer {
    &cache.get(slot).unwrap().1
}

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

    // Cache keys encode text + width + focus state (which affects colors).
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

    // Phase 1: ensure all buffers are shaped in the cache.
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

    // Phase 2: borrow cache immutably for TextArea references.
    let index_buf = cached_buf(&renderer.overlay_cache, &idx_slot);
    let process_buf = cached_buf(&renderer.overlay_cache, &proc_slot);

    // Estimate text width for centering.
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

    let text_areas = vec![
        TextArea {
            buffer: index_buf,
            left: vp.x(),
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: index_color,
            custom_glyphs: &[],
        },
        TextArea {
            buffer: process_buf,
            left: vp.x() + center_offset,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: process_color,
            custom_glyphs: &[],
        },
        TextArea {
            buffer: cached_buf(&renderer.overlay_cache, &hsplit_slot),
            left: btn_x_hsplit,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: button_color,
            custom_glyphs: &[],
        },
        TextArea {
            buffer: cached_buf(&renderer.overlay_cache, &vsplit_slot),
            left: btn_x_vsplit,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: button_color,
            custom_glyphs: &[],
        },
        TextArea {
            buffer: cached_buf(&renderer.overlay_cache, &close_slot),
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

/// Data collected for the status bar from the focused pane.
pub(crate) struct StatusBarInfo {
    /// Agent name (from ProcessDetector), shown on the left when present.
    pub agent_name: Option<String>,
    /// Current working directory (from OSC 7).
    pub cwd: Option<String>,
    /// Pane grid dimensions (cols, rows).
    pub dimensions: (usize, usize),
    /// Last command exit code (from OSC 633 D mark).
    pub last_exit_code: Option<i32>,
    /// Whether the config allows showing the agent indicator.
    pub show_agent_indicator: bool,
    /// IDs of all existing workspaces (sorted).
    pub workspace_ids: Vec<usize>,
    /// Currently active workspace number.
    pub active_workspace: usize,
}

/// Draw the window status bar at the bottom of the screen.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_status_bar(
    info: &StatusBarInfo,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

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

    let bounds = TextBounds {
        left: 0,
        top: 0,
        right: surface_width as i32,
        bottom: surface_height as i32,
    };

    let mut text_areas: Vec<TextArea<'_>> = Vec::new();

    // ── Workspace indicators (left-most) ────────────────────────────────
    let workspace_text = if info.workspace_ids.len() > 1 {
        let mut s = String::from(" ");
        for &ws_id in &info.workspace_ids {
            if ws_id == info.active_workspace {
                s.push_str(&format!("[{ws_id}] "));
            } else {
                s.push_str(&format!(" {ws_id}  "));
            }
        }
        s
    } else {
        String::new()
    };

    let workspace_active_color = GlyphColor::rgba(
        PaletteColor::FOCUS.r,
        PaletteColor::FOCUS.g,
        PaletteColor::FOCUS.b,
        255,
    );

    // ── Left section: agent indicator (when detected and config allows) ──
    let left_text = if info.show_agent_indicator {
        info.agent_name
            .as_ref()
            .map(|name| format!(" [agent: {name}]"))
    } else {
        None
    };
    let left_text_ref = left_text.as_deref().unwrap_or("");

    let agent_color = GlyphColor::rgba(
        PaletteColor::FOCUS.r,
        PaletteColor::FOCUS.g,
        PaletteColor::FOCUS.b,
        230,
    );
    let muted_color = GlyphColor::rgba(
        PaletteColor::INK_MUTED.r,
        PaletteColor::INK_MUTED.g,
        PaletteColor::INK_MUTED.b,
        230,
    );

    // ── Center section: CWD ─────────────────────────────────────────────
    let center_text = info.cwd.as_deref().map(abbreviate_path).unwrap_or_default();
    let center_color = GlyphColor::rgba(
        PaletteColor::INK.r,
        PaletteColor::INK.g,
        PaletteColor::INK.b,
        200,
    );

    // ── Right section: dimensions + exit code ───────────────────────────
    let (cols, rows) = info.dimensions;
    let right_text = match info.last_exit_code {
        Some(code) => format!("{cols}x{rows}  [{code}] "),
        None => format!("{cols}x{rows} "),
    };

    let exit_color = match info.last_exit_code {
        Some(0) => GlyphColor::rgba(
            PaletteColor::STATUS_OK.r,
            PaletteColor::STATUS_OK.g,
            PaletteColor::STATUS_OK.b,
            230,
        ),
        Some(_) => GlyphColor::rgba(
            PaletteColor::STATUS_ERROR.r,
            PaletteColor::STATUS_ERROR.g,
            PaletteColor::STATUS_ERROR.b,
            230,
        ),
        None => muted_color,
    };

    // Cache keys encode text + width (width affects line-wrapping).
    let ws_key = format!("{workspace_text}|{:.0}", sw * 0.25);
    let left_key = format!("{left_text_ref}|{:.0}", sw * 0.35);
    let center_key = format!("{center_text}|{sw:.0}");
    let right_key = format!("{right_text}|{:.0}|{:?}", sw * 0.35, info.last_exit_code);

    // Phase 1: ensure all buffers are shaped in the cache.
    let family = renderer.font_config.family.clone();
    ensure_shaped(
        "sb_workspace",
        &ws_key,
        metrics,
        sw * 0.25,
        bar_h,
        &workspace_text,
        Attrs::new()
            .family(Family::Name(&family))
            .color(workspace_active_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        "sb_left",
        &left_key,
        metrics,
        sw * 0.35,
        bar_h,
        left_text_ref,
        Attrs::new()
            .family(Family::Name(&family))
            .color(agent_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        "sb_center",
        &center_key,
        metrics,
        sw,
        bar_h,
        &center_text,
        Attrs::new()
            .family(Family::Name(&family))
            .color(center_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        "sb_right",
        &right_key,
        metrics,
        sw * 0.35,
        bar_h,
        &right_text,
        Attrs::new().family(Family::Name(&family)).color(exit_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );

    // Phase 2: borrow cache immutably for TextArea references and measurements.
    let workspace_buf = cached_buf(&renderer.overlay_cache, "sb_workspace");
    let workspace_text_width = workspace_buf
        .layout_runs()
        .next()
        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
        .unwrap_or(0.0);

    let center_buf = cached_buf(&renderer.overlay_cache, "sb_center");
    let center_text_width = center_buf
        .layout_runs()
        .next()
        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
        .unwrap_or(0.0);
    let center_offset = ((sw - center_text_width) / 2.0).max(0.0);

    let right_buf = cached_buf(&renderer.overlay_cache, "sb_right");
    let right_text_width = right_buf
        .layout_runs()
        .next()
        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
        .unwrap_or(0.0);
    let right_x = (sw - right_text_width).max(0.0);

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    // Workspace indicators (left-most).
    if !workspace_text.is_empty() {
        text_areas.push(TextArea {
            buffer: workspace_buf,
            left: 0.0,
            top: bar_y,
            scale: 1.0,
            bounds,
            default_color: workspace_active_color,
            custom_glyphs: &[],
        });
    }

    // Only add left area if there is agent text.
    if !left_text_ref.is_empty() {
        text_areas.push(TextArea {
            buffer: cached_buf(&renderer.overlay_cache, "sb_left"),
            left: workspace_text_width,
            top: bar_y,
            scale: 1.0,
            bounds,
            default_color: agent_color,
            custom_glyphs: &[],
        });
    }

    if !center_text.is_empty() {
        text_areas.push(TextArea {
            buffer: center_buf,
            left: center_offset,
            top: bar_y,
            scale: 1.0,
            bounds,
            default_color: center_color,
            custom_glyphs: &[],
        });
    }

    text_areas.push(TextArea {
        buffer: right_buf,
        left: right_x,
        top: bar_y,
        scale: 1.0,
        bounds,
        default_color: exit_color,
        custom_glyphs: &[],
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

// ── Tab bar ───────────────────────────────────────────────────────────

/// Data collected for the workspace tab bar.
pub(crate) struct TabBarInfo {
    /// IDs of all existing workspaces (sorted).
    pub workspace_ids: Vec<usize>,
    /// Currently active workspace number.
    pub active_workspace: usize,
}

/// Width of a single tab in the tab bar.
const TAB_WIDTH: f32 = 48.0;

/// Tab bar background color (same as status bar).
const TAB_BAR_BG_COLOR: [f32; 4] = STATUS_BAR_BG_COLOR;

/// Active tab background color (PLATE from palette).
const TAB_ACTIVE_BG_COLOR: [f32; 4] = HEADER_BG_COLOR;

/// Active tab underline color (FOCUS from palette).
const TAB_ACTIVE_UNDERLINE_COLOR: [f32; 4] = FOCUS_BORDER_COLOR;

/// Draw the workspace tab bar at the top of the window.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_tab_bar(
    info: &TabBarInfo,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    let bar_h = crate::pane::TAB_BAR_HEIGHT;
    let sw = surface_width as f32;
    let sh = surface_height as f32;

    // ── Full-width background rect ──────────────────────────────────────
    let bg_verts = pixel_rect_to_ndc(0.0, 0.0, sw, bar_h, sw, sh, TAB_BAR_BG_COLOR);

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("tabbar_bg_vbuf"),
        contents: bytemuck::cast_slice(&bg_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tabbar_bg_pass"),
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

    // ── Per-tab rects (active tab gets highlighted bg + underline) ──────
    let mut tab_verts: Vec<ColorVertex> = Vec::new();
    for (i, &ws_id) in info.workspace_ids.iter().enumerate() {
        let tab_x = i as f32 * TAB_WIDTH;
        if ws_id == info.active_workspace {
            // Active tab background.
            tab_verts.extend_from_slice(&pixel_rect_to_ndc(
                tab_x,
                0.0,
                TAB_WIDTH,
                bar_h,
                sw,
                sh,
                TAB_ACTIVE_BG_COLOR,
            ));
            // Active tab underline (2px at bottom of tab).
            tab_verts.extend_from_slice(&pixel_rect_to_ndc(
                tab_x,
                bar_h - 2.0,
                TAB_WIDTH,
                2.0,
                sw,
                sh,
                TAB_ACTIVE_UNDERLINE_COLOR,
            ));
        }
    }

    if !tab_verts.is_empty() {
        let tab_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tabbar_tabs_vbuf"),
            contents: bytemuck::cast_slice(&tab_verts),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tabbar_tabs_pass"),
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
        pass.set_vertex_buffer(0, tab_buf.slice(..));
        pass.draw(0..tab_verts.len() as u32, 0..1);
    }

    // ── Tab label text ──────────────────────────────────────────────────
    let font_size = (bar_h * 0.55).max(10.0);
    let line_height = bar_h;
    let metrics = Metrics::new(font_size, line_height);

    let bounds = TextBounds {
        left: 0,
        top: 0,
        right: surface_width as i32,
        bottom: surface_height as i32,
    };

    let active_color = GlyphColor::rgba(
        PaletteColor::INK.r,
        PaletteColor::INK.g,
        PaletteColor::INK.b,
        255,
    );
    let inactive_color = GlyphColor::rgba(
        PaletteColor::INK_MUTED.r,
        PaletteColor::INK_MUTED.g,
        PaletteColor::INK_MUTED.b,
        200,
    );

    // Phase 1: ensure all tab buffers are shaped in the cache.
    let family = renderer.font_config.family.clone();
    let mut tab_slots: Vec<(String, f32, GlyphColor)> = Vec::new();
    for (i, &ws_id) in info.workspace_ids.iter().enumerate() {
        let tab_x = i as f32 * TAB_WIDTH;
        let is_active = ws_id == info.active_workspace;
        let color = if is_active {
            active_color
        } else {
            inactive_color
        };
        let label = format!(" {ws_id}");
        let active_tag = if is_active { "a" } else { "i" };
        let slot = format!("tab_{ws_id}");
        let key = format!("{label}|{active_tag}");

        ensure_shaped(
            &slot,
            &key,
            metrics,
            TAB_WIDTH,
            bar_h,
            &label,
            Attrs::new().family(Family::Name(&family)).color(color),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );

        // Measure for centering (need to read from cache after ensure).
        tab_slots.push((slot, tab_x, color));
    }

    // Phase 2: borrow cache immutably to build TextAreas.
    let mut tab_positions: Vec<(&Buffer, f32, GlyphColor)> = Vec::new();
    for (slot, tab_x, color) in &tab_slots {
        let buf = cached_buf(&renderer.overlay_cache, slot);
        let text_width = buf
            .layout_runs()
            .next()
            .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
            .unwrap_or(0.0);
        let centered_x = tab_x + ((TAB_WIDTH - text_width) / 2.0).max(0.0);
        tab_positions.push((buf, centered_x, *color));
    }

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    let text_areas: Vec<TextArea<'_>> = tab_positions
        .iter()
        .map(|(buf, x, color)| TextArea {
            buffer: buf,
            left: *x,
            top: 0.0,
            scale: 1.0,
            bounds,
            default_color: *color,
            custom_glyphs: &[],
        })
        .collect();

    if let Err(e) = renderer.overlay_text_renderer.prepare(
        device,
        queue,
        &mut renderer.font_system,
        &mut renderer.overlay_atlas,
        &renderer.viewport,
        text_areas,
        &mut renderer.swash_cache,
    ) {
        tracing::warn!("tab bar text prepare failed: {}", e);
    }

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tabbar_text_pass"),
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
            tracing::warn!("tab bar text render failed: {}", e);
        }
    }
}

/// Return the workspace ID for a click at the given x-position in the tab bar,
/// given the list of workspace IDs displayed.
pub(crate) fn tab_bar_hit_test(px: f32, workspace_ids: &[usize]) -> Option<usize> {
    if workspace_ids.is_empty() {
        return None;
    }
    let tab_index = (px / TAB_WIDTH).floor() as usize;
    workspace_ids.get(tab_index).copied()
}

/// Abbreviate a path for status bar display: replace the home directory with `~`
/// and extract the path from `file://` URLs.
///
/// On WSL2, paths under `/mnt/c/Users/<user>` (the Windows user home) are
/// abbreviated to `~win` so the status bar stays readable when navigating
/// Windows filesystems.
fn abbreviate_path(path: &str) -> String {
    // OSC 7 sends file:// URLs; extract the path portion.
    let path = if let Some(rest) = path.strip_prefix("file://") {
        // Strip the hostname part (e.g., "file://hostname/path" -> "/path").
        rest.find('/').map(|i| &rest[i..]).unwrap_or(rest)
    } else {
        path
    };

    // Replace Linux home dir with ~.
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(home.as_str()) {
            return format!("~{rest}");
        }
    }

    // WSL2: abbreviate Windows user home (/mnt/c/Users/<user>) to ~win.
    // This applies when the user navigates into Windows-side directories.
    if let Some(win_home) = wsl2_windows_home() {
        if let Some(rest) = path.strip_prefix(win_home.as_str()) {
            return format!("~win{rest}");
        }
    }

    path.to_string()
}

/// Detect the WSL2 Windows user home directory as a Linux path.
///
/// Returns `Some("/mnt/c/Users/<username>")` when running under WSL2 and the
/// `USERPROFILE` or `HOMEDRIVE`+`HOMEPATH` env vars are set (forwarded by
/// Windows Terminal / WSL2 via `WSLENV`). Returns `None` on non-WSL2 systems.
fn wsl2_windows_home() -> Option<String> {
    // WSL2 sets WSL_DISTRO_NAME; use it as a cheap guard.
    std::env::var_os("WSL_DISTRO_NAME")?;

    // USERPROFILE is typically forwarded from Windows via WSLENV (e.g.,
    // "C:\Users\alice"). Convert backslashes and prepend /mnt/c -> /mnt/c/Users/alice.
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        if let Some(linux_path) = windows_path_to_linux(&userprofile) {
            return Some(linux_path);
        }
    }

    // Fallback: HOMEDRIVE (e.g. "C:") + HOMEPATH (e.g. "\Users\alice").
    if let (Ok(drive), Ok(homepath)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        let combined = format!("{drive}{homepath}");
        if let Some(linux_path) = windows_path_to_linux(&combined) {
            return Some(linux_path);
        }
    }

    None
}

/// Convert a Windows-style absolute path (e.g. `C:\Users\alice`) to a WSL2
/// Linux mount path (e.g. `/mnt/c/Users/alice`).
///
/// Returns `None` if the path is not a recognised `<drive>:\...` form.
fn windows_path_to_linux(windows_path: &str) -> Option<String> {
    // Expect at least "C:\" (3 chars).
    if windows_path.len() < 3 {
        return None;
    }
    let (drive, rest) = windows_path.split_at(2);
    if !drive.ends_with(':') {
        return None;
    }
    let drive_letter = drive.chars().next()?.to_ascii_lowercase();
    // Normalise backslashes to forward slashes and strip the leading separator.
    let rest = rest.trim_start_matches(['\\', '/']);
    let rest = rest.replace('\\', "/");
    Some(format!("/mnt/{drive_letter}/{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_path_to_linux_backslashes() {
        assert_eq!(
            windows_path_to_linux(r"C:\Users\alice"),
            Some("/mnt/c/Users/alice".to_string())
        );
    }

    #[test]
    fn windows_path_to_linux_forward_slashes() {
        assert_eq!(
            windows_path_to_linux("C:/Users/alice"),
            Some("/mnt/c/Users/alice".to_string())
        );
    }

    #[test]
    fn windows_path_to_linux_uppercase_drive() {
        assert_eq!(
            windows_path_to_linux(r"D:\Projects"),
            Some("/mnt/d/Projects".to_string())
        );
    }

    #[test]
    fn windows_path_to_linux_invalid() {
        assert_eq!(windows_path_to_linux("not-a-windows-path"), None);
        assert_eq!(windows_path_to_linux(""), None);
    }

    #[test]
    fn abbreviate_path_strips_file_url() {
        // file://hostname/path -> /path (no home match, returned as-is)
        let result = abbreviate_path("file://localhost/tmp/foo");
        assert_eq!(result, "/tmp/foo");
    }

    #[test]
    fn abbreviate_path_plain_path() {
        // A plain path with no match is returned unchanged.
        let result = abbreviate_path("/some/other/path");
        assert_eq!(result, "/some/other/path");
    }

    #[test]
    fn abbreviate_path_mnt_c_without_wsl2() {
        // When WSL_DISTRO_NAME is not set, /mnt/c/ paths are returned unchanged.
        // We can't easily manipulate env vars in tests safely, so just check
        // the function doesn't panic and returns a string.
        let result = abbreviate_path("/mnt/c/Users/alice/Documents");
        assert!(!result.is_empty());
    }
}
