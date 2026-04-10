//! Pane layout tree and per-pane state.
//!
//! Implements a binary tree of splits where each leaf is a pane with a
//! pluggable backend (terminal, webview, etc.). Supports horizontal/vertical
//! splits, focus navigation, ratio-based resize, and pane close with
//! tree rebalancing.

pub mod auto_tile;
pub mod backend;
pub mod geometry;
pub mod layout;
pub mod remote_spawn;
pub mod spawn;
pub mod state;
pub mod swarm_watcher;
pub mod workspace;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Listener forwarded from each pane's Term.
///
/// Carries an optional bell flag that the reader thread checks after each
/// `advance_with_interceptor` call to detect BEL events without needing
/// a full channel.
#[derive(Clone)]
pub(crate) struct PaneListener {
    /// Set to `true` when `Event::Bell` fires. The reader thread clears
    /// it after forwarding the bell to the event loop.
    pub(crate) bell_pending: Arc<AtomicBool>,
}

impl PaneListener {
    /// Create a new listener with a shared bell flag.
    pub(crate) fn new() -> Self {
        Self {
            bell_pending: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl EventListener for PaneListener {
    fn send_event(&self, event: TermEvent) {
        match event {
            TermEvent::Title(title) => tracing::debug!("Pane title: {title}"),
            TermEvent::Wakeup => {}
            TermEvent::Bell => {
                self.bell_pending.store(true, Ordering::Release);
            }
            _ => tracing::debug!("Pane event: {event:?}"),
        }
    }
}

// ── Re-exports for backward compatibility ───────────────────────────────
// All public types remain importable from `crate::pane::*`.
// Some re-exports are only consumed within submodules or tests, but we keep
// them here so external callers can use the flat `crate::pane::Foo` path.

#[allow(unused_imports)]
pub use self::auto_tile::{AutoTileAction, AutoTileDebouncer};
#[allow(unused_imports)]
pub use self::backend::{PaneBackend, PaneBackendKind};
#[allow(unused_imports)]
pub use self::geometry::{
    CSD_BUTTON_COUNT, CSD_BUTTON_HEIGHT, CSD_BUTTON_WIDTH, CSD_BUTTONS_TOTAL_WIDTH,
    CSD_TAB_BAR_HEIGHT, MIN_PANE_HEIGHT, MIN_PANE_WIDTH, PANE_HEADER_HEIGHT, SEPARATOR_GAP,
    STATUS_BAR_HEIGHT, TAB_BAR_HEIGHT, content_area_rect, content_area_rect_csd,
    effective_header_height, effective_status_bar_height, effective_tab_bar_height,
    effective_tab_bar_height_csd,
};
pub use self::layout::{FocusDirection, LayoutNode, LayoutSnapshot, SpatialDirection};
#[allow(unused_imports)]
pub use self::spawn::{PaneCallbacks, next_pane_id, spawn_pane};
#[allow(unused_imports)]
pub use self::state::{PaneState, PaneStatus, grid_size_for_rect, grid_size_for_rect_with_header};
#[allow(unused_imports)]
pub use self::workspace::{PaneRemoveResult, Workspace, WorkspaceManager};
