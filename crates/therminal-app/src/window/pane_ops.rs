//! Pane operations: split, close, focus, resize, clipboard, workspace.
//!
//! All pane manipulation methods that modify the layout tree or interact
//! with pane PTYs for clipboard/selection operations.

use std::io::Write as IoWrite;

use alacritty_terminal::term::TermMode;
use tracing::{info, warn};

use crate::pane::{FocusDirection, LayoutNode, LayoutSnapshot, PaneId, SplitDirection};
use therminal_core::geometry::Rect;
use therminal_terminal::interceptor::InterceptorConfig;

use super::App;

/// Helper macro: get the active layout mutably from workspaces.
macro_rules! ws_layout_mut {
    ($self:expr) => {
        match $self.workspaces.as_mut() {
            Some(wm) => Some(wm.layout_mut()),
            None => None,
        }
    };
}

/// Helper macro: get the active layout ref from workspaces.
macro_rules! ws_layout {
    ($self:expr) => {
        match $self.workspaces.as_ref() {
            Some(wm) => Some(wm.layout()),
            None => None,
        }
    };
}

/// Helper macro: get focused pane id from workspaces.
macro_rules! ws_focused {
    ($self:expr) => {
        $self.workspaces.as_ref().and_then(|wm| wm.focused_pane())
    };
}

/// Helper macro: set focused pane id in workspaces.
macro_rules! ws_set_focused {
    ($self:expr, $val:expr) => {
        if let Some(wm) = $self.workspaces.as_mut() {
            wm.set_focused_pane($val);
        }
    };
}

impl App {
    /// Split the currently focused pane with auto-detected direction.
    pub(crate) fn split_focused_pane_auto(&mut self) {
        let focused = match ws_focused!(self) {
            Some(id) => id,
            None => return,
        };
        let layout = match ws_layout!(self) {
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
        let focused = match ws_focused!(self) {
            Some(id) => id,
            None => return,
        };
        let layout = match ws_layout_mut!(self) {
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
            osc_7777: self.config.terminal.osc_7777,
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
            let full_rect = self.content_area_rect(gpu.config.width as f32, gpu.config.height as f32);
            let layout = ws_layout_mut!(self).unwrap();
            let renderer = self.grid_renderer.as_ref().unwrap();
            layout.layout(full_rect);
            layout.resize_all_panes(renderer);

            // Focus the new pane.
            ws_set_focused!(self, Some(new_id));
        }

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Close the currently focused pane.
    pub(crate) fn close_focused_pane(&mut self) {
        let focused = match ws_focused!(self) {
            Some(id) => id,
            None => return,
        };

        let layout = match ws_layout_mut!(self) {
            Some(l) => l,
            None => return,
        };

        match layout.remove_pane(focused) {
            None => {
                // Last pane -- close the window.
                info!("Last pane closed, exiting");
                ws_set_focused!(self, None);
                self.workspaces = None;
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            Some(true) => {
                info!("Closed pane {focused}");
                // Move focus to first available pane.
                let new_focus = {
                    let layout = ws_layout_mut!(self).unwrap();
                    layout.pane_ids().first().copied()
                };
                ws_set_focused!(self, new_focus);

                // Relayout.
                let gpu = self.gpu.as_ref().unwrap();
                let full_rect =
                    self.content_area_rect(gpu.config.width as f32, gpu.config.height as f32);
                let layout = ws_layout_mut!(self).unwrap();
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
        let layout = match ws_layout_mut!(self) {
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
            osc_7777: self.config.terminal.osc_7777,
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
            let full_rect = self.content_area_rect(gpu.config.width as f32, gpu.config.height as f32);
            let layout = ws_layout_mut!(self).unwrap();
            let renderer = self.grid_renderer.as_ref().unwrap();
            layout.layout(full_rect);
            layout.resize_all_panes(renderer);

            ws_set_focused!(self, Some(new_id));
        }

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Close a specific pane by ID.
    pub(crate) fn close_pane_by_id(&mut self, target_id: PaneId) {
        let layout = match ws_layout_mut!(self) {
            Some(l) => l,
            None => return,
        };

        match layout.remove_pane(target_id) {
            None => {
                info!("Last pane closed, exiting");
                ws_set_focused!(self, None);
                self.workspaces = None;
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            Some(true) => {
                info!("Closed pane {target_id}");
                // If we closed the focused pane, move focus.
                if ws_focused!(self) == Some(target_id) {
                    let layout = ws_layout_mut!(self).unwrap();
                    let ids = layout.pane_ids();
                    ws_set_focused!(self, ids.first().copied());
                }

                let gpu = self.gpu.as_ref().unwrap();
                let full_rect =
                    self.content_area_rect(gpu.config.width as f32, gpu.config.height as f32);
                let layout = ws_layout_mut!(self).unwrap();
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
        let focused = match ws_focused!(self) {
            Some(id) => id,
            None => return,
        };
        let layout = match ws_layout!(self) {
            Some(l) => l,
            None => return,
        };

        if let Some(new_id) = layout.adjacent_pane(focused, direction) {
            ws_set_focused!(self, Some(new_id));
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    /// Swap the focused pane with the adjacent pane in the given direction.
    /// Focus follows the moved pane (stays on the same pane ID).
    pub(crate) fn swap_focused_pane(&mut self, direction: FocusDirection) {
        let focused = match ws_focused!(self) {
            Some(id) => id,
            None => return,
        };
        let layout = match ws_layout_mut!(self) {
            Some(l) => l,
            None => return,
        };

        if let Some(target_id) = layout.adjacent_pane(focused, direction) {
            if layout.swap_pane(focused, target_id) {
                // Focus stays on the original pane ID (it moved to the new position).
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
        }
    }

    /// Adjust the split ratio around the focused pane.
    pub(crate) fn adjust_focused_ratio(&mut self, delta: f32) {
        let focused = match ws_focused!(self) {
            Some(id) => id,
            None => return,
        };
        let layout = match ws_layout_mut!(self) {
            Some(l) => l,
            None => return,
        };

        if layout.adjust_ratio(focused, delta) {
            // Relayout.
            let gpu = self.gpu.as_ref().unwrap();
            let full_rect = self.content_area_rect(gpu.config.width as f32, gpu.config.height as f32);
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
        let pane_id = match self.selection_pane.or(ws_focused!(self)) {
            Some(id) => id,
            None => return,
        };
        let layout = match ws_layout!(self) {
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
        let focused = match ws_focused!(self) {
            Some(id) => id,
            None => return,
        };
        let mode = self.pane_term_mode(focused);
        let bracketed = mode.contains(TermMode::BRACKETED_PASTE);
        let layout = match ws_layout_mut!(self) {
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
            if let Some(layout) = ws_layout_mut!(self) {
                if let Some(pane) = layout.find_pane_mut(pane_id) {
                    pane.term.lock().selection = None;
                }
            }
        }
        self.selection_in_progress = false;
    }

    // ── Batch pane operations ─────────────────────────────────────────

    /// Close all panes, snapshotting the layout tree for later restore.
    /// Drops all PTYs immediately and does a single rebalance at the end.
    pub(crate) fn close_all_panes(&mut self) {
        let layout = match self.workspaces.as_mut().map(|wm| wm.take_layout()) {
            Some(l) => l,
            None => return,
        };

        // Snapshot the tree structure before destroying it.
        self.saved_layout = Some(layout.snapshot());

        // Drop the entire layout tree -- this drops all PaneState including
        // PTY masters and writers, causing reader threads to hit EOF and exit.
        drop(layout);

        ws_set_focused!(self, None);
        self.selection_pane = None;
        self.selection_in_progress = false;

        info!("Closed all panes (layout snapshot saved)");

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
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
        let snapshot = match self.saved_layout.take() {
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
        if ws_layout!(self).is_some() {
            let layout = self.workspaces.as_mut().unwrap().take_layout();
            drop(layout);
            ws_set_focused!(self, None);
        }

        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };
        let gpu = match self.gpu.as_ref() {
            Some(g) => g,
            None => return,
        };

        let full_rect = self.content_area_rect(gpu.config.width as f32, gpu.config.height as f32);

        // Rebuild the layout tree from the snapshot by spawning new panes.
        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
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
                // Relayout and resize.
                let layout = ws_layout_mut!(self).unwrap();
                let renderer = self.grid_renderer.as_ref().unwrap();
                layout.layout(full_rect);
                layout.resize_all_panes(renderer);

                // Focus the first pane.
                let ids = layout.pane_ids();
                ws_set_focused!(self, ids.first().copied());

                info!(panes = ids.len(), "Restored layout from snapshot");
            }
            None => {
                warn!("Failed to restore layout from snapshot");
            }
        }

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
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
                let p = proxy.clone();
                let cfg = interceptor_cfg.clone();
                match crate::pane::spawn_pane(
                    rect,
                    renderer,
                    scrollback,
                    cfg,
                    scan_interval_secs,
                    spawn_options,
                    |_pane_id| {
                        let p = p.clone();
                        Box::new(move || {
                            let _ = p.send_event(super::UserEvent::PtyOutput);
                        })
                    },
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
        let wm = match self.workspaces.as_mut() {
            Some(wm) => wm,
            None => return,
        };

        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };
        let gpu = match self.gpu.as_ref() {
            Some(g) => g,
            None => return,
        };

        let full_rect =
            self.content_area_rect(gpu.config.width as f32, gpu.config.height as f32);

        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
        };
        let proxy = self.event_proxy.clone();

        let switched = wm.switch_to(n as usize, || {
            match crate::pane::spawn_pane(
                full_rect,
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
            // Relayout the newly active workspace.
            let wm = self.workspaces.as_mut().unwrap();
            let layout = wm.layout_mut();
            let renderer = self.grid_renderer.as_ref().unwrap();
            layout.layout(full_rect);
            layout.resize_all_panes(renderer);

            info!("Switched to workspace {n}");
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    /// Send the focused pane to workspace `n` (1-9).
    pub(crate) fn send_to_workspace(&mut self, n: u8) {
        let focused = match ws_focused!(self) {
            Some(id) => id,
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
        let gpu = match self.gpu.as_ref() {
            Some(g) => g,
            None => return,
        };

        let status_bar_h =
            crate::pane::effective_status_bar_height(self.config.general.show_status_bar);
        let full_rect = Rect::new(
            0.0,
            0.0,
            gpu.config.width as f32,
            gpu.config.height as f32 - status_bar_h,
        );

        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
        };
        let proxy = self.event_proxy.clone();

        let moved = wm.send_pane_to(focused, n as usize, || {
            match crate::pane::spawn_pane(
                full_rect,
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
            // Relayout the current workspace after pane removal.
            let wm = self.workspaces.as_mut().unwrap();
            let layout = wm.layout_mut();
            let renderer = self.grid_renderer.as_ref().unwrap();
            layout.layout(full_rect);
            layout.resize_all_panes(renderer);

            info!("Sent pane {focused} to workspace {n}");
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }
}
