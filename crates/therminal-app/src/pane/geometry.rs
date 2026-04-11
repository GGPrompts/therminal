//! Pane geometry constants and content-area helpers.

use therminal_core::geometry::Rect;

/// Separator gap between panes in physical pixels.
pub const SEPARATOR_GAP: f32 = 2.0;

/// Height of the pane header strip in physical pixels (when multiple panes exist).
pub const PANE_HEADER_HEIGHT: f32 = 20.0;

/// Return the effective header height.
///
/// Returns [`PANE_HEADER_HEIGHT`] when `show_pane_headers` is `true`, `0.0`
/// otherwise. `pane_count` is unused and kept only for call-site compatibility.
pub fn effective_header_height(_pane_count: usize, show_pane_headers: bool) -> f32 {
    if show_pane_headers {
        PANE_HEADER_HEIGHT
    } else {
        0.0
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

/// Number of CSD window control buttons (settings, minimize, maximize, close).
pub const CSD_BUTTON_COUNT: u32 = 4;

/// Total width reserved for all CSD window control buttons.
pub const CSD_BUTTONS_TOTAL_WIDTH: f32 = CSD_BUTTON_WIDTH * CSD_BUTTON_COUNT as f32;

/// Height of a CSD window control button.
#[allow(dead_code)]
pub const CSD_BUTTON_HEIGHT: f32 = 36.0;

/// Return the effective status bar height: 0 when disabled, STATUS_BAR_HEIGHT otherwise.
pub fn effective_status_bar_height(show: bool) -> f32 {
    if show { STATUS_BAR_HEIGHT } else { 0.0 }
}

/// Decide whether the workspace tab bar should be visible.
///
/// Single-workspace layouts hide the bar automatically — nobody needs a tab
/// strip to "switch between one thing". A second workspace causes the bar to
/// appear. This is the single predicate every call site funnels through so
/// that future overrides (e.g. tn-t2yd.2 focus mode) can be added in one
/// place.
pub fn should_show_tab_bar(workspace_count: usize) -> bool {
    workspace_count >= 2
}

/// Return the effective tab bar height: 0 when `workspace_count < 2`,
/// [`TAB_BAR_HEIGHT`] otherwise.
#[cfg_attr(not(test), allow(dead_code))]
pub fn effective_tab_bar_height(workspace_count: usize) -> f32 {
    if should_show_tab_bar(workspace_count) {
        TAB_BAR_HEIGHT
    } else {
        0.0
    }
}

/// Return the effective tab bar height when CSD may be active.
///
/// When CSD is on, the title-bar strip is always reserved (it hosts the window
/// control buttons even without tabs). When CSD is off, the bar is hidden for
/// single-workspace layouts and reserved at [`TAB_BAR_HEIGHT`] otherwise.
pub fn effective_tab_bar_height_csd(workspace_count: usize, use_csd: bool) -> f32 {
    if use_csd {
        CSD_TAB_BAR_HEIGHT
    } else if should_show_tab_bar(workspace_count) {
        TAB_BAR_HEIGHT
    } else {
        0.0
    }
}

/// Compute the content area rect (window area minus status bar and tab bar).
#[cfg_attr(not(test), allow(dead_code))]
pub fn content_area_rect(
    width: f32,
    height: f32,
    show_status_bar: bool,
    workspace_count: usize,
) -> Rect {
    let status_bar_h = effective_status_bar_height(show_status_bar);
    let tab_bar_h = effective_tab_bar_height(workspace_count);
    Rect::new(0.0, tab_bar_h, width, height - status_bar_h - tab_bar_h)
}

/// Compute the content area rect with CSD awareness.
#[allow(dead_code)]
pub fn content_area_rect_csd(
    width: f32,
    height: f32,
    show_status_bar: bool,
    workspace_count: usize,
    use_csd: bool,
) -> Rect {
    let status_bar_h = effective_status_bar_height(show_status_bar);
    let tab_bar_h = effective_tab_bar_height_csd(workspace_count, use_csd);
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
    fn header_height_present_for_single_pane_when_enabled() {
        assert_eq!(effective_header_height(1, true), PANE_HEADER_HEIGHT);
    }

    #[test]
    fn header_height_present_for_zero_panes_when_enabled() {
        assert_eq!(effective_header_height(0, true), PANE_HEADER_HEIGHT);
    }

    #[test]
    fn header_height_present_for_multiple_panes() {
        assert_eq!(effective_header_height(2, true), PANE_HEADER_HEIGHT);
        assert_eq!(effective_header_height(10, true), PANE_HEADER_HEIGHT);
    }

    #[test]
    fn header_height_zero_when_disabled_even_with_many_panes() {
        // show_pane_headers = false should suppress headers regardless of pane count.
        assert_eq!(effective_header_height(2, false), 0.0);
        assert_eq!(effective_header_height(10, false), 0.0);
        assert_eq!(effective_header_height(1, false), 0.0);
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

    // ── should_show_tab_bar ───────────────────────────────────────────

    #[test]
    fn tab_bar_hidden_for_zero_or_one_workspace() {
        assert!(!should_show_tab_bar(0));
        assert!(!should_show_tab_bar(1));
    }

    #[test]
    fn tab_bar_shown_for_two_or_more_workspaces() {
        assert!(should_show_tab_bar(2));
        assert!(should_show_tab_bar(5));
        assert!(should_show_tab_bar(100));
    }

    // ── effective_tab_bar_height ──────────────────────────────────────

    #[test]
    fn tab_bar_height_zero_for_single_workspace() {
        assert_eq!(effective_tab_bar_height(0), 0.0);
        assert_eq!(effective_tab_bar_height(1), 0.0);
    }

    #[test]
    fn tab_bar_height_reserved_for_multi_workspace() {
        assert_eq!(effective_tab_bar_height(2), TAB_BAR_HEIGHT);
        assert_eq!(effective_tab_bar_height(10), TAB_BAR_HEIGHT);
    }

    // ── effective_tab_bar_height_csd ──────────────────────────────────

    #[test]
    fn tab_bar_height_csd_always_reserved_with_csd() {
        // CSD mode reserves the title-bar strip for window controls even
        // when there is only one workspace.
        assert_eq!(effective_tab_bar_height_csd(1, true), CSD_TAB_BAR_HEIGHT);
        assert_eq!(effective_tab_bar_height_csd(3, true), CSD_TAB_BAR_HEIGHT);
    }

    #[test]
    fn tab_bar_height_csd_hidden_without_csd_and_single_workspace() {
        assert_eq!(effective_tab_bar_height_csd(1, false), 0.0);
        assert_eq!(effective_tab_bar_height_csd(0, false), 0.0);
    }

    #[test]
    fn tab_bar_height_csd_shown_without_csd_and_multi_workspace() {
        assert_eq!(effective_tab_bar_height_csd(2, false), TAB_BAR_HEIGHT);
    }

    // ── content_area_rect ────────────────────────────────────────────

    #[test]
    fn content_area_no_bars() {
        let r = content_area_rect(800.0, 600.0, false, 1);
        assert_eq!(r.x(), 0.0);
        assert_eq!(r.y(), 0.0);
        assert_eq!(r.width(), 800.0);
        assert_eq!(r.height(), 600.0);
    }

    #[test]
    fn content_area_status_bar_only() {
        let r = content_area_rect(800.0, 600.0, true, 1);
        assert_eq!(r.x(), 0.0);
        assert_eq!(r.y(), 0.0);
        assert_eq!(r.width(), 800.0);
        assert_eq!(r.height(), 600.0 - STATUS_BAR_HEIGHT);
    }

    #[test]
    fn content_area_tab_bar_only() {
        let r = content_area_rect(800.0, 600.0, false, 2);
        assert_eq!(r.x(), 0.0);
        assert_eq!(r.y(), TAB_BAR_HEIGHT);
        assert_eq!(r.width(), 800.0);
        assert_eq!(r.height(), 600.0 - TAB_BAR_HEIGHT);
    }

    #[test]
    fn content_area_both_bars() {
        let r = content_area_rect(800.0, 600.0, true, 2);
        assert_eq!(r.x(), 0.0);
        assert_eq!(r.y(), TAB_BAR_HEIGHT);
        assert_eq!(r.width(), 800.0);
        assert_eq!(r.height(), 600.0 - STATUS_BAR_HEIGHT - TAB_BAR_HEIGHT);
    }

    #[test]
    fn content_area_preserves_width_with_bars() {
        // Width should never be affected by bars.
        let r = content_area_rect(1920.0, 1080.0, true, 3);
        assert_eq!(r.width(), 1920.0);
    }

    #[test]
    fn content_area_small_window() {
        // Even with a tiny window, the math should not panic.
        let r = content_area_rect(100.0, 50.0, true, 2);
        assert_eq!(r.y(), TAB_BAR_HEIGHT);
        // Height might go negative for pathologically small windows -- that is fine,
        // the layout code handles it. We just verify no panic.
        let expected_h = 50.0 - STATUS_BAR_HEIGHT - TAB_BAR_HEIGHT;
        assert_eq!(r.height(), expected_h);
    }

    // ── Rect identity checks ─────────────────────────────────────────

    #[test]
    fn content_area_rect_origin_at_top_left_when_no_tab_bar() {
        let r = content_area_rect(640.0, 480.0, true, 1);
        assert_eq!(r.x(), 0.0);
        assert_eq!(r.y(), 0.0);
        assert_eq!(r.right(), 640.0);
        assert_eq!(r.bottom(), 480.0 - STATUS_BAR_HEIGHT);
    }
}
