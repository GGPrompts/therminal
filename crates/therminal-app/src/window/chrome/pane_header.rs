//! Pane focus borders, split separators, and per-pane header strips.

use std::collections::HashMap;

use glyphon::{Attrs, Color as GlyphColor, Family, Metrics, Resolution, TextArea, TextBounds};
use wgpu::util::DeviceExt;

use crate::grid_renderer::{ColorVertex, GridRenderer};
use crate::pane::{LayoutNode, PaneId, PaneState, SplitDirection};
use therminal_core::palette::Color as PaletteColor;

use super::colors::{
    EXIT_ERROR_COLOR, EXIT_OK_COLOR, FOCUS_BORDER_COLOR, HEADER_BG_COLOR, HEADER_BG_DIM_COLOR,
    HEADER_BUTTON_MARGIN, HEADER_BUTTON_WIDTH, SEPARATOR_COLOR, SEPARATOR_FOCUS_COLOR,
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
    // Slightly stronger border (3px) so focus is unambiguous when per-pane
    // headers are hidden via `general.show_pane_headers = false`.
    let t = 3.0_f32;
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
///
/// `center_title` is an optional string rendered in the center of the
/// header. When `Some`, it replaces the default "pane N" center label
/// (used for Claude session titles / working-dir basenames). The
/// far-left always shows the real daemon `PaneId`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_pane_header(
    pane: &PaneState,
    is_focused: bool,
    is_zoomed: bool,
    center_title: Option<&str>,
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

    // ── Exit-code indicator stripe (left edge) ──
    {
        let exit_code = pane.status.lock().unwrap().last_exit_code;
        if let Some(code) = exit_code {
            let stripe_w = 4.0_f32;
            let stripe_color = if code == 0 {
                EXIT_OK_COLOR
            } else {
                EXIT_ERROR_COLOR
            };
            let stripe_verts =
                pixel_rect_to_ndc(vp.x(), vp.y(), stripe_w, header_h, sw, sh, stripe_color);

            let stripe_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("header_exit_stripe_vbuf"),
                contents: bytemuck::cast_slice(&stripe_verts),
                usage: wgpu::BufferUsages::VERTEX,
            });

            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("header_exit_stripe_pass"),
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
            pass.set_vertex_buffer(0, stripe_buf.slice(..));
            pass.draw(0..6, 0..1);
        }
    }

    // ── Header text ──
    let font_size = (header_h * 0.6).max(9.0);
    let line_height = header_h;
    let metrics = Metrics::new(font_size, line_height);

    let pane_id = pane.id;
    let index_text = format!(" {pane_id}");
    let index_color = GlyphColor::rgba(
        PaletteColor::INK_MUTED.r,
        PaletteColor::INK_MUTED.g,
        PaletteColor::INK_MUTED.b,
        if is_focused { 255 } else { 200 },
    );

    let process_text = match center_title {
        Some(title) => title.to_string(),
        None => format!("pane {pane_id}"),
    };
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

    // tn-166y: tag badges + tn-e97n: git branch
    let (tags, git_state) = {
        let s = pane.status.lock().unwrap();
        (s.tags.clone(), s.git_state.clone())
    };
    let badge_text = format_tag_badges(&tags);
    let git_branch_text = git_state
        .as_ref()
        .map(crate::git_state::format_for_header)
        .unwrap_or_default();
    let git_branch_color = if let Some(ref gs) = git_state {
        if gs.detached {
            GlyphColor::rgba(
                PaletteColor::WARN.r,
                PaletteColor::WARN.g,
                PaletteColor::WARN.b,
                if is_focused { 230 } else { 170 },
            )
        } else if crate::git_state::is_default_branch(&gs.branch) {
            GlyphColor::rgba(
                PaletteColor::INK_MUTED.r,
                PaletteColor::INK_MUTED.g,
                PaletteColor::INK_MUTED.b,
                if is_focused { 220 } else { 160 },
            )
        } else {
            GlyphColor::rgba(
                PaletteColor::FOCUS.r,
                PaletteColor::FOCUS.g,
                PaletteColor::FOCUS.b,
                if is_focused { 230 } else { 170 },
            )
        }
    } else {
        index_color
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

    // Button positions right-to-left: [H] [V] [Z] [X]
    let btn_x_close = vp.x() + vp.width() - HEADER_BUTTON_MARGIN - HEADER_BUTTON_WIDTH;
    let btn_x_zoom = btn_x_close - HEADER_BUTTON_WIDTH;
    let btn_x_vsplit = btn_x_zoom - HEADER_BUTTON_WIDTH;
    let btn_x_hsplit = btn_x_vsplit - HEADER_BUTTON_WIDTH;

    // Zoom glyph: filled square when zoomed (restore), empty square when normal (maximize).
    let zoom_label = if is_zoomed { " \u{25a3}" } else { " \u{25a1}" };

    let focus_tag = if is_focused { "f" } else { "u" };
    let zoom_tag = if is_zoomed { "z" } else { "n" };
    let idx_slot = format!("hdr_idx_{pane_id}");
    let idx_key = format!("{index_text}|{:.0}|{focus_tag}", vp.width());
    let proc_slot = format!("hdr_proc_{pane_id}");
    let proc_key = format!("{process_text}|{:.0}|{focus_tag}", vp.width());
    let close_slot = format!("hdr_close_{pane_id}");
    let close_key = format!("X|{focus_tag}");
    let zoom_slot = format!("hdr_zoom_{pane_id}");
    let zoom_key = format!("{zoom_label}|{focus_tag}|{zoom_tag}");
    let vsplit_slot = format!("hdr_vsplit_{pane_id}");
    let vsplit_key = format!("V|{focus_tag}");
    let hsplit_slot = format!("hdr_hsplit_{pane_id}");
    let hsplit_key = format!("H|{focus_tag}");
    let badge_slot = format!("hdr_badge_{pane_id}");
    let badge_key = format!("{badge_text}|{:.0}|{focus_tag}", vp.width());
    let git_slot = format!("hdr_git_{pane_id}");
    let git_key = format!("{git_branch_text}|{:.0}|{focus_tag}", vp.width());

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
    if !badge_text.is_empty() {
        ensure_shaped(
            &badge_slot,
            &badge_key,
            metrics,
            vp.width(),
            header_h,
            &badge_text,
            Attrs::new()
                .family(Family::Name(&family))
                .color(index_color),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );
    }
    if !git_branch_text.is_empty() {
        ensure_shaped(
            &git_slot,
            &git_key,
            metrics,
            vp.width(),
            header_h,
            &git_branch_text,
            Attrs::new()
                .family(Family::Name(&family))
                .color(git_branch_color),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );
    }
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
        &zoom_slot,
        &zoom_key,
        metrics,
        HEADER_BUTTON_WIDTH,
        header_h,
        zoom_label,
        Attrs::new()
            .family(Family::Name(&family))
            .color(button_color),
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

    let mut text_areas: Vec<TextArea<'_>> = Vec::with_capacity(8);
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
    if !badge_text.is_empty()
        && let Some(badge_buf) = cached_buf(&renderer.overlay_cache, &badge_slot)
    {
        let badge_left = vp.x() + center_offset + process_text_width + 8.0;
        text_areas.push(TextArea {
            buffer: badge_buf,
            left: badge_left,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: index_color,
            custom_glyphs: &[],
        });
    }
    // tn-e97n: git branch, right-aligned before the button cluster.
    if !git_branch_text.is_empty()
        && let Some(git_buf) = cached_buf(&renderer.overlay_cache, &git_slot)
    {
        let git_text_width = git_buf
            .layout_runs()
            .next()
            .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
            .unwrap_or(0.0);
        let git_left = (btn_x_hsplit - git_text_width - 8.0).max(vp.x());
        text_areas.push(TextArea {
            buffer: git_buf,
            left: git_left,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: git_branch_color,
            custom_glyphs: &[],
        });
    }
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
    if let Some(buf) = cached_buf(&renderer.overlay_cache, &zoom_slot) {
        text_areas.push(TextArea {
            buffer: buf,
            left: btn_x_zoom,
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

// ── Tag badge formatting (tn-166y) ────────────────────────────────────

const VALUE_ONLY_KEYS: &[&str] = &["issue_id", "branch", "worker"];
const MAX_VISIBLE_BADGES: usize = 3;

fn format_tag_badges(tags: &HashMap<String, String>) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut keys: Vec<&String> = tags.keys().collect();
    keys.sort();
    let total = keys.len();
    let mut parts: Vec<String> = Vec::with_capacity(MAX_VISIBLE_BADGES + 1);
    for key in keys.iter().take(MAX_VISIBLE_BADGES) {
        let value = &tags[key.as_str()];
        if VALUE_ONLY_KEYS.contains(&key.as_str()) {
            parts.push(value.clone());
        } else {
            parts.push(format!("{key}={value}"));
        }
    }
    if total > MAX_VISIBLE_BADGES {
        parts.push("\u{2026}".to_string());
    }
    parts.join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tag_badges_empty() {
        assert_eq!(format_tag_badges(&HashMap::new()), "");
    }

    #[test]
    fn format_tag_badges_well_known_value_only() {
        let mut t = HashMap::new();
        t.insert("issue_id".into(), "tn-abc".into());
        t.insert("branch".into(), "feat/foo".into());
        let r = format_tag_badges(&t);
        assert!(r.contains("feat/foo") && r.contains("tn-abc"));
        assert!(!r.contains("issue_id=") && !r.contains("branch="));
    }

    #[test]
    fn format_tag_badges_custom_key_value() {
        let mut t = HashMap::new();
        t.insert("env".into(), "prod".into());
        assert_eq!(format_tag_badges(&t), "env=prod");
    }

    #[test]
    fn format_tag_badges_overflow() {
        let mut t = HashMap::new();
        for c in &["a", "b", "c", "d"] {
            t.insert(c.to_string(), "x".into());
        }
        let r = format_tag_badges(&t);
        assert!(r.contains('\u{2026}'));
        assert!(r.contains("a=x") && r.contains("b=x") && r.contains("c=x"));
        assert!(!r.contains("d=x"));
    }

    #[test]
    fn format_tag_badges_deterministic() {
        let mut t = HashMap::new();
        t.insert("worker".into(), "alice".into());
        t.insert("branch".into(), "main".into());
        t.insert("issue_id".into(), "tn-1".into());
        assert_eq!(format_tag_badges(&t), "main  tn-1  alice");
    }
}
