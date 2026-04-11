//! Chrome rendering: pane headers, status bar, separators, focus borders,
//! workspace tab bar, and CSD window controls.
//!
//! All non-terminal-content rendering lives under this module -- the
//! decorative UI elements that surround the actual grid content. See the
//! individual submodules for details on each element's layout and hit-test
//! semantics.

mod colors;
mod csd;
mod delegate_summary;
mod overlays;
mod pane_header;
mod status_bar;
mod tab_bar;
mod text_cache;

// Re-exports for use by `window/mod.rs`, `window/render.rs`, and
// `window/mouse.rs`. These preserve the public surface of the pre-split
// `chrome.rs` so no callers need to change their imports.
pub(crate) use colors::{HEADER_BUTTON_MARGIN, HEADER_BUTTON_WIDTH};
pub(crate) use csd::{CsdAction, csd_button_hit_test, draw_csd_buttons};
pub(crate) use delegate_summary::{DelegateState, DelegateSummaryState};
pub(crate) use overlays::push_visual_bell_overlay;
pub(crate) use pane_header::{draw_pane_focus_border, draw_pane_header, draw_split_separator};
pub(crate) use status_bar::{
    StatusBarHit, StatusBarHitAreas, StatusBarInfo, draw_status_bar, status_bar_hit_test,
};
pub(crate) use tab_bar::{TAB_ELLIPSIS, TabBarInfo, draw_tab_bar, tab_bar_hit_test};

#[cfg(test)]
mod tests {
    use super::status_bar::{ChromeRect, abbreviate_path, windows_path_to_linux};
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
        let result = abbreviate_path("file://localhost/tmp/foo");
        assert_eq!(result, "/tmp/foo");
    }

    #[test]
    fn abbreviate_path_plain_path() {
        let result = abbreviate_path("/some/other/path");
        assert_eq!(result, "/some/other/path");
    }

    #[test]
    fn abbreviate_path_mnt_c_without_wsl2() {
        let result = abbreviate_path("/mnt/c/Users/alice/Documents");
        assert!(!result.is_empty());
    }

    // ── status_bar_hit_test ──

    fn hit_areas_with_indicator(rect: ChromeRect) -> StatusBarHitAreas {
        StatusBarHitAreas {
            agent_indicator: Some(rect),
        }
    }

    #[test]
    fn status_bar_hit_inside_agent_indicator() {
        let areas = hit_areas_with_indicator((10.0, 100.0, 80.0, 24.0));
        let hit = status_bar_hit_test(50.0, 110.0, &areas);
        assert!(matches!(hit, Some(StatusBarHit::AgentIndicator)));
    }

    #[test]
    fn status_bar_hit_inside_agent_indicator_with_zoom_offset() {
        let zoom_prefix_width = 60.0;
        let areas = hit_areas_with_indicator((10.0 + zoom_prefix_width, 100.0, 80.0, 24.0));
        assert!(status_bar_hit_test(50.0, 110.0, &areas).is_none());
        let hit = status_bar_hit_test(80.0, 110.0, &areas);
        assert!(matches!(hit, Some(StatusBarHit::AgentIndicator)));
    }

    #[test]
    fn status_bar_hit_outside_left_edge() {
        let areas = hit_areas_with_indicator((10.0, 100.0, 80.0, 24.0));
        assert!(status_bar_hit_test(9.999, 110.0, &areas).is_none());
    }

    #[test]
    fn status_bar_hit_outside_right_edge() {
        let areas = hit_areas_with_indicator((10.0, 100.0, 80.0, 24.0));
        assert!(status_bar_hit_test(90.0, 110.0, &areas).is_none());
    }

    #[test]
    fn status_bar_hit_outside_vertical_bounds() {
        let areas = hit_areas_with_indicator((10.0, 100.0, 80.0, 24.0));
        assert!(status_bar_hit_test(50.0, 99.0, &areas).is_none());
        assert!(status_bar_hit_test(50.0, 124.0, &areas).is_none());
    }

    #[test]
    fn status_bar_hit_in_bar_but_not_on_indicator() {
        let areas = hit_areas_with_indicator((10.0, 100.0, 80.0, 24.0));
        assert!(status_bar_hit_test(400.0, 110.0, &areas).is_none());
    }

    #[test]
    fn status_bar_hit_no_indicator_registered() {
        let areas = StatusBarHitAreas::default();
        assert!(areas.agent_indicator.is_none());
        assert!(status_bar_hit_test(50.0, 110.0, &areas).is_none());
    }
}
