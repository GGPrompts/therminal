//! Pane layout tree and per-pane terminal state.
//!
//! Implements a binary tree of splits where each leaf is a terminal pane
//! with its own PTY, Term, and VTE parser. Supports horizontal/vertical
//! splits, focus navigation, ratio-based resize, and pane close with
//! tree rebalancing.

use std::io::{Read as IoRead, Write as IoWrite};
use std::sync::Arc;
use std::thread;

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi;
use portable_pty::MasterPty;
use therminal_core::geometry::Rect;
use tracing::{info, warn};

use crate::grid_renderer::GridRenderer;

// ── Re-export canonical PaneId from protocol ────────────────────────────

pub use therminal_protocol::PaneId;

/// Direction of a split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    /// Side-by-side (left | right).
    Horizontal,
    /// Stacked (top / bottom).
    Vertical,
}

/// Separator gap between panes in physical pixels.
pub const SEPARATOR_GAP: f32 = 2.0;

// ── EventListener for per-pane Term ─────────────────────────────────────

/// Minimal listener forwarded from each pane's Term.
#[derive(Clone)]
pub(crate) struct PaneListener;

impl EventListener for PaneListener {
    fn send_event(&self, event: TermEvent) {
        match event {
            TermEvent::Title(title) => tracing::debug!("Pane title: {title}"),
            TermEvent::Wakeup => {}
            _ => tracing::debug!("Pane event: {event:?}"),
        }
    }
}

// ── Dimensions adapter ──────────────────────────────────────────────────

struct PaneTermSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for PaneTermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

// ── Per-pane state ──────────────────────────────────────────────────────

/// State for a single terminal pane.
pub struct PaneState {
    pub id: PaneId,
    pub term: Arc<FairMutex<Term<PaneListener>>>,
    pub pty_writer: Box<dyn IoWrite + Send>,
    pub pty_master: Box<dyn MasterPty + Send>,
    /// Current viewport rect in physical pixels (set by layout computation).
    pub viewport: Rect,
    /// Scrollback configuration.
    #[allow(dead_code)]
    pub scrollback_lines: usize,
}

impl PaneState {
    /// Resize this pane's terminal and PTY to match a new viewport rect.
    pub fn resize_to_viewport(&mut self, rect: Rect, renderer: &GridRenderer) {
        self.viewport = rect;
        let (cols, rows) = grid_size_for_rect(rect, renderer);
        if cols == 0 || rows == 0 {
            return;
        }
        {
            let mut term_guard = self.term.lock();
            let size = PaneTermSize {
                columns: cols,
                screen_lines: rows,
            };
            term_guard.resize(size);
        }
        if let Err(e) =
            therminal_terminal::pty::resize(self.pty_master.as_ref(), cols as u16, rows as u16)
        {
            warn!("Failed to resize pane {} PTY: {e}", self.id);
        }
    }
}

/// Compute (cols, rows) for a viewport rect using the renderer's cell metrics.
pub fn grid_size_for_rect(rect: Rect, renderer: &GridRenderer) -> (usize, usize) {
    let usable_w = rect.width() - renderer.padding_x() * 2.0;
    let usable_h = rect.height() - renderer.padding_y() * 2.0;
    let cols = (usable_w / renderer.cell_width).floor().max(2.0) as usize;
    let rows = (usable_h / renderer.cell_height).floor().max(1.0) as usize;
    (cols, rows)
}

// ── Layout tree ─────────────────────────────────────────────────────────

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
        match self {
            LayoutNode::Leaf(pane) => {
                let rect = pane.viewport;
                pane.resize_to_viewport(rect, renderer);
            }
            LayoutNode::Split { first, second, .. } => {
                first.resize_all_panes(renderer);
                second.resize_all_panes(renderer);
            }
            LayoutNode::Empty => {}
        }
    }

    /// Collect all pane IDs in order (left-to-right / top-to-bottom).
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        self.collect_ids(&mut ids);
        ids
    }

    fn collect_ids(&self, ids: &mut Vec<PaneId>) {
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
    pub fn find_pane(&self, id: PaneId) -> Option<&PaneState> {
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
    pub fn find_pane_mut(&mut self, id: PaneId) -> Option<&mut PaneState> {
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
        target_id: PaneId,
        direction: SplitDirection,
        spawn_fn: F,
    ) -> Option<PaneId>
    where
        F: FnOnce(Rect) -> Option<PaneState>,
    {
        let mut slot = Some(spawn_fn);
        self.split_pane_impl(target_id, direction, &mut slot)
    }

    fn split_pane_impl<F>(
        &mut self,
        target_id: PaneId,
        direction: SplitDirection,
        spawn_fn: &mut Option<F>,
    ) -> Option<PaneId>
    where
        F: FnOnce(Rect) -> Option<PaneState>,
    {
        match self {
            LayoutNode::Leaf(pane) if pane.id == target_id => {
                let factory = spawn_fn.take().expect("spawn_fn already consumed");
                let rect = pane.viewport;
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
                    return result;
                }
                second.split_pane_impl(target_id, direction, spawn_fn)
            }
            LayoutNode::Empty => None,
        }
    }

    /// Remove a pane by ID. Returns true if found and removed.
    /// After removal the sibling takes the parent's position.
    /// Returns None if the pane is the only one (root leaf).
    pub fn remove_pane(&mut self, target_id: PaneId) -> Option<bool> {
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
                    return Some(true);
                }

                if second_is_target {
                    let sibling = std::mem::replace(first.as_mut(), LayoutNode::Empty);
                    *self = sibling;
                    return Some(true);
                }

                // Recurse.
                let removed = first.remove_pane(target_id);
                if removed == Some(true) {
                    return Some(true);
                }
                second.remove_pane(target_id)
            }
            LayoutNode::Empty => Some(false),
        }
    }

    /// Find the next pane ID in the given direction relative to the focused pane.
    pub fn adjacent_pane(&self, focused_id: PaneId, direction: FocusDirection) -> Option<PaneId> {
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

    /// Adjust the split ratio for the split containing the focused pane.
    pub fn adjust_ratio(&mut self, focused_id: PaneId, delta: f32) -> bool {
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
}

/// Direction for focus navigation.
#[derive(Debug, Clone, Copy)]
pub enum FocusDirection {
    Next,
    Prev,
}

// ── Pane spawning ───────────────────────────────────────────────────────

/// Counter for generating unique pane IDs.
static NEXT_PANE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_pane_id() -> PaneId {
    NEXT_PANE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Spawn a new pane with its own PTY, Term, and reader thread.
///
/// `proxy_fn` is called with the pane_id to create a wake callback that
/// notifies the event loop when new PTY data arrives.
///
/// `interceptor_config` controls which OSC sequence families are intercepted.
/// `scan_interval_secs` sets the process-detector scan interval (0 = disabled).
pub fn spawn_pane<F>(
    viewport: Rect,
    renderer: &GridRenderer,
    scrollback_lines: usize,
    interceptor_config: therminal_terminal::interceptor::InterceptorConfig,
    scan_interval_secs: u64,
    spawn_options: &therminal_terminal::pty::SpawnOptions,
    proxy_fn: F,
) -> Result<PaneState, anyhow::Error>
where
    F: FnOnce(PaneId) -> Box<dyn Fn() + Send + 'static>,
{
    let id = next_pane_id();
    let (cols, rows) = grid_size_for_rect(viewport, renderer);
    let cols = cols.max(2);
    let rows = rows.max(1);

    let term_config = TermConfig {
        scrolling_history: scrollback_lines,
        ..Default::default()
    };
    let term_size = PaneTermSize {
        columns: cols,
        screen_lines: rows,
    };
    let term = Term::new(term_config, &term_size, PaneListener);
    let term = Arc::new(FairMutex::new(term));

    let (pty_master, _child) =
        therminal_terminal::pty::spawn_shell_with_options(cols as u16, rows as u16, spawn_options)
            .map_err(|e| anyhow::anyhow!("failed to spawn shell for pane: {e}"))?;

    let pty_reader = pty_master
        .try_clone_reader()
        .map_err(|e| anyhow::anyhow!("failed to clone PTY reader for pane: {e}"))?;
    let pty_writer = pty_master
        .take_writer()
        .map_err(|e| anyhow::anyhow!("failed to get PTY writer for pane: {e}"))?;

    // Spawn PTY reader thread for this pane.
    let term_for_reader = Arc::clone(&term);
    let wake = proxy_fn(id);
    thread::Builder::new()
        .name(format!("pty-reader-{id}"))
        .spawn(move || {
            pane_pty_reader_loop(
                pty_reader,
                term_for_reader,
                wake,
                interceptor_config,
                scan_interval_secs,
            );
        })
        .map_err(|e| anyhow::anyhow!("failed to spawn pane PTY reader thread: {e}"))?;

    info!(pane_id = id, cols, rows, "Pane spawned");

    Ok(PaneState {
        id,
        term,
        pty_writer,
        pty_master,
        viewport,
        scrollback_lines,
    })
}

/// PTY reader loop for a single pane.
fn pane_pty_reader_loop(
    mut reader: Box<dyn IoRead + Send>,
    term: Arc<FairMutex<Term<PaneListener>>>,
    wake: Box<dyn Fn() + Send + 'static>,
    interceptor_config: therminal_terminal::interceptor::InterceptorConfig,
    scan_interval_secs: u64,
) {
    use std::time::Duration;

    use therminal_terminal::interceptor::TherminalInterceptor;
    use therminal_terminal::process_detector::ProcessDetector;

    let mut processor = ansi::Processor::<ansi::StdSyncHandler>::new();
    let (mut interceptor, _event_rx) = TherminalInterceptor::new(interceptor_config);

    // Build process detector; 0 = disabled (interval set to 0 yields instant rescans,
    // so we gate on the configured value before constructing).
    let scan_interval = if scan_interval_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(scan_interval_secs))
    };
    let mut process_detector =
        scan_interval.map(|interval| ProcessDetector::new(None).with_interval(interval));

    let mut buf = [0u8; 4096];

    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                info!("Pane PTY closed (EOF)");
                break;
            }
            Ok(n) => {
                {
                    let mut term_guard = term.lock();
                    processor.advance_with_interceptor(
                        &mut *term_guard,
                        &mut interceptor,
                        &buf[..n],
                    );
                }
                // Run process-tree scan if enabled and interval has elapsed.
                if let Some(ref mut detector) = process_detector {
                    if let Some(agents) = detector.scan_if_due() {
                        if !agents.is_empty() {
                            tracing::debug!("detected agents: {:?}", agents);
                        }
                    }
                }
                wake();
            }
            Err(e) => {
                warn!("Pane PTY read error: {e}");
                break;
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use therminal_core::geometry::Rect;

    /// Helper to create a minimal test leaf node (no real PTY).
    fn test_leaf(id: PaneId, rect: Rect) -> LayoutNode {
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
}
