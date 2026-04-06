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

/// Height of the workspace tab bar in physical pixels (standard mode).
pub const TAB_BAR_HEIGHT: f32 = 24.0;

/// Height of the tab bar when client-side decorations are active.
/// Taller to accommodate window control buttons.
pub const CSD_TAB_BAR_HEIGHT: f32 = 36.0;

/// Width of a single CSD window control button (minimize, maximize, close).
#[allow(dead_code)]
pub const CSD_BUTTON_WIDTH: f32 = 46.0;

/// Height of a CSD window control button.
#[allow(dead_code)]
pub const CSD_BUTTON_HEIGHT: f32 = 36.0;

/// Return the effective status bar height: 0 when disabled, STATUS_BAR_HEIGHT otherwise.
pub fn effective_status_bar_height(show: bool) -> f32 {
    if show { STATUS_BAR_HEIGHT } else { 0.0 }
}

/// Return the effective tab bar height: 0 when disabled, TAB_BAR_HEIGHT otherwise.
pub fn effective_tab_bar_height(show: bool) -> f32 {
    if show { TAB_BAR_HEIGHT } else { 0.0 }
}

/// Return the effective tab bar height when CSD may be active.
/// When CSD is on, the tab bar is always shown (it is the title bar).
pub fn effective_tab_bar_height_csd(show_tab_bar: bool, use_csd: bool) -> f32 {
    if use_csd {
        CSD_TAB_BAR_HEIGHT
    } else if show_tab_bar {
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

/// Compute the content area rect with CSD awareness.
#[allow(dead_code)]
pub fn content_area_rect_csd(
    width: f32,
    height: f32,
    show_status_bar: bool,
    show_tab_bar: bool,
    use_csd: bool,
) -> Rect {
    let status_bar_h = effective_status_bar_height(show_status_bar);
    let tab_bar_h = effective_tab_bar_height_csd(show_tab_bar, use_csd);
    Rect::new(0.0, tab_bar_h, width, height - status_bar_h - tab_bar_h)
}

/// Minimum pane width in physical pixels.
pub const MIN_PANE_WIDTH: f32 = 80.0;

/// Minimum pane height in physical pixels.
pub const MIN_PANE_HEIGHT: f32 = 60.0;

#[cfg(test)]
mod tests {
    use super::*;

    // ── effective_header_height ────────────────────────────────────────

    #[test]
    fn header_height_zero_for_single_pane() {
        assert_eq!(effective_header_height(1), 0.0);
    }

    #[test]
    fn header_height_zero_for_zero_panes() {
        assert_eq!(effective_header_height(0), 0.0);
    }

    #[test]
    fn header_height_present_for_multiple_panes() {
        assert_eq!(effective_header_height(2), PANE_HEADER_HEIGHT);
        assert_eq!(effective_header_height(10), PANE_HEADER_HEIGHT);
    }

    // ── effective_status_bar_height ───────────────────────────────────

    #[test]
    fn status_bar_height_when_shown() {
        assert_eq!(effective_status_bar_height(true), STATUS_BAR_HEIGHT);
    }

    #[test]
    fn status_bar_height_when_hidden() {
        assert_eq!(effective_status_bar_height(false), 0.0);
    }

    // ── effective_tab_bar_height ──────────────────────────────────────

    #[test]
    fn tab_bar_height_when_shown() {
        assert_eq!(effective_tab_bar_height(true), TAB_BAR_HEIGHT);
    }

    #[test]
    fn tab_bar_height_when_hidden() {
        assert_eq!(effective_tab_bar_height(false), 0.0);
    }

    // ── content_area_rect ────────────────────────────────────────────

    #[test]
    fn content_area_no_bars() {
        let r = content_area_rect(800.0, 600.0, false, false);
        assert_eq!(r.x(), 0.0);
        assert_eq!(r.y(), 0.0);
        assert_eq!(r.width(), 800.0);
        assert_eq!(r.height(), 600.0);
    }

    #[test]
    fn content_area_status_bar_only() {
        let r = content_area_rect(800.0, 600.0, true, false);
        assert_eq!(r.x(), 0.0);
        assert_eq!(r.y(), 0.0);
        assert_eq!(r.width(), 800.0);
        assert_eq!(r.height(), 600.0 - STATUS_BAR_HEIGHT);
    }

    #[test]
    fn content_area_tab_bar_only() {
        let r = content_area_rect(800.0, 600.0, false, true);
        assert_eq!(r.x(), 0.0);
        assert_eq!(r.y(), TAB_BAR_HEIGHT);
        assert_eq!(r.width(), 800.0);
        assert_eq!(r.height(), 600.0 - TAB_BAR_HEIGHT);
    }

    #[test]
    fn content_area_both_bars() {
        let r = content_area_rect(800.0, 600.0, true, true);
        assert_eq!(r.x(), 0.0);
        assert_eq!(r.y(), TAB_BAR_HEIGHT);
        assert_eq!(r.width(), 800.0);
        assert_eq!(r.height(), 600.0 - STATUS_BAR_HEIGHT - TAB_BAR_HEIGHT);
    }

    #[test]
    fn content_area_preserves_width_with_bars() {
        // Width should never be affected by bars.
        let r = content_area_rect(1920.0, 1080.0, true, true);
        assert_eq!(r.width(), 1920.0);
    }

    #[test]
    fn content_area_small_window() {
        // Even with a tiny window, the math should not panic.
        let r = content_area_rect(100.0, 50.0, true, true);
        assert_eq!(r.y(), TAB_BAR_HEIGHT);
        // Height might go negative for pathologically small windows -- that is fine,
        // the layout code handles it. We just verify no panic.
        let expected_h = 50.0 - STATUS_BAR_HEIGHT - TAB_BAR_HEIGHT;
        assert_eq!(r.height(), expected_h);
    }

    // ── Rect identity checks ─────────────────────────────────────────

    #[test]
    fn content_area_rect_origin_at_top_left_when_no_tab_bar() {
        let r = content_area_rect(640.0, 480.0, true, false);
        assert_eq!(r.x(), 0.0);
        assert_eq!(r.y(), 0.0);
        assert_eq!(r.right(), 640.0);
        assert_eq!(r.bottom(), 480.0 - STATUS_BAR_HEIGHT);
    }
}
