//! Lightweight snapshot of a `LayoutNode` tree structure (no PTY state).

use crate::pane::SplitDirection;

use super::tree::LayoutNode;

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
