//! ANSI-to-thermal color mapping helpers for the GPU terminal renderer.

use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use glyphon::Color as GlyphColor;
use therminal_core::palette::{ChromePalette, Color as PaletteColor};
use therminal_terminal::hotspot_detection::HotspotKind;

use crate::grid_renderer::{ColorVertex, RenderCell, TERM_BG};

/// Map an alacritty_terminal ANSI Color to an [f32; 4] RGBA array.
pub(crate) fn ansi_to_glyphon_fg(color: &AnsiColor) -> [f32; 4] {
    match color {
        AnsiColor::Named(named) => named_to_thermal_fg(*named),
        AnsiColor::Spec(rgb) => [
            rgb.r as f32 / 255.0,
            rgb.g as f32 / 255.0,
            rgb.b as f32 / 255.0,
            1.0,
        ],
        AnsiColor::Indexed(idx) => indexed_color(*idx),
    }
}

pub(crate) fn ansi_to_glyphon_bg(color: &AnsiColor) -> Option<[f32; 4]> {
    match color {
        AnsiColor::Named(NamedColor::Background) => None,
        AnsiColor::Named(NamedColor::Black) => None,
        AnsiColor::Named(named) => Some(named_to_thermal_bg(*named)),
        AnsiColor::Spec(rgb) => {
            if (rgb.r == 0 && rgb.g == 0 && rgb.b == 0)
                || (rgb.r == 6 && rgb.g == 10 && rgb.b == 18)
            {
                None
            } else {
                Some([
                    rgb.r as f32 / 255.0,
                    rgb.g as f32 / 255.0,
                    rgb.b as f32 / 255.0,
                    1.0,
                ])
            }
        }
        AnsiColor::Indexed(idx) => {
            if *idx == 0 {
                None
            } else {
                Some(indexed_color(*idx))
            }
        }
    }
}

/// Map named ANSI colors to thermal palette foreground colors.
///
/// Semantically correct: red is red, green is green, magenta is distinct
/// from blue, cyan is distinct from green. Each color meets WCAG AA
/// (4.5:1) contrast against the dark background.
pub(crate) fn named_to_thermal_fg(named: NamedColor) -> [f32; 4] {
    match named {
        NamedColor::Black => PaletteColor::BG.to_f32_array(),
        NamedColor::Red => [1.0, 0.37, 0.43, 1.0], // #ff5f6d — coral red
        NamedColor::Green => [0.31, 0.79, 0.44, 1.0], // #4ec970 — true green
        NamedColor::Yellow => [0.91, 0.77, 0.28, 1.0], // #e7c547 — warm yellow
        NamedColor::Blue => [0.29, 0.56, 0.85, 1.0], // #4a8fd9 — medium blue
        NamedColor::Magenta => [0.79, 0.48, 0.86, 1.0], // #c97bdb — purple-pink
        NamedColor::Cyan => [0.34, 0.78, 0.85, 1.0], // #56c8d8 — teal-cyan
        NamedColor::White | NamedColor::Foreground => PaletteColor::TEXT_BRIGHT.to_f32_array(),

        NamedColor::BrightBlack => PaletteColor::INK_DIM.to_f32_array(),
        NamedColor::BrightRed => [1.0, 0.56, 0.63, 1.0], // #ff8fa0 — lighter coral
        NamedColor::BrightGreen => [0.45, 0.89, 0.60, 1.0], // #74e39a — bright green
        NamedColor::BrightYellow => [1.0, 0.84, 0.37, 1.0], // #ffd75f — bright gold
        NamedColor::BrightBlue => [0.47, 0.72, 0.97, 1.0], // #79b8f8 — lighter blue
        NamedColor::BrightMagenta => [0.87, 0.63, 0.93, 1.0], // #dda0ee — lighter purple
        NamedColor::BrightCyan => [0.49, 0.86, 0.91, 1.0], // #7edce8 — lighter teal
        NamedColor::BrightWhite | NamedColor::BrightForeground => {
            PaletteColor::WHITE_HOT.to_f32_array()
        }

        NamedColor::DimBlack => TERM_BG,
        NamedColor::DimRed => [0.80, 0.35, 0.40, 1.0], // muted coral
        NamedColor::DimGreen => [0.22, 0.58, 0.32, 1.0], // muted green
        NamedColor::DimYellow => [0.68, 0.58, 0.22, 1.0], // muted yellow
        NamedColor::DimBlue => [0.30, 0.50, 0.75, 1.0], // muted blue
        NamedColor::DimMagenta => [0.58, 0.36, 0.63, 1.0], // muted purple
        NamedColor::DimCyan => [0.25, 0.58, 0.63, 1.0], // muted teal
        NamedColor::DimWhite | NamedColor::DimForeground => PaletteColor::TEXT.to_f32_array(),

        NamedColor::Background => TERM_BG,
        NamedColor::Cursor => PaletteColor::WHITE_HOT.to_f32_array(),
    }
}

/// Map named ANSI colors to thermal palette background colors.
///
/// Background variants are darkened enough that white/bright text remains
/// readable on top. Semantically correct: green bg is green, blue bg is
/// blue, magenta bg is purple. Bright/Dim variants use muted/dark palette
/// entries. `Black` and `Background` are handled upstream in
/// `ansi_to_glyphon_bg` (both return `None` -> transparent), so those arms
/// are retained here only as a safety fallback.
pub(crate) fn named_to_thermal_bg(named: NamedColor) -> [f32; 4] {
    match named {
        NamedColor::Black => TERM_BG,
        NamedColor::Red => [0.65, 0.15, 0.20, 1.0], // dark red bg — white text safe
        NamedColor::Green => [0.10, 0.40, 0.18, 1.0], // dark green bg — white text safe
        NamedColor::Yellow => [0.50, 0.40, 0.05, 1.0], // dark yellow/brown bg — white text safe
        NamedColor::Blue => [0.12, 0.25, 0.55, 1.0], // dark blue bg — white text safe
        NamedColor::Magenta => [0.40, 0.15, 0.50, 1.0], // dark purple bg — white text safe
        NamedColor::Cyan => [0.05, 0.35, 0.40, 1.0], // dark teal bg — white text safe
        NamedColor::White => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::Foreground => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::Background => TERM_BG,
        NamedColor::Cursor => PaletteColor::BG_SURFACE.to_f32_array(),

        // Bright backgrounds — slightly lighter but still dark enough for text
        NamedColor::BrightBlack => [0.18, 0.18, 0.20, 1.0], // dark grey
        NamedColor::BrightRed => [0.55, 0.12, 0.15, 1.0],   // deep red
        NamedColor::BrightGreen => [0.08, 0.35, 0.15, 1.0], // deep green
        NamedColor::BrightYellow => [0.45, 0.35, 0.05, 1.0], // deep gold
        NamedColor::BrightBlue => [0.10, 0.22, 0.48, 1.0],  // deep blue
        NamedColor::BrightMagenta => [0.35, 0.12, 0.45, 1.0], // deep purple
        NamedColor::BrightCyan => [0.04, 0.30, 0.35, 1.0],  // deep teal
        NamedColor::BrightWhite => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::BrightForeground => PaletteColor::TEXT_MUTED.to_f32_array(),

        // Dim backgrounds — deep dark palette entries
        NamedColor::DimBlack => TERM_BG,
        NamedColor::DimRed => [0.30, 0.08, 0.10, 1.0], // very dark red
        NamedColor::DimGreen => [0.05, 0.20, 0.08, 1.0], // very dark green
        NamedColor::DimYellow => [0.25, 0.20, 0.03, 1.0], // very dark yellow
        NamedColor::DimBlue => [0.06, 0.12, 0.30, 1.0], // very dark blue
        NamedColor::DimMagenta => [0.20, 0.08, 0.25, 1.0], // very dark purple
        NamedColor::DimCyan => [0.03, 0.18, 0.20, 1.0], // very dark teal
        NamedColor::DimWhite => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::DimForeground => PaletteColor::TEXT_MUTED.to_f32_array(),
    }
}

/// Standard xterm-256 color palette lookup.
pub(crate) fn indexed_color(idx: u8) -> [f32; 4] {
    match idx {
        0 => named_to_thermal_fg(NamedColor::Black),
        1 => named_to_thermal_fg(NamedColor::Red),
        2 => named_to_thermal_fg(NamedColor::Green),
        3 => named_to_thermal_fg(NamedColor::Yellow),
        4 => named_to_thermal_fg(NamedColor::Blue),
        5 => named_to_thermal_fg(NamedColor::Magenta),
        6 => named_to_thermal_fg(NamedColor::Cyan),
        7 => named_to_thermal_fg(NamedColor::White),
        8 => named_to_thermal_fg(NamedColor::BrightBlack),
        9 => named_to_thermal_fg(NamedColor::BrightRed),
        10 => named_to_thermal_fg(NamedColor::BrightGreen),
        11 => named_to_thermal_fg(NamedColor::BrightYellow),
        12 => named_to_thermal_fg(NamedColor::BrightBlue),
        13 => named_to_thermal_fg(NamedColor::BrightMagenta),
        14 => named_to_thermal_fg(NamedColor::BrightCyan),
        15 => named_to_thermal_fg(NamedColor::BrightWhite),

        // 216-color cube (indices 16..=231).
        16..=231 => {
            let idx = idx - 16;
            let r_idx = idx / 36;
            let g_idx = (idx % 36) / 6;
            let b_idx = idx % 6;
            let r = if r_idx == 0 { 0 } else { 55 + r_idx * 40 };
            let g = if g_idx == 0 { 0 } else { 55 + g_idx * 40 };
            let b = if b_idx == 0 { 0 } else { 55 + b_idx * 40 };
            [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
        }

        // 24-step grayscale (indices 232..=255).
        232..=255 => {
            let level = 8 + (idx - 232) * 10;
            let v = level as f32 / 255.0;
            [v, v, v, 1.0]
        }
    }
}

/// Determine the background color for a cell (returns None for default BG).
pub(crate) fn cell_bg_color(cell: &RenderCell) -> Option<[f32; 4]> {
    if cell
        .flags
        .contains(alacritty_terminal::term::cell::Flags::INVERSE)
    {
        Some(ansi_to_glyphon_fg(&cell.fg))
    } else {
        ansi_to_glyphon_bg(&cell.bg)
    }
}

/// Convert pixel-space rect to 6 NDC vertices (two triangles).
pub(crate) fn pixel_rect_to_ndc(
    px: f32,
    py: f32,
    pw: f32,
    ph: f32,
    screen_w: f32,
    screen_h: f32,
    color: [f32; 4],
) -> [ColorVertex; 6] {
    let x0 = (px / screen_w) * 2.0 - 1.0;
    let x1 = ((px + pw) / screen_w) * 2.0 - 1.0;
    let y0 = 1.0 - (py / screen_h) * 2.0;
    let y1 = 1.0 - ((py + ph) / screen_h) * 2.0;

    [
        ColorVertex {
            position: [x0, y0],
            color,
        },
        ColorVertex {
            position: [x1, y0],
            color,
        },
        ColorVertex {
            position: [x0, y1],
            color,
        },
        ColorVertex {
            position: [x1, y0],
            color,
        },
        ColorVertex {
            position: [x1, y1],
            color,
        },
        ColorVertex {
            position: [x0, y1],
            color,
        },
    ]
}

/// Convert an [f32; 4] RGBA color to a glyphon Color.
pub(crate) fn f32_to_glyph_color(c: [f32; 4]) -> GlyphColor {
    GlyphColor::rgba(
        (c[0] * 255.0) as u8,
        (c[1] * 255.0) as u8,
        (c[2] * 255.0) as u8,
        (c[3] * 255.0) as u8,
    )
}

/// Map a [`HotspotKind`] to a distinct dotted-underline color.
///
/// Each hotspot kind gets a visually differentiated color so users can tell at
/// a glance whether a hotspot is a URL, file path, error, git ref, or issue
/// reference. Colors are sourced from the runtime [`ChromePalette`] (tn-g7oo)
/// so a theme override re-skins underlines automatically. All defaults pass
/// WCAG AA text contrast (>= 4.5:1) against the dark `PaletteColor::BG`.
pub(crate) fn hotspot_kind_color(kind: &HotspotKind, palette: &ChromePalette) -> [f32; 4] {
    match kind {
        HotspotKind::Url => palette.hotspot_url,
        HotspotKind::FilePath => palette.hotspot_filepath,
        HotspotKind::ErrorLocation => palette.hotspot_error,
        HotspotKind::GitRef => palette.hotspot_gitref,
        HotspotKind::IssueRef => palette.hotspot_issueref,
    }
}

/// Convert a line between two pixel coordinates into a thin rectangle (6 vertices).
///
/// The rectangle is `thickness` pixels wide, oriented along the line direction.
/// Returns vertices in NDC for the rect pipeline.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn line_to_rect_verts(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    thickness: f32,
    screen_w: f32,
    screen_h: f32,
    color: [f32; 4],
) -> [ColorVertex; 6] {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = (dx * dx + dy * dy).sqrt().max(0.001);

    // Perpendicular unit vector.
    let nx = -dy / len * thickness * 0.5;
    let ny = dx / len * thickness * 0.5;

    // Four corners of the thin rectangle in pixel coordinates.
    let corners = [
        (x0 + nx, y0 + ny),
        (x0 - nx, y0 - ny),
        (x1 + nx, y1 + ny),
        (x1 - nx, y1 - ny),
    ];

    // Convert to NDC.
    let to_ndc = |px: f32, py: f32| -> [f32; 2] {
        [(px / screen_w) * 2.0 - 1.0, 1.0 - (py / screen_h) * 2.0]
    };

    let p0 = to_ndc(corners[0].0, corners[0].1);
    let p1 = to_ndc(corners[1].0, corners[1].1);
    let p2 = to_ndc(corners[2].0, corners[2].1);
    let p3 = to_ndc(corners[3].0, corners[3].1);

    [
        ColorVertex {
            position: p0,
            color,
        },
        ColorVertex {
            position: p2,
            color,
        },
        ColorVertex {
            position: p1,
            color,
        },
        ColorVertex {
            position: p1,
            color,
        },
        ColorVertex {
            position: p2,
            color,
        },
        ColorVertex {
            position: p3,
            color,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba_to_palette_color(rgba: [f32; 4]) -> PaletteColor {
        PaletteColor::from_rgba(
            (rgba[0] * 255.0).round() as u8,
            (rgba[1] * 255.0).round() as u8,
            (rgba[2] * 255.0).round() as u8,
            (rgba[3] * 255.0).round() as u8,
        )
    }

    fn assert_wcag_text(name: &str, rgba: [f32; 4]) {
        let color = rgba_to_palette_color(rgba);
        let ratio = color.contrast_ratio(PaletteColor::BG);
        assert!(
            ratio >= 4.5,
            "{name} contrast {ratio:.2}:1 is below 4.5:1 against BG"
        );
    }

    #[test]
    fn hardcoded_foreground_overrides_meet_wcag_text() {
        assert_wcag_text("BrightBlack", named_to_thermal_fg(NamedColor::BrightBlack));
        assert_wcag_text("BrightRed", named_to_thermal_fg(NamedColor::BrightRed));
        assert_wcag_text("BrightGreen", named_to_thermal_fg(NamedColor::BrightGreen));
        assert_wcag_text("BrightCyan", named_to_thermal_fg(NamedColor::BrightCyan));
        assert_wcag_text("DimRed", named_to_thermal_fg(NamedColor::DimRed));
        assert_wcag_text("DimGreen", named_to_thermal_fg(NamedColor::DimGreen));
        assert_wcag_text("DimBlue", named_to_thermal_fg(NamedColor::DimBlue));
        assert_wcag_text("DimCyan", named_to_thermal_fg(NamedColor::DimCyan));
    }

    /// tn-oei7 — core ANSI fg colors (Red, Green, Yellow, Blue, Magenta,
    /// Cyan) must all meet WCAG AA against the dark BG.
    #[test]
    fn core_ansi_fg_colors_meet_wcag_text() {
        assert_wcag_text("Red", named_to_thermal_fg(NamedColor::Red));
        assert_wcag_text("Green", named_to_thermal_fg(NamedColor::Green));
        assert_wcag_text("Yellow", named_to_thermal_fg(NamedColor::Yellow));
        assert_wcag_text("Blue", named_to_thermal_fg(NamedColor::Blue));
        assert_wcag_text("Magenta", named_to_thermal_fg(NamedColor::Magenta));
        assert_wcag_text("Cyan", named_to_thermal_fg(NamedColor::Cyan));
    }

    /// tn-oei7 — ANSI bg colors (when used as backgrounds) must provide
    /// enough contrast for white text (3:1 minimum for large text elements).
    #[test]
    fn ansi_bg_colors_safe_for_white_text() {
        let white = PaletteColor::from_hex(0xFFFFFF);
        for (name, named) in [
            ("Red", NamedColor::Red),
            ("Green", NamedColor::Green),
            ("Blue", NamedColor::Blue),
            ("Magenta", NamedColor::Magenta),
            ("Cyan", NamedColor::Cyan),
        ] {
            let bg = named_to_thermal_bg(named);
            let bg_color = rgba_to_palette_color(bg);
            let ratio = white.contrast_ratio(bg_color);
            assert!(
                ratio >= 3.0,
                "{name} bg: white text contrast {ratio:.2}:1 < 3.0 ({bg:?})"
            );
        }
    }

    /// tn-oei7 — Green, Magenta, Cyan foreground colors must be
    /// semantically correct (green looks green, not amber/orange, etc.)
    #[test]
    fn ansi_fg_colors_are_semantically_correct() {
        let green = named_to_thermal_fg(NamedColor::Green);
        // Green: G channel dominates
        assert!(
            green[1] > green[0],
            "Green: G ({}) should exceed R ({})",
            green[1],
            green[0]
        );
        assert!(
            green[1] > green[2],
            "Green: G ({}) should exceed B ({})",
            green[1],
            green[2]
        );

        let magenta = named_to_thermal_fg(NamedColor::Magenta);
        let blue = named_to_thermal_fg(NamedColor::Blue);
        // Magenta must differ from Blue
        assert_ne!(magenta, blue, "Magenta and Blue must be different colors");
        // Magenta has higher R than G
        assert!(
            magenta[0] > magenta[1],
            "Magenta: R ({}) should exceed G ({})",
            magenta[0],
            magenta[1]
        );

        let cyan = named_to_thermal_fg(NamedColor::Cyan);
        // Cyan: B channel should be significant, not dominated by G alone
        assert!(cyan[2] > 0.5, "Cyan: B ({}) should be > 0.5", cyan[2]);
        // Cyan should differ from green visually (blue component distinguishes)
        assert!(cyan[2] > green[2], "Cyan should have more blue than Green");
    }

    #[test]
    fn hotspot_kind_colors_meet_wcag_text() {
        let palette = ChromePalette::default();
        assert_wcag_text(
            "Url (ACCENT_COOL)",
            hotspot_kind_color(&HotspotKind::Url, &palette),
        );
        assert_wcag_text(
            "FilePath (ACCENT_NEUTRAL)",
            hotspot_kind_color(&HotspotKind::FilePath, &palette),
        );
        assert_wcag_text(
            "ErrorLocation (STATUS_ERROR)",
            hotspot_kind_color(&HotspotKind::ErrorLocation, &palette),
        );
        assert_wcag_text(
            "GitRef (HOT)",
            hotspot_kind_color(&HotspotKind::GitRef, &palette),
        );
        assert_wcag_text(
            "IssueRef (purple)",
            hotspot_kind_color(&HotspotKind::IssueRef, &palette),
        );
    }

    #[test]
    fn hotspot_kind_colors_are_distinct() {
        let palette = ChromePalette::default();
        let colors = [
            hotspot_kind_color(&HotspotKind::Url, &palette),
            hotspot_kind_color(&HotspotKind::FilePath, &palette),
            hotspot_kind_color(&HotspotKind::ErrorLocation, &palette),
            hotspot_kind_color(&HotspotKind::GitRef, &palette),
            hotspot_kind_color(&HotspotKind::IssueRef, &palette),
        ];
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i], colors[j],
                    "hotspot colors at index {i} and {j} must be distinct"
                );
            }
        }
    }
}
