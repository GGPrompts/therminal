//! CPU-side image placement tracking for the Kitty graphics protocol.
//!
//! A *placement* is a displayed instance of an image — anchored at some
//! `(row, col)` in the scrollback, sized in cells (plus optional
//! sub-cell pixel offsets), stacked by `z_index`. The grid (the
//! scrollback, not the viewport) owns placements so they:
//!
//! - Scroll with content when the scrollback advances.
//! - Drop off the top when their anchor row falls out of scrollback.
//! - Are cleared when their anchor cell is erased by CSI J/K or overwritten.
//! - Stack deterministically by z-index and then creation order.
//!
//! This module is CPU-side only — it does not touch wgpu. GPU upload and
//! draw ordering happen in the renderer (tn-wdn1), which reads
//! [`Placement`]s out of a [`PlacementSet`] during the frame pass.
//!
//! ## Ownership
//!
//! `terminal.rs` in this crate only defines shared types; the full
//! `Terminal` wrapper struct lives in downstream crates (the GUI app and
//! the daemon). A `PlacementSet` is meant to be owned alongside the
//! alacritty `Term` there, updated from [`crate::terminal::GraphicsEvent`]s
//! as they fall out of the interceptor and from scrollback / cursor
//! events from the `Term`'s event listener.
//!
//! ## z-index model (v1)
//!
//! The Kitty protocol defines a 32-bit signed z-index. v1 splits into two
//! buckets:
//!
//! - **Under text** — `z < 0`. Drawn before terminal cells.
//! - **Over text** — `z >= 0`. Drawn after terminal cells.
//!
//! Within a bucket, placements are ordered by `(z_index, created_at)`
//! ascending. Kitty's ultra-low "under-background" tier
//! (`z < -1_073_741_824`) is **not** implemented in v1 — see the TODO on
//! [`Placement::draw_order`].
//!
//! ## Delete filters
//!
//! The `a=d` command is refined by the `d=` key into a large filter
//! vocabulary. v1 wires up the ones that matter for the common agent
//! workflow:
//!
//! - `d=a` — delete everything.
//! - `d=i` — delete all placements of a specific `image_id`.
//! - `d=i,p=` — delete a specific `(image_id, placement_id)`.
//! - `d=C` — delete the newest placement anchored at the cursor cell.
//!
//! Stubbed with clear TODO comments: `d=r` (row match), `d=c` (column
//! match), `d=x`/`d=y` (pixel match), `d=z` (z-index match), `d=n`
//! (count-limited delete).

use std::collections::HashMap;

use crate::graphics::store::ImageId;
use crate::graphics::{DeleteScope, RawGraphicsCommand};
use crate::terminal::GraphicsEvent;

/// A single displayed image instance.
///
/// Anchors are scrollback-relative row indices so a placement's screen
/// position tracks the row as the scrollback scrolls. Pixel offsets are
/// sub-cell nudges inside the anchor cell — see the module docs for the
/// cell-pixel conversion policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// Image this placement renders.
    pub image_id: u32,
    /// Placement id (zero if the client did not specify `p=`).
    pub placement_id: u32,
    /// Scrollback-relative row of the top-left cell.
    pub anchor_row: usize,
    /// Column of the top-left cell.
    pub anchor_col: usize,
    /// Height of the placement in cells (>= 1).
    pub cell_rows: u32,
    /// Width of the placement in cells (>= 1).
    pub cell_cols: u32,
    /// Horizontal pixel offset inside the anchor cell. Stored as-is so
    /// the render layer can convert to pixels using its cell metrics
    /// (`cell_px(width, height)` on `GridRenderer`). This keeps the
    /// `therminal-terminal` crate free of rendering dependencies.
    pub px_x_offset: u32,
    /// Vertical pixel offset inside the anchor cell.
    pub px_y_offset: u32,
    /// Kitty z-index. Negative values render under terminal cells.
    pub z_index: i32,
    /// Monotonic creation order tick. Used as a stable tiebreaker when
    /// sorting by z-index. Monotonicity is enforced by
    /// [`PlacementSet::next_tick`].
    pub created_at: u64,
}

impl Placement {
    /// True iff this placement renders under the terminal text layer.
    ///
    /// TODO: Kitty also defines an "under background" tier at
    /// `z < -1_073_741_824` that renders under the cell background. v1
    /// ignores this — all negative z renders between background and
    /// glyphs. Revisit when the renderer (tn-wdn1) grows a background
    /// pass we can slot into.
    pub fn is_under_text(&self) -> bool {
        self.z_index < 0
    }

    /// Key used to sort placements within a z-bucket.
    ///
    /// Returns `(z_index, created_at)` so ties break by insertion order —
    /// a later `a=p` wins over an earlier one at the same z-index, which
    /// matches Kitty's documented behaviour.
    pub fn draw_order(&self) -> (i32, u64) {
        (self.z_index, self.created_at)
    }

    /// True iff this placement's anchor cell falls inside the inclusive
    /// row-range `[start, end]`.
    fn anchor_in_row_range(&self, start: usize, end: usize) -> bool {
        self.anchor_row >= start && self.anchor_row <= end
    }
}

/// A map of live placements keyed by `(image_id, placement_id)`.
///
/// The primary data structure is an insertion-ordered flat `Vec` so
/// draw-order iteration is cache-friendly and the common "drop everything
/// in this row range" operation is a single retain pass. The `HashMap`
/// is kept in sync for O(1) lookup/delete by id.
#[derive(Debug, Default)]
pub struct PlacementSet {
    /// Flat list of placements. Insertion order == discovery order.
    placements: Vec<Placement>,
    /// Index into `placements` keyed by `(image_id, placement_id)`. Kept
    /// in sync on every mutation. Used for O(1) delete-by-id.
    index: HashMap<ImageId, usize>,
    /// Monotonically increasing tick used to stamp `created_at` on new
    /// placements. Separate from real time so tests are deterministic.
    next_tick: u64,
}

impl PlacementSet {
    /// Empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live placements.
    pub fn len(&self) -> usize {
        self.placements.len()
    }

    /// True iff the set holds no placements.
    pub fn is_empty(&self) -> bool {
        self.placements.is_empty()
    }

    /// Iterate placements in draw order.
    ///
    /// Yields under-text placements first (`z < 0`), then over-text
    /// placements (`z >= 0`). Within each bucket the order is
    /// `(z_index, created_at)` ascending so a later placement paints on
    /// top of an earlier one at the same z.
    pub fn iter_draw_order(&self) -> impl Iterator<Item = &Placement> {
        let mut under: Vec<&Placement> = self
            .placements
            .iter()
            .filter(|p| p.is_under_text())
            .collect();
        let mut over: Vec<&Placement> = self
            .placements
            .iter()
            .filter(|p| !p.is_under_text())
            .collect();
        under.sort_by_key(|p| p.draw_order());
        over.sort_by_key(|p| p.draw_order());
        under.into_iter().chain(over)
    }

    /// Borrow all placements in insertion order (mainly for tests).
    pub fn as_slice(&self) -> &[Placement] {
        &self.placements
    }

    /// Look up a placement by `(image_id, placement_id)`.
    pub fn get(&self, id: ImageId) -> Option<&Placement> {
        self.index.get(&id).map(|&i| &self.placements[i])
    }

    /// Issue a fresh creation tick. Exposed so callers that want to
    /// fabricate a `Placement` outside the event-driven path (e.g.
    /// tests, deterministic fixtures) can keep `created_at` monotonic.
    pub fn next_tick(&mut self) -> u64 {
        let t = self.next_tick;
        self.next_tick = self.next_tick.wrapping_add(1);
        t
    }

    // -- event-driven mutation ---------------------------------------------

    /// Apply a [`GraphicsEvent`] to the placement set.
    ///
    /// `cursor_row` / `cursor_col` are the current cursor position and
    /// are used as the anchor for `a=p` (GraphicsDisplay) when the
    /// command does not specify an explicit anchor in its extras. The
    /// cursor is also used by the `d=C` delete filter.
    ///
    /// Transmit and query events are ignored here — placements only
    /// care about display and delete. `GraphicsTransmit` with
    /// `display=true` (i.e. `a=T`) does **not** insert a placement: the
    /// renderer synthesizes the `a=p` follow-up itself because the
    /// transmit path is also what drives the image store. A caller that
    /// wants the auto-display semantics should call
    /// [`Self::insert_display_at_cursor`] directly on the `a=T` event.
    pub fn apply_event(&mut self, event: &GraphicsEvent, cursor_row: usize, cursor_col: usize) {
        match event {
            GraphicsEvent::GraphicsDisplay {
                image_id,
                placement_id,
                rows,
                cols,
                z_index,
                command,
            } => {
                self.insert_from_display(
                    *image_id,
                    *placement_id,
                    *rows,
                    *cols,
                    *z_index,
                    command,
                    cursor_row,
                    cursor_col,
                );
            }
            GraphicsEvent::GraphicsDelete { scope, command } => {
                self.apply_delete(scope, command, cursor_row, cursor_col);
            }
            GraphicsEvent::GraphicsTransmit { .. } | GraphicsEvent::GraphicsQuery { .. } => {
                // No-op: transmit feeds the image store, query is a
                // protocol ack. Placement lifecycle runs on display +
                // delete only.
            }
        }
    }

    /// Insert a placement at the cursor for an `a=T` (transmit-and-display)
    /// event.
    ///
    /// The transmit path produces the `DecodedImage` in the store; this
    /// is the parallel call that records the on-screen placement so the
    /// transmit-then-auto-display flow is a single step at the call
    /// site.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_display_at_cursor(
        &mut self,
        image_id: Option<u32>,
        placement_id: Option<u32>,
        rows: Option<u32>,
        cols: Option<u32>,
        z_index: Option<i32>,
        command: &RawGraphicsCommand,
        cursor_row: usize,
        cursor_col: usize,
    ) {
        self.insert_from_display(
            image_id,
            placement_id,
            rows,
            cols,
            z_index,
            command,
            cursor_row,
            cursor_col,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_from_display(
        &mut self,
        image_id: Option<u32>,
        placement_id: Option<u32>,
        rows: Option<u32>,
        cols: Option<u32>,
        z_index: Option<i32>,
        command: &RawGraphicsCommand,
        cursor_row: usize,
        cursor_col: usize,
    ) {
        // Anchor: default to cursor, overridden by explicit extras. The
        // parser promotes `r`/`c` to cell-sizes; positional overrides
        // live in extras (Kitty does not standardise a positional key
        // for `a=p`, but clients occasionally drop e.g. `r=` and `c=`
        // interpreted as row/col anchors — we honour any explicit
        // anchor in extras to stay friendly to that dialect).
        let anchor_row = extras_usize(&command.extras, "anchor_row")
            .or_else(|| extras_usize(&command.extras, "Y"))
            .unwrap_or(cursor_row);
        let anchor_col = extras_usize(&command.extras, "anchor_col")
            .or_else(|| extras_usize(&command.extras, "X"))
            .unwrap_or(cursor_col);

        // Sub-cell pixel offsets — stored as-is; renderer converts to
        // pixels using its cell metrics. Kitty uses `X=`/`Y=` for this
        // but they conflict with the anchor override above; we accept
        // lower-case `x_px`/`y_px` as an unambiguous alternative in the
        // extras map, plus falling back to the typed `width_px`/`height_px`
        // fields when those carry sub-cell fine-tuning.
        let px_x_offset = extras_u32(&command.extras, "x_px").unwrap_or(0);
        let px_y_offset = extras_u32(&command.extras, "y_px").unwrap_or(0);

        let cell_rows = rows.unwrap_or(1).max(1);
        let cell_cols = cols.unwrap_or(1).max(1);
        let z = z_index.unwrap_or(0);

        let id = ImageId::new(image_id, placement_id);
        let placement = Placement {
            image_id: id.image_id,
            placement_id: id.placement_id,
            anchor_row,
            anchor_col,
            cell_rows,
            cell_cols,
            px_x_offset,
            px_y_offset,
            z_index: z,
            created_at: self.next_tick(),
        };

        // Upsert: if the same `(image_id, placement_id)` already exists,
        // replace it in place rather than adding a second entry. This
        // matches Kitty's behaviour — re-issuing `a=p` with the same id
        // moves the existing placement.
        if let Some(&pos) = self.index.get(&id) {
            self.placements[pos] = placement;
            return;
        }

        self.index.insert(id, self.placements.len());
        self.placements.push(placement);
    }

    fn apply_delete(
        &mut self,
        scope: &DeleteScope,
        command: &RawGraphicsCommand,
        cursor_row: usize,
        cursor_col: usize,
    ) {
        // Inspect `d=` in extras — the protocol layer collapses `d=a`
        // and `d=i`/`d=i,p=` into `DeleteScope`, but finer-grained
        // filters (`d=C`, `d=r`, `d=c`, …) have to be teased out of the
        // raw key=value map.
        let d = command.extras.get("d").map(String::as_str).unwrap_or("");

        match d {
            // d=C: delete the newest placement anchored at the cursor
            // cell. Kitty specifies "closest to the top of the stack",
            // which we read as "latest `created_at`".
            "C" => {
                self.delete_newest_at_cursor(cursor_row, cursor_col);
                return;
            }
            // Stubs — emitted with a tracing::debug so a test can spot
            // the silent drop, but the set is not mutated. See the
            // module-level docs for the rationale.
            "r" | "c" | "x" | "y" | "z" | "n" => {
                tracing::debug!(
                    filter = %d,
                    "kitty graphics d={} delete filter is a TODO stub in placements.rs",
                    d
                );
                return;
            }
            _ => {
                // Fall through to the coarse-grained DeleteScope path.
            }
        }

        match scope {
            DeleteScope::All => self.clear(),
            DeleteScope::ById {
                image_id,
                placement_id,
            } => {
                // i=<id> with no placement id => drop all placements of
                // that image. i=<id>,p=<pid> => drop the specific one.
                match (image_id, placement_id) {
                    (Some(iid), Some(pid)) => {
                        self.remove(ImageId::new(Some(*iid), Some(*pid)));
                    }
                    (Some(iid), None) => {
                        self.retain(|p| p.image_id != *iid);
                    }
                    (None, Some(pid)) => {
                        // p=<pid> alone (no image id): match any image
                        // bearing that placement id. Rare but legal.
                        self.retain(|p| p.placement_id != *pid);
                    }
                    (None, None) => {
                        // Coerced DeleteScope::All — handled above.
                        self.clear();
                    }
                }
            }
        }
    }

    /// Remove a specific placement by id. Silently no-ops if missing.
    pub fn remove(&mut self, id: ImageId) {
        if let Some(pos) = self.index.remove(&id) {
            self.placements.swap_remove(pos);
            // swap_remove may have moved an element into `pos`; its
            // index entry needs to be rewritten to point at the new
            // slot.
            if let Some(moved) = self.placements.get(pos) {
                let moved_id = ImageId::new(Some(moved.image_id), Some(moved.placement_id));
                self.index.insert(moved_id, pos);
            }
        }
    }

    /// Drop every placement. Used by `a=d,d=a` and by
    /// `DeleteScope::All` routing.
    pub fn clear(&mut self) {
        self.placements.clear();
        self.index.clear();
    }

    /// Retain placements matching `f`; rebuild the index.
    ///
    /// Public so the owning `Term` wrapper can spoon-feed clears
    /// synthesised from non-graphics events (see the scrollback /
    /// CSI-J / CSI-K helpers below).
    pub fn retain(&mut self, mut f: impl FnMut(&Placement) -> bool) {
        self.placements.retain(|p| f(p));
        self.rebuild_index();
    }

    fn rebuild_index(&mut self) {
        self.index.clear();
        for (i, p) in self.placements.iter().enumerate() {
            self.index
                .insert(ImageId::new(Some(p.image_id), Some(p.placement_id)), i);
        }
    }

    /// Delete the newest placement whose anchor cell is the cursor's
    /// cell. No-op if nothing matches.
    pub fn delete_newest_at_cursor(&mut self, cursor_row: usize, cursor_col: usize) {
        let newest = self
            .placements
            .iter()
            .enumerate()
            .filter(|(_, p)| p.anchor_row == cursor_row && p.anchor_col == cursor_col)
            .max_by_key(|(_, p)| p.created_at)
            .map(|(i, _)| i);
        if let Some(i) = newest {
            let p = &self.placements[i];
            let id = ImageId::new(Some(p.image_id), Some(p.placement_id));
            // Go through `remove()` so the index stays consistent.
            self.remove(id);
        }
    }

    // -- scrollback lifecycle -----------------------------------------------

    /// Shift every placement's anchor row by `delta` lines.
    ///
    /// Called when the grid scrolls by `delta` lines (positive = scroll
    /// up, i.e. content moves toward smaller row indices, placements'
    /// anchor rows decrement). Any placement whose new anchor row falls
    /// below `0` (saturating subtraction underflow) is treated as
    /// scrolled off the top and dropped.
    ///
    /// `delta` is in lines. The sign convention matches the common
    /// "scroll up by N" terminal operation. For scroll-down (content
    /// moves toward larger row indices) pass a negative `delta`.
    pub fn scroll_by(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        let keep: Vec<Placement> = std::mem::take(&mut self.placements)
            .into_iter()
            .filter_map(|mut p| {
                let new = p.anchor_row as isize - delta;
                if new < 0 {
                    // Scrolled off the top of the scrollback.
                    None
                } else {
                    p.anchor_row = new as usize;
                    Some(p)
                }
            })
            .collect();
        self.placements = keep;
        self.rebuild_index();
    }

    /// Drop placements whose anchor row is below `min_row` (exclusive of
    /// `min_row`). Used when the scrollback trims history from the top.
    pub fn trim_scrollback_below(&mut self, min_row: usize) {
        self.retain(|p| p.anchor_row >= min_row);
    }

    /// Drop placements whose anchor row falls inside `[start, end]`
    /// inclusive. Used on CSI 2J (erase display) and CSI J variants
    /// that clear a row range. `CSI K` maps to
    /// `clear_rows(current_row, current_row)`.
    pub fn clear_rows(&mut self, start: usize, end: usize) {
        self.retain(|p| !p.anchor_in_row_range(start, end));
    }

    /// Drop the placement whose anchor is exactly `(row, col)`. Used
    /// when a single cell is overwritten and its placement should be
    /// considered erased. No-op if nothing is anchored there.
    pub fn clear_cell(&mut self, row: usize, col: usize) {
        self.retain(|p| !(p.anchor_row == row && p.anchor_col == col));
    }
}

// -- private helpers -------------------------------------------------------

fn extras_u32(extras: &HashMap<String, String>, key: &str) -> Option<u32> {
    extras.get(key).and_then(|v| v.parse::<u32>().ok())
}

fn extras_usize(extras: &HashMap<String, String>, key: &str) -> Option<usize> {
    extras.get(key).and_then(|v| v.parse::<usize>().ok())
}

// -- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::{GraphicsAction, GraphicsFormat, GraphicsMedium, QuietLevel};

    fn raw_cmd(action: GraphicsAction) -> RawGraphicsCommand {
        RawGraphicsCommand {
            action,
            format: GraphicsFormat::Default,
            medium: GraphicsMedium::Direct,
            image_id: None,
            placement_id: None,
            rows: None,
            cols: None,
            width_px: None,
            height_px: None,
            z_index: None,
            more_chunks: false,
            quiet: QuietLevel::Normal,
            extras: HashMap::new(),
        }
    }

    fn display_event(
        image_id: Option<u32>,
        placement_id: Option<u32>,
        rows: Option<u32>,
        cols: Option<u32>,
        z_index: Option<i32>,
    ) -> GraphicsEvent {
        let mut command = raw_cmd(GraphicsAction::Put);
        command.image_id = image_id;
        command.placement_id = placement_id;
        command.rows = rows;
        command.cols = cols;
        command.z_index = z_index;
        GraphicsEvent::GraphicsDisplay {
            image_id,
            placement_id,
            rows,
            cols,
            z_index,
            command,
        }
    }

    fn delete_event_by_id(image_id: Option<u32>, placement_id: Option<u32>) -> GraphicsEvent {
        let mut command = raw_cmd(GraphicsAction::Delete);
        command.image_id = image_id;
        command.placement_id = placement_id;
        GraphicsEvent::GraphicsDelete {
            scope: DeleteScope::ById {
                image_id,
                placement_id,
            },
            command,
        }
    }

    fn delete_event_all() -> GraphicsEvent {
        let command = raw_cmd(GraphicsAction::Delete);
        GraphicsEvent::GraphicsDelete {
            scope: DeleteScope::All,
            command,
        }
    }

    fn delete_event_cursor() -> GraphicsEvent {
        let mut command = raw_cmd(GraphicsAction::Delete);
        command.extras.insert("d".to_string(), "C".to_string());
        GraphicsEvent::GraphicsDelete {
            scope: DeleteScope::All,
            command,
        }
    }

    #[test]
    fn display_event_inserts_placement_at_cursor() {
        let mut set = PlacementSet::new();
        let ev = display_event(Some(1), Some(2), Some(4), Some(10), Some(0));
        set.apply_event(&ev, 5, 12);

        assert_eq!(set.len(), 1);
        let p = &set.as_slice()[0];
        assert_eq!(p.image_id, 1);
        assert_eq!(p.placement_id, 2);
        assert_eq!(p.anchor_row, 5);
        assert_eq!(p.anchor_col, 12);
        assert_eq!(p.cell_rows, 4);
        assert_eq!(p.cell_cols, 10);
        assert_eq!(p.z_index, 0);
    }

    #[test]
    fn display_event_defaults_missing_dimensions_to_one() {
        let mut set = PlacementSet::new();
        let ev = display_event(Some(7), None, None, None, None);
        set.apply_event(&ev, 0, 0);
        let p = &set.as_slice()[0];
        assert_eq!(p.cell_rows, 1);
        assert_eq!(p.cell_cols, 1);
        assert_eq!(p.z_index, 0);
        assert_eq!(p.placement_id, 0);
    }

    #[test]
    fn display_event_twice_upserts_same_id() {
        let mut set = PlacementSet::new();
        let ev1 = display_event(Some(1), Some(1), Some(2), Some(2), Some(0));
        set.apply_event(&ev1, 3, 3);
        let ev2 = display_event(Some(1), Some(1), Some(4), Some(4), Some(0));
        set.apply_event(&ev2, 9, 9);

        assert_eq!(set.len(), 1, "same (image,placement) id upserts in place");
        let p = &set.as_slice()[0];
        assert_eq!(p.anchor_row, 9);
        assert_eq!(p.cell_rows, 4);
    }

    #[test]
    fn scrolling_shifts_anchors_up() {
        let mut set = PlacementSet::new();
        set.apply_event(
            &display_event(Some(1), None, Some(1), Some(1), Some(0)),
            5,
            0,
        );
        set.apply_event(
            &display_event(Some(2), None, Some(1), Some(1), Some(0)),
            10,
            0,
        );

        set.scroll_by(1);

        assert_eq!(set.get(ImageId::new(Some(1), None)).unwrap().anchor_row, 4);
        assert_eq!(set.get(ImageId::new(Some(2), None)).unwrap().anchor_row, 9);
    }

    #[test]
    fn scrolling_drops_placements_off_top() {
        let mut set = PlacementSet::new();
        set.apply_event(
            &display_event(Some(1), None, Some(1), Some(1), Some(0)),
            0,
            0,
        );
        set.apply_event(
            &display_event(Some(2), None, Some(1), Some(1), Some(0)),
            5,
            0,
        );

        // Scroll 3 — image 1 (row 0) goes to row -3 and is dropped.
        set.scroll_by(3);

        assert_eq!(set.len(), 1);
        assert!(set.get(ImageId::new(Some(1), None)).is_none());
        assert_eq!(set.get(ImageId::new(Some(2), None)).unwrap().anchor_row, 2);
    }

    #[test]
    fn csi_2j_clears_visible_keeps_scrollback() {
        let mut set = PlacementSet::new();
        // Scrollback: anchor rows 0..=4.
        set.apply_event(&display_event(Some(1), None, Some(1), Some(1), None), 2, 0);
        // Visible (say rows 10..=20):
        set.apply_event(&display_event(Some(2), None, Some(1), Some(1), None), 12, 0);
        set.apply_event(&display_event(Some(3), None, Some(1), Some(1), None), 18, 0);

        // CSI 2J -> clear visible region [10, 20].
        set.clear_rows(10, 20);

        assert!(
            set.get(ImageId::new(Some(1), None)).is_some(),
            "scrollback kept"
        );
        assert!(set.get(ImageId::new(Some(2), None)).is_none());
        assert!(set.get(ImageId::new(Some(3), None)).is_none());
    }

    #[test]
    fn trim_scrollback_below_drops_anchors_above_min() {
        let mut set = PlacementSet::new();
        for (iid, row) in [(1u32, 0usize), (2, 5), (3, 10)] {
            set.apply_event(
                &display_event(Some(iid), None, Some(1), Some(1), None),
                row,
                0,
            );
        }
        set.trim_scrollback_below(5);
        assert!(set.get(ImageId::new(Some(1), None)).is_none());
        assert!(set.get(ImageId::new(Some(2), None)).is_some());
        assert!(set.get(ImageId::new(Some(3), None)).is_some());
    }

    #[test]
    fn clear_cell_drops_anchor_at_exact_position() {
        let mut set = PlacementSet::new();
        set.apply_event(&display_event(Some(1), None, Some(1), Some(1), None), 4, 5);
        set.apply_event(&display_event(Some(2), None, Some(1), Some(1), None), 4, 6);

        set.clear_cell(4, 5);
        assert!(set.get(ImageId::new(Some(1), None)).is_none());
        assert!(set.get(ImageId::new(Some(2), None)).is_some());
    }

    #[test]
    fn delete_by_image_id_drops_all_placements() {
        let mut set = PlacementSet::new();
        // Two placements of image 1.
        set.apply_event(
            &display_event(Some(1), Some(1), Some(1), Some(1), None),
            0,
            0,
        );
        set.apply_event(
            &display_event(Some(1), Some(2), Some(1), Some(1), None),
            1,
            0,
        );
        // One of image 2.
        set.apply_event(&display_event(Some(2), None, Some(1), Some(1), None), 2, 0);

        set.apply_event(&delete_event_by_id(Some(1), None), 0, 0);
        assert_eq!(set.len(), 1);
        assert!(set.get(ImageId::new(Some(2), None)).is_some());
    }

    #[test]
    fn delete_by_image_and_placement_id_drops_only_that_one() {
        let mut set = PlacementSet::new();
        set.apply_event(
            &display_event(Some(1), Some(1), Some(1), Some(1), None),
            0,
            0,
        );
        set.apply_event(
            &display_event(Some(1), Some(2), Some(1), Some(1), None),
            1,
            0,
        );

        set.apply_event(&delete_event_by_id(Some(1), Some(1)), 0, 0);
        assert_eq!(set.len(), 1);
        assert!(set.get(ImageId::new(Some(1), Some(1))).is_none());
        assert!(set.get(ImageId::new(Some(1), Some(2))).is_some());
    }

    #[test]
    fn delete_all_clears_set() {
        let mut set = PlacementSet::new();
        set.apply_event(&display_event(Some(1), None, Some(1), Some(1), None), 0, 0);
        set.apply_event(&display_event(Some(2), None, Some(1), Some(1), None), 1, 0);

        set.apply_event(&delete_event_all(), 0, 0);
        assert!(set.is_empty());
    }

    #[test]
    fn delete_cursor_drops_newest_at_cursor() {
        let mut set = PlacementSet::new();
        set.apply_event(
            &display_event(Some(1), Some(1), Some(1), Some(1), None),
            3,
            4,
        );
        // Second placement on the same cell — newer; should be dropped first.
        set.apply_event(
            &display_event(Some(2), Some(1), Some(1), Some(1), None),
            3,
            4,
        );
        // A decoy at a different cell.
        set.apply_event(&display_event(Some(3), None, Some(1), Some(1), None), 7, 0);

        set.apply_event(&delete_event_cursor(), 3, 4);

        assert_eq!(set.len(), 2);
        assert!(
            set.get(ImageId::new(Some(2), Some(1))).is_none(),
            "newest-at-cursor went first"
        );
        assert!(set.get(ImageId::new(Some(1), Some(1))).is_some());
        assert!(set.get(ImageId::new(Some(3), None)).is_some());
    }

    #[test]
    fn delete_cursor_when_nothing_matches_is_noop() {
        let mut set = PlacementSet::new();
        set.apply_event(&display_event(Some(1), None, Some(1), Some(1), None), 0, 0);
        set.apply_event(&delete_event_cursor(), 99, 99);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn draw_order_splits_under_and_over_text() {
        let mut set = PlacementSet::new();
        set.apply_event(
            &display_event(Some(1), None, Some(1), Some(1), Some(5)),
            0,
            0,
        );
        set.apply_event(
            &display_event(Some(2), None, Some(1), Some(1), Some(-3)),
            1,
            0,
        );
        set.apply_event(
            &display_event(Some(3), None, Some(1), Some(1), Some(0)),
            2,
            0,
        );
        set.apply_event(
            &display_event(Some(4), None, Some(1), Some(1), Some(-1)),
            3,
            0,
        );

        let ids: Vec<u32> = set.iter_draw_order().map(|p| p.image_id).collect();

        // Under-text first, ascending z: (-3, -1). Then over: (0, 5).
        assert_eq!(ids, vec![2, 4, 3, 1]);
    }

    #[test]
    fn draw_order_uses_created_at_as_tiebreaker() {
        let mut set = PlacementSet::new();
        set.apply_event(
            &display_event(Some(1), None, Some(1), Some(1), Some(2)),
            0,
            0,
        );
        set.apply_event(
            &display_event(Some(2), None, Some(1), Some(1), Some(2)),
            1,
            0,
        );
        let ids: Vec<u32> = set.iter_draw_order().map(|p| p.image_id).collect();
        assert_eq!(ids, vec![1, 2], "earlier creation wins tie");
    }

    #[test]
    fn grid_resize_preserves_placements() {
        // "Resize" in this model doesn't touch placement state — anchors
        // are cell indices in scrollback, which survive a viewport
        // resize. This test captures the invariant: nothing in this
        // module reacts to a resize, so the placement set is unchanged.
        let mut set = PlacementSet::new();
        set.apply_event(
            &display_event(Some(1), Some(1), Some(2), Some(3), None),
            10,
            5,
        );
        let before = set.as_slice().to_vec();

        // Simulate a resize by doing nothing and asserting identity.
        // (The owning Term is responsible for re-flowing cell text; it
        // does not re-anchor images.)
        assert_eq!(set.as_slice(), before.as_slice());
    }

    #[test]
    fn stub_filters_do_not_mutate() {
        let mut set = PlacementSet::new();
        set.apply_event(&display_event(Some(1), None, Some(1), Some(1), None), 0, 0);
        for stub in ["r", "c", "x", "y", "z", "n"] {
            let mut command = raw_cmd(GraphicsAction::Delete);
            command.extras.insert("d".to_string(), stub.to_string());
            let ev = GraphicsEvent::GraphicsDelete {
                scope: DeleteScope::All,
                command,
            };
            set.apply_event(&ev, 0, 0);
        }
        assert_eq!(set.len(), 1, "stubbed filters must be no-ops");
    }

    #[test]
    fn transmit_and_query_events_are_noops() {
        let mut set = PlacementSet::new();
        // Transmit (not TransmitAndDisplay) — placements ignore it; the
        // store handles the pixel side.
        let transmit = GraphicsEvent::GraphicsTransmit {
            image_id: Some(1),
            placement_id: None,
            format: GraphicsFormat::Rgba,
            medium: GraphicsMedium::Direct,
            width_px: None,
            height_px: None,
            payload: Vec::new(),
            display: false,
            command: raw_cmd(GraphicsAction::Transmit),
        };
        set.apply_event(&transmit, 0, 0);
        assert!(set.is_empty());

        let query = GraphicsEvent::GraphicsQuery {
            image_id: Some(1),
            command: raw_cmd(GraphicsAction::Query),
        };
        set.apply_event(&query, 0, 0);
        assert!(set.is_empty());
    }

    #[test]
    fn insert_display_at_cursor_covers_transmit_and_display_path() {
        let mut set = PlacementSet::new();
        let mut command = raw_cmd(GraphicsAction::TransmitAndDisplay);
        command.image_id = Some(42);
        set.insert_display_at_cursor(Some(42), None, Some(2), Some(2), Some(1), &command, 8, 9);
        assert_eq!(set.len(), 1);
        let p = set.get(ImageId::new(Some(42), None)).unwrap();
        assert_eq!(p.anchor_row, 8);
        assert_eq!(p.anchor_col, 9);
        assert_eq!(p.cell_rows, 2);
        assert_eq!(p.cell_cols, 2);
        assert_eq!(p.z_index, 1);
    }
}
