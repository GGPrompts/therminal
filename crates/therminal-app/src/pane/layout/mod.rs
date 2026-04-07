//! Binary layout tree for pane splits.
//!
//! Split across two submodules:
//! - [`tree`] — the `LayoutNode` enum plus all tree operations (split, merge,
//!   find, traversal, hit-testing, rebalancing).
//! - [`snapshot`] — `LayoutSnapshot` for lightweight structural persistence.

mod snapshot;
mod tree;

pub use snapshot::LayoutSnapshot;
pub use tree::{FocusDirection, LayoutNode, SpatialDirection};

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use alacritty_terminal::sync::FairMutex;
    use alacritty_terminal::term::{Config as TermConfig, Term};
    use therminal_core::geometry::Rect;

    use super::*;
    use crate::pane::PaneListener;
    use crate::pane::SplitDirection;
    use crate::pane::backend::PaneBackendKind;
    use crate::pane::state::{PaneState, PaneTermSize};

    /// Helper to create a minimal PaneState for tests (no real PTY semantics).
    fn test_pane_state(id: crate::pane::PaneId, rect: Rect) -> PaneState {
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
            viewport: rect,
            status: Arc::new(Mutex::new(crate::pane::PaneStatus::default())),
            region_index: Arc::new(Mutex::new(
                therminal_terminal::region_index::RegionIndex::new(),
            )),
            backend: PaneBackendKind::Terminal {
                term: Arc::new(FairMutex::new(term)),
                pty_writer: writer,
                pty_master: pair.master,
                scrollback_lines: 1000,
            },
        }
    }

    /// Helper to create a minimal test leaf node (no real PTY).
    fn test_leaf(id: crate::pane::PaneId, rect: Rect) -> LayoutNode {
        LayoutNode::Leaf(test_pane_state(id, rect))
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
            Some(test_pane_state(2, r))
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
    fn make_leaf(id: crate::pane::PaneId) -> LayoutNode {
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
        // Use a large rect so all 5 horizontal splits succeed without hitting MIN_PANE_WIDTH.
        let rect = Rect::new(0.0, 0.0, 6400.0, 600.0);
        let mut root = test_leaf(1, rect);

        for id in 2u64..=6 {
            let rightmost_id = *root.pane_ids().last().unwrap();
            root.split_pane(rightmost_id, SplitDirection::Horizontal, |r| {
                Some(test_pane_state(id, r))
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
        use crate::pane::geometry::SEPARATOR_GAP;

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

        let result = node.split_pane(1, SplitDirection::Horizontal, |r| {
            Some(test_pane_state(2, r))
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

        root.split_pane(2, SplitDirection::Horizontal, |r| {
            Some(test_pane_state(3, r))
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

    // ── Spatial navigation tests ──────────────────────────────────────────

    #[test]
    fn spatial_nav_two_panes_horizontal() {
        // [1 | 2]  -- side by side
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        root.layout(rect);

        // From pane 1, right should reach pane 2.
        assert_eq!(
            root.spatial_adjacent_pane(1, SpatialDirection::Right),
            Some(2)
        );
        // From pane 2, left should reach pane 1.
        assert_eq!(
            root.spatial_adjacent_pane(2, SpatialDirection::Left),
            Some(1)
        );
        // No pane above or below in a horizontal-only split.
        assert_eq!(root.spatial_adjacent_pane(1, SpatialDirection::Up), None);
        assert_eq!(root.spatial_adjacent_pane(1, SpatialDirection::Down), None);
    }

    #[test]
    fn spatial_nav_two_panes_vertical() {
        // [1]
        // ---
        // [2]
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = make_split(SplitDirection::Vertical, 0.5, make_leaf(1), make_leaf(2));
        root.layout(rect);

        assert_eq!(
            root.spatial_adjacent_pane(1, SpatialDirection::Down),
            Some(2)
        );
        assert_eq!(root.spatial_adjacent_pane(2, SpatialDirection::Up), Some(1));
        assert_eq!(root.spatial_adjacent_pane(1, SpatialDirection::Left), None);
        assert_eq!(root.spatial_adjacent_pane(1, SpatialDirection::Right), None);
    }

    #[test]
    fn spatial_nav_three_panes_l_shape() {
        // [1 | 2]
        //   [3]       -- pane 3 spans the bottom half
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let top = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        let mut root = make_split(SplitDirection::Vertical, 0.5, top, make_leaf(3));
        root.layout(rect);

        // From 1, right -> 2
        assert_eq!(
            root.spatial_adjacent_pane(1, SpatialDirection::Right),
            Some(2)
        );
        // From 1, down -> 3
        assert_eq!(
            root.spatial_adjacent_pane(1, SpatialDirection::Down),
            Some(3)
        );
        // From 3, up -> 1 (nearest, since 1 center.x is closer to 3 center.x)
        // Pane 3 center is at (400, 450). Pane 1 center is at (200, 150).
        // Pane 2 center is at (600, 150). Both are "up". Distance to 1: primary=300, cross=200*0.5=100 => 400.
        // Distance to 2: primary=300, cross=200*0.5=100 => 400. Tie broken by iteration order -> 1.
        let up_from_3 = root.spatial_adjacent_pane(3, SpatialDirection::Up);
        assert!(
            up_from_3 == Some(1) || up_from_3 == Some(2),
            "up from 3 should be 1 or 2"
        );
        // From 2, down -> 3
        assert_eq!(
            root.spatial_adjacent_pane(2, SpatialDirection::Down),
            Some(3)
        );
    }

    #[test]
    fn spatial_nav_four_pane_grid() {
        // [1 | 2]
        // [3 | 4]
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let top = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        let bottom = make_split(SplitDirection::Horizontal, 0.5, make_leaf(3), make_leaf(4));
        let mut root = make_split(SplitDirection::Vertical, 0.5, top, bottom);
        root.layout(rect);

        // Right navigation
        assert_eq!(
            root.spatial_adjacent_pane(1, SpatialDirection::Right),
            Some(2)
        );
        assert_eq!(
            root.spatial_adjacent_pane(3, SpatialDirection::Right),
            Some(4)
        );

        // Left navigation
        assert_eq!(
            root.spatial_adjacent_pane(2, SpatialDirection::Left),
            Some(1)
        );
        assert_eq!(
            root.spatial_adjacent_pane(4, SpatialDirection::Left),
            Some(3)
        );

        // Down navigation
        assert_eq!(
            root.spatial_adjacent_pane(1, SpatialDirection::Down),
            Some(3)
        );
        assert_eq!(
            root.spatial_adjacent_pane(2, SpatialDirection::Down),
            Some(4)
        );

        // Up navigation
        assert_eq!(root.spatial_adjacent_pane(3, SpatialDirection::Up), Some(1));
        assert_eq!(root.spatial_adjacent_pane(4, SpatialDirection::Up), Some(2));

        // No wrap: top-left pane has nothing above or to the left.
        assert_eq!(root.spatial_adjacent_pane(1, SpatialDirection::Up), None);
        assert_eq!(root.spatial_adjacent_pane(1, SpatialDirection::Left), None);

        // No wrap: bottom-right pane has nothing below or to the right.
        assert_eq!(root.spatial_adjacent_pane(4, SpatialDirection::Down), None);
        assert_eq!(root.spatial_adjacent_pane(4, SpatialDirection::Right), None);
    }

    #[test]
    fn spatial_nav_unknown_id_returns_none() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        root.layout(rect);

        assert_eq!(
            root.spatial_adjacent_pane(99, SpatialDirection::Right),
            None
        );
    }

    // ── find_largest_pane tests ──────────────────────────────────────────

    #[test]
    fn find_largest_pane_single_leaf() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = test_leaf(1, rect);
        root.layout(rect);
        assert_eq!(root.find_largest_pane(), Some(1));
    }

    #[test]
    fn find_largest_pane_equal_split() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        root.layout(rect);
        // Both have equal area -- first encountered wins.
        let largest = root.find_largest_pane();
        assert!(largest == Some(1) || largest == Some(2));
    }

    #[test]
    fn find_largest_pane_unequal_split() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        // Pane 1 gets 80% of width, pane 2 gets 20%.
        let mut root = make_split(SplitDirection::Horizontal, 0.8, make_leaf(1), make_leaf(2));
        root.layout(rect);
        assert_eq!(root.find_largest_pane(), Some(1));
    }

    #[test]
    fn find_largest_pane_deep_tree() {
        // Build a tree where pane 5 is the full bottom half (largest).
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let top = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        let mut root = make_split(SplitDirection::Vertical, 0.5, top, make_leaf(5));
        root.layout(rect);
        // Pane 5 spans full width * half height = 800*300 = 240000
        // Panes 1,2 each span half width * half height = 400*300 = 120000 (approx)
        assert_eq!(root.find_largest_pane(), Some(5));
    }

    #[test]
    fn find_largest_pane_empty_tree() {
        let root = LayoutNode::Empty;
        assert_eq!(root.find_largest_pane(), None);
    }

    // ── compact_layout tests ──────────────────────────────────────────────

    #[test]
    fn compact_layout_removes_empty_leaves() {
        // Manually create a split with an Empty child.
        let mut root = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(make_leaf(1)),
            second: Box::new(LayoutNode::Empty),
        };
        root.compact_layout();
        // Should collapse to just the leaf.
        assert_eq!(root.pane_count(), 1);
        assert_eq!(root.pane_ids(), vec![1]);
        assert!(matches!(root, LayoutNode::Leaf(_)));
    }

    #[test]
    fn compact_layout_both_empty_becomes_empty() {
        let mut root = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Empty),
            second: Box::new(LayoutNode::Empty),
        };
        root.compact_layout();
        assert!(matches!(root, LayoutNode::Empty));
    }

    #[test]
    fn compact_layout_nested_empty() {
        // Split { Split { leaf(1), Empty }, leaf(2) }
        let inner = LayoutNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(make_leaf(1)),
            second: Box::new(LayoutNode::Empty),
        };
        let mut root = make_split(SplitDirection::Horizontal, 0.5, inner, make_leaf(2));
        root.compact_layout();
        // Inner split should collapse, leaving Split { leaf(1), leaf(2) }.
        assert_eq!(root.pane_count(), 2);
        assert_eq!(root.pane_ids(), vec![1, 2]);
        // Root should be rebalanced to 0.5.
        if let LayoutNode::Split { ratio, .. } = &root {
            assert!((*ratio - 0.5).abs() < 0.01);
        }
    }

    #[test]
    fn compact_layout_no_empty_is_noop() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        root.layout(rect);
        root.compact_layout();
        assert_eq!(root.pane_count(), 2);
        assert_eq!(root.pane_ids(), vec![1, 2]);
    }

    // ── Minimum size enforcement in auto-tile context ─────────────────────

    #[test]
    fn find_largest_pane_split_refused_when_too_small() {
        // Create a layout where the largest pane is still too small to split.
        let tiny_rect = Rect::new(0.0, 0.0, 150.0, 100.0);
        let mut root = test_leaf(1, tiny_rect);
        root.layout(tiny_rect);

        let largest = root.find_largest_pane();
        assert_eq!(largest, Some(1));

        // Both split directions should be refused.
        assert!(!LayoutNode::can_split(
            tiny_rect,
            SplitDirection::Horizontal
        ));
        assert!(!LayoutNode::can_split(tiny_rect, SplitDirection::Vertical));
    }

    #[test]
    fn spatial_nav_single_pane_returns_none() {
        let rect = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut root = make_leaf(1);
        root.layout(rect);

        assert_eq!(root.spatial_adjacent_pane(1, SpatialDirection::Up), None);
        assert_eq!(root.spatial_adjacent_pane(1, SpatialDirection::Down), None);
        assert_eq!(root.spatial_adjacent_pane(1, SpatialDirection::Left), None);
        assert_eq!(root.spatial_adjacent_pane(1, SpatialDirection::Right), None);
    }

    #[test]
    fn spatial_nav_asymmetric_five_panes() {
        // Layout:
        //   [1 | 2]
        //   [3 | 4]
        //     [5]       -- full width bottom
        let rect = Rect::new(0.0, 0.0, 900.0, 900.0);
        let top = make_split(SplitDirection::Horizontal, 0.5, make_leaf(1), make_leaf(2));
        let mid = make_split(SplitDirection::Horizontal, 0.5, make_leaf(3), make_leaf(4));
        let upper = make_split(SplitDirection::Vertical, 0.5, top, mid);
        let mut root = make_split(SplitDirection::Vertical, 0.67, upper, make_leaf(5));
        root.layout(rect);

        // From 3, down -> 5
        assert_eq!(
            root.spatial_adjacent_pane(3, SpatialDirection::Down),
            Some(5)
        );
        // From 4, down -> 5
        assert_eq!(
            root.spatial_adjacent_pane(4, SpatialDirection::Down),
            Some(5)
        );
        // From 5, up -> closest of 3 or 4 (center of 5 is at ~450, center
        // of 3 is at ~225, center of 4 is at ~675; 3 is closer in x).
        let up_from_5 = root.spatial_adjacent_pane(5, SpatialDirection::Up);
        assert!(
            up_from_5 == Some(3) || up_from_5 == Some(4),
            "up from 5 should be 3 or 4, got {up_from_5:?}"
        );
        // From 5, nothing below.
        assert_eq!(root.spatial_adjacent_pane(5, SpatialDirection::Down), None);
    }

    // ── extract_pane / insert_pane_at_empty tests ────────────────────────

    #[test]
    fn extract_single_leaf_leaves_empty() {
        let mut root = make_leaf(1);
        let extracted = root.extract_pane(1);
        assert!(extracted.is_some());
        assert_eq!(extracted.unwrap().id, 1);
        assert!(matches!(root, LayoutNode::Empty));
        assert_eq!(root.pane_count(), 0);
    }

    #[test]
    fn extract_from_nested_split_preserves_others() {
        // Build: Split(Split(1, 2), Split(3, 4))
        let root_inner_a = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(make_leaf(1)),
            second: Box::new(make_leaf(2)),
        };
        let root_inner_b = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(make_leaf(3)),
            second: Box::new(make_leaf(4)),
        };
        let mut root = LayoutNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(root_inner_a),
            second: Box::new(root_inner_b),
        };

        let extracted = root.extract_pane(3);
        assert!(extracted.is_some());
        assert_eq!(extracted.unwrap().id, 3);

        // Other panes still reachable
        let ids = root.pane_ids();
        assert_eq!(ids, vec![1, 2, 4]);
        // Tree still has the structural Split nodes (Empty leaf preserved
        // until cleanup); root is still a Split.
        assert!(matches!(root, LayoutNode::Split { .. }));
    }

    #[test]
    fn extract_nonexistent_pane_returns_none() {
        let mut root = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(make_leaf(1)),
            second: Box::new(make_leaf(2)),
        };
        let original_ids = root.pane_ids();
        let extracted = root.extract_pane(999);
        assert!(extracted.is_none());
        assert_eq!(root.pane_ids(), original_ids);
    }

    #[test]
    fn insert_into_empty_slot_fills_it() {
        // Build a split, then extract one to leave an Empty.
        let mut root = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(make_leaf(1)),
            second: Box::new(make_leaf(2)),
        };
        let extracted = root.extract_pane(2).unwrap();
        // Now the second child is Empty.
        let leftover = root.insert_pane_at_empty(extracted);
        assert!(leftover.is_none(), "insert should succeed");
        assert_eq!(root.pane_ids(), vec![1, 2]);
    }

    #[test]
    fn insert_with_no_empty_slot_returns_pane() {
        let mut root = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(make_leaf(1)),
            second: Box::new(make_leaf(2)),
        };
        let pane = test_pane_state(99, Rect::new(0.0, 0.0, 100.0, 100.0));
        let returned = root.insert_pane_at_empty(pane);
        assert!(returned.is_some(), "no empty slot, pane should come back");
        assert_eq!(returned.unwrap().id, 99);
        assert_eq!(root.pane_ids(), vec![1, 2]);
    }

    #[test]
    fn extract_then_insert_round_trip() {
        // 4-pane mixed split tree:
        // Vertical split of (Horizontal(1, 2)) and (Horizontal(3, 4))
        let mut root = LayoutNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(make_leaf(1)),
                second: Box::new(make_leaf(2)),
            }),
            second: Box::new(LayoutNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(make_leaf(3)),
                second: Box::new(make_leaf(4)),
            }),
        };
        let original_ids = root.pane_ids();
        assert_eq!(original_ids, vec![1, 2, 3, 4]);

        // Extract pane B (id=2).
        let extracted = root.extract_pane(2).expect("pane 2 exists");
        assert_eq!(extracted.id, 2);
        assert_eq!(root.pane_ids(), vec![1, 3, 4]);

        // Insert it back; should land in the Empty slot we just created.
        let leftover = root.insert_pane_at_empty(extracted);
        assert!(leftover.is_none());

        // Round-trip: same ids in same traversal order.
        assert_eq!(root.pane_ids(), original_ids);
    }
}
