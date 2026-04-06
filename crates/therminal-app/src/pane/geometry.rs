//! Pane geometry constants and content-area helpers.

use therminal_core::geometry::Rect;

/// Separator gap between panes in physical pixels.
pub const SEPARATOR_GAP: f32 = 2.0;

/// Height of the pane header strip in physical pixels (when multiple panes exist).
pub const PANE_HEADER_HEIGHT: f32 = 20.0;

/// Return the effective header height: 0 when single pane, PANE_HEADER_HEIGHT otherwise.
pub fn effective_header_height(pane_count: usize) -> f32 {
    if pane_count <= 1 {
        0.0
    } else {
        PANE_HEADER_HEIGHT
    }
}

/// Height of the window status bar in physical pixels.
pub const STATUS_BAR_HEIGHT: f32 = 24.0;

/// Height of the workspace tab bar in physical pixels.
pub const TAB_BAR_HEIGHT: f32 = 24.0;

/// Return the effective status bar height: 0 when disabled, STATUS_BAR_HEIGHT otherwise.
pub fn effective_status_bar_height(show: bool) -> f32 {
    if show {
        STATUS_BAR_HEIGHT
    } else {
        0.0
    }
}

/// Return the effective tab bar height: 0 when disabled, TAB_BAR_HEIGHT otherwise.
pub fn effective_tab_bar_height(show: bool) -> f32 {
    if show {
        TAB_BAR_HEIGHT
    } else {
        0.0
    }
}

/// Compute the content area rect (window area minus status bar and tab bar).
pub fn content_area_rect(
    width: f32,
    height: f32,
    show_status_bar: bool,
    show_tab_bar: bool,
) -> Rect {
    let status_bar_h = effective_status_bar_height(show_status_bar);
    let tab_bar_h = effective_tab_bar_height(show_tab_bar);
    Rect::new(0.0, tab_bar_h, width, height - status_bar_h - tab_bar_h)
}

/// Minimum pane width in physical pixels.
pub const MIN_PANE_WIDTH: f32 = 80.0;

/// Minimum pane height in physical pixels.
pub const MIN_PANE_HEIGHT: f32 = 60.0;
