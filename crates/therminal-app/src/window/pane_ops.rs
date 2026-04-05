//! Pane operations: split, close, focus, resize, clipboard.
//!
//! All pane manipulation methods that modify the layout tree or interact
//! with pane PTYs for clipboard/selection operations.

use std::io::Write as IoWrite;

use alacritty_terminal::term::TermMode;
use tracing::{info, warn};

use crate::pane::{FocusDirection, LayoutNode, PaneId, SplitDirection};
use therminal_core::geometry::Rect;
use therminal_terminal::interceptor::InterceptorConfig;

use super::App;

impl App {
    /// Split the currently focused pane with auto-detected direction.
    pub(crate) fn split_focused_pane_auto(&mut self) {
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return,
        };
        let layout = match self.layout.as_ref() {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane(focused) {
            Some(p) => p,
            None => return,
        };
        let fallback = match self.last_split_direction {
            SplitDirection::Horizontal => SplitDirection::Vertical,
            SplitDirection::Vertical => SplitDirection::Horizontal,
        };
        let direction = LayoutNode::auto_split_direction(pane.viewport, fallback);
        self.split_focused_pane(direction);
    }

    /// Split the currently focused pane.
    pub(crate) fn split_focused_pane(&mut self, direction: SplitDirection) {
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return,
        };
        let layout = match self.layout.as_mut() {
            Some(l) => l,
            None => return,
        };
        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };
        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_1337: self.config.terminal.osc_1337,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
        };
        let proxy = self.event_proxy.clone();

        let new_id = layout.split_pane(
            focused,
            direction,
            |viewport| match crate::pane::spawn_pane(
                viewport,
                renderer,
                scrollback,
                interceptor_cfg.clone(),
                scan_interval_secs,
                &spawn_options,
                |_pane_id| {
                    let p = proxy.clone();
                    Box::new(move || {
                        let _ = p.send_event(super::UserEvent::PtyOutput);
                    })
                },
            ) {
                Ok(pane) => Some(pane),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to spawn pane for split");
                    None
                }
            },
        );

        if let Some(new_id) = new_id {
            info!("Split pane {focused} {:?} -> new pane {new_id}", direction);
            self.last_split_direction = direction;

            // Resize all panes after split.
            let gpu = self.gpu.as_ref().unwrap();
            let full_rect = Rect::new(0.0, 0.0, gpu.config.width as f32, gpu.config.height as f32);
            let layout = self.layout.as_mut().unwrap();
            let renderer = self.grid_renderer.as_ref().unwrap();
            layout.layout(full_rect);
            layout.resize_all_panes(renderer);

            // Focus the new pane.
            self.focused_pane = Some(new_id);
        }

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Close the currently focused pane.
    pub(crate) fn close_focused_pane(&mut self) {
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return,
        };

        let layout = match self.layout.as_mut() {
            Some(l) => l,
            None => return,
        };

        match layout.remove_pane(focused) {
            None => {
                // Last pane -- close the window.
                info!("Last pane closed, exiting");
                // We can't exit from here directly, but we can request the window close.
                // The next event loop iteration will handle CloseRequested.
                // Signal exit: layout=None causes exit at next RedrawRequested.
                self.focused_pane = None;
                self.layout = None;
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            Some(true) => {
                info!("Closed pane {focused}");
                // Move focus to first available pane.
                let layout = self.layout.as_mut().unwrap();
                let ids = layout.pane_ids();
                self.focused_pane = ids.first().copied();

                // Relayout.
                let gpu = self.gpu.as_ref().unwrap();
                let full_rect =
                    Rect::new(0.0, 0.0, gpu.config.width as f32, gpu.config.height as f32);
                let renderer = self.grid_renderer.as_ref().unwrap();
                layout.layout(full_rect);
                layout.resize_all_panes(renderer);

                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            Some(false) => {
                // Pane not found (shouldn't happen).
                warn!("Focused pane {focused} not found in layout");
            }
        }
    }

    /// Split a specific pane by ID.
    pub(crate) fn split_pane_by_id(&mut self, target_id: PaneId, direction: SplitDirection) {
        let layout = match self.layout.as_mut() {
            Some(l) => l,
            None => return,
        };
        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };
        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_1337: self.config.terminal.osc_1337,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
        };
        let proxy = self.event_proxy.clone();

        let new_id =
            layout.split_pane(
                target_id,
                direction,
                |viewport| match crate::pane::spawn_pane(
                    viewport,
                    renderer,
                    scrollback,
                    interceptor_cfg.clone(),
                    scan_interval_secs,
                    &spawn_options,
                    |_pane_id| {
                        let p = proxy.clone();
                        Box::new(move || {
                            let _ = p.send_event(super::UserEvent::PtyOutput);
                        })
                    },
                ) {
                    Ok(pane) => Some(pane),
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to spawn pane for split");
                        None
                    }
                },
            );

        if let Some(new_id) = new_id {
            info!(
                "Split pane {target_id} {:?} -> new pane {new_id}",
                direction
            );
            self.last_split_direction = direction;

            let gpu = self.gpu.as_ref().unwrap();
            let full_rect = Rect::new(0.0, 0.0, gpu.config.width as f32, gpu.config.height as f32);
            let layout = self.layout.as_mut().unwrap();
            let renderer = self.grid_renderer.as_ref().unwrap();
            layout.layout(full_rect);
            layout.resize_all_panes(renderer);

            self.focused_pane = Some(new_id);
        }

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Close a specific pane by ID.
    pub(crate) fn close_pane_by_id(&mut self, target_id: PaneId) {
        let layout = match self.layout.as_mut() {
            Some(l) => l,
            None => return,
        };

        match layout.remove_pane(target_id) {
            None => {
                info!("Last pane closed, exiting");
                self.focused_pane = None;
                self.layout = None;
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            Some(true) => {
                info!("Closed pane {target_id}");
                // If we closed the focused pane, move focus.
                if self.focused_pane == Some(target_id) {
                    let layout = self.layout.as_mut().unwrap();
                    let ids = layout.pane_ids();
                    self.focused_pane = ids.first().copied();
                }

                let gpu = self.gpu.as_ref().unwrap();
                let full_rect =
                    Rect::new(0.0, 0.0, gpu.config.width as f32, gpu.config.height as f32);
                let layout = self.layout.as_mut().unwrap();
                let renderer = self.grid_renderer.as_ref().unwrap();
                layout.layout(full_rect);
                layout.resize_all_panes(renderer);

                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            Some(false) => {
                warn!("Pane {target_id} not found in layout");
            }
        }
    }

    /// Move focus to the next or previous pane.
    pub(crate) fn move_focus(&mut self, direction: FocusDirection) {
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return,
        };
        let layout = match self.layout.as_ref() {
            Some(l) => l,
            None => return,
        };

        if let Some(new_id) = layout.adjacent_pane(focused, direction) {
            self.focused_pane = Some(new_id);
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    /// Adjust the split ratio around the focused pane.
    pub(crate) fn adjust_focused_ratio(&mut self, delta: f32) {
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return,
        };
        let layout = match self.layout.as_mut() {
            Some(l) => l,
            None => return,
        };

        if layout.adjust_ratio(focused, delta) {
            // Relayout.
            let gpu = self.gpu.as_ref().unwrap();
            let full_rect = Rect::new(0.0, 0.0, gpu.config.width as f32, gpu.config.height as f32);
            let renderer = self.grid_renderer.as_ref().unwrap();
            layout.layout(full_rect);
            layout.resize_all_panes(renderer);

            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    // ── Clipboard operations ───────────────────────────────────────────

    /// Copy the current selection to the clipboard (for Ctrl+Shift+C keybinding).
    pub(crate) fn copy_selection(&mut self) {
        let pane_id = match self.selection_pane.or(self.focused_pane) {
            Some(id) => id,
            None => return,
        };
        let layout = match self.layout.as_ref() {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane(pane_id) {
            Some(p) => p,
            None => return,
        };
        let term_guard = pane.term.lock();
        if let Some(text) = term_guard.selection_to_string() {
            if !text.is_empty() {
                crate::clipboard::copy_to_clipboard(&text);
            }
        }
    }

    /// Paste clipboard contents to the focused pane's PTY (with bracketed paste support).
    pub(crate) fn paste_clipboard(&mut self) {
        let text = crate::clipboard::paste_from_clipboard();
        if text.is_empty() {
            return;
        }
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return,
        };
        let mode = self.pane_term_mode(focused);
        let bracketed = mode.contains(TermMode::BRACKETED_PASTE);
        let layout = match self.layout.as_mut() {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane_mut(focused) {
            Some(p) => p,
            None => return,
        };
        if bracketed {
            let _ = pane.pty_writer.write_all(b"\x1b[200~");
        }
        let _ = pane.pty_writer.write_all(text.as_bytes());
        if bracketed {
            let _ = pane.pty_writer.write_all(b"\x1b[201~");
        }
    }

    /// Clear the active selection on all panes.
    pub(crate) fn clear_selection(&mut self) {
        if let Some(pane_id) = self.selection_pane.take() {
            if let Some(layout) = self.layout.as_mut() {
                if let Some(pane) = layout.find_pane_mut(pane_id) {
                    pane.term.lock().selection = None;
                }
            }
        }
        self.selection_in_progress = false;
    }
}
