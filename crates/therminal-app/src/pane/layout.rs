//! Binary layout tree for pane splits.
//!
//! `LayoutNode` is a recursive enum: each node is either a `Leaf` (terminal pane),
//! a `Split` (two children with a direction and ratio), or `Empty` (transient placeholder).

use therminal_core::geometry::Rect;

use super::geometry::{effective_header_height, MIN_PANE_HEIGHT, MIN_PANE_WIDTH, SEPARATOR_GAP};
use super::state::PaneState;
use super::SplitDirection;
use crate::grid_renderer::GridRenderer;

/// A node in the binary layout tree.
pub enum LayoutNode {
    /// A terminal pane leaf.
    Leaf(PaneState),
    /// A split containing two children.
    Split {
        direction: SplitDirection,
        /// Ratio of first child's share (0.0..1.0).
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
    /// A zero-cost placeholder used transiently during tree restructuring
    /// (e.g. with `std::mem::replace`). Should never persist in the final tree.
    Empty,
}

impl LayoutNode {
    /// Compute viewport rects for all leaves given the available rect.
    pub fn layout(&mut self, rect: Rect) {
        match self {
            LayoutNode::Leaf(pane) => {
                pane.viewport = rect;
            }
            LayoutNode::Split {
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
                first.layout(r1);
                second.layout(r2);
            }
            LayoutNode::Empty => {}
        }
    }

    /// Resize all pane PTYs to match their current viewport rects.
    pub fn resize_all_panes(&mut self, renderer: &GridRenderer) {
        let header_h = effective_header_height(self.pane_count());
        self.resize_all_panes_with_header(renderer, header_h);
    }

    /// Resize all panes with an explicit header height.
    pub fn resize_all_panes_with_header(&mut self, renderer: &GridRenderer, header_h: f32) {
        match self {
            LayoutNode::Leaf(pane) => {
                let rect = pane.viewport;
                pane.resize_to_viewport_with_header(rect, renderer, header_h);
            }
            LayoutNode::Split { first, second, .. } => {
                first.resize_all_panes_with_header(renderer, header_h);
                second.resize_all_panes_with_header(renderer, header_h);
            }
            LayoutNode::Empty => {}
        }
    }

    /// Collect all pane IDs in order (left-to-right / top-to-bottom).
    pub fn pane_ids(&self) -> Vec<super::PaneId> {
        let mut ids = Vec::new();
        self.collect_ids(&mut ids);
        ids
    }

    fn collect_ids(&self, ids: &mut Vec<super::PaneId>) {
        match self {
            LayoutNode::Leaf(pane) => ids.push(pane.id),
            LayoutNode::Split { first, second, .. } => {
                first.collect_ids(ids);
                second.collect_ids(ids);
            }
            LayoutNode::Empty => {}
        }
    }

    /// Find a pane by ID.
    pub fn find_pane(&self, id: super::PaneId) -> Option<&PaneState> {
        match self {
            LayoutNode::Leaf(pane) if pane.id == id => Some(pane),
            LayoutNode::Leaf(_) => None,
            LayoutNode::Split { first, second, .. } => {
                first.find_pane(id).or_else(|| second.find_pane(id))
            }
            LayoutNode::Empty => None,
        }
    }

    /// Find a mutable pane by ID.
    pub fn find_pane_mut(&mut self, id: super::PaneId) -> Option<&mut PaneState> {
        match self {
            LayoutNode::Leaf(pane) if pane.id == id => Some(pane),
            LayoutNode::Leaf(_) => None,
            LayoutNode::Split { first, second, .. } => {
                first.find_pane_mut(id).or_else(|| second.find_pane_mut(id))
            }
            LayoutNode::Empty => None,
        }
    }

    /// Split the pane with the given ID. Returns the new pane's ID, or None if not found.
    ///
    /// The `spawn_fn` is called to create the new pane's state.
    pub fn split_pane<F>(
        &mut self,
        target_id: super::PaneId,
        direction: SplitDirection,
        spawn_fn: F,
    ) -> Option<super::PaneId>
    where
        F: FnOnce(Rect) -> Option<PaneState>,
    {
        let mut slot = Some(spawn_fn);
        self.split_pane_impl(target_id, direction, &mut slot)
    }

    fn split_pane_impl<F>(
        &mut self,
        target_id: super::PaneId,
        direction: SplitDirection,
        spawn_fn: &mut Option<F>,
    ) -> Option<super::PaneId>
    where
        F: FnOnce(Rect) -> Option<PaneState>,
    {
        match self {
            LayoutNode::Leaf(pane) if pane.id == target_id => {
                let rect = pane.viewport;

                // Refuse to split if result would be below minimum pane size.
                if !LayoutNode::can_split(rect, direction) {
                    tracing::warn!(
                        "Cannot split pane {}: result would be below minimum size",
                        target_id
                    );
                    return None;
                }

                let factory = spawn_fn.take().expect("spawn_fn already consumed");
                let (r1, r2) = match direction {
                    SplitDirection::Horizontal => rect.split_horizontal_ratio(0.5, SEPARATOR_GAP),
                    SplitDirection::Vertical => rect.split_vertical_ratio(0.5, SEPARATOR_GAP),
                };

                let new_pane = factory(r2)?;
                let new_id = new_pane.id;

                // Take self out via dummy, then replace with the Split node.
                let old_self = std::mem::replace(self, LayoutNode::Empty);

                let mut old_leaf = match old_self {
                    LayoutNode::Leaf(p) => p,
                    _ => unreachable!(),
                };
                old_leaf.viewport = r1;

                *self = LayoutNode::Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(LayoutNode::Leaf(old_leaf)),
                    second: Box::new(LayoutNode::Leaf(new_pane)),
                };

                Some(new_id)
            }
            LayoutNode::Leaf(_) => None,
            LayoutNode::Split { first, second, .. } => {
                let result = first.split_pane_impl(target_id, direction, spawn_fn);
                if result.is_some() {
                    // Rebalance this node after a child was split.
                    let first_leaves = first.leaf_count() as f32;
                    let total_leaves = first_leaves + second.leaf_count() as f32;
                    if let LayoutNode::Split { ratio, .. } = self {
                        *ratio = (first_leaves / total_leaves).clamp(0.1, 0.9);
                    }
                    return result;
                }
                let result = second.split_pane_impl(target_id, direction, spawn_fn);
                if result.is_some() {
                    // Rebalance this node after a child was split.
                    let first_leaves = first.leaf_count() as f32;
                    let total_leaves = first_leaves + second.leaf_count() as f32;
                    if let LayoutNode::Split { ratio, .. } = self {
                        *ratio = (first_leaves / total_leaves).clamp(0.1, 0.9);
                    }
                }
                result
            }
            LayoutNode::Empty => None,
        }
    }

    /// Remove a pane by ID. Returns true if found and removed.
    /// After removal the sibling takes the parent's position and the tree
    /// is rebalanced so remaining panes share space proportionally.
    /// Returns None if the pane is the only one (root leaf).
    pub fn remove_pane(&mut self, target_id: super::PaneId) -> Option<bool> {
        match self {
            LayoutNode::Leaf(pane) if pane.id == target_id => {
                // This is the root leaf -- caller should handle window close.
                None
            }
            LayoutNode::Leaf(_) => Some(false),
            LayoutNode::Split { first, second, .. } => {
                // Check if target is a direct child.
                let first_is_target =
                    matches!(first.as_ref(), LayoutNode::Leaf(p) if p.id == target_id);
                let second_is_target =
                    matches!(second.as_ref(), LayoutNode::Leaf(p) if p.id == target_id);

                if first_is_target {
                    let sibling = std::mem::replace(second.as_mut(), LayoutNode::Empty);
                    *self = sibling;
                    // Rebalance the promoted subtree.
                    self.rebalance();
                    return Some(true);
                }

                if second_is_target {
                    let sibling = std::mem::replace(first.as_mut(), LayoutNode::Empty);
                    *self = sibling;
                    // Rebalance the promoted subtree.
                    self.rebalance();
                    return Some(true);
                }

                // Recurse.
                let removed = first.remove_pane(target_id);
                if removed == Some(true) {
                    // Rebalance this node after child removal changed leaf counts.
                    let first_leaves = first.leaf_count() as f32;
                    let total_leaves = first_leaves + second.leaf_count() as f32;
                    if let LayoutNode::Split { ratio, .. } = self {
                        *ratio = (first_leaves / total_leaves).clamp(0.1, 0.9);
                    }
                    return Some(true);
                }
                let removed = second.remove_pane(target_id);
                if removed == Some(true) {
                    let first_leaves = first.leaf_count() as f32;
                    let total_leaves = first_leaves + second.leaf_count() as f32;
                    if let LayoutNode::Split { ratio, .. } = self {
                        *ratio = (first_leaves / total_leaves).clamp(0.1, 0.9);
                    }
                }
                removed
            }
            LayoutNode::Empty => Some(false),
        }
    }

    /// Find the next pane ID in the given direction relative to the focused pane.
    pub fn adjacent_pane(
        &self,
        focused_id: super::PaneId,
        direction: FocusDirection,
    ) -> Option<super::PaneId> {
        let ids = self.pane_ids();
        let idx = ids.iter().position(|&id| id == focused_id)?;
        match direction {
            FocusDirection::Next => {
                let next = (idx + 1) % ids.len();
                Some(ids[next])
            }
            FocusDirection::Prev => {
                let prev = if idx == 0 { ids.len() - 1 } else { idx - 1 };
                Some(ids[prev])
            }
        }
    }

    /// Swap the positions of two panes in the tree by exchanging their `PaneState` contents.
    /// Returns `true` if both panes were found and swapped.
    pub fn swap_pane(&mut self, a: super::PaneId, b: super::PaneId) -> bool {
        // Collect mutable pointers to the two leaf PaneStates.
        fn find_leaf(node: &mut LayoutNode, id: super::PaneId) -> Option<*mut PaneState> {
            match node {
                LayoutNode::Leaf(ps) if ps.id == id => Some(ps as *mut PaneState),
                LayoutNode::Split { first, second, .. } => {
                    find_leaf(first, id).or_else(|| find_leaf(second, id))
                }
                _ => None,
            }
        }

        let ptr_a = find_leaf(self, a);
        let ptr_b = find_leaf(self, b);

        match (ptr_a, ptr_b) {
            (Some(pa), Some(pb)) if pa != pb => {
                // SAFETY: pa and pb point to distinct leaf nodes in the tree,
                // so there is no aliasing. We swap the PaneState contents and
                // then restore each node's original viewport so the visual
                // layout stays the same.
                unsafe {
                    std::ptr::swap(pa, pb);
                    // After swapping, the viewports followed the PaneState data.
                    // Exchange them back so each position keeps its original rect.
                    std::mem::swap(&mut (*pa).viewport, &mut (*pb).viewport);
                }
                true
            }
            _ => false,
        }
    }

    /// Adjust the split ratio for the split containing the focused pane.
    pub fn adjust_ratio(&mut self, focused_id: super::PaneId, delta: f32) -> bool {
        match self {
            LayoutNode::Leaf(_) => false,
            LayoutNode::Split {
                ratio,
                first,
                second,
                ..
            } => {
                let first_ids = first.pane_ids();
                let second_ids = second.pane_ids();
                let in_first = first_ids.contains(&focused_id);
                let in_second = second_ids.contains(&focused_id);

                if in_first || in_second {
                    // Try to adjust in child first (deeper splits).
                    let adjusted_in_child = if in_first {
                        first.adjust_ratio(focused_id, delta)
                    } else {
                        second.adjust_ratio(focused_id, delta)
                    };
                    if adjusted_in_child {
                        return true;
                    }
                    // Adjust this split's ratio.
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    true
                } else {
                    false
                }
            }
            LayoutNode::Empty => false,
        }
    }

    /// Hit-test for separator drag: find the split node whose separator is
    /// within `tolerance` pixels of `(px, py)`.
    ///
    /// Returns `Some((path, direction, parent_rect))` where `path` is the
    /// sequence of `false` (first child) / `true` (second child) steps from
    /// the root to the split node, `direction` is the split direction, and
    /// `parent_rect` is the bounding rect of the split node (used for ratio
    /// computation during drag).
    pub fn separator_hit_test(
        &self,
        px: f32,
        py: f32,
        tolerance: f32,
        parent_rect: Rect,
    ) -> Option<(Vec<bool>, SplitDirection, Rect)> {
        match self {
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                // Compute child rects the same way layout() does.
                let (r1, r2) = match direction {
                    SplitDirection::Horizontal => {
                        parent_rect.split_horizontal_ratio(*ratio, SEPARATOR_GAP)
                    }
                    SplitDirection::Vertical => {
                        parent_rect.split_vertical_ratio(*ratio, SEPARATOR_GAP)
                    }
                };

                // Build the separator hit zone (expanded by tolerance).
                let hit = match direction {
                    SplitDirection::Horizontal => {
                        // Vertical separator between left and right.
                        let sep_x = r1.right();
                        let sep_y = parent_rect.y();
                        let sep_h = parent_rect.height();
                        px >= sep_x - tolerance
                            && px <= sep_x + SEPARATOR_GAP + tolerance
                            && py >= sep_y
                            && py <= sep_y + sep_h
                    }
                    SplitDirection::Vertical => {
                        // Horizontal separator between top and bottom.
                        let sep_y = r1.bottom();
                        let sep_x = parent_rect.x();
                        let sep_w = parent_rect.width();
                        py >= sep_y - tolerance
                            && py <= sep_y + SEPARATOR_GAP + tolerance
                            && px >= sep_x
                            && px <= sep_x + sep_w
                    }
                };

                if hit {
                    return Some((vec![], *direction, parent_rect));
                }

                // Recurse into children.
                if let Some((mut path, dir, rect)) = first.separator_hit_test(px, py, tolerance, r1)
                {
                    path.insert(0, false);
                    return Some((path, dir, rect));
                }
                if let Some((mut path, dir, rect)) =
                    second.separator_hit_test(px, py, tolerance, r2)
                {
                    path.insert(0, true);
                    return Some((path, dir, rect));
                }

                None
            }
            _ => None,
        }
    }

    /// Set the ratio of a split node identified by `path` (from `separator_hit_test`).
    /// Returns `true` if the ratio was set.
    pub fn set_ratio_at_path(&mut self, path: &[bool], new_ratio: f32) -> bool {
        if path.is_empty() {
            if let LayoutNode::Split { ratio, .. } = self {
                *ratio = new_ratio.clamp(0.1, 0.9);
                return true;
            }
            return false;
        }
        match self {
            LayoutNode::Split { first, second, .. } => {
                if path[0] {
                    second.set_ratio_at_path(&path[1..], new_ratio)
                } else {
                    first.set_ratio_at_path(&path[1..], new_ratio)
                }
            }
            _ => false,
        }
    }

    /// Collect separator rects for drawing.
    #[allow(dead_code)]
    pub fn separator_rects(&self) -> Vec<Rect> {
        let mut rects = Vec::new();
        self.collect_separators(&mut rects);
        rects
    }

    fn collect_separators(&self, rects: &mut Vec<Rect>) {
        if let LayoutNode::Split {
            direction,
            first,
            second,
            ..
        } = self
        {
            // The separator is in the gap between first and second children.
            // Get the boundary from the first child's far edge.
            let first_leaves = first.leaf_rects();
            let second_leaves = second.leaf_rects();

            if let (Some(f), Some(s)) = (first_leaves.last(), second_leaves.first()) {
                match direction {
                    SplitDirection::Horizontal => {
                        // Vertical separator line between left and right.
                        let sep_x = f.right();
                        let sep_y = f.y().min(s.y());
                        let sep_h = f.bottom().max(s.bottom()) - sep_y;
                        rects.push(Rect::new(sep_x, sep_y, SEPARATOR_GAP, sep_h));
                    }
                    SplitDirection::Vertical => {
                        // Horizontal separator line between top and bottom.
                        let sep_x = f.x().min(s.x());
                        let sep_y = f.bottom();
                        let sep_w = f.right().max(s.right()) - sep_x;
                        rects.push(Rect::new(sep_x, sep_y, sep_w, SEPARATOR_GAP));
                    }
                }
            }

            first.collect_separators(rects);
            second.collect_separators(rects);
        }
    }

    /// Collect leaf viewport rects (public version for separator drawing).
    pub fn leaf_rects_pub(&self) -> Vec<Rect> {
        self.leaf_rects()
    }

    fn leaf_rects(&self) -> Vec<Rect> {
        match self {
            LayoutNode::Leaf(pane) => vec![pane.viewport],
            LayoutNode::Split { first, second, .. } => {
                let mut rects = first.leaf_rects();
                rects.extend(second.leaf_rects());
                rects
            }
            LayoutNode::Empty => vec![],
        }
    }

    /// Count panes.
    pub fn pane_count(&self) -> usize {
        match self {
            LayoutNode::Leaf(_) => 1,
            LayoutNode::Split { first, second, .. } => first.pane_count() + second.pane_count(),
            LayoutNode::Empty => 0,
        }
    }

    /// Count leaf nodes (alias for pane_count, used for ratio computation).
    pub fn leaf_count(&self) -> usize {
        self.pane_count()
    }

    /// Rebalance the tree so that all leaves get approximately equal space.
    ///
    /// For each Split node, sets `ratio` = (first child leaf count) / (total leaf count)
    /// so that space is divided proportionally to the number of leaves on each side.
    pub fn rebalance(&mut self) {
        if let LayoutNode::Split {
            ratio,
            first,
            second,
            ..
        } = self
        {
            let first_leaves = first.leaf_count() as f32;
            let total_leaves = first_leaves + second.leaf_count() as f32;
            if total_leaves > 0.0 {
                *ratio = (first_leaves / total_leaves).clamp(0.1, 0.9);
            }
            first.rebalance();
            second.rebalance();
        }
    }

    /// Determine the best split direction for a pane based on its viewport rect.
    ///
    /// - If width > height * 1.2: Horizontal (side-by-side) to use the wide space
    /// - If height > width * 1.2: Vertical (stacked) to use the tall space
    /// - Otherwise: use `fallback` (caller alternates based on last split)
    pub fn auto_split_direction(rect: Rect, fallback: SplitDirection) -> SplitDirection {
        if rect.width() > rect.height() * 1.2 {
            SplitDirection::Horizontal
        } else if rect.height() > rect.width() * 1.2 {
            SplitDirection::Vertical
        } else {
            fallback
        }
    }

    /// Check whether splitting `rect` in `direction` would produce children
    /// below the minimum pane size.
    pub fn can_split(rect: Rect, direction: SplitDirection) -> bool {
        match direction {
            SplitDirection::Horizontal => {
                let usable = rect.width() - SEPARATOR_GAP;
                // Each child gets half
                usable / 2.0 >= MIN_PANE_WIDTH
            }
            SplitDirection::Vertical => {
                let usable = rect.height() - SEPARATOR_GAP;
                usable / 2.0 >= MIN_PANE_HEIGHT
            }
        }
    }
}

/// Direction for focus navigation.
#[derive(Debug, Clone, Copy)]
pub enum FocusDirection {
    Next,
    Prev,
}

// ── Layout snapshot for restore ────────────────────────────────────────

/// A lightweight snapshot of the layout tree structure (no PTY state).
/// Used to restore a layout after `close_all_panes()`.
#[derive(Debug, Clone)]
pub enum LayoutSnapshot {
    /// A single pane leaf (no state -- will be respawned).
    Leaf,
    /// A split with its direction, ratio, and child snapshots.
    Split {
        direction: SplitDirection,
        ratio: f32,
        first: Box<LayoutSnapshot>,
        second: Box<LayoutSnapshot>,
    },
}

impl LayoutNode {
    /// Take a snapshot of the layout tree structure (directions + ratios only).
    pub fn snapshot(&self) -> LayoutSnapshot {
        match self {
            LayoutNode::Leaf(_) => LayoutSnapshot::Leaf,
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => LayoutSnapshot::Split {
                direction: *direction,
                ratio: *ratio,
                first: Box::new(first.snapshot()),
                second: Box::new(second.snapshot()),
            },
            LayoutNode::Empty => LayoutSnapshot::Leaf,
        }
    }

    /// Count the number of leaves in a snapshot.
    pub fn snapshot_leaf_count(snap: &LayoutSnapshot) -> usize {
        match snap {
            LayoutSnapshot::Leaf => 1,
            LayoutSnapshot::Split { first, second, .. } => {
                Self::snapshot_leaf_count(first) + Self::snapshot_leaf_count(second)
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use alacritty_terminal::sync::FairMutex;
    use alacritty_terminal::term::{Config as TermConfig, Term};
    use therminal_core::geometry::Rect;

    use super::*;
    use crate::pane::state::PaneTermSize;
    use crate::pane::PaneListener;

    /// Helper to create a minimal test leaf node (no real PTY).
    fn test_leaf(id: super::super::PaneId, rect: Rect) -> LayoutNode {
        let term = Term::new(
            TermConfig::default(),
            &PaneTermSize {
                columns: 80,
                screen_lines: 24,
            },
            PaneListener,
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
            term: Arc::new(FairMutex::new(term)),
            pty_writer: writer,
            pty_master: pair.master,
            viewport: rect,
            scrollback_lines: 1000,
            status: Arc::new(Mutex::new(super::super::PaneStatus::default())),
        })
    }

    #[test]
    fn single_pane_layout() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut node = test_leaf(1, rect);
        node.layout(rect);
        assert_eq!(node.pane_count(), 1);
        assert_eq!(node.pane_ids(), vec![1]);
    }

    #[test]
    fn split_and_count() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut node = test_leaf(1, rect);

        let new_id = node.split_pane(1, SplitDirection::Horizontal, |r| {
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            let writer = pair.master.take_writer().unwrap();
            Some(PaneState {
                id: 2,
                term: Arc::new(FairMutex::new(Term::new(
                    TermConfig::default(),
                    &PaneTermSize {
                        columns: 80,
                        screen_lines: 24,
                    },
                    PaneListener,
                ))),
                pty_writer: writer,
                pty_master: pair.master,
                viewport: r,
                scrollback_lines: 1000,
                status: Arc::new(Mutex::new(super::super::PaneStatus::default())),
            })
        });

        assert_eq!(new_id, Some(2));
        assert_eq!(node.pane_count(), 2);
        assert_eq!(node.pane_ids(), vec![1, 2]);
    }

    #[test]
    fn remove_pane_rebalances() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let first = test_leaf(1, rect);
        let second = test_leaf(2, rect);
        let mut root = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        };

        let result = root.remove_pane(1);
        assert_eq!(result, Some(true));
        assert_eq!(root.pane_count(), 1);
        assert_eq!(root.pane_ids(), vec![2]);
    }

    #[test]
    fn remove_last_pane_returns_none() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = test_leaf(1, rect);
        assert_eq!(root.remove_pane(1), None);
    }

    #[test]
    fn adjacent_pane_wraps() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let first = test_leaf(1, rect);
        let second = test_leaf(2, rect);
        let root = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        };

        assert_eq!(root.adjacent_pane(1, FocusDirection::Next), Some(2));
        assert_eq!(root.adjacent_pane(2, FocusDirection::Next), Some(1));
        assert_eq!(root.adjacent_pane(1, FocusDirection::Prev), Some(2));
        assert_eq!(root.adjacent_pane(2, FocusDirection::Prev), Some(1));
    }

    #[test]
    fn adjust_ratio_clamps() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let first = test_leaf(1, rect);
        let second = test_leaf(2, rect);
        let mut root = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        };

        // Adjust by a lot to test clamping.
        root.adjust_ratio(1, 10.0);
        if let LayoutNode::Split { ratio, .. } = &root {
            assert!((*ratio - 0.9).abs() < f32::EPSILON);
        }
    }

    // ── Edge-case tests ──────────────────────────────────────────────────

    /// Helper to build a leaf without a real PTY pair using sequential IDs.
    /// Returns (node, id).
    fn make_leaf(id: super::super::PaneId) -> LayoutNode {
        test_leaf(id, Rect::new(0.0, 0.0, 800.0, 600.0))
    }

    /// Build a split node from two pre-built children.
    fn make_split(
        direction: SplitDirection,
        ratio: f32,
        first: LayoutNode,
        second: LayoutNode,
    ) -> LayoutNode {
        LayoutNode::Split {
            direction,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// Collect all viewport rects from leaves in the tree.
    fn collect_leaf_rects(node: &LayoutNode) -> Vec<Rect> {
        match node {
            LayoutNode::Leaf(pane) => vec![pane.viewport],
            LayoutNode::Split { first, second, .. } => {
                let mut rects = collect_leaf_rects(first);
                rects.extend(collect_leaf_rects(second));
                rects
            }
            LayoutNode::Empty => vec![],
        }
    }

    // ── 1. Deep nesting: split 5+ times, close inner panes ──────────────

    #[test]
    fn deep_nesting_split_and_close_rebalances() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = test_leaf(1, rect);

        for id in 2u64..=6 {
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            let writer = pair.master.take_writer().unwrap();
            let new_pane = PaneState {
                id,
                term: Arc::new(FairMutex::new(Term::new(
                    TermConfig::default(),
                    &PaneTermSize {
                        columns: 80,
                        screen_lines: 24,
                    },
                    PaneListener,
                ))),
                pty_writer: writer,
                pty_master: pair.master,
                viewport: rect,
                scrollback_lines: 1000,
                status: Arc::new(Mutex::new(super::super::PaneStatus::default())),
            };
            let rightmost_id = *root.pane_ids().last().unwrap();
            root.split_pane(rightmost_id, SplitDirection::Horizontal, |r| {
                let _ = r;
                Some(new_pane)
            });
        }

        assert_eq!(root.pane_count(), 6);
        assert_eq!(root.pane_ids(), vec![1, 2, 3, 4, 5, 6]);

        for id in [3u64, 4, 5] {
            let result = root.remove_pane(id);
            assert_eq!(result, Some(true), "should remove pane {id}");
        }

        assert_eq!(root.pane_count(), 3);
        let ids = root.pane_ids();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(ids.contains(&6));
        assert!(!ids.contains(&3));
        assert!(!ids.contains(&4));
        assert!(!ids.contains(&5));
    }

    // ── 2. Resize propagation: leaf rects must tile the viewport exactly ─

    #[test]
    fn layout_rects_tile_viewport_exactly() {
        use super::super::geometry::SEPARATOR_GAP;

        let viewport = Rect::new(0.0, 0.0, 800.0, 600.0);

        let left = make_split(SplitDirection::Vertical, 0.5, make_leaf(1), make_leaf(2));
        let mut root = make_split(SplitDirection::Horizontal, 0.5, left, make_leaf(3));

        root.layout(viewport);

        let rects = collect_leaf_rects(&root);
        assert_eq!(rects.len(), 3);

        let leaf_area: f32 = rects.iter().map(|r| r.width() * r.height()).sum();
        let viewport_area = viewport.width() * viewport.height();

        let gap_budget = SEPARATOR_GAP * (viewport.width() + viewport.height());
        assert!(
            (viewport_area - leaf_area).abs() < gap_budget + 1.0,
            "leaf area {leaf_area} should be close to viewport area {viewport_area}"
        );

        for r in &rects {
            assert!(r.x() >= viewport.x() - f32::EPSILON);
            assert!(r.y() >= viewport.y() - f32::EPSILON);
            assert!(r.right() <= viewport.right() + f32::EPSILON);
            assert!(r.bottom() <= viewport.bottom() + f32::EPSILON);
        }
    }

    // ── 3. Asymmetric splits: close from the shallow side ───────────────

    #[test]
    fn asymmetric_tree_close_shallow_side() {
        let deep = make_split(SplitDirection::Horizontal, 0.5, make_leaf(3), make_leaf(4));
        let right_subtree = make_split(SplitDirection::Vertical, 0.5, make_leaf(2), deep);
        let mut root = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), right_subtree);

        assert_eq!(root.pane_count(), 4);

        let result = root.remove_pane(1);
        assert_eq!(result, Some(true));

        assert_eq!(root.pane_count(), 3);
        let ids = root.pane_ids();
        assert_eq!(ids, vec![2, 3, 4]);

        assert!(
            matches!(&root, LayoutNode::Split { .. }),
            "root should be a split node after promotion"
        );
    }

    // ── 4. Ratio extremes: verify minimum sizes enforced ────────────────

    #[test]
    fn ratio_extreme_low_layout_stays_within_bounds() {
        let viewport = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = make_split(SplitDirection::Horizontal, 0.1, make_leaf(1), make_leaf(2));
        root.layout(viewport);

        let rects = collect_leaf_rects(&root);
        assert_eq!(rects.len(), 2);

        for r in &rects {
            assert!(r.width() > 0.0, "pane width must be positive at ratio 0.1");
            assert!(r.height() > 0.0);
        }
        for r in &rects {
            assert!(r.right() <= viewport.right() + f32::EPSILON);
            assert!(r.bottom() <= viewport.bottom() + f32::EPSILON);
        }
    }

    #[test]
    fn ratio_extreme_high_layout_stays_within_bounds() {
        let viewport = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = make_split(SplitDirection::Vertical, 0.9, make_leaf(1), make_leaf(2));
        root.layout(viewport);

        let rects = collect_leaf_rects(&root);
        assert_eq!(rects.len(), 2);

        for r in &rects {
            assert!(r.width() > 0.0);
            assert!(
                r.height() > 0.0,
                "pane height must be positive at ratio 0.9"
            );
        }
        for r in &rects {
            assert!(r.right() <= viewport.right() + f32::EPSILON);
            assert!(r.bottom() <= viewport.bottom() + f32::EPSILON);
        }
    }

    // ── 5. Close all panes one-by-one: last close returns None ──────────

    #[test]
    fn close_all_panes_last_returns_none() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);

        let mut root = make_split(
            SplitDirection::Horizontal,
            0.5,
            make_split(SplitDirection::Vertical, 0.5, make_leaf(1), make_leaf(2)),
            make_split(SplitDirection::Vertical, 0.5, make_leaf(3), make_leaf(4)),
        );
        let _ = rect;

        assert_eq!(root.pane_count(), 4);

        for id in [1u64, 2, 3] {
            let result = root.remove_pane(id);
            assert_eq!(
                result,
                Some(true),
                "removing pane {id} should return Some(true)"
            );
        }

        assert_eq!(root.pane_count(), 1);
        let last_id = root.pane_ids()[0];
        let result = root.remove_pane(last_id);
        assert_eq!(result, None, "closing last pane must return None");
    }

    // ── 6. Adjacent pane navigation with complex trees ───────────────────

    #[test]
    fn adjacent_pane_navigation_complex_tree() {
        let root = make_split(
            SplitDirection::Horizontal,
            0.5,
            make_split(SplitDirection::Vertical, 0.5, make_leaf(1), make_leaf(2)),
            make_split(SplitDirection::Vertical, 0.5, make_leaf(3), make_leaf(4)),
        );

        assert_eq!(root.adjacent_pane(1, FocusDirection::Next), Some(2));
        assert_eq!(root.adjacent_pane(2, FocusDirection::Next), Some(3));
        assert_eq!(root.adjacent_pane(3, FocusDirection::Next), Some(4));
        assert_eq!(root.adjacent_pane(4, FocusDirection::Next), Some(1));

        assert_eq!(root.adjacent_pane(1, FocusDirection::Prev), Some(4));
        assert_eq!(root.adjacent_pane(2, FocusDirection::Prev), Some(1));
        assert_eq!(root.adjacent_pane(3, FocusDirection::Prev), Some(2));
        assert_eq!(root.adjacent_pane(4, FocusDirection::Prev), Some(3));
    }

    #[test]
    fn adjacent_pane_unknown_id_returns_none() {
        let root = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        assert_eq!(root.adjacent_pane(99, FocusDirection::Next), None);
        assert_eq!(root.adjacent_pane(99, FocusDirection::Prev), None);
    }

    // ── Rebalance tests ─────────────────────────────────────────────────

    #[test]
    fn rebalance_3_way_even() {
        let inner = make_split(SplitDirection::Horizontal, 0.5, make_leaf(2), make_leaf(3));
        let mut root = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), inner);

        root.rebalance();

        if let LayoutNode::Split { ratio, second, .. } = &root {
            assert!(
                (*ratio - 1.0 / 3.0).abs() < 0.01,
                "root ratio should be ~0.333, got {ratio}"
            );
            if let LayoutNode::Split { ratio: inner_r, .. } = second.as_ref() {
                assert!(
                    (*inner_r - 0.5).abs() < 0.01,
                    "inner ratio should be ~0.5, got {inner_r}"
                );
            }
        } else {
            panic!("expected Split at root");
        }
    }

    #[test]
    fn rebalance_4_way_even() {
        let left = make_split(SplitDirection::Horizontal, 0.3, make_leaf(1), make_leaf(2));
        let right = make_split(SplitDirection::Horizontal, 0.7, make_leaf(3), make_leaf(4));
        let mut root = make_split(SplitDirection::Horizontal, 0.8, left, right);

        root.rebalance();

        if let LayoutNode::Split {
            ratio,
            first,
            second,
            ..
        } = &root
        {
            assert!(
                (*ratio - 0.5).abs() < 0.01,
                "root ratio should be ~0.5, got {ratio}"
            );
            if let LayoutNode::Split { ratio: lr, .. } = first.as_ref() {
                assert!((*lr - 0.5).abs() < 0.01, "left inner ratio: {lr}");
            }
            if let LayoutNode::Split { ratio: rr, .. } = second.as_ref() {
                assert!((*rr - 0.5).abs() < 0.01, "right inner ratio: {rr}");
            }
        }
    }

    #[test]
    fn rebalance_asymmetric() {
        let inner = make_split(SplitDirection::Horizontal, 0.5, make_leaf(3), make_leaf(4));
        let mid = make_split(SplitDirection::Horizontal, 0.5, make_leaf(2), inner);
        let mut root = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), mid);

        root.rebalance();

        if let LayoutNode::Split { ratio, second, .. } = &root {
            assert!(
                (*ratio - 0.25).abs() < 0.01,
                "root ratio should be ~0.25, got {ratio}"
            );
            if let LayoutNode::Split {
                ratio: mid_r,
                second: inner_node,
                ..
            } = second.as_ref()
            {
                assert!(
                    (*mid_r - 1.0 / 3.0).abs() < 0.01,
                    "mid ratio should be ~0.333, got {mid_r}"
                );
                if let LayoutNode::Split { ratio: inner_r, .. } = inner_node.as_ref() {
                    assert!((*inner_r - 0.5).abs() < 0.01, "inner ratio: {inner_r}");
                }
            }
        }
    }

    // ── Auto-direction tests ────────────────────────────────────────────

    #[test]
    fn auto_direction_wide_rect() {
        let rect = Rect::new(0.0, 0.0, 800.0, 200.0);
        let dir = LayoutNode::auto_split_direction(rect, SplitDirection::Vertical);
        assert_eq!(dir, SplitDirection::Horizontal);
    }

    #[test]
    fn auto_direction_tall_rect() {
        let rect = Rect::new(0.0, 0.0, 200.0, 800.0);
        let dir = LayoutNode::auto_split_direction(rect, SplitDirection::Horizontal);
        assert_eq!(dir, SplitDirection::Vertical);
    }

    #[test]
    fn auto_direction_square_uses_fallback() {
        let rect = Rect::new(0.0, 0.0, 400.0, 400.0);
        assert_eq!(
            LayoutNode::auto_split_direction(rect, SplitDirection::Horizontal),
            SplitDirection::Horizontal,
        );
        assert_eq!(
            LayoutNode::auto_split_direction(rect, SplitDirection::Vertical),
            SplitDirection::Vertical,
        );
    }

    // ── Minimum size enforcement tests ──────────────────────────────────

    #[test]
    fn can_split_respects_minimum_width() {
        let rect = Rect::new(0.0, 0.0, 150.0, 600.0);
        assert!(!LayoutNode::can_split(rect, SplitDirection::Horizontal));
    }

    #[test]
    fn can_split_allows_sufficient_width() {
        let rect = Rect::new(0.0, 0.0, 200.0, 600.0);
        assert!(LayoutNode::can_split(rect, SplitDirection::Horizontal));
    }

    #[test]
    fn can_split_respects_minimum_height() {
        let rect = Rect::new(0.0, 0.0, 800.0, 100.0);
        assert!(!LayoutNode::can_split(rect, SplitDirection::Vertical));
    }

    #[test]
    fn can_split_allows_sufficient_height() {
        let rect = Rect::new(0.0, 0.0, 800.0, 200.0);
        assert!(LayoutNode::can_split(rect, SplitDirection::Vertical));
    }

    #[test]
    fn split_refused_when_below_minimum_size() {
        let tiny_rect = Rect::new(0.0, 0.0, 150.0, 600.0);
        let mut node = make_leaf(1);
        node.layout(tiny_rect);

        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let writer = pair.master.take_writer().unwrap();
        let result = node.split_pane(1, SplitDirection::Horizontal, |r| {
            Some(PaneState {
                id: 2,
                term: Arc::new(FairMutex::new(Term::new(
                    TermConfig::default(),
                    &PaneTermSize {
                        columns: 80,
                        screen_lines: 24,
                    },
                    PaneListener,
                ))),
                pty_writer: writer,
                pty_master: pair.master,
                viewport: r,
                scrollback_lines: 1000,
                status: Arc::new(Mutex::new(super::super::PaneStatus::default())),
            })
        });
        assert_eq!(result, None, "split should be refused when too small");
        assert_eq!(node.pane_count(), 1, "should still have 1 pane");
    }

    // ── Rebalance on split/close integration tests ──────────────────────

    #[test]
    fn split_rebalances_parent_ratio() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        root.layout(rect);

        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let writer = pair.master.take_writer().unwrap();
        root.split_pane(2, SplitDirection::Horizontal, |r| {
            Some(PaneState {
                id: 3,
                term: Arc::new(FairMutex::new(Term::new(
                    TermConfig::default(),
                    &PaneTermSize {
                        columns: 80,
                        screen_lines: 24,
                    },
                    PaneListener,
                ))),
                pty_writer: writer,
                pty_master: pair.master,
                viewport: r,
                scrollback_lines: 1000,
                status: Arc::new(Mutex::new(super::super::PaneStatus::default())),
            })
        });

        assert_eq!(root.pane_count(), 3);
        if let LayoutNode::Split { ratio, .. } = &root {
            assert!(
                (*ratio - 1.0 / 3.0).abs() < 0.05,
                "after split, root ratio should be ~0.333, got {ratio}"
            );
        }
    }

    #[test]
    fn close_rebalances_remaining_tree() {
        let left = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        let right = make_split(SplitDirection::Horizontal, 0.5, make_leaf(3), make_leaf(4));
        let mut root = make_split(SplitDirection::Horizontal, 0.5, left, right);

        root.remove_pane(1);
        assert_eq!(root.pane_count(), 3);

        if let LayoutNode::Split { ratio, .. } = &root {
            assert!(
                (*ratio - 1.0 / 3.0).abs() < 0.05,
                "after close, root ratio should be ~0.333, got {ratio}"
            );
        }
    }

    // ── swap_pane tests ────────────────────────────────────────────────

    #[test]
    fn swap_pane_two_leaves() {
        let mut root = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        root.layout(Rect::new(0.0, 0.0, 800.0, 600.0));

        let rects_before = collect_leaf_rects(&root);
        assert!(root.swap_pane(1, 2));

        assert_eq!(root.pane_ids(), vec![2, 1]);
        let rects_after = collect_leaf_rects(&root);
        assert_eq!(rects_before, rects_after);
    }

    #[test]
    fn swap_pane_preserves_tree_structure() {
        let left = make_split(SplitDirection::Vertical, 0.5, make_leaf(1), make_leaf(2));
        let right = make_split(SplitDirection::Vertical, 0.5, make_leaf(3), make_leaf(4));
        let mut root = make_split(SplitDirection::Horizontal, 0.5, left, right);
        root.layout(Rect::new(0.0, 0.0, 800.0, 600.0));

        assert!(root.swap_pane(1, 4));
        assert_eq!(root.pane_ids(), vec![4, 2, 3, 1]);
        assert_eq!(root.pane_count(), 4);
    }

    #[test]
    fn swap_pane_same_id_returns_false() {
        let mut root = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        assert!(!root.swap_pane(1, 1));
    }

    #[test]
    fn swap_pane_unknown_id_returns_false() {
        let mut root = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        assert!(!root.swap_pane(1, 99));
    }

    #[test]
    fn swap_pane_single_leaf_returns_false() {
        let mut root = make_leaf(1);
        assert!(!root.swap_pane(1, 2));
    }
}
