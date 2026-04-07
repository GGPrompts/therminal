//! Pane operations: split, close, focus, resize, clipboard, workspace.
//!
//! All pane manipulation methods that modify the layout tree or interact
//! with pane PTYs for clipboard/selection operations.

use std::sync::Arc;

use alacritty_terminal::term::TermMode;
use tracing::{debug, info, warn};

use crate::pane::{
    FocusDirection, LayoutNode, LayoutSnapshot, PaneCallbacks, PaneId, PaneRemoveResult,
    SpatialDirection, SplitDirection,
};
use therminal_core::geometry::Rect;
use therminal_terminal::interceptor::InterceptorConfig;

use super::{App, EventLoopProxy, NotificationSource, UserEvent};

/// Build `PaneCallbacks` from an event-loop proxy.
fn make_pane_callbacks(proxy: &EventLoopProxy<UserEvent>, pane_id: PaneId) -> PaneCallbacks {
    let p1 = proxy.clone();
    let p2 = proxy.clone();
    let p3 = proxy.clone();
    let p4 = proxy.clone();
    PaneCallbacks {
        wake: Box::new(move || {
            let _ = p1.send_event(UserEvent::PtyOutput);
        }),
        on_exit: Box::new(move || {
            let _ = p2.send_event(UserEvent::PaneExited(pane_id));
        }),
        on_bell: Box::new(move || {
            let _ = p3.send_event(UserEvent::Bell(pane_id));
        }),
        on_notification: Box::new(move |text| {
            let _ = p4.send_event(UserEvent::DesktopNotification {
                title: "Therminal".to_string(),
                body: text,
                source: NotificationSource::Osc9,
            });
        }),
    }
}

impl App {
    /// Split the currently focused pane with auto-detected direction.
    pub(crate) fn split_focused_pane_auto(&mut self) {
        // Restore layout before splitting so the new pane joins the full tree.
        if self.zoomed_layout.is_some() {
            self.zoom_toggle_focused_pane();
        }
        let focused = match self.focused_pane() {
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
        let fallback = match self.last_split_direction {
            SplitDirection::Horizontal => SplitDirection::Vertical,
            SplitDirection::Vertical => SplitDirection::Horizontal,
        };
        let direction = LayoutNode::auto_split_direction(pane.viewport, fallback);
        self.split_focused_pane(direction);
    }

    /// Split the currently focused pane.
    pub(crate) fn split_focused_pane(&mut self, direction: SplitDirection) {
        let focused = match self.focused_pane() {
            Some(id) => id,
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
            osc_9: self.config.terminal.osc_9,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
            ..Default::default()
        };
        let proxy = self.event_proxy.clone();
        let registry = Some(Arc::clone(&self.agent_registry));
        // Direct field access needed here: layout_mut + renderer + config must coexist.
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };

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
                registry.clone(),
                |pane_id| make_pane_callbacks(&proxy, pane_id),
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
            self.set_focused_pane(Some(new_id));
            self.relayout_and_redraw();
        } else {
            self.request_redraw();
        }
    }

    /// Open a horizontal split running `tail -F` on the focused pane's
    /// agent event log JSONL file.
    ///
    /// Triggered by clicking the `[agent: <name>]` indicator in the status
    /// bar. The new pane is small and narrow (horizontal split) so it acts
    /// as a side panel without dominating the layout.
    pub(crate) fn open_focused_agent_event_log_tail(&mut self) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => {
                debug!("open_focused_agent_event_log_tail: no focused pane");
                return;
            }
        };

        // The session_id used for event logs corresponds 1:1 with the pane
        // id in this single-process app. The daemon uses the same naming
        // scheme, so this matches if/when the daemon is also writing logs.
        let session_id = format!("pane-{focused}");
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
            let user = std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "unknown".to_string());
            format!("/tmp/therminal-{user}")
        });
        let log_path = std::path::PathBuf::from(runtime_dir)
            .join("therminal")
            .join("sessions")
            .join(format!("{session_id}.events.jsonl"));
        let log_path_str = log_path.to_string_lossy().into_owned();

        info!(
            "Opening agent event log tail pane for pane {} at {}",
            focused, log_path_str
        );

        // Horizontal split keeps the tail pane narrow (top/bottom layout).
        self.split_focused_pane(SplitDirection::Horizontal);

        // After split, the new pane is focused. Send the tail command.
        let new_pane = match self.focused_pane() {
            Some(id) if id != focused => id,
            _ => {
                warn!("open_focused_agent_event_log_tail: split did not produce a new pane");
                return;
            }
        };

        // `tail -F` follows file rotation/recreation and tolerates a
        // non-existent file (it will retry until the file appears).
        let cmd = format!("tail -F {log_path_str}\n");
        self.pty_write_to_pane(cmd.as_bytes(), new_pane);
    }

    /// Close the currently focused pane.
    ///
    /// Includes a 100ms cooldown to prevent double-close from keyboard repeat
    /// firing two events in the same winit event batch.
    pub(crate) fn close_focused_pane(&mut self) {
        if let Some(last) = self.last_close_action
            && last.elapsed() < std::time::Duration::from_millis(100)
        {
            debug!("close_focused_pane: debounced (< 100ms since last close)");
            return;
        }
        self.last_close_action = Some(std::time::Instant::now());

        // If zoomed, restore the full layout before closing so the tree is intact.
        if self.zoomed_layout.is_some() {
            self.zoom_toggle_focused_pane();
        }

        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };

        // Use remove_pane_any which searches all workspaces and handles cleanup.
        let wm = match self.workspaces.as_mut() {
            Some(wm) => wm,
            None => return,
        };

        match wm.remove_pane_any(focused) {
            PaneRemoveResult::LastInWorkspace => {
                if wm.gc_empty_workspaces() {
                    // Switched to another workspace that still has panes.
                    info!(
                        "Last pane in workspace closed, switched to workspace {}",
                        wm.active_id()
                    );
                    let focus = wm.focused_pane();
                    self.set_focused_pane(focus);
                    self.relayout_and_redraw();
                } else {
                    // Truly the last pane across all workspaces.
                    info!("Last pane closed, exiting");
                    self.set_focused_pane(None);
                    self.workspaces = None;
                    self.request_redraw();
                }
            }
            PaneRemoveResult::Removed => {
                info!("Closed pane {focused}");
                // Move focus to first available pane.
                let new_focus = self
                    .get_layout()
                    .map(|l| l.pane_ids())
                    .and_then(|ids| ids.first().copied());
                self.set_focused_pane(new_focus);
                self.relayout_and_redraw();
            }
            PaneRemoveResult::NotFound => {
                // Pane not found (shouldn't happen for focused pane).
                warn!("Focused pane {focused} not found in layout");
            }
        }
    }

    /// Toggle zoom on the focused pane.
    ///
    /// When not zoomed: saves the current layout tree (with the focused pane
    /// extracted), replaces the workspace layout with a single leaf containing
    /// only the focused pane, and stores the saved layout for later restore.
    ///
    /// When zoomed: restores the saved layout tree, re-inserting the zoomed
    /// pane back into its original position.
    pub(crate) fn zoom_toggle_focused_pane(&mut self) {
        if self.zoomed_layout.is_some() {
            // ── Unzoom: restore saved layout ────────────────────────────
            let wm = match self.workspaces.as_mut() {
                Some(wm) => wm,
                None => return,
            };

            // Take the current single-leaf layout (the zoomed pane).
            let zoomed_leaf = wm.take_layout();
            let pane = match zoomed_leaf {
                LayoutNode::Leaf(p) => p,
                _ => {
                    warn!("zoom_toggle: expected Leaf in zoomed layout");
                    // Put it back if something went wrong.
                    wm.set_layout(zoomed_leaf);
                    return;
                }
            };

            let pane_id = pane.id;

            // Put the pane back into the saved layout at the Empty slot.
            let mut saved = self.zoomed_layout.take().unwrap();
            if saved.insert_pane_at_empty(pane).is_some() {
                warn!("zoom_toggle: no Empty slot found in saved layout, pane lost");
            }

            // Restore the full layout.
            let Some(wm) = self.workspaces.as_mut() else {
                return;
            };
            wm.set_layout(saved);
            wm.set_focused_pane(Some(pane_id));
            info!("Unzoomed pane {pane_id}");
            self.relayout_and_redraw();
        } else {
            // ── Zoom: save layout, show only focused pane ───────────────
            let focused = match self.focused_pane() {
                Some(id) => id,
                None => return,
            };

            let wm = match self.workspaces.as_mut() {
                Some(wm) => wm,
                None => return,
            };

            // Only zoom if there are multiple panes.
            if wm.layout().pane_count() <= 1 {
                debug!("zoom_toggle: only one pane, nothing to zoom");
                return;
            }

            // Take the full layout, extract the focused pane leaf.
            let mut full_layout = wm.take_layout();
            let pane = match full_layout.extract_pane(focused) {
                Some(p) => p,
                None => {
                    warn!("zoom_toggle: focused pane {focused} not found in layout");
                    wm.set_layout(full_layout);
                    return;
                }
            };

            // Store the (now-holey) layout for later restore.
            self.zoomed_layout = Some(full_layout);

            // Set the workspace to just this pane.
            let Some(wm) = self.workspaces.as_mut() else {
                return;
            };
            wm.set_layout(LayoutNode::Leaf(pane));
            wm.set_focused_pane(Some(focused));
            info!("Zoomed pane {focused}");
            self.relayout_and_redraw();
        }
    }

    /// Split a specific pane by ID.
    pub(crate) fn split_pane_by_id(&mut self, target_id: PaneId, direction: SplitDirection) {
        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };
        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_9: self.config.terminal.osc_9,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
            ..Default::default()
        };
        let proxy = self.event_proxy.clone();
        let registry = Some(Arc::clone(&self.agent_registry));
        // Direct field access needed here: layout_mut + renderer + config must coexist.
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };

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
                    registry.clone(),
                    |pane_id| make_pane_callbacks(&proxy, pane_id),
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
            self.set_focused_pane(Some(new_id));
            self.relayout_and_redraw();
        } else {
            self.request_redraw();
        }
    }

    /// Close a specific pane by ID.
    ///
    /// Includes a 100ms cooldown to prevent double-close from keyboard repeat.
    pub(crate) fn close_pane_by_id(&mut self, target_id: PaneId) {
        if let Some(last) = self.last_close_action
            && last.elapsed() < std::time::Duration::from_millis(100)
        {
            debug!("close_pane_by_id: debounced (< 100ms since last close)");
            return;
        }
        self.last_close_action = Some(std::time::Instant::now());

        // If zoomed, restore the full layout so tree removal works correctly.
        if self.zoomed_layout.is_some() {
            self.zoom_toggle_focused_pane();
        }

        let wm = match self.workspaces.as_mut() {
            Some(wm) => wm,
            None => {
                warn!(
                    target_id,
                    "close_pane_by_id: no workspaces (already torn down?)"
                );
                return;
            }
        };

        let pane_count_before = wm.total_pane_count();
        info!(
            target_id,
            pane_count_before,
            focused = ?wm.focused_pane(),
            "close_pane_by_id called"
        );

        // Search all workspaces for the pane, not just the active one.
        match wm.remove_pane_any(target_id) {
            PaneRemoveResult::LastInWorkspace => {
                // Last pane in some workspace — check if others remain.
                if wm.total_pane_count() == 0 && !wm.gc_empty_workspaces() {
                    // Truly the last pane across all workspaces.
                    info!("Last pane closed, exiting");
                    self.set_focused_pane(None);
                    self.workspaces = None;
                    self.request_redraw();
                } else {
                    // Other workspaces have panes; clean up the empty one.
                    wm.gc_empty_workspaces();
                    info!(
                        "Pane {target_id} was last in its workspace, switched to workspace {}",
                        wm.active_id()
                    );
                    // Update focused pane from the now-active workspace.
                    let focus = wm.focused_pane();
                    self.set_focused_pane(focus);
                    self.relayout_and_redraw();
                }
            }
            PaneRemoveResult::Removed => {
                let pane_count_after = wm.total_pane_count();
                info!(
                    target_id,
                    pane_count_before, pane_count_after, "Closed pane"
                );
                // If we closed the focused pane of the active workspace, move focus.
                if self.focused_pane() == Some(target_id) {
                    let new_focus = self
                        .get_layout()
                        .map(|l| l.pane_ids())
                        .and_then(|ids| ids.first().copied());
                    self.set_focused_pane(new_focus);
                }
                self.relayout_and_redraw();
            }
            PaneRemoveResult::NotFound => {
                warn!(
                    target_id,
                    pane_count_before,
                    "Pane not found in any workspace (double-close or stale event?)"
                );
            }
        }
    }

    /// Move focus to the next or previous pane (cycling order).
    pub(crate) fn move_focus(&mut self, direction: FocusDirection) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        let layout = match self.get_layout() {
            Some(l) => l,
            None => return,
        };

        if let Some(new_id) = layout.adjacent_pane(focused, direction) {
            self.set_focused_pane(Some(new_id));
            self.request_redraw();
        }
    }

    /// Move focus to the nearest pane in a spatial direction.
    pub(crate) fn move_focus_spatial(&mut self, direction: SpatialDirection) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        let layout = match self.get_layout() {
            Some(l) => l,
            None => return,
        };

        if let Some(new_id) = layout.spatial_adjacent_pane(focused, direction) {
            self.set_focused_pane(Some(new_id));
            self.request_redraw();
        }
    }

    /// Swap the focused pane with the adjacent pane in the given direction.
    /// Focus follows the moved pane (stays on the same pane ID).
    pub(crate) fn swap_focused_pane(&mut self, direction: FocusDirection) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        let layout = match self.get_layout_mut() {
            Some(l) => l,
            None => return,
        };

        if let Some(target_id) = layout.adjacent_pane(focused, direction)
            && layout.swap_pane(focused, target_id)
        {
            // Focus stays on the original pane ID (it moved to the new position).
            self.request_redraw();
        }
    }

    /// Adjust the split ratio around the focused pane.
    pub(crate) fn adjust_focused_ratio(&mut self, delta: f32) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };

        // Compute rect before the mutable borrow of workspaces via layout.
        let full_rect = match self.compute_layout_rect() {
            Some(r) => r,
            None => return,
        };

        // Direct field access needed: layout_mut + grid_renderer must coexist.
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };

        if layout.adjust_ratio(focused, delta) {
            layout.layout(full_rect);
            if let Some(renderer) = self.grid_renderer.as_ref() {
                layout.resize_all_panes(renderer);
            }
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    /// Reset all split ratios to 50/50.
    pub(crate) fn reset_all_ratios(&mut self) {
        let full_rect = match self.compute_layout_rect() {
            Some(r) => r,
            None => return,
        };

        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };

        layout.reset_all_ratios();
        layout.layout(full_rect);
        if let Some(renderer) = self.grid_renderer.as_ref() {
            layout.resize_all_panes(renderer);
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    // ── Clipboard operations ───────────────────────────────────────────

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

    /// Paste clipboard contents to the focused pane's PTY (with bracketed paste support).
    pub(crate) fn paste_clipboard(&mut self) {
        let text = crate::clipboard::paste_from_clipboard();
        if text.is_empty() {
            return;
        }
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        let mode = self.pane_term_mode(focused);
        let bracketed = mode.contains(TermMode::BRACKETED_PASTE);
        let layout = match self.get_layout_mut() {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane_mut(focused) {
            Some(p) => p,
            None => return,
        };
        if bracketed && let Err(e) = pane.write_input(b"\x1b[200~") {
            tracing::warn!("paste write failed: {e}");
        }
        if let Err(e) = pane.write_input(text.as_bytes()) {
            tracing::warn!("paste write failed: {e}");
        }
        if bracketed && let Err(e) = pane.write_input(b"\x1b[201~") {
            tracing::warn!("paste write failed: {e}");
        }
    }

    /// Open a file path in the user's `$EDITOR` or via `xdg-open` / `open`.
    ///
    /// The path may include `:line` or `:line:col` suffixes. If `$EDITOR` supports
    /// `+line` syntax (vim, nvim, nano, code, etc.), we pass it; otherwise we
    /// fall back to `xdg-open` / `open` with just the file path.
    pub(crate) fn open_in_editor(&self, path_with_loc: &str) {
        use std::process::Command;

        // Split path from optional :line:col.
        let (path, line) = match path_with_loc.find(':') {
            Some(idx) if path_with_loc[idx + 1..].starts_with(|c: char| c.is_ascii_digit()) => {
                let rest = &path_with_loc[idx + 1..];
                let line_str = rest.split(':').next().unwrap_or("1");
                (&path_with_loc[..idx], line_str)
            }
            _ => (path_with_loc, "1"),
        };

        // Validate hotspot is a real file before spawning editor — hotspot paths
        // come from terminal screen content and may be attacker-controlled.
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => {
                tracing::warn!("open_in_editor: {path} is not a regular file, skipping");
                return;
            }
            Err(e) => {
                tracing::warn!("open_in_editor: cannot stat {path}: {e}, skipping");
                return;
            }
        }

        if let Ok(editor) = std::env::var("EDITOR") {
            // Many editors support +line syntax.
            let arg = format!("+{line}");
            match Command::new(&editor).arg(&arg).arg(path).spawn() {
                Ok(_) => return,
                Err(e) => {
                    tracing::warn!("failed to launch $EDITOR ({editor}): {e}");
                }
            }
        }

        // Fallback: xdg-open / open.
        if let Err(e) = open::that(path) {
            tracing::warn!("failed to open {path}: {e}");
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

    // ── Batch pane operations ─────────────────────────────────────────

    /// Close all panes in the current workspace.
    ///
    /// If other workspaces still have panes, removes the now-empty workspace
    /// and switches to the nearest one. Only exits the app when no panes
    /// remain across all workspaces.
    pub(crate) fn close_all_panes(&mut self) {
        if let Some(last) = self.last_close_action
            && last.elapsed() < std::time::Duration::from_millis(100)
        {
            debug!("close_all_panes: debounced (< 100ms since last close)");
            return;
        }
        self.last_close_action = Some(std::time::Instant::now());

        let wm = match self.workspaces.as_mut() {
            Some(wm) => wm,
            None => return,
        };

        // Drop the active workspace's layout (kills all PTYs in this tab).
        let layout = wm.take_layout();
        drop(layout);

        if wm.gc_empty_workspaces() {
            // Other workspaces have panes — switch to one.
            info!(
                "Closed all panes in workspace, switched to workspace {}",
                wm.active_id()
            );
            let focus = wm.focused_pane();
            self.set_focused_pane(focus);
            self.relayout_and_redraw();
        } else {
            // No panes anywhere — exit.
            info!("Closed all panes, exiting");
            self.set_focused_pane(None);
            self.workspaces = None;
            self.request_redraw();
        }

        self.selection_pane = None;
        self.selection_in_progress = false;
    }

    /// Spawn N panes with auto-tiling layout.
    /// Creates panes one at a time using the existing split infrastructure,
    /// with a single relayout at the end.
    #[allow(dead_code)]
    pub(crate) fn spawn_n_panes(&mut self, n: usize) {
        if n == 0 {
            return;
        }

        if self.workspaces.is_none() {
            info!("No layout exists, cannot spawn panes without initial setup");
            return;
        }

        for _ in 0..n {
            self.split_focused_pane_auto();
        }

        info!("Spawned {n} additional panes via auto-split");
    }

    /// Restore a previously saved layout by respawning panes to match the snapshot.
    pub(crate) fn restore_layout(&mut self) {
        let snapshot = match self
            .workspaces
            .as_mut()
            .and_then(|wm| wm.take_saved_layout())
        {
            Some(s) => s,
            None => {
                info!("No saved layout to restore");
                return;
            }
        };

        let leaf_count = LayoutNode::snapshot_leaf_count(&snapshot);
        if leaf_count == 0 {
            return;
        }

        // If there's already a layout, close it first (no re-snapshot).
        if self.get_layout().is_some() {
            let layout = self.workspaces.as_mut().unwrap().take_layout();
            drop(layout);
            self.set_focused_pane(None);
        }

        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };
        let full_rect = match self.compute_layout_rect() {
            Some(r) => r,
            None => return,
        };

        // Rebuild the layout tree from the snapshot by spawning new panes.
        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_9: self.config.terminal.osc_9,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
            ..Default::default()
        };
        let proxy = self.event_proxy.clone();

        match self.rebuild_from_snapshot(
            &snapshot,
            full_rect,
            renderer,
            scrollback,
            &interceptor_cfg,
            scan_interval_secs,
            &spawn_options,
            &proxy,
        ) {
            Some(node) => {
                if let Some(wm) = self.workspaces.as_mut() {
                    wm.set_layout(node);
                }
                // Focus the first pane (must read IDs before relayout borrows layout).
                let first_id = self
                    .get_layout()
                    .map(|l| l.pane_ids())
                    .and_then(|ids| ids.first().copied());
                let pane_count = self.get_layout().map(|l| l.pane_ids().len()).unwrap_or(0);
                self.set_focused_pane(first_id);
                self.relayout_and_redraw();

                info!(panes = pane_count, "Restored layout from snapshot");
            }
            None => {
                warn!("Failed to restore layout from snapshot");
            }
        }
    }

    /// Recursively rebuild a LayoutNode tree from a snapshot.
    #[allow(clippy::too_many_arguments)]
    fn rebuild_from_snapshot(
        &self,
        snapshot: &LayoutSnapshot,
        rect: Rect,
        renderer: &crate::grid_renderer::GridRenderer,
        scrollback: usize,
        interceptor_cfg: &InterceptorConfig,
        scan_interval_secs: u64,
        spawn_options: &therminal_terminal::pty::SpawnOptions,
        proxy: &super::EventLoopProxy<super::UserEvent>,
    ) -> Option<LayoutNode> {
        use crate::pane::SEPARATOR_GAP;

        match snapshot {
            LayoutSnapshot::Leaf => {
                let cfg = interceptor_cfg.clone();
                let registry = Some(Arc::clone(&self.agent_registry));
                match crate::pane::spawn_pane(
                    rect,
                    renderer,
                    scrollback,
                    cfg,
                    scan_interval_secs,
                    spawn_options,
                    registry,
                    |pane_id| make_pane_callbacks(proxy, pane_id),
                ) {
                    Ok(pane) => Some(LayoutNode::Leaf(pane)),
                    Err(e) => {
                        warn!(error = %e, "failed to spawn pane during layout restore");
                        None
                    }
                }
            }
            LayoutSnapshot::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let (r1, r2) = match direction {
                    SplitDirection::Horizontal => {
                        rect.split_horizontal_ratio(*ratio, SEPARATOR_GAP)
                    }
                    SplitDirection::Vertical => rect.split_vertical_ratio(*ratio, SEPARATOR_GAP),
                };

                let first_node = self.rebuild_from_snapshot(
                    first,
                    r1,
                    renderer,
                    scrollback,
                    interceptor_cfg,
                    scan_interval_secs,
                    spawn_options,
                    proxy,
                )?;
                let second_node = self.rebuild_from_snapshot(
                    second,
                    r2,
                    renderer,
                    scrollback,
                    interceptor_cfg,
                    scan_interval_secs,
                    spawn_options,
                    proxy,
                )?;

                Some(LayoutNode::Split {
                    direction: *direction,
                    ratio: *ratio,
                    first: Box::new(first_node),
                    second: Box::new(second_node),
                })
            }
        }
    }

    // ── Workspace operations ──────────────────────────────────────────

    /// Switch to workspace `n` (1-9).
    pub(crate) fn switch_workspace(&mut self, n: u8) {
        // Restore layout before switching so the saved tree goes back to the
        // current workspace, not the target.
        if self.zoomed_layout.is_some() {
            self.zoom_toggle_focused_pane();
        }

        let full_rect = match self.compute_layout_rect() {
            Some(r) => r,
            None => return,
        };
        let wm = match self.workspaces.as_mut() {
            Some(wm) => wm,
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
            osc_9: self.config.terminal.osc_9,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
            ..Default::default()
        };
        let proxy = self.event_proxy.clone();
        let registry = Some(Arc::clone(&self.agent_registry));

        let switched = wm.switch_to(n as usize, || {
            match crate::pane::spawn_pane(
                full_rect,
                renderer,
                scrollback,
                interceptor_cfg.clone(),
                scan_interval_secs,
                &spawn_options,
                registry.clone(),
                |pane_id| make_pane_callbacks(&proxy, pane_id),
            ) {
                Ok(pane) => {
                    let id = pane.id;
                    Some((LayoutNode::Leaf(pane), id))
                }
                Err(e) => {
                    warn!(error = %e, "failed to spawn pane for new workspace");
                    None
                }
            }
        });

        if switched {
            info!("Switched to workspace {n}");
            self.relayout_and_redraw();
        }
    }

    /// Create a new workspace tab by finding the next unused slot (1-9).
    pub(crate) fn create_new_workspace(&mut self) {
        let existing = self
            .workspaces
            .as_ref()
            .map(|wm| wm.workspace_ids())
            .unwrap_or_default();
        // Find the lowest unused workspace ID in 1..=9.
        let next_id = (1..=9u8).find(|n| !existing.contains(&(*n as usize)));
        match next_id {
            Some(n) => self.switch_workspace(n),
            None => {
                info!("all workspace slots (1-9) are in use");
            }
        }
    }

    /// Send the focused pane to workspace `n` (1-9).
    pub(crate) fn send_to_workspace(&mut self, n: u8) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };

        let full_rect = match self.compute_layout_rect() {
            Some(r) => r,
            None => return,
        };
        let wm = match self.workspaces.as_mut() {
            Some(wm) => wm,
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
            osc_9: self.config.terminal.osc_9,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
            ..Default::default()
        };
        let proxy = self.event_proxy.clone();
        let registry = Some(Arc::clone(&self.agent_registry));

        let moved = wm.send_pane_to(focused, n as usize, || {
            match crate::pane::spawn_pane(
                full_rect,
                renderer,
                scrollback,
                interceptor_cfg.clone(),
                scan_interval_secs,
                &spawn_options,
                registry.clone(),
                |pane_id| make_pane_callbacks(&proxy, pane_id),
            ) {
                Ok(pane) => {
                    let id = pane.id;
                    Some((LayoutNode::Leaf(pane), id))
                }
                Err(e) => {
                    warn!(error = %e, "failed to spawn replacement pane");
                    None
                }
            }
        });

        if moved {
            info!("Sent pane {focused} to workspace {n}");
            self.relayout_and_redraw();
        }
    }

    // ── Auto-tile ───────────────────────────────────────────────────────

    /// Poll the auto-tile debouncer and apply any ready actions.
    pub(crate) fn poll_auto_tile(&mut self) {
        let actions = match self.auto_tile_debouncer.as_mut() {
            Some(debouncer) => debouncer.poll(),
            None => return,
        };

        for action in actions {
            match action {
                crate::pane::AutoTileAction::Split {
                    parent_pane_id,
                    agent_name,
                    ..
                } => {
                    // WM-style: split the largest pane instead of always
                    // splitting the parent -- avoids tiny unusable panes
                    // from nested binary splits.
                    let target_pane_id = self
                        .get_layout()
                        .and_then(|l| l.find_largest_pane())
                        .unwrap_or(parent_pane_id);

                    info!(
                        parent_pane_id,
                        target_pane_id, agent_name, "Auto-tiling: splitting largest pane for agent"
                    );
                    // Determine split direction from target pane's viewport.
                    let direction = self
                        .get_layout()
                        .and_then(|l| l.find_pane(target_pane_id))
                        .map(|p| {
                            LayoutNode::auto_split_direction(p.viewport, SplitDirection::Horizontal)
                        })
                        .unwrap_or(SplitDirection::Horizontal);

                    // Perform the split (reuses existing split_pane_by_id logic).
                    let renderer = match self.grid_renderer.as_ref() {
                        Some(r) => r,
                        None => continue,
                    };
                    let scrollback = self.config.general.scrollback_lines;
                    let interceptor_cfg = InterceptorConfig {
                        osc_633: self.config.terminal.osc_633,
                        osc_133: self.config.terminal.osc_133,
                        osc_7: self.config.terminal.osc_7,
                        osc_9: self.config.terminal.osc_9,
                        osc_1337: self.config.terminal.osc_1337,
                        osc_7777: self.config.terminal.osc_7777,
                    };
                    let scan_interval_secs = self.config.trust.agent_scan_interval;
                    let spawn_options = therminal_terminal::pty::SpawnOptions {
                        shell: self.config.general.shell.clone(),
                        env: self.config.general.env.clone(),
                        ..Default::default()
                    };
                    let proxy = self.event_proxy.clone();
                    let registry = Some(Arc::clone(&self.agent_registry));
                    let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
                        Some(l) => l,
                        None => continue,
                    };

                    let new_id = layout.split_pane(target_pane_id, direction, |viewport| {
                        match crate::pane::spawn_pane(
                            viewport,
                            renderer,
                            scrollback,
                            interceptor_cfg.clone(),
                            scan_interval_secs,
                            &spawn_options,
                            registry.clone(),
                            |pane_id| make_pane_callbacks(&proxy, pane_id),
                        ) {
                            Ok(pane) => Some(pane),
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "failed to spawn pane for auto-tile split"
                                );
                                None
                            }
                        }
                    });

                    if let Some(new_id) = new_id {
                        info!(parent_pane_id, new_id, "Auto-tile split complete");
                        // Register the auto-tiled pane so we can reclaim it later.
                        if let Some(ref mut debouncer) = self.auto_tile_debouncer {
                            debouncer.register_auto_tiled(parent_pane_id, new_id);
                        }
                        // Don't change focus for auto-tiled panes.
                        self.relayout_and_redraw();
                    }
                }
                crate::pane::AutoTileAction::Reclaim { pane_id } => {
                    info!(pane_id, "Auto-tiling: reclaiming pane after agent exit");
                    self.close_pane_by_id(pane_id);
                    // Clean up any Empty leaves and rebalance after reclaim.
                    if let Some(layout) = self.get_layout_mut() {
                        layout.compact_layout();
                    }
                }
            }
        }
    }
}
