//! Bridge from pattern-engine `ResolvedAction::Widget` to the app-side
//! `WidgetManager` (tn-068b).
//!
//! The pattern engine produces `ResolvedWidget` values when a rule's action
//! is `widget { ... }`. These carry a semantic `WidgetKind` (Badge, Gauge,
//! Sparkline, Card), an anchor strategy, and optional label/value/color
//! data. This module converts them into the rasterizer's `WidgetSpec` and
//! computes pixel placement so `WidgetManager::upsert` can cache/draw them.
//!
//! ## Widget ID allocation
//!
//! Pattern-sourced widgets use a deterministic ID derived from the pane ID,
//! row index, and match byte range. This means the same match at the same
//! screen position reuses the same cache entry (no re-rasterization), and
//! matches that scroll off-screen naturally stop being upserted -- the
//! `WidgetManager` still holds the stale entry but it is not drawn. A
//! periodic GC pass (not in v1) can reclaim those.
//!
//! ## Mapping to rasterizer kinds
//!
//! - `Badge` -> `Pill` (rounded pill with optional dot + label)
//! - `Gauge` -> `Pill` with proportional dot color (v1 approximation)
//! - `Sparkline` -> `Pill` (v1 fallback; proper sparkline kind is a follow-up)
//! - `Card` -> `Pill` with wider dimensions (v1 fallback)
//!
//! Follow-up issues should add native `Gauge`, `Sparkline`, and `Card`
//! variants to `rasterizer::WidgetKind` once their visual design is settled.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use therminal_terminal::semantic_patterns::types::{
    ResolvedWidget, WidgetAnchor, WidgetKind as PatternWidgetKind,
};

use super::WidgetId;
use super::rasterizer::{PillSpec, WidgetKind, WidgetSpec};

/// A pattern-engine widget match with enough context for pixel placement.
///
/// Collected during the per-pane render pass in `render.rs` and consumed
/// by `draw_widget_overlays` in `render_driver.rs`.
#[derive(Debug, Clone)]
pub struct PatternWidgetMatch {
    /// Pane that produced the match.
    pub pane_id: u64,
    /// Row index within the visible grid (0-based).
    pub row: usize,
    /// Start column of the matched text.
    pub start_col: usize,
    /// End column (exclusive) of the matched text.
    pub end_col: usize,
    /// Pane viewport origin X in physical pixels.
    pub pane_vp_x: f32,
    /// Pane viewport origin Y in physical pixels (includes header offset).
    pub pane_vp_y: f32,
    /// The resolved widget data from the pattern engine.
    pub widget: ResolvedWidget,
}

// -- Widget ID ----------------------------------------------------------------

/// Base offset for pattern-sourced widget IDs. Keeps them out of the
/// range used by hard-coded widgets (badge = 0x4147.., timeline = 0x544C..).
const PATTERN_WIDGET_ID_BASE: u64 = 0x5057_0000_0000_0000; // "PW......"

/// Compute a deterministic `WidgetId` for a pattern-sourced widget.
///
/// The ID is stable across frames for the same pane + screen position,
/// so the `WidgetManager` freshness cache works correctly.
pub fn widget_id_for(pane_id: u64, row: usize, start_col: usize) -> WidgetId {
    let mut h = DefaultHasher::new();
    pane_id.hash(&mut h);
    row.hash(&mut h);
    start_col.hash(&mut h);
    PATTERN_WIDGET_ID_BASE ^ (h.finish() & 0x00FF_FFFF_FFFF_FFFF)
}

// -- Spec construction --------------------------------------------------------

/// Default pill background for pattern-sourced widgets.
fn default_background() -> [f32; 4] {
    [0.08, 0.10, 0.18, 0.85]
}

/// Parse a CSS-style hex color string ("#RRGGBB" or "#RRGGBBAA") into
/// an f32 RGBA array. Returns `None` on parse failure.
fn parse_hex_color(s: &str) -> Option<[f32; 4]> {
    let s = s.strip_prefix('#')?;
    let (r, g, b, a) = match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            (r, g, b, 255u8)
        }
        8 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            let a = u8::from_str_radix(&s[6..8], 16).ok()?;
            (r, g, b, a)
        }
        _ => return None,
    };
    Some([
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        a as f32 / 255.0,
    ])
}

/// Resolve the dot color from the pattern widget's `color` field.
/// Falls back to a default teal when no color or unparseable.
fn resolve_dot_color(color: Option<&str>) -> [f32; 4] {
    color
        .and_then(parse_hex_color)
        .unwrap_or([0.3, 0.8, 0.7, 1.0])
}

/// Build a `WidgetSpec` from a `ResolvedWidget`.
///
/// Returns `(spec, pixel_width, pixel_height)`. The caller uses these
/// dimensions together with the match's row/col to compute the final
/// on-screen position before calling `WidgetManager::upsert`.
pub fn spec_from_resolved(rw: &ResolvedWidget) -> (WidgetSpec, u32, u32) {
    let label_text = rw.label.as_deref().unwrap_or("");
    let approx_text_w = (label_text.chars().count() as u32).max(4) * 7;

    match rw.kind {
        PatternWidgetKind::Badge => {
            let height: u32 = 22;
            let width: u32 = approx_text_w + height + 16;
            let dot_color = resolve_dot_color(rw.color.as_deref());
            let data_hash = hash_resolved(rw);
            let spec = WidgetSpec {
                data_hash,
                kind: WidgetKind::Pill(PillSpec {
                    width,
                    height,
                    corner_radius: height as f32 / 2.0,
                    background: default_background(),
                    border: Some(([0.25, 0.5, 0.8, 0.7], 1.0)),
                    dot: Some(dot_color),
                }),
            };
            (spec, width, height)
        }
        PatternWidgetKind::Gauge => {
            // v1: render as a wider pill whose dot color reflects the
            // value/max ratio (green->yellow->red gradient).
            let height: u32 = 22;
            let width: u32 = approx_text_w + height + 24;
            let ratio = match (rw.value, rw.max) {
                (Some(v), Some(m)) if m > 0.0 => (v / m).clamp(0.0, 1.0) as f32,
                _ => 0.5,
            };
            // Green (low) -> yellow (mid) -> red (high).
            let dot_color = [
                (ratio * 2.0).min(1.0),         // R
                ((1.0 - ratio) * 2.0).min(1.0), // G
                0.2,                            // B
                1.0,                            // A
            ];
            let data_hash = hash_resolved(rw);
            let spec = WidgetSpec {
                data_hash,
                kind: WidgetKind::Pill(PillSpec {
                    width,
                    height,
                    corner_radius: height as f32 / 2.0,
                    background: default_background(),
                    border: Some(([0.3, 0.5, 0.7, 0.6], 1.0)),
                    dot: Some(dot_color),
                }),
            };
            (spec, width, height)
        }
        PatternWidgetKind::Sparkline | PatternWidgetKind::Card => {
            // v1 fallback: render as a plain pill.
            let height: u32 = 22;
            let width: u32 = approx_text_w + height + 16;
            let dot_color = resolve_dot_color(rw.color.as_deref());
            let data_hash = hash_resolved(rw);
            let spec = WidgetSpec {
                data_hash,
                kind: WidgetKind::Pill(PillSpec {
                    width,
                    height,
                    corner_radius: height as f32 / 2.0,
                    background: default_background(),
                    border: Some(([0.2, 0.4, 0.6, 0.5], 1.0)),
                    dot: Some(dot_color),
                }),
            };
            (spec, width, height)
        }
    }
}

/// Compute a data hash for cache freshness. Hashes the fields that
/// affect visual output (kind, label, value, color).
fn hash_resolved(rw: &ResolvedWidget) -> u64 {
    let mut h = DefaultHasher::new();
    rw.kind.as_str().hash(&mut h);
    rw.anchor.as_str().hash(&mut h);
    rw.label.hash(&mut h);
    // Hash f64 bits for determinism (NaN != NaN is fine -- it just
    // forces a re-rasterize, which is harmless).
    rw.value.map(|v| v.to_bits()).hash(&mut h);
    rw.max.map(|v| v.to_bits()).hash(&mut h);
    rw.title.hash(&mut h);
    rw.body.hash(&mut h);
    rw.color.hash(&mut h);
    h.finish()
}

// -- Pixel placement ----------------------------------------------------------

/// Compute the on-screen pixel position for a pattern widget.
///
/// `cell_width` / `cell_height` are the grid renderer's cell metrics.
/// `widget_w` is the rasterized width.
/// `surface_width` is the full surface width for clamp/overlay anchoring.
///
/// Returns `(x, y)` in physical surface pixels.
pub fn compute_placement(
    m: &PatternWidgetMatch,
    cell_width: f32,
    cell_height: f32,
    widget_w: u32,
    surface_width: u32,
) -> (f32, f32) {
    match m.widget.anchor {
        WidgetAnchor::Inline => {
            // Place at the match start column, vertically centered on the row.
            let x = m.pane_vp_x + m.start_col as f32 * cell_width;
            let y = m.pane_vp_y + m.row as f32 * cell_height;
            (x, y)
        }
        WidgetAnchor::LineRight => {
            // Place at the right edge of the match's row, inset from the
            // pane's right edge.
            let x = m.pane_vp_x + m.end_col as f32 * cell_width + 4.0;
            let y = m.pane_vp_y + m.row as f32 * cell_height;
            // Clamp to surface bounds.
            let x = x.min(surface_width as f32 - widget_w as f32 - 4.0);
            (x, y)
        }
        WidgetAnchor::Overlay => {
            // Floating placement: right edge of the surface at the match row.
            let x = (surface_width as f32 - widget_w as f32 - 16.0).max(0.0);
            let y = m.pane_vp_y + m.row as f32 * cell_height;
            (x, y)
        }
    }
}

// -- Tests --------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_resolved(kind: PatternWidgetKind) -> ResolvedWidget {
        ResolvedWidget {
            kind,
            anchor: WidgetAnchor::Inline,
            label: Some("test".to_string()),
            value: Some(42.0),
            max: Some(100.0),
            title: None,
            body: None,
            color: Some("#40CC80".to_string()),
        }
    }

    #[test]
    fn widget_id_stable_for_same_inputs() {
        let a = widget_id_for(1, 5, 10);
        let b = widget_id_for(1, 5, 10);
        assert_eq!(a, b);
    }

    #[test]
    fn widget_id_differs_for_different_inputs() {
        let a = widget_id_for(1, 5, 10);
        let b = widget_id_for(1, 5, 11);
        assert_ne!(a, b);
        let c = widget_id_for(2, 5, 10);
        assert_ne!(a, c);
    }

    #[test]
    fn widget_id_in_pattern_range() {
        let id = widget_id_for(1, 0, 0);
        // Must have the pattern widget base prefix.
        assert_ne!(id & PATTERN_WIDGET_ID_BASE, 0);
    }

    #[test]
    fn spec_from_badge_produces_pill() {
        let rw = sample_resolved(PatternWidgetKind::Badge);
        let (spec, w, h) = spec_from_resolved(&rw);
        assert!(w > 0);
        assert!(h > 0);
        assert!(matches!(spec.kind, WidgetKind::Pill(_)));
    }

    #[test]
    fn spec_from_gauge_produces_pill() {
        let rw = sample_resolved(PatternWidgetKind::Gauge);
        let (spec, w, h) = spec_from_resolved(&rw);
        assert!(w > 0);
        assert!(h > 0);
        assert!(matches!(spec.kind, WidgetKind::Pill(_)));
    }

    #[test]
    fn spec_hash_stable_for_same_input() {
        let rw = sample_resolved(PatternWidgetKind::Badge);
        let (a, _, _) = spec_from_resolved(&rw);
        let (b, _, _) = spec_from_resolved(&rw);
        assert_eq!(a.data_hash, b.data_hash);
    }

    #[test]
    fn spec_hash_changes_with_label() {
        let mut rw1 = sample_resolved(PatternWidgetKind::Badge);
        let mut rw2 = sample_resolved(PatternWidgetKind::Badge);
        rw1.label = Some("alpha".to_string());
        rw2.label = Some("beta".to_string());
        let (a, _, _) = spec_from_resolved(&rw1);
        let (b, _, _) = spec_from_resolved(&rw2);
        assert_ne!(a.data_hash, b.data_hash);
    }

    #[test]
    fn parse_hex_color_valid() {
        let c = parse_hex_color("#FF8040").unwrap();
        assert!((c[0] - 1.0).abs() < 0.01);
        assert!((c[1] - 0.502).abs() < 0.01);
        assert!((c[2] - 0.251).abs() < 0.01);
        assert!((c[3] - 1.0).abs() < 0.01);
    }

    #[test]
    fn parse_hex_color_with_alpha() {
        let c = parse_hex_color("#FF804080").unwrap();
        assert!((c[3] - 0.502).abs() < 0.01);
    }

    #[test]
    fn parse_hex_color_invalid() {
        assert!(parse_hex_color("not_a_color").is_none());
        assert!(parse_hex_color("#GG0000").is_none());
        assert!(parse_hex_color("#FFF").is_none()); // 3-char not supported
    }

    #[test]
    fn inline_placement_at_match_start() {
        let m = PatternWidgetMatch {
            pane_id: 1,
            row: 3,
            start_col: 10,
            end_col: 20,
            pane_vp_x: 100.0,
            pane_vp_y: 50.0,
            widget: sample_resolved(PatternWidgetKind::Badge),
        };
        let (x, y) = compute_placement(&m, 8.0, 16.0, 60, 1920);
        assert!((x - (100.0 + 10.0 * 8.0)).abs() < 0.01);
        assert!((y - (50.0 + 3.0 * 16.0)).abs() < 0.01);
    }

    #[test]
    fn line_right_placement_after_match() {
        let mut rw = sample_resolved(PatternWidgetKind::Badge);
        rw.anchor = WidgetAnchor::LineRight;
        let m = PatternWidgetMatch {
            pane_id: 1,
            row: 3,
            start_col: 10,
            end_col: 20,
            pane_vp_x: 100.0,
            pane_vp_y: 50.0,
            widget: rw,
        };
        let (x, _y) = compute_placement(&m, 8.0, 16.0, 60, 1920);
        // Should be after end_col.
        assert!(x > 100.0 + 20.0 * 8.0 - 1.0);
    }

    #[test]
    fn overlay_placement_near_right_edge() {
        let mut rw = sample_resolved(PatternWidgetKind::Badge);
        rw.anchor = WidgetAnchor::Overlay;
        let m = PatternWidgetMatch {
            pane_id: 1,
            row: 3,
            start_col: 10,
            end_col: 20,
            pane_vp_x: 100.0,
            pane_vp_y: 50.0,
            widget: rw,
        };
        let (x, _y) = compute_placement(&m, 8.0, 16.0, 60, 1920);
        // Should be near the right edge of the surface.
        assert!(x > 1800.0);
    }
}
