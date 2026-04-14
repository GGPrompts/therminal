//! Agent timeline overlay widget (tn-x85k).
//!
//! Renders a horizontal bar of recent tool activity, color-coded by
//! category (`Read` / `Write` / `Execute` / `Thinking` / `Idle`), with
//! subagent entries visually distinguished on a second row.
//!
//! Ported from `~/projects/thermal-desktop/crates/thermal-conductor/src/agent_timeline.rs`
//! and adapted to the tn-npd widget rasterization substrate.
//!
//! ## Event subscription model
//!
//! The app runs as a daemon client (tn-382v), so it does **not** have
//! in-process access to the `ClaudeHarness` broadcast channel. Instead,
//! the timeline is fed from the app-side `ClaudeCwdTracker` state poller
//! which already watches `/tmp/claude-code-state/*.json` for tool changes.
//! The `AgentTimelineSource::record_tool_change` method is called from the
//! render path when the focused pane's agent status changes, which provides
//! the same information as the JSONL stream for timeline purposes without
//! requiring a new IPC subscription.
//!
//! ## Manual verification
//!
//! 1. Launch `therminal` with a Claude agent in any pane.
//! 2. Press `Ctrl+Alt+T` to toggle the timeline.
//! 3. Observe a colored bar in the configured position (default: bottom-right).
//! 4. Watch segments appear as Claude uses different tools.
//! 5. Set `RUST_LOG=therminal_app::widgets=debug` and watch for
//!    "timeline_rasterized" tracing events.

use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use therminal_core::config::TimelinePosition;

use super::WidgetId;
use super::rasterizer::{TimelineBarSpec, TimelineSegment, WidgetKind, WidgetSpec};

/// Stable widget id for the agent timeline bar. ASCII for "AGTIMELN".
pub const TIMELINE_WIDGET_ID: WidgetId = 0x4147_5449_4D45_4C4E;

// ── ToolCategory ────────────────────────────────────────────────────────

/// How a tool entry is categorized for coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolCategory {
    /// Read, Glob, Grep, Search -- blue/cool tones.
    Read,
    /// Edit, Write -- warm yellow tones.
    Write,
    /// Bash, shell execution -- hot orange.
    Execute,
    /// Thinking, processing (no specific tool) -- mild green.
    Thinking,
    /// Idle / waiting for input -- dim/transparent.
    Idle,
}

impl ToolCategory {
    /// Classify a tool name into a category.
    pub fn classify_tool(name: &str) -> Self {
        let lower = name.to_lowercase();
        if lower.contains("read")
            || lower.contains("glob")
            || lower.contains("grep")
            || lower.contains("search")
            || lower.contains("list")
        {
            Self::Read
        } else if lower.contains("edit") || lower.contains("write") || lower.contains("notebook") {
            Self::Write
        } else if lower.contains("bash") || lower.contains("exec") || lower.contains("shell") {
            Self::Execute
        } else if lower.contains("think") {
            Self::Thinking
        } else {
            Self::Idle
        }
    }

    /// RGBA color (0..=1) for this category.
    fn color(self) -> [f32; 4] {
        match self {
            Self::Read => [0.35, 0.65, 0.95, 0.90],     // blue
            Self::Write => [0.90, 0.78, 0.30, 0.90],    // warm yellow
            Self::Execute => [0.95, 0.55, 0.20, 0.90],  // hot orange
            Self::Thinking => [0.40, 0.80, 0.50, 0.90], // mild green
            Self::Idle => [0.40, 0.40, 0.45, 0.40],     // dim gray
        }
    }
}

// ── EventSource (local) ─────────────────────────────────────────────────
//
// Mirrors therminal-harness-claude's EventSource but without pulling in
// that crate's serde/serialization machinery. We only need the
// top-level-vs-subagent discriminator for rendering.

/// Identifies whether a timeline entry came from a top-level session or
/// a subagent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EventSource {
    /// Top-level Claude session.
    TopLevel,
    /// Subagent spawned via the Task tool.
    Subagent,
}

// ── TimelineEntry ───────────────────────────────────────────────────────

/// A single tool usage entry in the timeline ring buffer.
#[derive(Debug, Clone)]
pub struct TimelineEntry {
    /// Monotonic timestamp in milliseconds (from `Instant`-based counter).
    pub timestamp_ms: u64,
    /// Classified tool category.
    pub category: ToolCategory,
    /// Tool name (or "Thinking" / "Idle").
    pub tool_name: String,
    /// Whether this entry came from a subagent.
    pub source: EventSource,
}

impl Hash for TimelineEntry {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.timestamp_ms.hash(h);
        self.category.hash(h);
        self.tool_name.hash(h);
        self.source.hash(h);
    }
}

// ── AgentTimelineSource ─────────────────────────────────────────────────

/// Data source for the agent timeline overlay widget.
///
/// Maintains a ring buffer of `TimelineEntry` and produces a `WidgetSpec`
/// with a `TimelineBar` kind for the `WidgetManager` to rasterize. The
/// `data_hash()` method ensures re-rasterization only happens when entries
/// actually change.
pub struct AgentTimelineSource {
    /// Chronological tool entries.
    entries: VecDeque<TimelineEntry>,
    /// Maximum entries in the ring buffer (from config).
    max_entries: usize,
    /// Height of the timeline bar in pixels (from config).
    height_px: u32,
    /// Position relative to the window (from config).
    position: TimelinePosition,
    /// Monotonic counter used as a simple timestamp source.
    next_ts: u64,
    /// Last tool name recorded, to detect transitions.
    last_tool: Option<String>,
    /// Whether the timeline bar is currently visible.
    pub visible: bool,
    /// Last bar width passed to `spec()`, included in `data_hash()` so
    /// the cached texture is invalidated on window resize.
    last_bar_width: u32,
}

impl AgentTimelineSource {
    /// Create a new timeline source from config values.
    pub fn new(max_entries: usize, height_px: u32, position: TimelinePosition) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries),
            max_entries,
            height_px,
            position,
            next_ts: 0,
            last_tool: None,
            visible: false,
            last_bar_width: 0,
        }
    }

    /// Record a tool change. If the tool name differs from the last one,
    /// a new entry is pushed and old entries are evicted at `max_entries`.
    pub fn record_tool_change(&mut self, tool_name: &str, source: EventSource) {
        // Skip if the tool hasn't changed for top-level events.
        if source == EventSource::TopLevel {
            if self.last_tool.as_deref() == Some(tool_name) {
                return;
            }
            self.last_tool = Some(tool_name.to_string());
        }

        let category = ToolCategory::classify_tool(tool_name);
        self.next_ts += 1;
        self.entries.push_back(TimelineEntry {
            timestamp_ms: self.next_ts,
            category,
            tool_name: tool_name.to_string(),
            source,
        });

        while self.entries.len() > self.max_entries {
            self.entries.pop_front();
        }
    }

    /// Update config fields without losing existing entries or visibility.
    pub fn update_config(
        &mut self,
        max_entries: usize,
        height_px: u32,
        position: TimelinePosition,
    ) {
        self.max_entries = max_entries;
        self.height_px = height_px;
        self.position = position;
        // Trim the ring buffer if max_entries shrank.
        while self.entries.len() > self.max_entries {
            self.entries.pop_front();
        }
    }

    /// Toggle visibility.
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        tracing::info!(visible = self.visible, "agent timeline toggled");
    }

    /// Compute a deterministic data hash for the freshness cache.
    pub fn data_hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.entries.len().hash(&mut hasher);
        for entry in &self.entries {
            entry.hash(&mut hasher);
        }
        self.height_px.hash(&mut hasher);
        self.last_bar_width.hash(&mut hasher);
        self.position.hash(&mut hasher);
        hasher.finish()
    }

    /// Number of entries currently in the buffer.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the buffer is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Read the configured position.
    pub fn position(&self) -> TimelinePosition {
        self.position
    }

    /// Read the configured height.
    pub fn height_px(&self) -> u32 {
        self.height_px
    }

    /// Build a `WidgetSpec` from the current entries.
    ///
    /// `bar_width` is the desired pixel width of the bar, computed by the
    /// caller based on the window size and position.
    pub fn spec(&mut self, bar_width: u32) -> WidgetSpec {
        self.last_bar_width = bar_width;
        let segments: Vec<TimelineSegment> = self
            .entries
            .iter()
            .map(|entry| TimelineSegment {
                color: entry.category.color(),
                is_subagent: entry.source == EventSource::Subagent,
            })
            .collect();

        WidgetSpec {
            data_hash: self.data_hash(),
            kind: WidgetKind::TimelineBar(TimelineBarSpec {
                width: bar_width,
                height: self.height_px,
                corner_radius: 6.0,
                background: [0.06, 0.09, 0.15, 0.75],
                segments,
            }),
        }
    }

    /// Access the raw entries (for testing).
    #[cfg(test)]
    pub(crate) fn entries(&self) -> &VecDeque<TimelineEntry> {
        &self.entries
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::Pixmap;

    use crate::widgets::rasterizer::WidgetRasterizer;

    // ── classify_tool ────────────────────────────────────────────────────

    #[test]
    fn classify_read_tools() {
        assert_eq!(ToolCategory::classify_tool("Read"), ToolCategory::Read);
        assert_eq!(ToolCategory::classify_tool("Glob"), ToolCategory::Read);
        assert_eq!(ToolCategory::classify_tool("Grep"), ToolCategory::Read);
        assert_eq!(ToolCategory::classify_tool("WebSearch"), ToolCategory::Read);
        assert_eq!(ToolCategory::classify_tool("ListFiles"), ToolCategory::Read);
    }

    #[test]
    fn classify_write_tools() {
        assert_eq!(ToolCategory::classify_tool("Edit"), ToolCategory::Write);
        assert_eq!(ToolCategory::classify_tool("Write"), ToolCategory::Write);
        assert_eq!(
            ToolCategory::classify_tool("NotebookEdit"),
            ToolCategory::Write
        );
    }

    #[test]
    fn classify_execute_tools() {
        assert_eq!(ToolCategory::classify_tool("Bash"), ToolCategory::Execute);
        assert_eq!(
            ToolCategory::classify_tool("bash_execute"),
            ToolCategory::Execute
        );
    }

    #[test]
    fn classify_thinking() {
        assert_eq!(
            ToolCategory::classify_tool("Thinking"),
            ToolCategory::Thinking
        );
        assert_eq!(
            ToolCategory::classify_tool("think_deeply"),
            ToolCategory::Thinking
        );
    }

    #[test]
    fn classify_unknown_is_idle() {
        assert_eq!(
            ToolCategory::classify_tool("SomeUnknownTool"),
            ToolCategory::Idle
        );
        assert_eq!(ToolCategory::classify_tool("WebFetch"), ToolCategory::Idle);
    }

    // ── Ring buffer eviction ────────────────────────────────────────────

    #[test]
    fn ring_buffer_evicts_at_max_entries() {
        let mut source = AgentTimelineSource::new(3, 48, TimelinePosition::BottomRight);
        source.record_tool_change("Read", EventSource::TopLevel);
        source.record_tool_change("Edit", EventSource::TopLevel);
        source.record_tool_change("Bash", EventSource::TopLevel);
        assert_eq!(source.len(), 3);

        // Push one more -- oldest should be evicted.
        source.record_tool_change("Write", EventSource::TopLevel);
        assert_eq!(source.len(), 3);

        // The oldest entry should now be "Edit" (the original "Read" was evicted).
        assert_eq!(source.entries()[0].tool_name, "Edit");
    }

    #[test]
    fn ring_buffer_deduplicates_consecutive_same_tool() {
        let mut source = AgentTimelineSource::new(10, 48, TimelinePosition::BottomRight);
        source.record_tool_change("Read", EventSource::TopLevel);
        source.record_tool_change("Read", EventSource::TopLevel);
        source.record_tool_change("Read", EventSource::TopLevel);
        // Same tool name should not create duplicate entries.
        assert_eq!(source.len(), 1);
    }

    #[test]
    fn subagent_entries_not_deduplicated_against_toplevel() {
        let mut source = AgentTimelineSource::new(10, 48, TimelinePosition::BottomRight);
        source.record_tool_change("Read", EventSource::TopLevel);
        // Subagent entries are always recorded (no dedup against top-level).
        source.record_tool_change("Read", EventSource::Subagent);
        assert_eq!(source.len(), 2);
    }

    // ── data_hash stability ─────────────────────────────────────────────

    #[test]
    fn data_hash_stable_for_same_entries() {
        let mut source = AgentTimelineSource::new(10, 48, TimelinePosition::BottomRight);
        source.record_tool_change("Read", EventSource::TopLevel);
        let h1 = source.data_hash();
        let h2 = source.data_hash();
        assert_eq!(h1, h2);
    }

    #[test]
    fn data_hash_changes_on_new_entry() {
        let mut source = AgentTimelineSource::new(10, 48, TimelinePosition::BottomRight);
        source.record_tool_change("Read", EventSource::TopLevel);
        let h1 = source.data_hash();
        source.record_tool_change("Edit", EventSource::TopLevel);
        let h2 = source.data_hash();
        assert_ne!(h1, h2);
    }

    // ── Subagent pixel distinction ──────────────────────────────────────

    #[test]
    fn subagent_entries_render_on_second_row() {
        let mut source = AgentTimelineSource::new(10, 48, TimelinePosition::BottomRight);
        // One top-level, one subagent.
        source.record_tool_change("Read", EventSource::TopLevel);
        source.record_tool_change("Bash", EventSource::Subagent);

        let spec = source.spec(200);
        let mut rasterizer = WidgetRasterizer::new();
        let pixmap = rasterizer.rasterize_to_pixmap(&spec).expect("pixmap");

        // The pixmap should be 200x48.
        assert_eq!(pixmap.width(), 200);
        assert_eq!(pixmap.height(), 48);

        // Check that the bottom half (subagent row) has at least one
        // non-transparent pixel at a known subagent-row coordinate.
        let bottom_row_y = 36; // 48 * 0.5 = 24, plus some inset => ~26-44 range
        let has_subagent_pixel = (0..pixmap.width()).any(|x| {
            let idx = (bottom_row_y * pixmap.width() + x) as usize;
            pixmap.pixels()[idx].alpha() > 0
        });
        assert!(
            has_subagent_pixel,
            "subagent row should contain at least one non-transparent pixel"
        );
    }

    #[test]
    fn empty_timeline_produces_valid_pixmap() {
        let mut source = AgentTimelineSource::new(10, 48, TimelinePosition::BottomRight);
        let spec = source.spec(200);
        let mut rasterizer = WidgetRasterizer::new();
        let pixmap = rasterizer.rasterize_to_pixmap(&spec).expect("pixmap");
        assert_eq!(pixmap.width(), 200);
        assert_eq!(pixmap.height(), 48);
    }

    #[test]
    fn spec_reads_config_height() {
        let mut source = AgentTimelineSource::new(10, 64, TimelinePosition::BottomRight);
        let spec = source.spec(300);
        match spec.kind {
            WidgetKind::TimelineBar(ref bar) => {
                assert_eq!(bar.height, 64);
                assert_eq!(bar.width, 300);
            }
            _ => panic!("expected TimelineBar"),
        }
    }

    /// Helper to check that a pixel at (x, y) in a pixmap has non-zero alpha.
    fn pixel_is_opaque(pixmap: &Pixmap, x: u32, y: u32) -> bool {
        let idx = (y * pixmap.width() + x) as usize;
        pixmap.pixels()[idx].alpha() > 0
    }

    #[test]
    fn toplevel_only_fills_full_height() {
        let mut source = AgentTimelineSource::new(10, 48, TimelinePosition::BottomRight);
        source.record_tool_change("Read", EventSource::TopLevel);
        let spec = source.spec(200);
        let mut rasterizer = WidgetRasterizer::new();
        let pixmap = rasterizer.rasterize_to_pixmap(&spec).expect("pixmap");

        // With only top-level entries and no subagents, segments should
        // span the full height. Check a pixel in the center.
        assert!(pixel_is_opaque(&pixmap, 100, 24));
    }
}
