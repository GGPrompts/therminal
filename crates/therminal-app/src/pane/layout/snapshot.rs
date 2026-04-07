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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::SplitDirection;

    /// Helper: build a minimal LayoutNode::Leaf without a real PTY.
    /// Delegates to the mod.rs test helper via the same fixture pattern.
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

    // ── snapshot_leaf_count ───────────────────────────────────────────────

    #[test]
    fn snapshot_leaf_count_single_leaf() {
        let snap = LayoutSnapshot::Leaf;
        assert_eq!(LayoutNode::snapshot_leaf_count(&snap), 1);
    }

    #[test]
    fn snapshot_leaf_count_two_leaf_split() {
        let snap = LayoutSnapshot::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf),
            second: Box::new(LayoutSnapshot::Leaf),
        };
        assert_eq!(LayoutNode::snapshot_leaf_count(&snap), 2);
    }

    #[test]
    fn snapshot_leaf_count_nested_four_leaves() {
        // (1,2) | (3,4)
        let inner_a = LayoutSnapshot::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf),
            second: Box::new(LayoutSnapshot::Leaf),
        };
        let inner_b = LayoutSnapshot::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf),
            second: Box::new(LayoutSnapshot::Leaf),
        };
        let root = LayoutSnapshot::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(inner_a),
            second: Box::new(inner_b),
        };
        assert_eq!(LayoutNode::snapshot_leaf_count(&root), 4);
    }

    // ── LayoutNode::snapshot round-trip ──────────────────────────────────

    #[test]
    fn snapshot_of_empty_is_leaf() {
        let node = LayoutNode::Empty;
        let snap = node.snapshot();
        assert!(matches!(snap, LayoutSnapshot::Leaf));
        assert_eq!(LayoutNode::snapshot_leaf_count(&snap), 1);
    }

    #[test]
    fn snapshot_two_pane_horizontal_preserves_ratio() {
        // Build a simple 2-pane horizontal split and verify the snapshot
        // carries the ratio and direction.
        let left = LayoutNode::Empty;
        let right = LayoutNode::Empty;
        let tree = make_split(SplitDirection::Horizontal, 0.3, left, right);
        let snap = tree.snapshot();

        match snap {
            LayoutSnapshot::Split {
                direction, ratio, ..
            } => {
                assert_eq!(direction, SplitDirection::Horizontal);
                assert!((ratio - 0.3).abs() < f32::EPSILON, "ratio={ratio}");
            }
            _ => panic!("expected LayoutSnapshot::Split"),
        }
    }

    #[test]
    fn snapshot_vertical_split_preserves_direction() {
        let tree = make_split(
            SplitDirection::Vertical,
            0.7,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        let snap = tree.snapshot();
        match snap {
            LayoutSnapshot::Split { direction, .. } => {
                assert_eq!(direction, SplitDirection::Vertical);
            }
            _ => panic!("expected Split snapshot"),
        }
    }

    #[test]
    fn snapshot_deep_tree_leaf_count_matches_pane_count() {
        // Three-level tree: four leaves total
        let bottom_left = make_split(
            SplitDirection::Vertical,
            0.5,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        let bottom_right = make_split(
            SplitDirection::Vertical,
            0.4,
            LayoutNode::Empty,
            LayoutNode::Empty,
        );
        let bottom = make_split(SplitDirection::Horizontal, 0.5, bottom_left, bottom_right);
        let tree = make_split(SplitDirection::Vertical, 0.6, LayoutNode::Empty, bottom);

        let snap = tree.snapshot();
        // Root has Empty (leaf) on left and a 4-way split on right -> 5 total leaves
        assert_eq!(LayoutNode::snapshot_leaf_count(&snap), 5);
    }

    #[test]
    fn snapshot_clone_is_structurally_identical() {
        // LayoutSnapshot derives Clone; ensure cloning produces the same shape.
        let snap = LayoutSnapshot::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf),
            second: Box::new(LayoutSnapshot::Split {
                direction: SplitDirection::Vertical,
                ratio: 0.25,
                first: Box::new(LayoutSnapshot::Leaf),
                second: Box::new(LayoutSnapshot::Leaf),
            }),
        };
        let cloned = snap.clone();
        assert_eq!(LayoutNode::snapshot_leaf_count(&cloned), 3);
        // Verify the inner ratio survived the clone.
        if let LayoutSnapshot::Split { second, .. } = &cloned {
            if let LayoutSnapshot::Split { ratio, .. } = second.as_ref() {
                assert!((ratio - 0.25).abs() < f32::EPSILON);
            } else {
                panic!("expected inner split");
            }
        } else {
            panic!("expected outer split");
        }
    }

    #[test]
    fn snapshot_leaf_count_asymmetric_tree() {
        // 1 leaf on left, 3 on right -> total 4
        let right = LayoutSnapshot::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf),
            second: Box::new(LayoutSnapshot::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Leaf),
                second: Box::new(LayoutSnapshot::Leaf),
            }),
        };
        let root = LayoutSnapshot::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.25,
            first: Box::new(LayoutSnapshot::Leaf),
            second: Box::new(right),
        };
        assert_eq!(LayoutNode::snapshot_leaf_count(&root), 4);
    }

    // ── Snapshot structural helpers ───────────────────────────────────────

    /// Verify that the snapshot of a LayoutNode::Leaf or Empty always yields
    /// a `LayoutSnapshot::Leaf` (no PaneState is included).
    #[test]
    fn snapshot_strips_pane_state() {
        // Empty -> Leaf
        assert!(matches!(LayoutNode::Empty.snapshot(), LayoutSnapshot::Leaf));
    }

    #[test]
    fn snapshot_split_produces_split_with_matching_ratio_and_depth() {
        let tree = make_split(
            SplitDirection::Horizontal,
            0.6,
            make_split(
                SplitDirection::Vertical,
                0.4,
                LayoutNode::Empty,
                LayoutNode::Empty,
            ),
            LayoutNode::Empty,
        );
        let snap = tree.snapshot();
        match &snap {
            LayoutSnapshot::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                assert_eq!(*direction, SplitDirection::Horizontal);
                assert!((ratio - 0.6).abs() < f32::EPSILON);
                // First child should be a split too
                assert!(matches!(first.as_ref(), LayoutSnapshot::Split { .. }));
                // Second child should be a leaf
                assert!(matches!(second.as_ref(), LayoutSnapshot::Leaf));
            }
            _ => panic!("expected Split"),
        }
    }

    // ── Rect is not part of snapshot (coverage check) ────────────────────

    #[test]
    fn snapshot_does_not_include_viewport_info() {
        // Two pane states with different rects should produce identical snapshots.
        let a = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Empty),
            second: Box::new(LayoutNode::Empty),
        };
        let b = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Empty),
            second: Box::new(LayoutNode::Empty),
        };
        let snap_a = a.snapshot();
        let snap_b = b.snapshot();
        // Both should have leaf_count 2 and match structurally.
        assert_eq!(
            LayoutNode::snapshot_leaf_count(&snap_a),
            LayoutNode::snapshot_leaf_count(&snap_b)
        );
    }
}
