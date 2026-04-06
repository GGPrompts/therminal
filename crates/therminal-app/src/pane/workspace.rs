//! Workspace manager: named workspace slots with independent pane layouts.

use super::layout::LayoutNode;
use super::state::PaneState;
use super::PaneId;
use super::SplitDirection;

/// A workspace holds an independent pane layout with its own focused pane.
pub struct Workspace {
    /// Workspace number (1-9).
    pub id: usize,
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
}

impl WorkspaceManager {
    /// Create a new manager with workspace 1 containing the given layout.
    pub fn new(layout: LayoutNode, focused_pane: Option<PaneId>) -> Self {
        let ws = Workspace {
            id: 1,
            layout,
            focused_pane,
        };
        Self {
            workspaces: vec![ws],
            active_idx: 0,
        }
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

    /// Return all workspace IDs that currently exist, sorted.
    pub fn workspace_ids(&self) -> Vec<usize> {
        let mut ids: Vec<usize> = self.workspaces.iter().map(|ws| ws.id).collect();
        ids.sort();
        ids
    }

    /// Returns true if the manager has no workspaces (shouldn't normally happen).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
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
