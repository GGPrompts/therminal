//! TUI pages — each tab in the dashboard implements `TuiPage`.

pub mod agents;
pub mod panes;
pub mod sessions;

use ratatui::{Frame, layout::Rect};

use crate::backend::DaemonBackend;

/// Result from a key event handler.
#[derive(Default)]
pub struct KeyResult {
    /// The app should quit.
    pub quit: bool,
}

impl KeyResult {
    pub const NONE: Self = Self { quit: false };
    #[allow(dead_code)]
    pub const QUIT: Self = Self { quit: true };
}

/// Trait for a TUI page/tab.
pub trait TuiPage {
    /// Tab title shown in the tab bar.
    fn title(&self) -> &str;

    /// Called from the event loop on each iteration. Pages MUST throttle
    /// their own work (the loop calls this at the input-poll cadence,
    /// which can be much faster than once per second). Return `true` if
    /// any rendered state changed and a redraw should happen, `false` if
    /// the call was a no-op. This drives the loop's dirty flag — an
    /// always-true return defeats the redraw gate and brings the
    /// flicker back.
    fn tick(&mut self, backend: &DaemonBackend) -> bool;

    /// Render the page into the given area.
    fn render(&mut self, f: &mut Frame, area: Rect);

    /// Handle a key event.
    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> KeyResult;

    /// Handle a mouse event.
    fn handle_mouse(&mut self, event: crossterm::event::MouseEvent);

    /// Whether focus is currently on a text input field (suppresses global hotkeys).
    fn has_text_focus(&self) -> bool {
        false
    }
}
