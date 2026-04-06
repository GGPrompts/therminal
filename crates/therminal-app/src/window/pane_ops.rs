//! Pane operations: split, close, focus, resize, clipboard, workspace.
//!
//! All pane manipulation methods that modify the layout tree or interact
//! with pane PTYs for clipboard/selection operations.

use std::io::Write as IoWrite;

use alacritty_terminal::term::TermMode;
use tracing::{debug, info, warn};

use crate::pane::{
    FocusDirection, LayoutNode, LayoutSnapshot, PaneCallbacks, PaneId, PaneRemoveResult,
    SpatialDirection, SplitDirection,
};
use therminal_core::geometry::Rect;
use therminal_terminal::interceptor::InterceptorConfig;

use super::{App, EventLoopProxy, UserEvent};

/// Build `PaneCallbacks` from an event-loop proxy.
fn make_pane_callbacks(proxy: &EventLoopProxy<UserEvent>, pane_id: PaneId) -> PaneCallbacks {
    let p1 = proxy.clone();
    let p2 = proxy.clone();
    PaneCallbacks {
        wake: Box::new(move || {
            let _ = p1.send_event(UserEvent::PtyOutput);
        }),
        on_exit: Box::new(move || {
            let _ = p2.send_event(UserEvent::PaneExited(pane_id));
        }),
    }
}

impl App {
    /// Split the currently focused pane with auto-detected direction.
    pub(crate) fn split_focused_pane_auto(&mut self) {
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
        let term_guard = pane.term.lock();
        if let Some(text) = term_guard.selection_to_string()
            && !text.is_empty()
        {
            crate::clipboard::copy_to_clipboard(&text);
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
        if bracketed {
            let _ = pane.pty_writer.write_all(b"\x1b[200~");
        }
        let _ = pane.pty_writer.write_all(text.as_bytes());
        if bracketed {
            let _ = pane.pty_writer.write_all(b"\x1b[201~");
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
        {
            pane.term.lock().selection = None;
        }
        self.selection_in_progress = false;
    }

    // ── Batch pane operations ─────────────────────────────────────────

    /// Close all panes, snapshotting the layout tree for later restore.
    /// Drops all PTYs immediately and does a single rebalance at the end.
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

        // Snapshot the tree structure before destroying it.
        wm.save_layout();

        // Take and drop the layout tree -- this drops all PaneState including
        // PTY masters and writers, causing reader threads to hit EOF and exit.
        let layout = wm.take_layout();
        drop(layout);

        // Keep workspaces alive (saved_layout lives there now).
        // workspaces == None means "exit", not "waiting for restore".
        self.set_focused_pane(None);
        self.selection_pane = None;
        self.selection_in_progress = false;

        info!("Closed all panes (layout snapshot saved)");
        self.request_redraw();
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
                match crate::pane::spawn_pane(
                    rect,
                    renderer,
                    scrollback,
                    cfg,
                    scan_interval_secs,
                    spawn_options,
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

        let switched = wm.switch_to(n as usize, || {
            match crate::pane::spawn_pane(
                full_rect,
                renderer,
                scrollback,
                interceptor_cfg.clone(),
                scan_interval_secs,
                &spawn_options,
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

        let moved = wm.send_pane_to(focused, n as usize, || {
            match crate::pane::spawn_pane(
                full_rect,
                renderer,
                scrollback,
                interceptor_cfg.clone(),
                scan_interval_secs,
                &spawn_options,
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
}
