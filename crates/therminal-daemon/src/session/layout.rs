//! Layout tree helpers for workspace topology management.
//!
//! Functions for manipulating `LayoutSnapshot` trees: swapping, removing,
//! appending, splitting leaves, and computing leaf dimensions.

use therminal_protocol::PaneId;
use therminal_protocol::daemon::LayoutSnapshot;

use std::time::Duration;

/// Fallback delay before injecting a startup command when no prompt mark
/// arrives.
pub(super) const STARTUP_COMMAND_FALLBACK: Duration = Duration::from_millis(300);

/// Poll interval when waiting for the first OSC 133/633 prompt-start mark.
pub(super) const STARTUP_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Swap two pane IDs wherever they appear as leaves in a `LayoutSnapshot`.
pub(super) fn swap_layout_leaves(node: &mut LayoutSnapshot, a: PaneId, b: PaneId) {
    match node {
        LayoutSnapshot::Leaf { pane_id } => {
            if *pane_id == a {
                *pane_id = b;
            } else if *pane_id == b {
                *pane_id = a;
            }
        }
        LayoutSnapshot::Split { first, second, .. } => {
            swap_layout_leaves(first, a, b);
            swap_layout_leaves(second, a, b);
        }
    }
}

/// Remove a single leaf with the given `pane_id` from a `LayoutSnapshot`,
/// promoting its sibling. Returns `None` if the resulting tree would be
/// empty (the caller is the workspace state holder, which should then
/// drop the layout entirely or fall back to a flat reconstruction). Used
/// by [`super::manager::SessionManager::move_pane`] (tn-fi1k).
///
/// `target` MUST refer to a leaf actually present in the tree; if it is
/// missing, the original tree is returned unchanged so the caller's
/// workspace state stays consistent (the GUI will resync via
/// `SetWorkspaceState` shortly anyway).
pub(super) fn remove_layout_leaf(node: LayoutSnapshot, target: PaneId) -> Option<LayoutSnapshot> {
    match node {
        LayoutSnapshot::Leaf { pane_id } => {
            if pane_id == target {
                None
            } else {
                Some(LayoutSnapshot::Leaf { pane_id })
            }
        }
        LayoutSnapshot::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let first_out = remove_layout_leaf(*first, target);
            let second_out = remove_layout_leaf(*second, target);
            match (first_out, second_out) {
                (Some(f), Some(s)) => Some(LayoutSnapshot::Split {
                    direction,
                    ratio,
                    first: Box::new(f),
                    second: Box::new(s),
                }),
                (Some(f), None) => Some(f),
                (None, Some(s)) => Some(s),
                (None, None) => None,
            }
        }
    }
}

/// Append a new leaf to the right of an existing `LayoutSnapshot` via a
/// horizontal split (matching the GUI's `WorkspaceManager::send_pane_to`
/// behaviour for the cross-workspace pane transfer). Used by
/// [`super::manager::SessionManager::move_pane`] (tn-fi1k).
///
/// If the target workspace had no layout yet, the result is a single
/// leaf containing only `pane_id`.
pub(super) fn append_layout_leaf(
    layout: Option<LayoutSnapshot>,
    pane_id: PaneId,
) -> LayoutSnapshot {
    use therminal_protocol::daemon::LayoutSplitDirection;
    match layout {
        None => LayoutSnapshot::Leaf { pane_id },
        Some(LayoutSnapshot::Leaf { pane_id: existing }) if existing == pane_id => {
            // Already present as the only leaf — no change needed.
            LayoutSnapshot::Leaf { pane_id }
        }
        Some(other) => LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(other),
            second: Box::new(LayoutSnapshot::Leaf { pane_id }),
        },
    }
}

/// Replace the leaf for `source` with a new `Split` node whose first
/// child is `source` and second child is `new_leaf`, returning the new
/// tree. Used by the daemon-side split path (tn-ju04) to keep the stored
/// `workspace_state.layout` in step with `window.panes` after a
/// daemon-driven `SplitPane` (MCP / CLI).
///
/// If `source` is not found anywhere in `node`, the tree is returned
/// unchanged — the caller's workspace state will be corrected by the next
/// GUI `SetWorkspaceState` publish.
pub(super) fn split_layout_leaf(
    node: LayoutSnapshot,
    source: PaneId,
    new_leaf: PaneId,
    direction: therminal_protocol::daemon::LayoutSplitDirection,
    split_ratio: f32,
) -> LayoutSnapshot {
    match node {
        LayoutSnapshot::Leaf { pane_id } if pane_id == source => LayoutSnapshot::Split {
            direction,
            ratio: split_ratio,
            first: Box::new(LayoutSnapshot::Leaf { pane_id: source }),
            second: Box::new(LayoutSnapshot::Leaf { pane_id: new_leaf }),
        },
        LayoutSnapshot::Leaf { pane_id } => LayoutSnapshot::Leaf { pane_id },
        LayoutSnapshot::Split {
            direction: d,
            ratio,
            first,
            second,
        } => LayoutSnapshot::Split {
            direction: d,
            ratio,
            first: Box::new(split_layout_leaf(
                *first,
                source,
                new_leaf,
                direction,
                split_ratio,
            )),
            second: Box::new(split_layout_leaf(
                *second,
                source,
                new_leaf,
                direction,
                split_ratio,
            )),
        },
    }
}

pub(super) fn normalize_startup_command(startup_command: Option<&str>) -> Option<Vec<u8>> {
    let command = startup_command?;
    if command.is_empty() {
        return None;
    }

    let mut bytes = command.as_bytes().to_vec();
    if !matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.push(b'\n');
    }
    Some(bytes)
}

/// A leaf's computed cell dimensions inside a `LayoutSnapshot`. Produced
/// by [`layout_leaf_dims`] and consumed by the daemon-side resize
/// cascade (tn-ju04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LeafDims {
    pub pane_id: PaneId,
    pub cols: u16,
    pub rows: u16,
}

/// Reconstruct the total cell rect of a `LayoutSnapshot` from the live
/// dimensions of its leaves. For each `Split`, children are combined
/// along the split axis (plus a 1-cell separator the GUI renders) and
/// max-ed along the orthogonal axis.
///
/// `leaf_dims` returns `(cols, rows)` for a given `pane_id`; leaves with
/// no current dimensions (because the pane was just removed from the
/// window) are treated as `(0, 0)` so the remaining siblings still
/// contribute their share.
///
/// Used by the daemon-side `KillPane` path (tn-ju04) to recover the
/// pre-kill parent rect before removing the dead leaf, so the surviving
/// siblings can be cascaded back up to reclaim the freed cells.
pub(super) fn reconstruct_layout_rect<F>(
    node: &LayoutSnapshot,
    mut leaf_dims: F,
) -> Option<(u16, u16)>
where
    F: FnMut(PaneId) -> Option<(u16, u16)>,
{
    fn walk<F: FnMut(PaneId) -> Option<(u16, u16)>>(
        node: &LayoutSnapshot,
        leaf_dims: &mut F,
    ) -> (u16, u16) {
        use therminal_protocol::daemon::LayoutSplitDirection;
        match node {
            LayoutSnapshot::Leaf { pane_id } => leaf_dims(*pane_id).unwrap_or((0, 0)),
            LayoutSnapshot::Split {
                direction,
                first,
                second,
                ..
            } => {
                let (fc, fr) = walk(first, leaf_dims);
                let (sc, sr) = walk(second, leaf_dims);
                match direction {
                    LayoutSplitDirection::Horizontal => {
                        // Side-by-side: sum cols (+1 separator), max rows.
                        let cols = fc.saturating_add(sc).saturating_add(1);
                        let rows = fr.max(sr);
                        (cols, rows)
                    }
                    LayoutSplitDirection::Vertical => {
                        // Stacked: max cols, sum rows (+1 separator).
                        let cols = fc.max(sc);
                        let rows = fr.saturating_add(sr).saturating_add(1);
                        (cols, rows)
                    }
                }
            }
        }
    }
    let (cols, rows) = walk(node, &mut leaf_dims);
    if cols == 0 || rows == 0 {
        None
    } else {
        Some((cols, rows))
    }
}

/// Recursively walk `node` assuming a parent rect of `(cols, rows)`
/// cells, splitting by `ratio` at each `Split` node, and return one
/// `LeafDims` per leaf. Sibling gap (the 1-cell separator the GUI uses)
/// is modelled by subtracting 1 from the split axis before ratioing, so
/// 80 cols split 50/50 yields 39 + 40 + 1 gap rather than 40 + 40 = 81.
///
/// Used by the daemon-side `SplitPane` and `KillPane` paths so cascade
/// resizes match what the GUI would compute from the same layout tree.
pub(super) fn layout_leaf_dims(node: &LayoutSnapshot, cols: u16, rows: u16) -> Vec<LeafDims> {
    use therminal_protocol::daemon::LayoutSplitDirection;
    let mut out = Vec::new();
    match node {
        LayoutSnapshot::Leaf { pane_id } => {
            out.push(LeafDims {
                pane_id: *pane_id,
                cols,
                rows,
            });
        }
        LayoutSnapshot::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let ratio = ratio.clamp(0.05, 0.95);
            match direction {
                LayoutSplitDirection::Horizontal => {
                    // Side-by-side: split cols; rows unchanged; reserve 1
                    // cell for the separator (matches GUI SEPARATOR_GAP
                    // projected onto the cell grid).
                    let usable = cols.saturating_sub(1);
                    let first_cols = ((usable as f32) * ratio).round().max(1.0) as u16;
                    let second_cols = usable.saturating_sub(first_cols).max(1);
                    out.extend(layout_leaf_dims(first, first_cols, rows));
                    out.extend(layout_leaf_dims(second, second_cols, rows));
                }
                LayoutSplitDirection::Vertical => {
                    // Stacked: split rows; cols unchanged.
                    let usable = rows.saturating_sub(1);
                    let first_rows = ((usable as f32) * ratio).round().max(1.0) as u16;
                    let second_rows = usable.saturating_sub(first_rows).max(1);
                    out.extend(layout_leaf_dims(first, cols, first_rows));
                    out.extend(layout_leaf_dims(second, cols, second_rows));
                }
            }
        }
    }
    out
}
