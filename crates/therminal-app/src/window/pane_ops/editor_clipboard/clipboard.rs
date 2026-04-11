//! Clipboard operations: copy_selection, paste_clipboard, clear_selection.

use crate::window::App;

use super::super::normalize_paste_text;

impl App {
    /// Copy the current selection to the clipboard (for Ctrl+Shift+C keybinding).
    pub(crate) fn copy_selection(&mut self) {
        let pane_id = match self.selection_pane.or(self.focused_pane()) {
            Some(id) => id,
            None => return,
        };
        let layout = match self.get_layout() {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane(pane_id) {
            Some(p) => p,
            None => return,
        };
        if let Some(term) = pane.backend.term() {
            let term_guard = term.lock();
            if let Some(text) = term_guard.selection_to_string()
                && !text.is_empty()
            {
                crate::clipboard::copy_to_clipboard(&text);
            }
        }
    }

    /// Paste clipboard contents to the focused pane's PTY.
    ///
    /// Always wraps the payload with the bracketed-paste envelope
    /// (`\e[200~` ... `\e[201~`) regardless of the locally-tracked
    /// `TermMode::BRACKETED_PASTE` flag. The local flag is unreliable in
    /// daemon-client mode (tn-b77d): the GUI's `Term` is bootstrapped from
    /// a one-shot snapshot and never sees subsequent `\e[?2004h` mode-set
    /// sequences emitted by TUIs after attach. Modern TUIs (Claude Code,
    /// vim, helix, less, micro, fish) handle the envelope correctly even
    /// when they didn't request it; legacy line editors that don't
    /// recognize the markers display them as harmless garbage at worst,
    /// which is strictly better than the current bug where every embedded
    /// `\n` is interpreted as Enter and submits per line.
    ///
    /// Clipboard text is also normalized: `\r\n` and bare `\r` collapse to
    /// `\n` to avoid TUIs treating CR as a submit (common when pasting
    /// from Windows-origin clipboards on WSL2).
    ///
    /// See tn-5akk (the paste symptom) and tn-b77d (the underlying
    /// mode-flag drift in tn-382v Phase B).
    pub(crate) fn paste_clipboard(&mut self) {
        let raw = crate::clipboard::paste_from_clipboard();
        if raw.is_empty() {
            return;
        }
        let text = normalize_paste_text(&raw);
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        let layout = match self.get_layout_mut() {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane_mut(focused) {
            Some(p) => p,
            None => return,
        };
        if let Err(e) = pane.write_input(b"\x1b[200~") {
            tracing::warn!("paste write failed: {e}");
        }
        if let Err(e) = pane.write_input(text.as_bytes()) {
            tracing::warn!("paste write failed: {e}");
        }
        if let Err(e) = pane.write_input(b"\x1b[201~") {
            tracing::warn!("paste write failed: {e}");
        }
    }

    /// Clear the active selection on all panes.
    pub(crate) fn clear_selection(&mut self) {
        if let Some(pane_id) = self.selection_pane.take()
            && let Some(layout) = self.get_layout_mut()
            && let Some(pane) = layout.find_pane_mut(pane_id)
            && let Some(term) = pane.backend.term()
        {
            term.lock().selection = None;
        }
        self.selection_in_progress = false;
    }
}
