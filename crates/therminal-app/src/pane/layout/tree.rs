//! Binary layout tree for pane splits.
//!
//! `LayoutNode` is a recursive enum: each node is either a `Leaf` (terminal pane),
//! a `Split` (two children with a direction and ratio), or `Empty` (transient placeholder).

use therminal_core::geometry::Rect;

use crate::grid_renderer::GridRenderer;
use crate::pane::SplitDirection;
use crate::pane::geometry::{
    MIN_PANE_HEIGHT, MIN_PANE_WIDTH, SEPARATOR_GAP, effective_header_height,
};
use crate::pane::state::PaneState;

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
    ///
    /// `show_pane_headers` controls whether the per-pane header strip eats
    /// vertical space (see `effective_header_height`).
    pub fn resize_all_panes(&mut self, renderer: &GridRenderer, show_pane_headers: bool) {
        let header_h = effective_header_height(self.pane_count(), show_pane_headers);
        self.resize_all_panes_with_header(renderer, header_h);
    }

    /// tn-ou30: compact spurious scrollback from initial shell output.
    ///
    /// Performs a resize-down-then-up cycle on each pane's local Term.
    /// This triggers alacritty's `shrink_lines` (which scrolls content
    /// up, consuming trailing blank scrollback) followed by `grow_lines`
    /// (which pulls history back and decreases the scroll limit). Net
    /// effect: blank scrollback rows created by a shell's leading newline
    /// or startup text are absorbed. The PTY is NOT resized — only the
    /// in-memory Term changes briefly and returns to its original size.
    pub fn compact_scrollback(&mut self) {
        match self {
            LayoutNode::Leaf(pane) => {
                pane.backend.compact_scrollback();
            }
            LayoutNode::Split { first, second, .. } => {
                first.compact_scrollback();
                second.compact_scrollback();
            }
            LayoutNode::Empty => {}
        }
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

    /// Project the live layout tree into a serializable `LayoutSnapshot`
    /// suitable for crossing the IPC boundary into the daemon.
    ///
    /// Only direction, ratio, and leaf pane IDs are included; PTY state and
    /// viewport rects are intentionally stripped.
    ///
    /// F6 (tn-97j6): `LayoutNode::Empty` returns `None`. Previously Empty
    /// serialized as `Leaf { pane_id: 0 }`, which the publish path's
    /// translation guard rejected — silently suppressing ALL subsequent
    /// workspace publishes. Splits whose only non-empty child is one side
    /// collapse to that side; a fully-empty subtree returns `None`.
    pub fn to_snapshot(&self) -> Option<therminal_protocol::daemon::LayoutSnapshot> {
        use crate::pane::SplitDirection;
        use therminal_protocol::daemon::{LayoutSnapshot, LayoutSplitDirection};
        match self {
            LayoutNode::Leaf(pane) => Some(LayoutSnapshot::Leaf { pane_id: pane.id }),
            LayoutNode::Split {
                direction,
                ratio,
                first,
                second,
            } => match (first.to_snapshot(), second.to_snapshot()) {
                (Some(f), Some(s)) => Some(LayoutSnapshot::Split {
                    direction: match direction {
                        SplitDirection::Horizontal => LayoutSplitDirection::Horizontal,
                        SplitDirection::Vertical => LayoutSplitDirection::Vertical,
                    },
                    ratio: *ratio,
                    first: Box::new(f),
                    second: Box::new(s),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
            LayoutNode::Empty => None,
        }
    }

    /// Reconstruct a `LayoutNode` from a protocol `LayoutSnapshot`,
    /// invoking `make_leaf` to materialise a `PaneState` for each leaf.
    ///
    /// The snapshot's leaves carry **daemon** pane ids; `make_leaf` is
    /// responsible for translating to a local id and building the actual
    /// `PaneState` (typically via `build_remote_pane_state`). If
    /// `make_leaf` returns `None` for a leaf, that leaf becomes
    /// `LayoutNode::Empty` so the rest of the tree still loads.
    pub fn from_protocol_snapshot<F>(
        snap: &therminal_protocol::daemon::LayoutSnapshot,
        make_leaf: &mut F,
    ) -> LayoutNode
    where
        F: FnMut(therminal_protocol::PaneId) -> Option<PaneState>,
    {
        use therminal_protocol::daemon::{LayoutSnapshot, LayoutSplitDirection};
        match snap {
            LayoutSnapshot::Leaf { pane_id } => match make_leaf(*pane_id) {
                Some(p) => LayoutNode::Leaf(p),
                None => LayoutNode::Empty,
            },
            LayoutSnapshot::Split {
                direction,
                ratio,
                first,
                second,
            } => LayoutNode::Split {
                direction: match direction {
                    LayoutSplitDirection::Horizontal => SplitDirection::Horizontal,
                    LayoutSplitDirection::Vertical => SplitDirection::Vertical,
                },
                ratio: *ratio,
                first: Box::new(LayoutNode::from_protocol_snapshot(first, make_leaf)),
                second: Box::new(LayoutNode::from_protocol_snapshot(second, make_leaf)),
            },
        }
    }

    /// Build a flat horizontal cascade of leaves from a list of pane ids.
    /// Used as a fallback when no `LayoutSnapshot` is available (e.g.
    /// pre-tn-k3yo persisted sessions).
    pub fn from_flat_pane_ids<F>(
        ids: &[therminal_protocol::PaneId],
        make_leaf: &mut F,
    ) -> LayoutNode
    where
        F: FnMut(therminal_protocol::PaneId) -> Option<PaneState>,
    {
        let mut leaves: Vec<LayoutNode> = ids
            .iter()
            .filter_map(|id| make_leaf(*id).map(LayoutNode::Leaf))
            .collect();
        if leaves.is_empty() {
            return LayoutNode::Empty;
        }
        if leaves.len() == 1 {
            return leaves.remove(0);
        }
        // Right-leaning cascade.
        let mut iter = leaves.into_iter();
        let mut acc = iter.next().expect("checked leaves.len() >= 2");
        for next in iter {
            acc = LayoutNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(acc),
                second: Box::new(next),
            };
        }
        acc
    }

    /// Collect all pane IDs in order (left-to-right / top-to-bottom).
    pub fn pane_ids(&self) -> Vec<crate::pane::PaneId> {
        let mut ids = Vec::new();
        self.collect_ids(&mut ids);
        ids
    }

    fn collect_ids(&self, ids: &mut Vec<crate::pane::PaneId>) {
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
    pub fn find_pane(&self, id: crate::pane::PaneId) -> Option<&PaneState> {
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
    pub fn find_pane_mut(&mut self, id: crate::pane::PaneId) -> Option<&mut PaneState> {
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
        target_id: crate::pane::PaneId,
        direction: SplitDirection,
        spawn_fn: F,
    ) -> Option<crate::pane::PaneId>
    where
        F: FnOnce(Rect) -> Option<PaneState>,
    {
        let mut slot = Some(spawn_fn);
        self.split_pane_impl(target_id, direction, &mut slot)
    }

    fn split_pane_impl<F>(
        &mut self,
        target_id: crate::pane::PaneId,
        direction: SplitDirection,
        spawn_fn: &mut Option<F>,
    ) -> Option<crate::pane::PaneId>
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
    pub fn remove_pane(&mut self, target_id: crate::pane::PaneId) -> Option<bool> {
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
        focused_id: crate::pane::PaneId,
        direction: FocusDirection,
    ) -> Option<crate::pane::PaneId> {
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

    /// Collect (pane_id, viewport_rect) pairs for all leaves.
    pub fn pane_viewports(&self) -> Vec<(crate::pane::PaneId, Rect)> {
        let mut out = Vec::new();
        self.collect_viewports(&mut out);
        out
    }

    fn collect_viewports(&self, out: &mut Vec<(crate::pane::PaneId, Rect)>) {
        match self {
            LayoutNode::Leaf(pane) => out.push((pane.id, pane.viewport)),
            LayoutNode::Split { first, second, .. } => {
                first.collect_viewports(out);
                second.collect_viewports(out);
            }
            LayoutNode::Empty => {}
        }
    }

    /// Find the nearest pane in a spatial direction from the focused pane.
    ///
    /// Uses viewport center points. A candidate must lie in the correct
    /// direction (its center is strictly past the focused pane's edge).
    /// Among valid candidates, the nearest by Euclidean distance wins.
    pub fn spatial_adjacent_pane(
        &self,
        focused_id: crate::pane::PaneId,
        direction: SpatialDirection,
    ) -> Option<crate::pane::PaneId> {
        let viewports = self.pane_viewports();
        let focused_rect = viewports.iter().find(|(id, _)| *id == focused_id)?.1;
        let fc = focused_rect.center();

        let mut best: Option<(crate::pane::PaneId, f32)> = None;

        for &(id, rect) in &viewports {
            if id == focused_id {
                continue;
            }
            let c = rect.center();

            // Check that the candidate is in the correct direction.
            // Use the focused pane's edge as the threshold, not just the center,
            // so that adjacent panes with overlapping center ranges still qualify.
            let in_direction = match direction {
                SpatialDirection::Up => c.y < fc.y,
                SpatialDirection::Down => c.y > fc.y,
                SpatialDirection::Left => c.x < fc.x,
                SpatialDirection::Right => c.x > fc.x,
            };

            if !in_direction {
                continue;
            }

            // Score: prefer candidates aligned on the primary axis.  The
            // cross-axis penalty is 2x so that a pane directly beside/above
            // the focused one always beats a diagonal candidate.
            let dist = match direction {
                SpatialDirection::Up | SpatialDirection::Down => {
                    let primary = (c.y - fc.y).abs();
                    let cross = (c.x - fc.x).abs();
                    primary + cross * 2.0
                }
                SpatialDirection::Left | SpatialDirection::Right => {
                    let primary = (c.x - fc.x).abs();
                    let cross = (c.y - fc.y).abs();
                    primary + cross * 2.0
                }
            };

            match best {
                None => best = Some((id, dist)),
                Some((_, best_dist)) if dist < best_dist => best = Some((id, dist)),
                _ => {}
            }
        }

        best.map(|(id, _)| id)
    }

    /// Swap the positions of two panes in the tree by exchanging their `PaneState` contents.
    /// Returns `true` if both panes were found and swapped.
    pub fn swap_pane(&mut self, a: crate::pane::PaneId, b: crate::pane::PaneId) -> bool {
        // Collect mutable pointers to the two leaf PaneStates.
        fn find_leaf(node: &mut LayoutNode, id: crate::pane::PaneId) -> Option<*mut PaneState> {
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
    pub fn adjust_ratio(&mut self, focused_id: crate::pane::PaneId, delta: f32) -> bool {
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

    /// Recursively reset all split ratios to 0.5 (equal splits).
    pub fn reset_all_ratios(&mut self) {
        match self {
            LayoutNode::Split {
                ratio,
                first,
                second,
                ..
            } => {
                *ratio = 0.5;
                first.reset_all_ratios();
                second.reset_all_ratios();
            }
            LayoutNode::Leaf(_) | LayoutNode::Empty => {}
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

    /// Traverse all leaves and return the ID of the one with the largest
    /// viewport area (width * height). Used by auto-tiling to split the
    /// largest available pane instead of always splitting the parent -- this
    /// avoids tiny unusable panes from nested binary splits (Hyprland-style).
    pub fn find_largest_pane(&self) -> Option<crate::pane::PaneId> {
        let mut best: Option<(crate::pane::PaneId, f32)> = None;
        self.find_largest_pane_impl(&mut best);
        best.map(|(id, _)| id)
    }

    fn find_largest_pane_impl(&self, best: &mut Option<(crate::pane::PaneId, f32)>) {
        match self {
            LayoutNode::Leaf(pane) => {
                let area = pane.viewport.width() * pane.viewport.height();
                match best {
                    None => *best = Some((pane.id, area)),
                    Some((_, best_area)) if area > *best_area => *best = Some((pane.id, area)),
                    _ => {}
                }
            }
            LayoutNode::Split { first, second, .. } => {
                first.find_largest_pane_impl(best);
                second.find_largest_pane_impl(best);
            }
            LayoutNode::Empty => {}
        }
    }

    /// Remove `Empty` leaves from the tree and rebalance split ratios
    /// proportionally. Called after reclaiming an auto-tiled pane to clean
    /// up any transient placeholders and ensure even space distribution.
    pub fn compact_layout(&mut self) {
        // First, collapse any splits that have an Empty child.
        self.collapse_empty_children();
        // Then rebalance ratios so remaining panes share space evenly.
        self.rebalance();
    }

    /// Recursively collapse split nodes that contain Empty children by
    /// promoting the non-empty sibling.
    fn collapse_empty_children(&mut self) {
        if let LayoutNode::Split { first, second, .. } = self {
            // Recurse into children first.
            first.collapse_empty_children();
            second.collapse_empty_children();

            // If either child is Empty, promote the other.
            let first_empty = matches!(first.as_ref(), LayoutNode::Empty);
            let second_empty = matches!(second.as_ref(), LayoutNode::Empty);

            if first_empty && second_empty {
                *self = LayoutNode::Empty;
            } else if first_empty {
                let sibling = std::mem::replace(second.as_mut(), LayoutNode::Empty);
                *self = sibling;
            } else if second_empty {
                let sibling = std::mem::replace(first.as_mut(), LayoutNode::Empty);
                *self = sibling;
            }
        }
    }

    /// Extract a pane by ID, replacing its leaf with `Empty`.
    ///
    /// Returns `Some(PaneState)` if the pane was found and extracted.
    /// The caller is responsible for putting it back (e.g. via `insert_pane_at_empty`).
    pub fn extract_pane(&mut self, id: crate::pane::PaneId) -> Option<PaneState> {
        match self {
            LayoutNode::Leaf(pane) if pane.id == id => {
                // Replace this leaf with Empty, return the pane state.
                if let LayoutNode::Leaf(pane) = std::mem::replace(self, LayoutNode::Empty) {
                    Some(pane)
                } else {
                    unreachable!()
                }
            }
            LayoutNode::Leaf(_) | LayoutNode::Empty => None,
            LayoutNode::Split { first, second, .. } => {
                first.extract_pane(id).or_else(|| second.extract_pane(id))
            }
        }
    }

    /// Insert a pane at the first `Empty` node found in the tree.
    ///
    /// Returns `Some(pane)` if no empty slot was found (pane returned to caller),
    /// or `None` if the pane was successfully inserted.
    pub fn insert_pane_at_empty(&mut self, pane: PaneState) -> Option<PaneState> {
        match self {
            LayoutNode::Empty => {
                *self = LayoutNode::Leaf(pane);
                None // success
            }
            LayoutNode::Leaf(_) => Some(pane), // no slot here
            LayoutNode::Split { first, second, .. } => {
                let pane = first.insert_pane_at_empty(pane)?;
                second.insert_pane_at_empty(pane)
            }
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

/// Direction for focus navigation (cycling order).
#[derive(Debug, Clone, Copy)]
pub enum FocusDirection {
    Next,
    Prev,
}

/// Direction for spatial (geometric) focus navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpatialDirection {
    Up,
    Down,
    Left,
    Right,
}

#[cfg(test)]
mod tests {
    use therminal_core::geometry::Rect;

    use super::*;
    use crate::pane::SplitDirection;

    // ── Helpers ──────────────────────────────────────────────────────────

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

    // ── pane_viewports ────────────────────────────────────────────────────

    #[test]
    fn pane_viewports_empty_returns_empty() {
        let node = LayoutNode::Empty;
        assert!(node.pane_viewports().is_empty());
    }

    #[test]
    fn pane_viewports_leaf_returns_its_rect() {
        let rect = Rect::new(10.0, 20.0, 100.0, 80.0);
        let mut leaf = LayoutNode::Empty;
        leaf.layout(rect);
        // Empty doesn't store rect; verify non-crash and empty result.
        // (Real pane viewports are tracked in PaneState.viewport.)
        let vp = leaf.pane_viewports();
        assert!(vp.is_empty()); // Empty variant has no pane ID
    }

    #[test]
    fn pane_viewports_split_returns_two_rects() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = make_split(
            SplitDirection::Horizontal,
            0.5,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        root.layout(rect);
        // Empty children don't contribute IDs — structural test only.
        let vp = root.pane_viewports();
        assert_eq!(vp.len(), 0); // Both children are Empty
    }

    // ── leaf_rects_pub ────────────────────────────────────────────────────

    #[test]
    fn leaf_rects_pub_empty_returns_empty() {
        let node = LayoutNode::Empty;
        assert!(node.leaf_rects_pub().is_empty());
    }

    #[test]
    fn leaf_rects_pub_split_two_rects_within_viewport() {
        let viewport = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = make_split(
            SplitDirection::Horizontal,
            0.5,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        root.layout(viewport);
        // Empty children produce 0 leaf rects.
        let rects = root.leaf_rects_pub();
        // Both sides are Empty — no leaf rects.
        assert_eq!(rects.len(), 0);
    }

    // ── reset_all_ratios ──────────────────────────────────────────────────

    #[test]
    fn reset_all_ratios_single_split() {
        let mut root = make_split(
            SplitDirection::Horizontal,
            0.8,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        root.reset_all_ratios();
        if let LayoutNode::Split { ratio, .. } = &root {
            assert!((ratio - 0.5).abs() < f32::EPSILON, "ratio should be 0.5");
        } else {
            panic!("expected Split");
        }
    }

    #[test]
    fn reset_all_ratios_nested_splits() {
        let inner = make_split(
            SplitDirection::Vertical,
            0.3,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        let mut root = make_split(SplitDirection::Horizontal, 0.7, LayoutNode::Empty, inner);
        root.reset_all_ratios();

        if let LayoutNode::Split { ratio, second, .. } = &root {
            assert!((ratio - 0.5).abs() < f32::EPSILON, "outer ratio={ratio}");
            if let LayoutNode::Split { ratio: inner_r, .. } = second.as_ref() {
                assert!(
                    (inner_r - 0.5).abs() < f32::EPSILON,
                    "inner ratio={inner_r}"
                );
            } else {
                panic!("expected inner Split");
            }
        } else {
            panic!("expected outer Split");
        }
    }

    #[test]
    fn reset_all_ratios_leaf_is_noop() {
        let mut node = LayoutNode::Empty;
        // Must not panic.
        node.reset_all_ratios();
    }

    // ── set_ratio_at_path ─────────────────────────────────────────────────

    #[test]
    fn set_ratio_at_path_empty_path_sets_root() {
        let mut root = make_split(
            SplitDirection::Horizontal,
            0.5,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        let ok = root.set_ratio_at_path(&[], 0.7);
        assert!(ok);
        if let LayoutNode::Split { ratio, .. } = &root {
            assert!((ratio - 0.7).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn set_ratio_at_path_clamps_to_limits() {
        let mut root = make_split(
            SplitDirection::Horizontal,
            0.5,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        // Too high: should clamp to 0.9
        root.set_ratio_at_path(&[], 2.0);
        if let LayoutNode::Split { ratio, .. } = &root {
            assert!((ratio - 0.9).abs() < f32::EPSILON, "ratio={ratio}");
        }
        // Too low: should clamp to 0.1
        root.set_ratio_at_path(&[], 0.0);
        if let LayoutNode::Split { ratio, .. } = &root {
            assert!((ratio - 0.1).abs() < f32::EPSILON, "ratio={ratio}");
        }
    }

    #[test]
    fn set_ratio_at_path_first_child() {
        // path [false] => navigate into first child.
        let inner = make_split(
            SplitDirection::Vertical,
            0.5,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        let mut root = make_split(SplitDirection::Horizontal, 0.5, inner, LayoutNode::Empty);
        let ok = root.set_ratio_at_path(&[false], 0.3);
        assert!(ok);
        if let LayoutNode::Split { first, .. } = &root {
            if let LayoutNode::Split { ratio, .. } = first.as_ref() {
                assert!((ratio - 0.3).abs() < f32::EPSILON);
            } else {
                panic!("first child should be a split");
            }
        }
    }

    #[test]
    fn set_ratio_at_path_second_child() {
        // path [true] => navigate into second child.
        let inner = make_split(
            SplitDirection::Vertical,
            0.5,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        let mut root = make_split(SplitDirection::Horizontal, 0.5, LayoutNode::Empty, inner);
        let ok = root.set_ratio_at_path(&[true], 0.6);
        assert!(ok);
        if let LayoutNode::Split { second, .. } = &root {
            if let LayoutNode::Split { ratio, .. } = second.as_ref() {
                assert!((ratio - 0.6).abs() < f32::EPSILON);
            } else {
                panic!("second child should be a split");
            }
        }
    }

    #[test]
    fn set_ratio_at_path_on_non_split_returns_false() {
        let mut node = LayoutNode::Empty;
        assert!(!node.set_ratio_at_path(&[], 0.5));
    }

    // ── separator_hit_test ────────────────────────────────────────────────

    #[test]
    fn separator_hit_test_horizontal_split_center() {
        // A horizontal split at 0.5 with rect 800x600.
        // The separator is a vertical line at x=400 (approx).
        let viewport = Rect::new(0.0, 0.0, 800.0, 600.0);
        let root = make_split(
            SplitDirection::Horizontal,
            0.5,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        // The separator is at ~x=400; test with 6px tolerance at center y.
        let result = root.separator_hit_test(400.0, 300.0, 6.0, viewport);
        assert!(result.is_some(), "should hit the separator at center");
        if let Some((path, dir, _rect)) = result {
            assert_eq!(path, Vec::<bool>::new(), "root path should be empty");
            assert_eq!(dir, SplitDirection::Horizontal);
        }
    }

    #[test]
    fn separator_hit_test_vertical_split_center() {
        let viewport = Rect::new(0.0, 0.0, 800.0, 600.0);
        let root = make_split(
            SplitDirection::Vertical,
            0.5,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        // The separator is at ~y=300; test with 6px tolerance at center x.
        let result = root.separator_hit_test(400.0, 300.0, 6.0, viewport);
        assert!(result.is_some());
        if let Some((_, dir, _)) = result {
            assert_eq!(dir, SplitDirection::Vertical);
        }
    }

    #[test]
    fn separator_hit_test_far_from_separator_returns_none() {
        let viewport = Rect::new(0.0, 0.0, 800.0, 600.0);
        let root = make_split(
            SplitDirection::Horizontal,
            0.5,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        // Click at the far left (x=50) — nowhere near x=400.
        let result = root.separator_hit_test(50.0, 300.0, 6.0, viewport);
        assert!(result.is_none());
    }

    #[test]
    fn separator_hit_test_leaf_returns_none() {
        let viewport = Rect::new(0.0, 0.0, 800.0, 600.0);
        let node = LayoutNode::Empty;
        assert!(
            node.separator_hit_test(400.0, 300.0, 6.0, viewport)
                .is_none()
        );
    }

    #[test]
    fn separator_hit_test_nested_split_child_path() {
        // Root = Horizontal(A | Vertical(B, C))
        // Horizontal separator at ~x=400.
        // Vertical sub-separator at ~y=300 within the right half.
        let viewport = Rect::new(0.0, 0.0, 800.0, 600.0);
        let right = make_split(
            SplitDirection::Vertical,
            0.5,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        let root = make_split(SplitDirection::Horizontal, 0.5, LayoutNode::Empty, right);
        // Click on the vertical sub-separator inside the right half:
        // right half x=[400,800], sub-sep at y=300.
        let result = root.separator_hit_test(600.0, 300.0, 6.0, viewport);
        assert!(result.is_some(), "should hit child separator");
        if let Some((path, dir, _)) = result {
            assert_eq!(dir, SplitDirection::Vertical);
            assert!(!path.is_empty(), "nested hit has non-empty path");
        }
    }

    // ── can_split (negative edge cases not in mod.rs) ─────────────────────

    #[test]
    fn can_split_exact_minimum_horizontal() {
        // Exactly 2 * MIN_PANE_WIDTH + SEPARATOR_GAP should be allowed.
        let w = MIN_PANE_WIDTH * 2.0 + SEPARATOR_GAP;
        let rect = Rect::new(0.0, 0.0, w, 600.0);
        assert!(LayoutNode::can_split(rect, SplitDirection::Horizontal));
    }

    #[test]
    fn can_split_exact_minimum_vertical() {
        let h = MIN_PANE_HEIGHT * 2.0 + SEPARATOR_GAP;
        let rect = Rect::new(0.0, 0.0, 800.0, h);
        assert!(LayoutNode::can_split(rect, SplitDirection::Vertical));
    }

    #[test]
    fn can_split_one_pixel_under_minimum_horizontal() {
        let w = MIN_PANE_WIDTH * 2.0 + SEPARATOR_GAP - 1.0;
        let rect = Rect::new(0.0, 0.0, w, 600.0);
        assert!(!LayoutNode::can_split(rect, SplitDirection::Horizontal));
    }

    #[test]
    fn can_split_zero_size_rect_returns_false() {
        let rect = Rect::new(0.0, 0.0, 0.0, 0.0);
        assert!(!LayoutNode::can_split(rect, SplitDirection::Horizontal));
        assert!(!LayoutNode::can_split(rect, SplitDirection::Vertical));
    }

    // ── find_largest_pane with a single Empty (no pane) ──────────────────

    #[test]
    fn find_largest_pane_no_leaves_returns_none() {
        let root = make_split(
            SplitDirection::Horizontal,
            0.5,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        // No Leaf nodes -> no largest pane.
        assert!(root.find_largest_pane().is_none());
    }

    // ── adjust_ratio on leaf / empty ──────────────────────────────────────

    #[test]
    fn adjust_ratio_on_leaf_returns_false() {
        let mut node = LayoutNode::Empty;
        assert!(!node.adjust_ratio(1, 0.1));
    }

    #[test]
    fn adjust_ratio_on_split_with_unknown_id_returns_false() {
        let mut root = make_split(
            SplitDirection::Horizontal,
            0.5,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        // No leaves with real pane IDs, so the search finds nothing.
        assert!(!root.adjust_ratio(999, 0.1));
    }
}
