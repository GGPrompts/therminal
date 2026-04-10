//! tiny-skia pixmap rasterization for overlay widgets (tn-npd).
//!
//! This module is intentionally small: it takes a `WidgetSpec` (data
//! describing what to draw) and produces a `tiny_skia::Pixmap`. The
//! pixmap is then uploaded to the GPU by `gpu::WidgetRenderer`. The
//! rasterizer itself holds no GPU state, no tiny-skia statefuls — it
//! exists so that consumers have one pinned drawing API instead of
//! sprinkling path building throughout the app.

use tiny_skia::{
    Color, FillRule, Paint, PathBuilder, Pixmap, Rect as SkRect, Shader, Stroke, Transform,
};

/// A widget drawing specification + freshness hash.
///
/// `data_hash` identifies the data state this pixmap represents — the
/// `WidgetManager` only re-rasterizes when the incoming hash differs
/// from the cached hash for the same `WidgetId`. Producers own the
/// hash policy (see `AgentBadgeSource::snapshot`).
#[derive(Clone, Debug)]
pub struct WidgetSpec {
    /// Hash of the data that produced this spec. Used by `WidgetManager`
    /// to skip re-rasterization when nothing changed.
    pub data_hash: u64,
    /// The specific widget shape to draw.
    pub kind: WidgetKind,
}

/// Supported widget shape kinds.
///
/// v1 ships `Pill` (agent status badge) and `TimelineBar` (agent timeline,
/// tn-x85k). Follow-up issues will add more kinds (context gauge, thinking
/// indicator, tool call card) as they need them.
#[derive(Clone, Debug)]
pub enum WidgetKind {
    /// A rounded-pill background with an optional leading status dot.
    Pill(PillSpec),
    /// A horizontal bar of colored rectangles representing tool activity
    /// over time. Each segment is one tool invocation; the second row
    /// (if present) holds subagent entries.
    TimelineBar(TimelineBarSpec),
}

/// A rounded-rectangle "pill" widget with an optional status dot.
///
/// Pixel dimensions include padding. The pill is drawn inside
/// `(0, 0, width, height)` — the consumer positions it on screen via
/// `WidgetRenderer::draw`. Text labels are drawn separately by the
/// existing glyphon overlay text renderer; this keeps the rasterizer
/// free of text deps (tn-npd scope boundary).
#[derive(Clone, Debug)]
pub struct PillSpec {
    /// Full pixel width of the pill (including padding + dot area).
    pub width: u32,
    /// Full pixel height of the pill.
    pub height: u32,
    /// Corner radius in pixels.
    pub corner_radius: f32,
    /// Pill background RGBA (0..=1).
    pub background: [f32; 4],
    /// Optional border RGBA (0..=1) + border width in px.
    pub border: Option<([f32; 4], f32)>,
    /// Optional status dot RGBA (0..=1). Drawn as a filled circle near
    /// the leading edge of the pill; the radius is derived from the
    /// pill's height.
    pub dot: Option<[f32; 4]>,
}

impl PillSpec {
    /// Recommended pixel position (inside the pixmap) where a caller
    /// should place the text baseline, given the dot presence. Keeps
    /// text placement logic in the rasterizer module so consumers don't
    /// have to re-derive magic numbers.
    pub fn text_origin_px(&self) -> (f32, f32) {
        let dot_space = if self.dot.is_some() {
            // Dot + gap: dot radius is height * 0.2, gap is height * 0.18.
            (self.height as f32) * (0.2 * 2.0 + 0.18)
        } else {
            0.0
        };
        let pad_x = (self.height as f32) * 0.45;
        (pad_x + dot_space, (self.height as f32) * 0.25)
    }
}

/// A single colored segment within a `TimelineBar`.
#[derive(Clone, Debug)]
pub struct TimelineSegment {
    /// RGBA color for this segment (0..=1).
    pub color: [f32; 4],
    /// Whether this segment belongs to a subagent (drawn on the second row).
    pub is_subagent: bool,
}

/// A horizontal bar of colored segments representing tool activity (tn-x85k).
///
/// The bar is drawn as a rounded rectangle background with colored segments
/// inside. Top-level entries fill the top half (or full height if no
/// subagent entries exist); subagent entries fill the bottom half.
#[derive(Clone, Debug)]
pub struct TimelineBarSpec {
    /// Total pixel width of the bar.
    pub width: u32,
    /// Total pixel height of the bar.
    pub height: u32,
    /// Corner radius for the background pill.
    pub corner_radius: f32,
    /// Background RGBA (0..=1).
    pub background: [f32; 4],
    /// Ordered segments to draw left-to-right.
    pub segments: Vec<TimelineSegment>,
}

/// Rasterizer: builds a tiny-skia `Pixmap` from a `WidgetSpec`.
///
/// Holds no state today; the type exists so we can add (a) a scratch
/// pixmap pool for reuse across frames and (b) a path-builder reuse
/// buffer later without changing consumers.
#[derive(Default)]
pub struct WidgetRasterizer;

impl WidgetRasterizer {
    /// Create a new rasterizer.
    pub fn new() -> Self {
        Self
    }

    /// Rasterize the given spec into a fresh `Pixmap`.
    ///
    /// Returns `None` if the spec requests a zero-sized or otherwise
    /// invalid pixmap. Callers should treat `None` as "skip this frame"
    /// — a log line at debug level is emitted for visibility.
    pub fn rasterize_to_pixmap(&mut self, spec: &WidgetSpec) -> Option<Pixmap> {
        match &spec.kind {
            WidgetKind::Pill(pill) => rasterize_pill(pill),
            WidgetKind::TimelineBar(bar) => rasterize_timeline_bar(bar),
        }
    }
}

/// Convert an `f32` color in 0..=1 to tiny-skia `Color`.
fn ts_color(c: [f32; 4]) -> Color {
    Color::from_rgba(
        c[0].clamp(0.0, 1.0),
        c[1].clamp(0.0, 1.0),
        c[2].clamp(0.0, 1.0),
        c[3].clamp(0.0, 1.0),
    )
    .unwrap_or(Color::TRANSPARENT)
}

/// Build the rounded rectangle path for a pill.
fn build_pill_path(width: f32, height: f32, radius: f32) -> Option<tiny_skia::Path> {
    let r = radius.min(width * 0.5).min(height * 0.5).max(0.0);
    let mut pb = PathBuilder::new();
    // Start at top-left straight edge start.
    pb.move_to(r, 0.0);
    pb.line_to(width - r, 0.0);
    // Top-right corner.
    pb.quad_to(width, 0.0, width, r);
    pb.line_to(width, height - r);
    // Bottom-right corner.
    pb.quad_to(width, height, width - r, height);
    pb.line_to(r, height);
    // Bottom-left corner.
    pb.quad_to(0.0, height, 0.0, height - r);
    pb.line_to(0.0, r);
    // Top-left corner.
    pb.quad_to(0.0, 0.0, r, 0.0);
    pb.close();
    pb.finish()
}

fn rasterize_pill(pill: &PillSpec) -> Option<Pixmap> {
    if pill.width == 0 || pill.height == 0 {
        tracing::debug!(
            w = pill.width,
            h = pill.height,
            "rasterize_pill: zero-sized pixmap requested; skipping"
        );
        return None;
    }
    let mut pixmap = Pixmap::new(pill.width, pill.height)?;
    // Fully transparent background — pill path fills on top.
    pixmap.fill(Color::TRANSPARENT);

    let w = pill.width as f32;
    let h = pill.height as f32;

    // ── Fill the pill ────────────────────────────────────────────────
    let path = build_pill_path(w, h, pill.corner_radius)?;
    let paint = Paint {
        shader: Shader::SolidColor(ts_color(pill.background)),
        anti_alias: true,
        ..Paint::default()
    };
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    // ── Optional border ──────────────────────────────────────────────
    if let Some((border_rgba, border_w)) = pill.border {
        let border_paint = Paint {
            shader: Shader::SolidColor(ts_color(border_rgba)),
            anti_alias: true,
            ..Paint::default()
        };
        let stroke = Stroke {
            width: border_w,
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &border_paint, &stroke, Transform::identity(), None);
    }

    // ── Optional status dot ──────────────────────────────────────────
    if let Some(dot_rgba) = pill.dot {
        let dot_r = h * 0.2;
        let dot_cx = h * 0.45 + dot_r;
        let dot_cy = h * 0.5;
        let dot_rect = SkRect::from_xywh(dot_cx - dot_r, dot_cy - dot_r, dot_r * 2.0, dot_r * 2.0)?;
        let mut dot_pb = PathBuilder::new();
        dot_pb.push_circle(dot_rect.x() + dot_r, dot_rect.y() + dot_r, dot_r);
        let dot_path = dot_pb.finish()?;
        let dot_paint = Paint {
            shader: Shader::SolidColor(ts_color(dot_rgba)),
            anti_alias: true,
            ..Paint::default()
        };
        pixmap.fill_path(
            &dot_path,
            &dot_paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    Some(pixmap)
}

fn rasterize_timeline_bar(bar: &TimelineBarSpec) -> Option<Pixmap> {
    if bar.width == 0 || bar.height == 0 {
        tracing::debug!(
            w = bar.width,
            h = bar.height,
            "rasterize_timeline_bar: zero-sized pixmap requested; skipping"
        );
        return None;
    }
    let mut pixmap = Pixmap::new(bar.width, bar.height)?;
    pixmap.fill(Color::TRANSPARENT);

    let w = bar.width as f32;
    let h = bar.height as f32;

    // ── Fill the background pill ─────────────────────────────────────
    let path = build_pill_path(w, h, bar.corner_radius)?;
    let bg_paint = Paint {
        shader: Shader::SolidColor(ts_color(bar.background)),
        anti_alias: true,
        ..Paint::default()
    };
    pixmap.fill_path(
        &path,
        &bg_paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    if bar.segments.is_empty() {
        return Some(pixmap);
    }

    // Check whether any subagent segments exist to decide row layout.
    let has_subagent = bar.segments.iter().any(|s| s.is_subagent);
    let top_row_h = if has_subagent { h * 0.5 } else { h };
    let bottom_row_y = if has_subagent { h * 0.5 } else { 0.0 };
    let bottom_row_h = if has_subagent { h * 0.5 } else { h };

    // Count top-level and subagent segments separately.
    let top_count = bar.segments.iter().filter(|s| !s.is_subagent).count();
    let sub_count = bar.segments.iter().filter(|s| s.is_subagent).count();

    // Inset: leave a small margin inside the pill.
    let inset = bar.corner_radius.min(4.0);
    let inner_w = (w - 2.0 * inset).max(0.0);
    let gap = 1.0_f32; // 1px gap between segments.

    // Draw top-level segments.
    if top_count > 0 {
        let seg_w =
            ((inner_w - gap * (top_count as f32 - 1.0).max(0.0)) / top_count as f32).max(1.0);
        let mut x = inset;
        for seg in bar.segments.iter().filter(|s| !s.is_subagent) {
            let rect =
                SkRect::from_xywh(x, inset.min(2.0), seg_w, top_row_h - inset.min(2.0) * 2.0);
            if let Some(rect) = rect {
                let paint = Paint {
                    shader: Shader::SolidColor(ts_color(seg.color)),
                    anti_alias: false,
                    ..Paint::default()
                };
                pixmap.fill_rect(rect, &paint, Transform::identity(), None);
            }
            x += seg_w + gap;
        }
    }

    // Draw subagent segments on the bottom row.
    if sub_count > 0 {
        let seg_w =
            ((inner_w - gap * (sub_count as f32 - 1.0).max(0.0)) / sub_count as f32).max(1.0);
        let mut x = inset;
        for seg in bar.segments.iter().filter(|s| s.is_subagent) {
            let rect = SkRect::from_xywh(
                x,
                bottom_row_y + inset.min(2.0),
                seg_w,
                bottom_row_h - inset.min(2.0) * 2.0,
            );
            if let Some(rect) = rect {
                // Subagent segments are slightly dimmed (alpha * 0.7) to
                // visually distinguish them from top-level entries.
                let mut color = seg.color;
                color[3] *= 0.7;
                let paint = Paint {
                    shader: Shader::SolidColor(ts_color(color)),
                    anti_alias: false,
                    ..Paint::default()
                };
                pixmap.fill_rect(rect, &paint, Transform::identity(), None);
            }
            x += seg_w + gap;
        }
    }

    Some(pixmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pill() -> PillSpec {
        PillSpec {
            width: 160,
            height: 28,
            corner_radius: 14.0,
            background: [0.1, 0.2, 0.3, 0.85],
            border: Some(([0.3, 0.6, 0.9, 1.0], 1.5)),
            dot: Some([0.2, 0.8, 0.4, 1.0]),
        }
    }

    #[test]
    fn rasterize_produces_nonempty_pixmap() {
        let mut r = WidgetRasterizer::new();
        let spec = WidgetSpec {
            data_hash: 42,
            kind: WidgetKind::Pill(sample_pill()),
        };
        let pm = r.rasterize_to_pixmap(&spec).expect("pixmap");
        assert_eq!(pm.width(), 160);
        assert_eq!(pm.height(), 28);
        // Background filled somewhere — at least one pixel should have
        // non-zero alpha after the pill fill.
        let any_opaque = pm.pixels().iter().any(|p| p.alpha() > 0);
        assert!(any_opaque, "pixmap must contain at least one opaque pixel");
    }

    #[test]
    fn rasterize_zero_size_returns_none() {
        let mut r = WidgetRasterizer::new();
        let spec = WidgetSpec {
            data_hash: 0,
            kind: WidgetKind::Pill(PillSpec {
                width: 0,
                height: 28,
                corner_radius: 4.0,
                background: [0.0; 4],
                border: None,
                dot: None,
            }),
        };
        assert!(r.rasterize_to_pixmap(&spec).is_none());
    }

    #[test]
    fn rasterize_no_dot_still_produces_pixmap() {
        let mut r = WidgetRasterizer::new();
        let mut pill = sample_pill();
        pill.dot = None;
        let spec = WidgetSpec {
            data_hash: 1,
            kind: WidgetKind::Pill(pill),
        };
        let pm = r.rasterize_to_pixmap(&spec).expect("pixmap");
        assert_eq!(pm.width(), 160);
    }

    #[test]
    fn rasterize_no_border_still_produces_pixmap() {
        let mut r = WidgetRasterizer::new();
        let mut pill = sample_pill();
        pill.border = None;
        let spec = WidgetSpec {
            data_hash: 2,
            kind: WidgetKind::Pill(pill),
        };
        assert!(r.rasterize_to_pixmap(&spec).is_some());
    }

    #[test]
    fn text_origin_px_respects_dot_presence() {
        let mut pill = sample_pill();
        let (x_with, _) = pill.text_origin_px();
        pill.dot = None;
        let (x_without, _) = pill.text_origin_px();
        assert!(
            x_with > x_without,
            "dot presence should push text origin further right ({x_with} vs {x_without})"
        );
    }

    #[test]
    fn text_origin_px_is_always_inside_the_pixmap() {
        let pill = sample_pill();
        let (x, y) = pill.text_origin_px();
        assert!(x < pill.width as f32);
        assert!(y < pill.height as f32);
        assert!(x >= 0.0 && y >= 0.0);
    }

    #[test]
    fn large_corner_radius_is_clamped() {
        // A 100x20 pill asked for radius 9999 should not panic — the path
        // builder clamps radius to min(w,h)/2.
        let mut r = WidgetRasterizer::new();
        let spec = WidgetSpec {
            data_hash: 3,
            kind: WidgetKind::Pill(PillSpec {
                width: 100,
                height: 20,
                corner_radius: 9999.0,
                background: [1.0, 1.0, 1.0, 1.0],
                border: None,
                dot: None,
            }),
        };
        assert!(r.rasterize_to_pixmap(&spec).is_some());
    }
}
