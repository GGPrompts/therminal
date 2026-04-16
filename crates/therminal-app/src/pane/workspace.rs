//! Workspace manager: named workspace slots with independent pane layouts.

use super::PaneId;
use super::SplitDirection;
use super::layout::{LayoutNode, LayoutSnapshot};
use super::state::PaneState;
use therminal_protocol::daemon::WorkspaceInfo;

/// Result of removing a pane from across all workspaces.
#[derive(Debug, PartialEq)]
pub enum PaneRemoveResult {
    /// The pane was the last one in its workspace.
    LastInWorkspace,
    /// The pane was removed; other panes remain in that workspace.
    Removed,
    /// The pane was not found in any workspace.
    NotFound,
}

/// A workspace holds an independent pane layout with its own focused pane.
pub struct Workspace {
    /// Workspace number (1-9).
    pub id: usize,
    /// Human-readable workspace name (default: the slot number as a string).
    /// Used by `workspace_info()` for daemon sync.
    pub name: String,
    /// Root of this workspace's layout tree.
    pub layout: LayoutNode,
    /// Currently focused pane within this workspace.
    pub focused_pane: Option<PaneId>,
}

/// Manages multiple workspaces, each with an independent pane layout.
/// Supports up to 9 workspaces (1-9), Hyprland-style.
pub struct WorkspaceManager {
    /// All workspaces, indexed by workspace number (1-based).
    workspaces: Vec<Workspace>,
    /// Index into `workspaces` for the currently active workspace.
    active_idx: usize,
    /// Saved layout snapshot from close_all_panes(), for restore.
    /// This is the single source of truth for whether a restore is pending.
    saved_layout: Option<LayoutSnapshot>,
}

impl WorkspaceManager {
    /// Create a new manager with workspace 1 containing the given layout.
    pub fn new(layout: LayoutNode, focused_pane: Option<PaneId>) -> Self {
        let ws = Workspace {
            id: 1,
            name: "1".to_string(),
            layout,
            focused_pane,
        };
        Self {
            workspaces: vec![ws],
            active_idx: 0,
            saved_layout: None,
        }
    }

    /// Collect the Claude session IDs from every pane across every workspace.
    ///
    /// Reads `PaneStatus.claude_session_id` from each leaf pane. Used by the
    /// swarm watcher's "current" scope filter to determine which subagent
    /// parent sessions belong to this therminal instance (tn-twfg).
    pub fn collect_all_claude_session_ids(&self) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        for ws in &self.workspaces {
            for id in ws.layout.pane_ids() {
                if let Some(pane) = ws.layout.find_pane(id)
                    && let Ok(status) = pane.status.lock()
                    && let Some(ref sid) = status.claude_session_id
                {
                    out.insert(sid.clone());
                }
            }
        }
        out
    }

    /// The active workspace number (1-9).
    pub fn active_id(&self) -> usize {
        self.workspaces[self.active_idx].id
    }

    /// Get a reference to the active workspace's layout.
    pub fn layout(&self) -> &LayoutNode {
        &self.workspaces[self.active_idx].layout
    }

    /// Get a mutable reference to the active workspace's layout.
    pub fn layout_mut(&mut self) -> &mut LayoutNode {
        &mut self.workspaces[self.active_idx].layout
    }

    /// Get the focused pane of the active workspace.
    pub fn focused_pane(&self) -> Option<PaneId> {
        self.workspaces[self.active_idx].focused_pane
    }

    /// Set the focused pane of the active workspace.
    pub fn set_focused_pane(&mut self, pane_id: Option<PaneId>) {
        self.workspaces[self.active_idx].focused_pane = pane_id;
    }

    /// Take the active workspace's layout (replaces it with Empty).
    pub fn take_layout(&mut self) -> LayoutNode {
        std::mem::replace(
            &mut self.workspaces[self.active_idx].layout,
            LayoutNode::Empty,
        )
    }

    /// Set the active workspace's layout.
    pub fn set_layout(&mut self, layout: LayoutNode) {
        self.workspaces[self.active_idx].layout = layout;
    }

    /// Check if workspace `n` exists.
    fn workspace_index(&self, n: usize) -> Option<usize> {
        self.workspaces.iter().position(|ws| ws.id == n)
    }

    /// Switch to workspace `n` (1-9). Returns true if the workspace changed.
    /// `create_pane` is called if the workspace doesn't exist yet and needs a
    /// default pane.
    pub fn switch_to<F>(&mut self, n: usize, create_pane: F) -> bool
    where
        F: FnOnce() -> Option<(LayoutNode, PaneId)>,
    {
        if !(1..=9).contains(&n) {
            return false;
        }
        if self.workspaces[self.active_idx].id == n {
            return false; // already on this workspace
        }

        let target_idx = match self.workspace_index(n) {
            Some(idx) => idx,
            None => {
                // Create new workspace with a default pane.
                if let Some((layout, pane_id)) = create_pane() {
                    let ws = Workspace {
                        id: n,
                        name: n.to_string(),
                        layout,
                        focused_pane: Some(pane_id),
                    };
                    self.workspaces.push(ws);
                    self.workspaces.len() - 1
                } else {
                    return false;
                }
            }
        };

        self.active_idx = target_idx;
        true
    }

    /// Remove a pane from the active workspace's layout tree and return it.
    /// Returns the extracted `PaneState` if found, or None.
    fn extract_pane(&mut self, pane_id: PaneId) -> Option<PaneState> {
        let layout = &mut self.workspaces[self.active_idx].layout;

        // If this is the root leaf and it's the target, extract it.
        if matches!(layout, LayoutNode::Leaf(p) if p.id == pane_id) {
            let old = std::mem::replace(layout, LayoutNode::Empty);
            if let LayoutNode::Leaf(pane) = old {
                return Some(pane);
            }
        }

        // Otherwise use remove_pane logic but we need the actual PaneState.
        // We'll do a custom extraction.
        Self::extract_from_tree(layout, pane_id)
    }

    /// Recursively extract a pane from a layout tree, promoting the sibling.
    fn extract_from_tree(node: &mut LayoutNode, target_id: PaneId) -> Option<PaneState> {
        match node {
            LayoutNode::Leaf(_) => None,
            LayoutNode::Split { first, second, .. } => {
                // Check if target is a direct child.
                let first_is_target =
                    matches!(first.as_ref(), LayoutNode::Leaf(p) if p.id == target_id);
                let second_is_target =
                    matches!(second.as_ref(), LayoutNode::Leaf(p) if p.id == target_id);

                if first_is_target {
                    let extracted = std::mem::replace(first.as_mut(), LayoutNode::Empty);
                    let sibling = std::mem::replace(second.as_mut(), LayoutNode::Empty);
                    *node = sibling;
                    node.rebalance();
                    if let LayoutNode::Leaf(pane) = extracted {
                        return Some(pane);
                    }
                    return None;
                }

                if second_is_target {
                    let extracted = std::mem::replace(second.as_mut(), LayoutNode::Empty);
                    let sibling = std::mem::replace(first.as_mut(), LayoutNode::Empty);
                    *node = sibling;
                    node.rebalance();
                    if let LayoutNode::Leaf(pane) = extracted {
                        return Some(pane);
                    }
                    return None;
                }

                // Recurse into children.
                if let Some(pane) = Self::extract_from_tree(first, target_id) {
                    // Rebalance after extraction.
                    let first_leaves = first.leaf_count() as f32;
                    let total_leaves = first_leaves + second.leaf_count() as f32;
                    if let LayoutNode::Split { ratio, .. } = node {
                        *ratio = (first_leaves / total_leaves).clamp(0.1, 0.9);
                    }
                    return Some(pane);
                }
                if let Some(pane) = Self::extract_from_tree(second, target_id) {
                    let first_leaves = first.leaf_count() as f32;
                    let total_leaves = first_leaves + second.leaf_count() as f32;
                    if let LayoutNode::Split { ratio, .. } = node {
                        *ratio = (first_leaves / total_leaves).clamp(0.1, 0.9);
                    }
                    return Some(pane);
                }

                None
            }
            LayoutNode::Empty => None,
        }
    }

    /// Send a pane from the active workspace to workspace `n`.
    /// `create_default_pane` is called if the source workspace becomes empty
    /// and needs a replacement pane.
    /// Returns true if the pane was moved.
    pub fn send_pane_to<F>(
        &mut self,
        pane_id: PaneId,
        target_n: usize,
        create_default_pane: F,
    ) -> bool
    where
        F: FnOnce() -> Option<(LayoutNode, PaneId)>,
    {
        if !(1..=9).contains(&target_n) {
            return false;
        }
        let current_id = self.workspaces[self.active_idx].id;
        if current_id == target_n {
            return false; // already on target workspace
        }

        // Extract the pane from the active workspace.
        let pane = match self.extract_pane(pane_id) {
            Some(p) => p,
            None => return false,
        };

        // If the active workspace layout became Empty, create a default pane.
        let active_layout_empty =
            matches!(self.workspaces[self.active_idx].layout, LayoutNode::Empty);
        if active_layout_empty {
            if let Some((layout, new_id)) = create_default_pane() {
                self.workspaces[self.active_idx].layout = layout;
                self.workspaces[self.active_idx].focused_pane = Some(new_id);
            }
        } else {
            // If we removed the focused pane, pick a new one.
            if self.workspaces[self.active_idx].focused_pane == Some(pane_id) {
                let ids = self.workspaces[self.active_idx].layout.pane_ids();
                self.workspaces[self.active_idx].focused_pane = ids.first().copied();
            }
        }

        // Insert into target workspace.
        let target_idx = match self.workspace_index(target_n) {
            Some(idx) => idx,
            None => {
                // Create new workspace with the moved pane as the only pane.
                let ws = Workspace {
                    id: target_n,
                    name: target_n.to_string(),
                    layout: LayoutNode::Leaf(pane),
                    focused_pane: Some(pane_id),
                };
                self.workspaces.push(ws);
                return true;
            }
        };

        // Add pane to existing target workspace by creating a split.
        let target_layout = &mut self.workspaces[target_idx].layout;
        let old_layout = std::mem::replace(target_layout, LayoutNode::Empty);
        *target_layout = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(old_layout),
            second: Box::new(LayoutNode::Leaf(pane)),
        };
        target_layout.rebalance();

        true
    }

    /// Return the focused pane's `PaneStatus` for a given workspace ID, if available.
    pub fn focused_pane_status(&self, workspace_id: usize) -> Option<super::state::PaneStatus> {
        let ws = self.workspaces.iter().find(|ws| ws.id == workspace_id)?;
        let focused_id = ws.focused_pane?;
        let pane = ws.layout.find_pane(focused_id)?;
        let status = pane.status.lock().unwrap_or_else(|e| e.into_inner());
        Some(status.clone())
    }

    /// Return the focused pane id for a specific workspace, if available.
    pub fn focused_pane_for(&self, workspace_id: usize) -> Option<PaneId> {
        let ws = self.workspaces.iter().find(|ws| ws.id == workspace_id)?;
        ws.focused_pane
    }

    /// Return a reference to a specific workspace's layout, if it exists.
    pub fn layout_for(&self, workspace_id: usize) -> Option<&LayoutNode> {
        let ws = self.workspaces.iter().find(|ws| ws.id == workspace_id)?;
        Some(&ws.layout)
    }

    /// Return all workspace IDs that currently exist, sorted.
    pub fn workspace_ids(&self) -> Vec<usize> {
        let mut ids: Vec<usize> = self.workspaces.iter().map(|ws| ws.id).collect();
        ids.sort();
        ids
    }

    /// Save a snapshot of the current layout for later restore.
    #[allow(dead_code)]
    pub fn save_layout(&mut self) {
        let layout = &self.workspaces[self.active_idx].layout;
        self.saved_layout = Some(layout.snapshot());
    }

    /// Take the saved layout snapshot, if any.
    pub fn take_saved_layout(&mut self) -> Option<LayoutSnapshot> {
        self.saved_layout.take()
    }

    /// Whether a saved layout snapshot exists (restore is possible).
    #[allow(dead_code)]
    pub fn has_saved_layout(&self) -> bool {
        self.saved_layout.is_some()
    }

    /// Returns true if the manager has no workspaces (shouldn't normally happen).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    /// Number of workspaces currently managed.
    ///
    /// Used by chrome layout code to decide whether to reserve space for the
    /// tab bar: single-workspace layouts collapse the bar to zero height so
    /// the terminal grid can claim the extra pixels.
    pub fn len(&self) -> usize {
        self.workspaces.len()
    }

    /// Build a list of `WorkspaceInfo` for syncing to the daemon.
    ///
    /// This captures the current workspace topology (IDs, names, pane assignments,
    /// focus) so the daemon can store it for MCP tools and reattach.
    pub fn workspace_info(&self) -> Vec<WorkspaceInfo> {
        self.workspaces
            .iter()
            .enumerate()
            .map(|(order, ws)| WorkspaceInfo {
                id: ws.id as u64,
                name: ws.name.clone(),
                order: order as u32,
                pane_ids: ws.layout.pane_ids(),
                focused_pane: ws.focused_pane,
                layout: ws.layout.to_snapshot(),
            })
            .collect()
    }

    /// Reconstruct a `WorkspaceManager` from a list of `WorkspaceInfo`
    /// returned by `IpcRequest::GetWorkspaces`.
    ///
    /// `make_leaf` is invoked once per daemon `PaneId` and is responsible
    /// for materialising a `PaneState` (typically by allocating a fresh
    /// local id, inserting it into the caller's `PaneIdMap`, and calling
    /// `build_remote_pane_state`). When a pane returns `None`, that leaf
    /// is dropped from its workspace's layout (becomes `LayoutNode::Empty`).
    ///
    /// The reconstructed manager:
    /// - prefers `WorkspaceInfo.layout` (real binary tree) when present;
    /// - falls back to a flat horizontal cascade of `pane_ids` when not;
    /// - sets `focused_pane` from the daemon-side focus, translated via
    ///   `make_leaf`'s emitted `PaneState.id` (whichever local id was
    ///   assigned when that daemon pane was materialised);
    /// - selects the active workspace from `active_workspace_id` (falls
    ///   back to the first workspace).
    ///
    /// Returns `None` if `infos` is empty.
    pub fn from_workspace_info<F>(
        infos: &[WorkspaceInfo],
        active_workspace_id: therminal_protocol::WorkspaceId,
        make_leaf: &mut F,
    ) -> Option<Self>
    where
        F: FnMut(therminal_protocol::PaneId) -> Option<PaneState>,
    {
        if infos.is_empty() {
            return None;
        }
        let mut workspaces = Vec::with_capacity(infos.len());
        for info in infos {
            // Track daemon→local id mapping for *this* workspace so we
            // can translate `focused_pane` after the leaves are built.
            let mut daemon_to_local: std::collections::HashMap<therminal_protocol::PaneId, PaneId> =
                std::collections::HashMap::new();
            let mut leaf_builder = |dpid: therminal_protocol::PaneId| -> Option<PaneState> {
                let pane = make_leaf(dpid)?;
                daemon_to_local.insert(dpid, pane.id);
                Some(pane)
            };
            let layout = match &info.layout {
                Some(snap) => LayoutNode::from_protocol_snapshot(snap, &mut leaf_builder),
                None => LayoutNode::from_flat_pane_ids(&info.pane_ids, &mut leaf_builder),
            };
            let focused_pane = info
                .focused_pane
                .and_then(|d| daemon_to_local.get(&d).copied())
                .or_else(|| layout.pane_ids().first().copied());
            workspaces.push(Workspace {
                id: info.id as usize,
                name: info.name.clone(),
                layout,
                focused_pane,
            });
        }
        let active_idx = workspaces
            .iter()
            .position(|w| w.id == active_workspace_id as usize)
            .unwrap_or(0);
        Some(Self {
            workspaces,
            active_idx,
            saved_layout: None,
        })
    }

    /// Collect pinned pane IDs across all workspaces.
    #[allow(dead_code)]
    pub fn pinned_pane_ids(&self) -> Vec<super::PaneId> {
        let mut ids = Vec::new();
        for ws in &self.workspaces {
            for id in ws.layout.pane_ids() {
                if let Some(pane) = ws.layout.find_pane(id)
                    && pane.pinned
                {
                    ids.push(id);
                }
            }
        }
        ids
    }

    /// After a workspace switch, migrate pinned panes from non-active
    /// workspaces into the active workspace so they remain visible.
    ///
    /// Pinned panes are extracted from their source workspace and appended
    /// to the active workspace's layout via a horizontal split. The source
    /// workspace's tree is compacted after extraction. If extracting a
    /// pinned pane leaves the source workspace empty, the source workspace
    /// is left as Empty (it will be garbage collected by
    /// `gc_empty_workspaces`).
    pub fn migrate_pinned_to_active(&mut self) {
        // Collect (workspace_idx, pane_id) pairs for pinned panes that are
        // NOT already in the active workspace.
        let active_idx = self.active_idx;
        let mut to_migrate: Vec<(usize, super::PaneId)> = Vec::new();
        for (idx, ws) in self.workspaces.iter().enumerate() {
            if idx == active_idx {
                continue;
            }
            for id in ws.layout.pane_ids() {
                if let Some(pane) = ws.layout.find_pane(id)
                    && pane.pinned
                {
                    to_migrate.push((idx, id));
                }
            }
        }

        for (ws_idx, pane_id) in to_migrate {
            // Extract from source workspace.
            let extracted = self.workspaces[ws_idx].layout.extract_pane(pane_id);
            if let Some(pane) = extracted {
                // Compact the source layout.
                self.workspaces[ws_idx].layout.compact_layout();
                // Update source focus if we extracted the focused pane.
                if self.workspaces[ws_idx].focused_pane == Some(pane_id) {
                    let ids = self.workspaces[ws_idx].layout.pane_ids();
                    self.workspaces[ws_idx].focused_pane = ids.first().copied();
                }
                // Insert into the active workspace.
                let target = &mut self.workspaces[self.active_idx].layout;
                // Try to fill an Empty slot first.
                if let Some(returned) = target.insert_pane_at_empty(pane) {
                    // No empty slot. Create a split to add the pinned pane.
                    let old_layout = std::mem::replace(target, LayoutNode::Empty);
                    *target = LayoutNode::Split {
                        direction: super::SplitDirection::Horizontal,
                        ratio: 0.5,
                        first: Box::new(old_layout),
                        second: Box::new(LayoutNode::Leaf(returned)),
                    };
                    target.rebalance();
                }
            }
        }
    }

    /// Rename the active workspace.
    #[allow(dead_code)]
    pub fn rename_active(&mut self, name: String) {
        self.workspaces[self.active_idx].name = name;
    }

    /// Rename the workspace with the given id. Returns true if it existed.
    pub fn rename(&mut self, workspace_id: usize, name: String) -> bool {
        if let Some(idx) = self.workspace_index(workspace_id) {
            self.workspaces[idx].name = name;
            true
        } else {
            false
        }
    }

    /// Get the human-readable name of a workspace by id.
    pub fn name_for(&self, workspace_id: usize) -> Option<&str> {
        self.workspaces
            .iter()
            .find(|ws| ws.id == workspace_id)
            .map(|ws| ws.name.as_str())
    }

    /// Iterate over all workspaces (read-only).
    pub fn iter_workspaces(&self) -> impl Iterator<Item = &Workspace> {
        self.workspaces.iter()
    }

    /// Iterate over all workspaces (mutable).
    pub fn iter_workspaces_mut(&mut self) -> impl Iterator<Item = &mut Workspace> {
        self.workspaces.iter_mut()
    }

    /// Total pane count across all workspaces.
    pub fn total_pane_count(&self) -> usize {
        self.workspaces
            .iter()
            .map(|ws| ws.layout.pane_count())
            .sum()
    }

    /// Remove a pane from whichever workspace contains it.
    ///
    /// Returns:
    /// - `PaneRemoveResult::LastInWorkspace` if it was the last pane in that workspace
    /// - `PaneRemoveResult::Removed` if removed (other panes remain)
    /// - `PaneRemoveResult::NotFound` if the pane wasn't in any workspace
    pub fn remove_pane_any(&mut self, pane_id: PaneId) -> PaneRemoveResult {
        let idx = match self
            .workspaces
            .iter()
            .position(|ws| ws.layout.pane_ids().contains(&pane_id))
        {
            Some(idx) => idx,
            None => return PaneRemoveResult::NotFound,
        };
        let result = self.workspaces[idx].layout.remove_pane(pane_id);
        match result {
            None => {
                // Root leaf — remove_pane doesn't clear it, so we do.
                self.workspaces[idx].layout = LayoutNode::Empty;
                self.workspaces[idx].focused_pane = None;
                PaneRemoveResult::LastInWorkspace
            }
            Some(true) => {
                // Update focus if we removed the focused pane.
                if self.workspaces[idx].focused_pane == Some(pane_id) {
                    let ids = self.workspaces[idx].layout.pane_ids();
                    self.workspaces[idx].focused_pane = ids.first().copied();
                }
                PaneRemoveResult::Removed
            }
            Some(false) => PaneRemoveResult::NotFound, // shouldn't happen
        }
    }

    /// Remove any workspace whose layout is Empty, provided other workspaces
    /// exist. If the active workspace is removed, switches to the nearest.
    /// Returns true if a workspace was removed.
    pub fn gc_empty_workspaces(&mut self) -> bool {
        if self.workspaces.len() <= 1 {
            return false;
        }
        let idx =
            match self.workspaces.iter().position(|ws| {
                matches!(ws.layout, LayoutNode::Empty) || ws.layout.pane_count() == 0
            }) {
                Some(idx) => idx,
                None => return false,
            };
        let was_active = idx == self.active_idx;
        self.workspaces.remove(idx);
        // Adjust active_idx after removal.
        if self.active_idx >= self.workspaces.len() {
            self.active_idx = self.workspaces.len() - 1;
        } else if self.active_idx > idx {
            self.active_idx -= 1;
        }
        if was_active {
            // active_idx now points to the nearest workspace.
        }
        true
    }

    /// Remove the active workspace if its layout is Empty and there are other
    /// workspaces. Switches to the first available workspace. Returns true if removed.
    #[allow(dead_code)]
    pub fn remove_empty_active(&mut self) -> bool {
        if self.workspaces.len() <= 1 {
            return false;
        }
        if !matches!(self.workspaces[self.active_idx].layout, LayoutNode::Empty) {
            return false;
        }
        self.workspaces.remove(self.active_idx);
        if self.active_idx >= self.workspaces.len() {
            self.active_idx = 0;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use alacritty_terminal::sync::FairMutex;
    use alacritty_terminal::term::{Config as TermConfig, Term};
    use therminal_core::geometry::Rect;

    use super::*;
    use crate::pane::PaneListener;
    use crate::pane::backend::PaneBackendKind;
    use crate::pane::state::PaneTermSize;

    /// Helper: build a minimal `PaneState` (Terminal backend) for tests
    /// that need raw `PaneState` rather than a `LayoutNode::Leaf`.
    fn test_pane_state(id: PaneId) -> PaneState {
        let term = Term::new(
            TermConfig::default(),
            &PaneTermSize {
                columns: 80,
                screen_lines: 24,
            },
            PaneListener::new(),
        );
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let writer = pair.master.take_writer().unwrap();
        PaneState {
            id,
            viewport: default_rect(),
            status: Arc::new(Mutex::new(super::super::state::PaneStatus::default())),
            region_index: Arc::new(Mutex::new(
                therminal_terminal::region_index::RegionIndex::new(),
            )),
            backend: PaneBackendKind::Terminal {
                term: Arc::new(FairMutex::new(term)),
                pty_writer: writer,
                pty_master: pair.master,
                scrollback_lines: 1000,
            },
            pinned: false,
        }
    }

    /// Helper to create a minimal test leaf node (no real PTY).
    fn test_leaf(id: PaneId, rect: Rect) -> LayoutNode {
        let term = Term::new(
            TermConfig::default(),
            &PaneTermSize {
                columns: 80,
                screen_lines: 24,
            },
            PaneListener::new(),
        );
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let writer = pair.master.take_writer().unwrap();
        LayoutNode::Leaf(PaneState {
            id,
            viewport: rect,
            status: Arc::new(Mutex::new(super::super::state::PaneStatus::default())),
            region_index: Arc::new(Mutex::new(
                therminal_terminal::region_index::RegionIndex::new(),
            )),
            backend: PaneBackendKind::Terminal {
                term: Arc::new(FairMutex::new(term)),
                pty_writer: writer,
                pty_master: pair.master,
                scrollback_lines: 1000,
            },
            pinned: false,
        })
    }

    fn default_rect() -> Rect {
        Rect::new(0.0, 0.0, 800.0, 600.0)
    }

    /// Helper to build a two-pane split layout.
    fn two_pane_split(id_a: PaneId, id_b: PaneId) -> LayoutNode {
        let r = default_rect();
        LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(test_leaf(id_a, r)),
            second: Box::new(test_leaf(id_b, r)),
        }
    }

    // ── WorkspaceManager::new ────────────────────────────────────────

    #[test]
    fn new_manager_has_one_workspace() {
        let wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        assert_eq!(wm.active_id(), 1);
        assert_eq!(wm.focused_pane(), Some(1));
        assert_eq!(wm.workspace_ids(), vec![1]);
    }

    #[test]
    fn new_manager_layout_matches() {
        let wm = WorkspaceManager::new(test_leaf(42, default_rect()), Some(42));
        assert_eq!(wm.layout().pane_ids(), vec![42]);
    }

    // ── focused_pane / set_focused_pane ──────────────────────────────

    #[test]
    fn set_focused_pane_updates() {
        let layout = two_pane_split(1, 2);
        let mut wm = WorkspaceManager::new(layout, Some(1));
        assert_eq!(wm.focused_pane(), Some(1));

        wm.set_focused_pane(Some(2));
        assert_eq!(wm.focused_pane(), Some(2));
    }

    #[test]
    fn set_focused_pane_to_none() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        wm.set_focused_pane(None);
        assert_eq!(wm.focused_pane(), None);
    }

    // ── take_layout / set_layout ─────────────────────────────────────

    #[test]
    fn take_layout_replaces_with_empty() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        let taken = wm.take_layout();
        assert_eq!(taken.pane_ids(), vec![1]);
        assert_eq!(wm.layout().pane_count(), 0); // Now Empty
    }

    #[test]
    fn set_layout_replaces_current() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        wm.set_layout(test_leaf(99, default_rect()));
        assert_eq!(wm.layout().pane_ids(), vec![99]);
    }

    // ── save_layout / take_saved_layout / has_saved_layout ───────────

    #[test]
    fn save_and_take_layout_roundtrip() {
        let layout = two_pane_split(1, 2);
        let mut wm = WorkspaceManager::new(layout, Some(1));

        assert!(!wm.has_saved_layout());

        wm.save_layout();
        assert!(wm.has_saved_layout());

        let snap = wm.take_saved_layout();
        assert!(snap.is_some());
        assert!(!wm.has_saved_layout());

        // Verify snapshot structure: should be a Split with two Leaf children.
        let snap = snap.unwrap();
        match &snap {
            LayoutSnapshot::Split {
                direction,
                first,
                second,
                ..
            } => {
                assert_eq!(*direction, SplitDirection::Horizontal);
                assert!(matches!(first.as_ref(), LayoutSnapshot::Leaf));
                assert!(matches!(second.as_ref(), LayoutSnapshot::Leaf));
            }
            _ => panic!("Expected Split snapshot, got Leaf"),
        }
    }

    #[test]
    fn take_saved_layout_clears_it() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        wm.save_layout();
        assert!(wm.has_saved_layout());

        let _ = wm.take_saved_layout();
        assert!(!wm.has_saved_layout());

        // Second take returns None.
        assert!(wm.take_saved_layout().is_none());
    }

    #[test]
    fn take_saved_layout_without_save_returns_none() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        assert!(wm.take_saved_layout().is_none());
    }

    #[test]
    fn save_layout_snapshot_leaf_count_matches() {
        let layout = two_pane_split(1, 2);
        let mut wm = WorkspaceManager::new(layout, Some(1));
        wm.save_layout();
        let snap = wm.take_saved_layout().unwrap();
        assert_eq!(LayoutNode::snapshot_leaf_count(&snap), 2);
    }

    #[test]
    fn save_layout_single_pane_snapshot() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        wm.save_layout();
        let snap = wm.take_saved_layout().unwrap();
        assert!(matches!(snap, LayoutSnapshot::Leaf));
        assert_eq!(LayoutNode::snapshot_leaf_count(&snap), 1);
    }

    // ── switch_to ────────────────────────────────────────────────────

    #[test]
    fn switch_to_same_workspace_returns_false() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        let switched = wm.switch_to(1, || None);
        assert!(!switched);
        assert_eq!(wm.active_id(), 1);
    }

    #[test]
    fn switch_to_new_workspace_creates_it() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        let switched = wm.switch_to(3, || Some((test_leaf(10, default_rect()), 10)));
        assert!(switched);
        assert_eq!(wm.active_id(), 3);
        assert_eq!(wm.focused_pane(), Some(10));
        assert_eq!(wm.layout().pane_ids(), vec![10]);
    }

    #[test]
    fn switch_to_new_workspace_fails_if_no_pane_created() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        let switched = wm.switch_to(2, || None);
        assert!(!switched);
        assert_eq!(wm.active_id(), 1); // stays on workspace 1
    }

    #[test]
    fn switch_to_out_of_range_returns_false() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        assert!(!wm.switch_to(0, || None));
        assert!(!wm.switch_to(10, || None));
    }

    #[test]
    fn switch_back_to_existing_workspace_preserves_layout() {
        let mut wm = WorkspaceManager::new(two_pane_split(1, 2), Some(1));
        // Switch to workspace 2
        wm.switch_to(2, || Some((test_leaf(10, default_rect()), 10)));
        assert_eq!(wm.active_id(), 2);

        // Switch back to workspace 1
        let switched = wm.switch_to(1, || panic!("should not create"));
        assert!(switched);
        assert_eq!(wm.active_id(), 1);
        assert_eq!(wm.layout().pane_ids(), vec![1, 2]);
        assert_eq!(wm.focused_pane(), Some(1));
    }

    // ── workspace_ids ────────────────────────────────────────────────

    #[test]
    fn workspace_ids_sorted_after_creation() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        wm.switch_to(5, || Some((test_leaf(50, default_rect()), 50)));
        wm.switch_to(3, || Some((test_leaf(30, default_rect()), 30)));
        assert_eq!(wm.workspace_ids(), vec![1, 3, 5]);
    }

    #[test]
    fn workspace_ids_consistent_after_switch() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        wm.switch_to(2, || Some((test_leaf(20, default_rect()), 20)));
        wm.switch_to(1, || panic!("should not create"));
        // Switching back does not duplicate or remove workspace IDs.
        assert_eq!(wm.workspace_ids(), vec![1, 2]);
    }

    // ── send_pane_to ─────────────────────────────────────────────────

    #[test]
    fn send_pane_to_new_workspace() {
        let layout = two_pane_split(1, 2);
        let mut wm = WorkspaceManager::new(layout, Some(1));

        let sent = wm.send_pane_to(2, 3, || None);
        assert!(sent);

        // Workspace 1 should now have only pane 1.
        assert_eq!(wm.layout().pane_ids(), vec![1]);

        // Workspace 3 should exist with pane 2.
        assert_eq!(wm.workspace_ids(), vec![1, 3]);
    }

    #[test]
    fn send_pane_to_same_workspace_returns_false() {
        let layout = two_pane_split(1, 2);
        let mut wm = WorkspaceManager::new(layout, Some(1));
        let sent = wm.send_pane_to(1, 1, || None);
        assert!(!sent);
    }

    #[test]
    fn send_pane_to_out_of_range_returns_false() {
        let layout = two_pane_split(1, 2);
        let mut wm = WorkspaceManager::new(layout, Some(1));
        assert!(!wm.send_pane_to(1, 0, || None));
        assert!(!wm.send_pane_to(1, 10, || None));
    }

    #[test]
    fn send_focused_pane_updates_focus() {
        let layout = two_pane_split(1, 2);
        let mut wm = WorkspaceManager::new(layout, Some(1));

        // Send the focused pane (1) to workspace 2.
        let sent = wm.send_pane_to(1, 2, || None);
        assert!(sent);

        // Focus should move to the remaining pane (2).
        assert_eq!(wm.focused_pane(), Some(2));
        assert_eq!(wm.layout().pane_ids(), vec![2]);
    }

    #[test]
    fn send_last_pane_creates_default() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));

        let sent = wm.send_pane_to(1, 2, || Some((test_leaf(99, default_rect()), 99)));
        assert!(sent);

        // Workspace 1 should now have the default pane.
        assert_eq!(wm.layout().pane_ids(), vec![99]);
        assert_eq!(wm.focused_pane(), Some(99));
    }

    #[test]
    fn send_nonexistent_pane_returns_false() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        let sent = wm.send_pane_to(999, 2, || None);
        assert!(!sent);
    }

    #[test]
    fn send_pane_to_existing_workspace_adds_split() {
        let mut wm = WorkspaceManager::new(two_pane_split(1, 2), Some(1));
        // Create workspace 2 with a single pane.
        wm.switch_to(2, || Some((test_leaf(10, default_rect()), 10)));
        // Switch back to workspace 1.
        wm.switch_to(1, || panic!("should not create"));

        // Send pane 2 to workspace 2 (which already has pane 10).
        let sent = wm.send_pane_to(2, 2, || None);
        assert!(sent);

        // Workspace 1 should now have only pane 1.
        assert_eq!(wm.layout().pane_ids(), vec![1]);

        // Switch to workspace 2 and verify it has both panes.
        wm.switch_to(2, || panic!("should not create"));
        let ids = wm.layout().pane_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&10));
        assert!(ids.contains(&2));
    }

    // ── remove_empty_active ──────────────────────────────────────────

    #[test]
    fn remove_empty_active_with_single_workspace_returns_false() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        assert!(!wm.remove_empty_active());
    }

    #[test]
    fn remove_empty_active_with_non_empty_layout_returns_false() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        wm.switch_to(2, || Some((test_leaf(20, default_rect()), 20)));
        wm.switch_to(1, || panic!("should not create"));
        // Workspace 1 is not empty, so remove should fail.
        assert!(!wm.remove_empty_active());
    }

    #[test]
    fn remove_empty_active_removes_and_switches() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        wm.switch_to(2, || Some((test_leaf(20, default_rect()), 20)));
        wm.switch_to(1, || panic!("should not create"));

        // Make workspace 1 empty.
        wm.set_layout(LayoutNode::Empty);
        assert!(wm.remove_empty_active());

        // Should now be on workspace 2.
        assert_eq!(wm.workspace_ids(), vec![2]);
        assert_eq!(wm.active_id(), 2);
    }

    // ── workspace_info (daemon sync) ────────────────────────────────

    #[test]
    fn workspace_info_single_workspace() {
        let wm = WorkspaceManager::new(two_pane_split(1, 2), Some(1));
        let info = wm.workspace_info();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].id, 1);
        assert_eq!(info[0].name, "1");
        assert_eq!(info[0].order, 0);
        assert_eq!(info[0].focused_pane, Some(1));
        let mut pane_ids = info[0].pane_ids.clone();
        pane_ids.sort();
        assert_eq!(pane_ids, vec![1, 2]);
    }

    #[test]
    fn workspace_info_multiple_workspaces() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        wm.switch_to(3, || Some((test_leaf(30, default_rect()), 30)));
        wm.switch_to(1, || panic!("should not create"));

        let info = wm.workspace_info();
        assert_eq!(info.len(), 2);
        // Order matches internal Vec order.
        assert_eq!(info[0].id, 1);
        assert_eq!(info[0].order, 0);
        assert_eq!(info[1].id, 3);
        assert_eq!(info[1].order, 1);
        assert_eq!(info[1].pane_ids, vec![30]);
    }

    #[test]
    fn workspace_info_after_send_pane() {
        let mut wm = WorkspaceManager::new(two_pane_split(1, 2), Some(1));
        wm.send_pane_to(2, 3, || None);

        let info = wm.workspace_info();
        assert_eq!(info.len(), 2);

        // Workspace 1 should have pane 1.
        let ws1 = info.iter().find(|w| w.id == 1).unwrap();
        assert_eq!(ws1.pane_ids, vec![1]);

        // Workspace 3 should have pane 2.
        let ws3 = info.iter().find(|w| w.id == 3).unwrap();
        assert_eq!(ws3.pane_ids, vec![2]);
    }

    #[test]
    fn rename_active_workspace() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        wm.rename_active("build".to_string());
        let info = wm.workspace_info();
        assert_eq!(info[0].name, "build");
    }

    #[test]
    fn rename_by_id_existing() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        wm.switch_to(3, || Some((test_leaf(30, default_rect()), 30)));
        assert!(wm.rename(1, "main".to_string()));
        assert!(wm.rename(3, "test".to_string()));
        assert_eq!(wm.name_for(1), Some("main"));
        assert_eq!(wm.name_for(3), Some("test"));
    }

    #[test]
    fn rename_by_id_missing_returns_false() {
        let mut wm = WorkspaceManager::new(test_leaf(1, default_rect()), Some(1));
        assert!(!wm.rename(7, "ghost".to_string()));
        assert_eq!(wm.name_for(7), None);
    }

    // ── from_workspace_info (tn-ytw2 attach reconstruction) ─────────────

    #[test]
    fn from_workspace_info_reconstructs_split_layout_and_focus() {
        use therminal_protocol::daemon::{
            LayoutSnapshot as ProtoSnap, LayoutSplitDirection, WorkspaceInfo,
        };

        // Daemon-side topology: workspace 1 with a horizontal split between
        // daemon panes 100 and 200, focus on 200. Workspace 2 with a single
        // daemon pane 300 and no `layout` field (None → flat fallback).
        let ws1 = WorkspaceInfo {
            id: 1,
            name: "build".to_string(),
            order: 0,
            pane_ids: vec![100, 200],
            focused_pane: Some(200),
            layout: Some(ProtoSnap::Split {
                direction: LayoutSplitDirection::Horizontal,
                ratio: 0.4,
                first: Box::new(ProtoSnap::Leaf { pane_id: 100 }),
                second: Box::new(ProtoSnap::Leaf { pane_id: 200 }),
            }),
        };
        let ws2 = WorkspaceInfo {
            id: 2,
            name: "logs".to_string(),
            order: 1,
            pane_ids: vec![300],
            focused_pane: None,
            layout: None,
        };

        // make_leaf assigns sequential local ids and records the mapping
        // so the test can verify focus translation.
        let mut next_local: PaneId = 1;
        let mut mapping: std::collections::HashMap<therminal_protocol::PaneId, PaneId> =
            std::collections::HashMap::new();
        let mut make_leaf = |dpid: therminal_protocol::PaneId| -> Option<PaneState> {
            let local = next_local;
            next_local += 1;
            mapping.insert(dpid, local);
            Some(test_pane_state(local))
        };

        let wm = WorkspaceManager::from_workspace_info(&[ws1, ws2], 2, &mut make_leaf)
            .expect("reconstruction succeeded");
        // Closures don't implement Drop, so use scope-based dropping instead
        // of `drop(make_leaf)` to satisfy clippy::drop_non_drop. The closure
        // is no longer needed past this point.
        let _ = make_leaf;

        // Active workspace is the one matching active_workspace_id (2).
        assert_eq!(wm.active_id(), 2);
        assert_eq!(wm.workspace_ids(), vec![1, 2]);

        // Workspace 1: split layout preserved with two leaves.
        let ws1_info = &wm.workspaces[0];
        assert_eq!(ws1_info.id, 1);
        assert_eq!(ws1_info.name, "build");
        match &ws1_info.layout {
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                assert_eq!(*direction, SplitDirection::Horizontal);
                assert!((ratio - 0.4).abs() < 1e-6);
                assert!(matches!(first.as_ref(), LayoutNode::Leaf(_)));
                assert!(matches!(second.as_ref(), LayoutNode::Leaf(_)));
            }
            other => panic!(
                "expected Split, got {other:?}",
                other = match other {
                    LayoutNode::Leaf(_) => "Leaf",
                    LayoutNode::Empty => "Empty",
                    LayoutNode::Split { .. } => unreachable!(),
                }
            ),
        }
        // Focused pane should be the local id mapped from daemon id 200.
        let local_for_200 = mapping[&200];
        assert_eq!(ws1_info.focused_pane, Some(local_for_200));

        // Workspace 2: single-leaf flat fallback (layout was None).
        let ws2_info = &wm.workspaces[1];
        assert_eq!(ws2_info.id, 2);
        assert!(matches!(ws2_info.layout, LayoutNode::Leaf(_)));
        let local_for_300 = mapping[&300];
        assert_eq!(ws2_info.focused_pane, Some(local_for_300));
    }

    #[test]
    fn from_workspace_info_empty_returns_none() {
        let mut make_leaf = |_dpid: therminal_protocol::PaneId| -> Option<PaneState> { None };
        assert!(WorkspaceManager::from_workspace_info(&[], 1, &mut make_leaf).is_none());
    }
}
