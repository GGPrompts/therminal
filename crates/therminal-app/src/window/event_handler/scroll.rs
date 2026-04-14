//! Viewport scroll helpers for the focused pane (tn-5dpv).

use alacritty_terminal::grid::Scroll;

use super::super::App;

impl App {
    /// tn-5dpv: scroll the focused pane's viewport by the given `Scroll` command.
    /// No-op if the focused pane has no terminal backend.
    pub(super) fn scroll_focused_pane(&mut self, scroll: Scroll) {
        let term = {
            let focused = match self.workspaces.as_ref().and_then(|wm| wm.focused_pane()) {
                Some(id) => id,
                None => return,
            };
            let layout = match self.get_layout() {
                Some(l) => l,
                None => return,
            };
            let pane = match layout.find_pane(focused) {
                Some(p) => p,
                None => return,
            };
            match pane.backend.term() {
                Some(t) => std::sync::Arc::clone(t),
                None => return,
            }
        };
        let mut term_guard = term.lock();
        term_guard.scroll_display(scroll);
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// tn-5dpv: check if the focused pane is currently scrolled back
    /// (display_offset > 0). Returns false if there is no focused terminal pane.
    pub(super) fn focused_pane_is_scrolled_back(&self) -> bool {
        let focused = match self.workspaces.as_ref().and_then(|wm| wm.focused_pane()) {
            Some(id) => id,
            None => return false,
        };
        let layout = match self.get_layout() {
            Some(l) => l,
            None => return false,
        };
        let pane = match layout.find_pane(focused) {
            Some(p) => p,
            None => return false,
        };
        let term = match pane.backend.term() {
            Some(t) => t,
            None => return false,
        };
        let term_guard = term.lock();
        term_guard.grid().display_offset() > 0
    }
}
