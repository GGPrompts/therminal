//! Color constants and button dimensions for chrome rendering.

use therminal_core::palette::Color as PaletteColor;

/// Color for focused pane border indicator (FOCUS from Codex 2031 palette).
pub(crate) const FOCUS_BORDER_COLOR: [f32; 4] = {
    let c = PaletteColor::FOCUS;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        0.92,
    ]
};

/// Color for unfocused pane separators (LINE from palette).
pub(super) const SEPARATOR_COLOR: [f32; 4] = {
    let c = PaletteColor::LINE;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        0.9,
    ]
};

/// Color for separators adjacent to the focused pane (FOCUS from palette).
pub(super) const SEPARATOR_FOCUS_COLOR: [f32; 4] = {
    let c = PaletteColor::FOCUS;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        0.82,
    ]
};

/// Pane header background color for the focused pane.
pub(crate) const HEADER_BG_COLOR: [f32; 4] = {
    let c = PaletteColor::VOID_1;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        1.0,
    ]
};

/// Dimmed pane header background for unfocused panes.
pub(crate) const HEADER_BG_DIM_COLOR: [f32; 4] = {
    let c = PaletteColor::VOID_0;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        0.92,
    ]
};

/// Width of each header button in pixels.
pub(crate) const HEADER_BUTTON_WIDTH: f32 = 24.0;

/// Right-side margin for header buttons.
pub(crate) const HEADER_BUTTON_MARGIN: f32 = 4.0;

/// Status bar background color.
pub(super) const STATUS_BAR_BG_COLOR: [f32; 4] = {
    let c = PaletteColor::VOID_1;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        1.0,
    ]
};
