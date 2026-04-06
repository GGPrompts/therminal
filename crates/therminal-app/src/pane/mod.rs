//! Pane layout tree and per-pane terminal state.
//!
//! Implements a binary tree of splits where each leaf is a terminal pane
//! with its own PTY, Term, and VTE parser. Supports horizontal/vertical
//! splits, focus navigation, ratio-based resize, and pane close with
//! tree rebalancing.

pub mod geometry;
pub mod layout;
pub mod spawn;
pub mod state;
pub mod workspace;

use alacritty_terminal::event::{Event as TermEvent, EventListener};

// ── Re-export canonical PaneId from protocol ────────────────────────────
pub use therminal_protocol::PaneId;

/// Direction of a split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    /// Side-by-side (left | right).
    Horizontal,
    /// Stacked (top / bottom).
    Vertical,
}

// ── EventListener for per-pane Term ─────────────────────────────────────

/// Minimal listener forwarded from each pane's Term.
#[derive(Clone)]
pub(crate) struct PaneListener;

impl EventListener for PaneListener {
    fn send_event(&self, event: TermEvent) {
        match event {
            TermEvent::Title(title) => tracing::debug!("Pane title: {title}"),
            TermEvent::Wakeup => {}
            _ => tracing::debug!("Pane event: {event:?}"),
        }
    }
}

// ── Re-exports for backward compatibility ───────────────────────────────
// All public types remain importable from `crate::pane::*`.
// Some re-exports are only consumed within submodules or tests, but we keep
// them here so external callers can use the flat `crate::pane::Foo` path.

#[allow(unused_imports)]
pub use self::geometry::{
    CSD_BUTTON_HEIGHT, CSD_BUTTON_WIDTH, CSD_TAB_BAR_HEIGHT, MIN_PANE_HEIGHT, MIN_PANE_WIDTH,
    PANE_HEADER_HEIGHT, SEPARATOR_GAP, STATUS_BAR_HEIGHT, TAB_BAR_HEIGHT, content_area_rect,
    content_area_rect_csd, effective_header_height, effective_status_bar_height,
    effective_tab_bar_height, effective_tab_bar_height_csd,
};
pub use self::layout::{FocusDirection, LayoutNode, LayoutSnapshot};
#[allow(unused_imports)]
pub use self::spawn::{PaneCallbacks, next_pane_id, spawn_pane};
#[allow(unused_imports)]
pub use self::state::{PaneState, PaneStatus, grid_size_for_rect, grid_size_for_rect_with_header};
#[allow(unused_imports)]
pub use self::workspace::{Workspace, WorkspaceManager};
