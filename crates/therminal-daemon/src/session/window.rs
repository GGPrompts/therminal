//! Window: a container of panes within a session.

use therminal_protocol::{PaneId, WindowId};

use super::next_window_id;
use super::pane::Pane;

/// A window within a session, containing one or more panes.
pub struct Window {
    pub id: WindowId,
    pub panes: Vec<Pane>,
}

impl Window {
    pub(super) fn new() -> Self {
        Self {
            id: next_window_id(),
            panes: Vec::new(),
        }
    }

    /// Add a pane to this window.
    pub(super) fn add_pane(&mut self, pane: Pane) {
        self.panes.push(pane);
    }

    /// Find a pane by ID.
    pub fn pane(&self, pane_id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == pane_id)
    }

    /// Find a mutable pane by ID.
    pub fn pane_mut(&mut self, pane_id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == pane_id)
    }
}
