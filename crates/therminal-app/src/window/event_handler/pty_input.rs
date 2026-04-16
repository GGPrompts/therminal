//! PTY key encoding — translates winit key events to byte sequences for the PTY.

use tracing::warn;
use winit::event::KeyEvent;
use winit::keyboard::{Key, NamedKey};

use alacritty_terminal::grid::Scroll;
use therminal_terminal::input::{self, KeyCode, Modifiers as InputModifiers};

use super::super::App;

impl App {
    /// Handle a keyboard event: encode it and write to the focused pane's PTY.
    ///
    /// tn-5dpv: when the viewport is scrolled back (`display_offset > 0`),
    /// any regular keystroke that would be forwarded to the PTY also
    /// snaps the viewport to the live bottom, matching the behavior of
    /// Alacritty, Kitty, and other modern terminals.
    pub(super) fn handle_key_input(&mut self, key_event: &KeyEvent) {
        // tn-5dpv: snap to bottom on any PTY-bound keystroke while scrolled back.
        // TODO: [code-review] This fires before key_code is resolved, so modifier-only
        // presses (Shift, Ctrl, Alt alone) trigger a scroll snap — consider moving this
        // block after the `let key_code = match key_code { ... }` guard below (85%)
        if self.focused_pane_is_scrolled_back() {
            self.scroll_focused_pane(Scroll::Bottom);
        }

        let focused = match self.workspaces.as_ref().and_then(|wm| wm.focused_pane()) {
            Some(id) => id,
            None => return,
        };
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane_mut(focused) {
            Some(p) => p,
            None => return,
        };

        // tn-s5vj: WebView panes handle their own input through the
        // platform's native input system. Skip PTY encoding.
        if pane.is_webview() {
            return;
        }

        let key_code = match &key_event.logical_key {
            Key::Named(named) => match named {
                NamedKey::Enter => Some(KeyCode::Enter),
                NamedKey::Backspace => Some(KeyCode::Backspace),
                NamedKey::Tab => Some(KeyCode::Tab),
                NamedKey::Escape => Some(KeyCode::Escape),
                NamedKey::ArrowUp => Some(KeyCode::ArrowUp),
                NamedKey::ArrowDown => Some(KeyCode::ArrowDown),
                NamedKey::ArrowLeft => Some(KeyCode::ArrowLeft),
                NamedKey::ArrowRight => Some(KeyCode::ArrowRight),
                NamedKey::Home => Some(KeyCode::Home),
                NamedKey::End => Some(KeyCode::End),
                NamedKey::PageUp => Some(KeyCode::PageUp),
                NamedKey::PageDown => Some(KeyCode::PageDown),
                NamedKey::Insert => Some(KeyCode::Insert),
                NamedKey::Delete => Some(KeyCode::Delete),
                NamedKey::Space => Some(KeyCode::Char(' ')),
                NamedKey::F1 => Some(KeyCode::F1),
                NamedKey::F2 => Some(KeyCode::F2),
                NamedKey::F3 => Some(KeyCode::F3),
                NamedKey::F4 => Some(KeyCode::F4),
                NamedKey::F5 => Some(KeyCode::F5),
                NamedKey::F6 => Some(KeyCode::F6),
                NamedKey::F7 => Some(KeyCode::F7),
                NamedKey::F8 => Some(KeyCode::F8),
                NamedKey::F9 => Some(KeyCode::F9),
                NamedKey::F10 => Some(KeyCode::F10),
                NamedKey::F11 => Some(KeyCode::F11),
                NamedKey::F12 => Some(KeyCode::F12),
                _ => None,
            },
            Key::Character(s) => s.chars().next().map(KeyCode::Char),
            _ => None,
        };

        let key_code = match key_code {
            Some(k) => k,
            None => return,
        };

        let state = self.modifiers.state();
        let mods = InputModifiers {
            ctrl: state.control_key(),
            alt: state.alt_key(),
            shift: state.shift_key(),
        };

        if let Some(bytes) = input::encode_key(&key_code, &mods)
            && let Err(e) = pane.write_input(&bytes)
        {
            warn!("Failed to write to pane {} PTY: {e}", pane.id);
        }
    }
}
