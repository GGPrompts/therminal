//! In-memory semantic region index.
//!
//! Turns [`InterceptedEvent`]s from the sequence interceptor into a queryable,
//! typed index of terminal output regions. Each region represents a semantic
//! segment of the terminal scrollback (prompt, command, output, error, etc.).
//!
//! This is the foundation for MCP `query_semantic_history` and semantic
//! scrollback navigation.

use std::collections::HashMap;
use std::time::Instant;

use crate::interceptor::InterceptedEvent;
use crate::osc633::Osc633Mark;

// -- Region types ------------------------------------------------------------

/// The semantic kind of a terminal region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegionKind {
    /// Shell prompt text (OSC 133/633 A -> B).
    Prompt,
    /// User-typed command (OSC 633 B -> C).
    Command,
    /// Command output (OSC 633 C -> D).
    Output,
    /// Failed command output (exit_code != 0).
    Error,
    /// Agent tool invocation (detected from state_inference).
    ToolCall,
    /// Agent thinking/processing output.
    Thinking,
    /// Metadata (working directory, iTerm2 marks).
    Annotation,
}

/// A semantic region of terminal output.
#[derive(Debug, Clone)]
pub struct Region {
    /// What kind of content this region contains.
    pub kind: RegionKind,
    /// The terminal line where this region starts.
    pub start_line: usize,
    /// The terminal line where this region ends. `None` if still open.
    pub end_line: Option<usize>,
    /// When this region was created.
    pub timestamp: Instant,
    /// Arbitrary metadata: exit_code, cwd, command text, etc.
    pub metadata: HashMap<String, String>,
}

// -- RegionIndex -------------------------------------------------------------

/// Queryable index of semantic regions built from intercepted events.
///
/// Events are pushed in order; the index maintains a list of regions and
/// tracks which region is currently open (i.e. `end_line` is `None`).
#[derive(Debug, Default)]
pub struct RegionIndex {
    /// All regions, oldest first.
    regions: Vec<Region>,
    /// The current terminal line (set externally as the terminal scrolls).
    current_line: usize,
}

impl RegionIndex {
    /// Create a new, empty region index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the current line position. Call this before `push_event` when
    /// the terminal cursor has moved to a new line.
    pub fn set_current_line(&mut self, line: usize) {
        self.current_line = line;
    }

    /// Process an intercepted event, opening and closing regions as needed.
    pub fn push_event(&mut self, event: &InterceptedEvent) {
        match event {
            InterceptedEvent::Osc633(mark) | InterceptedEvent::Osc133(mark) => {
                self.apply_mark(mark);
            }
            InterceptedEvent::CurrentDirectory(path) => {
                let mut metadata = HashMap::new();
                metadata.insert("cwd".to_string(), path.clone());
                self.regions.push(Region {
                    kind: RegionKind::Annotation,
                    start_line: self.current_line,
                    end_line: Some(self.current_line),
                    timestamp: Instant::now(),
                    metadata,
                });
            }
            InterceptedEvent::Iterm2 { key, value } => {
                let mut metadata = HashMap::new();
                metadata.insert("key".to_string(), key.clone());
                metadata.insert("value".to_string(), value.clone());
                self.regions.push(Region {
                    kind: RegionKind::Annotation,
                    start_line: self.current_line,
                    end_line: Some(self.current_line),
                    timestamp: Instant::now(),
                    metadata,
                });
            }
            InterceptedEvent::AgentReport {
                agent,
                state,
                tool,
                tokens,
                model,
            } => {
                let mut metadata = HashMap::new();
                metadata.insert("agent".to_string(), agent.clone());
                if let Some(s) = state {
                    metadata.insert("state".to_string(), s.clone());
                }
                if let Some(t) = tool {
                    metadata.insert("tool".to_string(), t.clone());
                }
                if let Some(tk) = tokens {
                    metadata.insert("tokens".to_string(), tk.to_string());
                }
                if let Some(m) = model {
                    metadata.insert("model".to_string(), m.clone());
                }
                self.regions.push(Region {
                    kind: RegionKind::Annotation,
                    start_line: self.current_line,
                    end_line: Some(self.current_line),
                    timestamp: Instant::now(),
                    metadata,
                });
            }
            InterceptedEvent::DesktopNotification(_) => {
                // Desktop notifications don't map to semantic regions.
            }
        }
    }

    /// Map an OSC 633/133 mark to region open/close transitions.
    fn apply_mark(&mut self, mark: &Osc633Mark) {
        match mark {
            Osc633Mark::PromptStart => {
                // A: open a Prompt region.
                self.regions.push(Region {
                    kind: RegionKind::Prompt,
                    start_line: self.current_line,
                    end_line: None,
                    timestamp: Instant::now(),
                    metadata: HashMap::new(),
                });
            }
            Osc633Mark::PromptEnd => {
                // B: close the Prompt region, open a Command region.
                self.close_current(RegionKind::Prompt);
                self.regions.push(Region {
                    kind: RegionKind::Command,
                    start_line: self.current_line,
                    end_line: None,
                    timestamp: Instant::now(),
                    metadata: HashMap::new(),
                });
            }
            Osc633Mark::PreExec => {
                // C: close the Command region, open an Output region.
                self.close_current(RegionKind::Command);
                self.regions.push(Region {
                    kind: RegionKind::Output,
                    start_line: self.current_line,
                    end_line: None,
                    timestamp: Instant::now(),
                    metadata: HashMap::new(),
                });
            }
            Osc633Mark::CommandFinished { exit_code } => {
                // D: close the Output region. If exit_code != 0, convert to Error.
                let is_error = exit_code.map(|c| c != 0).unwrap_or(false);
                let line = self.current_line;

                if let Some(region) = self.find_open_mut(RegionKind::Output) {
                    region.end_line = Some(line);
                    if is_error {
                        region.kind = RegionKind::Error;
                    }
                    if let Some(code) = exit_code {
                        region
                            .metadata
                            .insert("exit_code".to_string(), code.to_string());
                    }
                }
            }
            Osc633Mark::CommandLine { command } => {
                // E: attach command text to the current Command region.
                if let Some(region) = self.find_open_mut(RegionKind::Command) {
                    region
                        .metadata
                        .insert("command".to_string(), command.clone());
                }
            }
        }
    }

    /// Close the most recent open region of the given kind.
    fn close_current(&mut self, kind: RegionKind) {
        let line = self.current_line;
        if let Some(region) = self.find_open_mut(kind) {
            region.end_line = Some(line);
        }
    }

    /// Find the most recent open (end_line == None) region of a given kind.
    fn find_open_mut(&mut self, kind: RegionKind) -> Option<&mut Region> {
        self.regions
            .iter_mut()
            .rev()
            .find(|r| r.kind == kind && r.end_line.is_none())
    }

    // -- Query API -----------------------------------------------------------

    /// Return all regions of a given kind.
    pub fn query_by_kind(&self, kind: RegionKind) -> Vec<&Region> {
        self.regions.iter().filter(|r| r.kind == kind).collect()
    }

    /// Find the region that contains a given terminal line.
    ///
    /// Returns the most specific (most recent) region whose range includes
    /// the line. For open regions, any line >= start_line matches.
    pub fn query_by_line(&self, line: usize) -> Option<&Region> {
        self.regions.iter().rev().find(|r| {
            let start = r.start_line;
            match r.end_line {
                Some(end) => line >= start && line <= end,
                None => line >= start,
            }
        })
    }

    /// Return the most recent N regions (newest first).
    pub fn last_n(&self, n: usize) -> Vec<&Region> {
        self.regions.iter().rev().take(n).collect()
    }

    /// Return the currently open region (end_line is None), if any.
    ///
    /// If multiple regions are open, returns the most recently opened one.
    pub fn current_region(&self) -> Option<&Region> {
        self.regions.iter().rev().find(|r| r.end_line.is_none())
    }

    /// Total number of regions in the index.
    pub fn len(&self) -> usize {
        self.regions.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// Immutable access to all regions (oldest first).
    pub fn regions(&self) -> &[Region] {
        &self.regions
    }

    /// Find the nearest region whose `start_line` is strictly before `line`.
    ///
    /// If `kinds` is non-empty, only regions matching one of the listed kinds
    /// are considered. Returns the closest such region, or `None`.
    pub fn region_before(&self, line: usize, kinds: &[RegionKind]) -> Option<&Region> {
        self.regions
            .iter()
            .rev()
            .filter(|r| kinds.is_empty() || kinds.contains(&r.kind))
            .find(|r| r.start_line < line)
    }

    /// Find the nearest region whose `start_line` is strictly after `line`.
    ///
    /// If `kinds` is non-empty, only regions matching one of the listed kinds
    /// are considered. Returns the closest such region, or `None`.
    pub fn region_after(&self, line: usize, kinds: &[RegionKind]) -> Option<&Region> {
        self.regions
            .iter()
            .filter(|r| kinds.is_empty() || kinds.contains(&r.kind))
            .find(|r| r.start_line > line)
    }
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build an Osc633 event from a mark.
    fn osc633(mark: Osc633Mark) -> InterceptedEvent {
        InterceptedEvent::Osc633(mark)
    }

    #[test]
    fn full_command_lifecycle() {
        let mut idx = RegionIndex::new();

        // A: prompt starts at line 0
        idx.set_current_line(0);
        idx.push_event(&osc633(Osc633Mark::PromptStart));
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.current_region().unwrap().kind, RegionKind::Prompt);

        // B: prompt ends, command starts at line 0
        idx.push_event(&osc633(Osc633Mark::PromptEnd));
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.current_region().unwrap().kind, RegionKind::Command);
        // Prompt region should be closed
        assert!(idx.regions[0].end_line.is_some());

        // E: command text
        idx.push_event(&osc633(Osc633Mark::CommandLine {
            command: "ls -la".to_string(),
        }));
        assert_eq!(
            idx.current_region().unwrap().metadata.get("command"),
            Some(&"ls -la".to_string())
        );

        // C: command ends, output starts at line 1
        idx.set_current_line(1);
        idx.push_event(&osc633(Osc633Mark::PreExec));
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.current_region().unwrap().kind, RegionKind::Output);
        // Command region should be closed
        assert!(idx.regions[1].end_line.is_some());

        // D: output ends at line 5, exit_code 0
        idx.set_current_line(5);
        idx.push_event(&osc633(Osc633Mark::CommandFinished { exit_code: Some(0) }));
        // Output region should be closed
        assert!(idx.current_region().is_none());
        assert_eq!(idx.regions[2].kind, RegionKind::Output);
        assert_eq!(idx.regions[2].end_line, Some(5));
    }

    #[test]
    fn error_detection() {
        let mut idx = RegionIndex::new();

        idx.set_current_line(0);
        idx.push_event(&osc633(Osc633Mark::PromptStart));
        idx.push_event(&osc633(Osc633Mark::PromptEnd));

        idx.set_current_line(1);
        idx.push_event(&osc633(Osc633Mark::PreExec));

        // Command fails with exit code 1
        idx.set_current_line(3);
        idx.push_event(&osc633(Osc633Mark::CommandFinished { exit_code: Some(1) }));

        // The output region should have been converted to Error
        let errors = idx.query_by_kind(RegionKind::Error);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].metadata.get("exit_code"), Some(&"1".to_string()));

        // No Output regions should remain (it was converted)
        let outputs = idx.query_by_kind(RegionKind::Output);
        assert_eq!(outputs.len(), 0);
    }

    #[test]
    fn query_by_kind_filtering() {
        let mut idx = RegionIndex::new();

        // Two full command cycles
        for line_base in [0, 10] {
            idx.set_current_line(line_base);
            idx.push_event(&osc633(Osc633Mark::PromptStart));
            idx.push_event(&osc633(Osc633Mark::PromptEnd));
            idx.set_current_line(line_base + 1);
            idx.push_event(&osc633(Osc633Mark::PreExec));
            idx.set_current_line(line_base + 5);
            idx.push_event(&osc633(Osc633Mark::CommandFinished { exit_code: Some(0) }));
        }

        assert_eq!(idx.query_by_kind(RegionKind::Prompt).len(), 2);
        assert_eq!(idx.query_by_kind(RegionKind::Command).len(), 2);
        assert_eq!(idx.query_by_kind(RegionKind::Output).len(), 2);
        assert_eq!(idx.query_by_kind(RegionKind::Error).len(), 0);
    }

    #[test]
    fn query_by_line_lookup() {
        let mut idx = RegionIndex::new();

        idx.set_current_line(0);
        idx.push_event(&osc633(Osc633Mark::PromptStart));
        idx.push_event(&osc633(Osc633Mark::PromptEnd));
        idx.set_current_line(1);
        idx.push_event(&osc633(Osc633Mark::PreExec));
        idx.set_current_line(5);
        idx.push_event(&osc633(Osc633Mark::CommandFinished { exit_code: Some(0) }));

        // Line 0 is in the prompt or command region
        let r = idx.query_by_line(0).unwrap();
        assert!(r.kind == RegionKind::Prompt || r.kind == RegionKind::Command);

        // Line 3 is in the output region
        let r = idx.query_by_line(3).unwrap();
        assert_eq!(r.kind, RegionKind::Output);

        // Line 100 is outside all regions
        assert!(idx.query_by_line(100).is_none());
    }

    #[test]
    fn current_region_returns_open() {
        let mut idx = RegionIndex::new();

        assert!(idx.current_region().is_none());

        idx.set_current_line(0);
        idx.push_event(&osc633(Osc633Mark::PromptStart));
        assert_eq!(idx.current_region().unwrap().kind, RegionKind::Prompt);

        idx.push_event(&osc633(Osc633Mark::PromptEnd));
        assert_eq!(idx.current_region().unwrap().kind, RegionKind::Command);

        idx.set_current_line(1);
        idx.push_event(&osc633(Osc633Mark::PreExec));
        assert_eq!(idx.current_region().unwrap().kind, RegionKind::Output);

        idx.set_current_line(5);
        idx.push_event(&osc633(Osc633Mark::CommandFinished { exit_code: Some(0) }));
        assert!(idx.current_region().is_none());
    }

    #[test]
    fn last_n_returns_newest_first() {
        let mut idx = RegionIndex::new();

        idx.set_current_line(0);
        idx.push_event(&osc633(Osc633Mark::PromptStart));
        idx.push_event(&osc633(Osc633Mark::PromptEnd));
        idx.set_current_line(1);
        idx.push_event(&osc633(Osc633Mark::PreExec));
        idx.set_current_line(5);
        idx.push_event(&osc633(Osc633Mark::CommandFinished { exit_code: Some(0) }));

        let last2 = idx.last_n(2);
        assert_eq!(last2.len(), 2);
        assert_eq!(last2[0].kind, RegionKind::Output);
        assert_eq!(last2[1].kind, RegionKind::Command);
    }

    #[test]
    fn annotation_from_cwd() {
        let mut idx = RegionIndex::new();
        idx.set_current_line(0);
        idx.push_event(&InterceptedEvent::CurrentDirectory(
            "/home/user".to_string(),
        ));

        assert_eq!(idx.len(), 1);
        let r = &idx.regions[0];
        assert_eq!(r.kind, RegionKind::Annotation);
        assert_eq!(r.metadata.get("cwd"), Some(&"/home/user".to_string()));
        // Annotation regions are immediately closed
        assert_eq!(r.end_line, Some(0));
    }

    #[test]
    fn annotation_from_iterm2() {
        let mut idx = RegionIndex::new();
        idx.set_current_line(0);
        idx.push_event(&InterceptedEvent::Iterm2 {
            key: "CurrentDir".to_string(),
            value: "/tmp".to_string(),
        });

        assert_eq!(idx.len(), 1);
        let r = &idx.regions[0];
        assert_eq!(r.kind, RegionKind::Annotation);
        assert_eq!(r.metadata.get("key"), Some(&"CurrentDir".to_string()));
        assert_eq!(r.metadata.get("value"), Some(&"/tmp".to_string()));
    }

    /// Helper: build a closed region with explicit start/end and kind.
    fn make_region(kind: RegionKind, start: usize, end: usize) -> Region {
        Region {
            kind,
            start_line: start,
            end_line: Some(end),
            timestamp: Instant::now(),
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn region_before_after_empty_index() {
        let idx = RegionIndex::new();
        assert!(idx.region_before(10, &[]).is_none());
        assert!(idx.region_after(10, &[]).is_none());
    }

    #[test]
    fn region_before_after_single_region() {
        let mut idx = RegionIndex::new();
        idx.regions.push(make_region(RegionKind::Output, 5, 10));

        // Before its start: region_after should find it; region_before should not.
        assert!(idx.region_before(5, &[]).is_none());
        assert_eq!(idx.region_after(4, &[]).unwrap().start_line, 5);

        // Inside it: start_line is 5; region_before(7) sees start<7 -> finds it.
        assert_eq!(idx.region_before(7, &[]).unwrap().start_line, 5);
        // region_after(7) needs start>7 -> none.
        assert!(idx.region_after(7, &[]).is_none());

        // After its end: region_before finds it, region_after does not.
        assert_eq!(idx.region_before(20, &[]).unwrap().start_line, 5);
        assert!(idx.region_after(20, &[]).is_none());
    }

    #[test]
    fn region_before_after_multiple_adjacent() {
        let mut idx = RegionIndex::new();
        idx.regions.push(make_region(RegionKind::Output, 0, 4));
        idx.regions.push(make_region(RegionKind::Output, 5, 9));
        idx.regions.push(make_region(RegionKind::Output, 10, 14));

        // Cursor at line 7: nearest before = start 5, nearest after = start 10.
        assert_eq!(idx.region_before(7, &[]).unwrap().start_line, 5);
        assert_eq!(idx.region_after(7, &[]).unwrap().start_line, 10);

        // Cursor at line 6: before = 5, after = 10.
        assert_eq!(idx.region_before(6, &[]).unwrap().start_line, 5);
        assert_eq!(idx.region_after(6, &[]).unwrap().start_line, 10);
    }

    #[test]
    fn region_before_after_kind_filter_errors_only() {
        let mut idx = RegionIndex::new();
        idx.regions.push(make_region(RegionKind::Output, 0, 4));
        idx.regions.push(make_region(RegionKind::Error, 5, 9));
        idx.regions.push(make_region(RegionKind::Output, 10, 14));
        idx.regions.push(make_region(RegionKind::Error, 15, 19));
        idx.regions.push(make_region(RegionKind::Output, 20, 24));

        // From line 12, nearest Error before is start 5, nearest Error after is start 15.
        let before = idx.region_before(12, &[RegionKind::Error]).unwrap();
        assert_eq!(before.start_line, 5);
        assert_eq!(before.kind, RegionKind::Error);

        let after = idx.region_after(12, &[RegionKind::Error]).unwrap();
        assert_eq!(after.start_line, 15);
        assert_eq!(after.kind, RegionKind::Error);

        // No Error after the last error.
        assert!(idx.region_after(20, &[RegionKind::Error]).is_none());
        // No Error before the first one.
        assert!(idx.region_before(5, &[RegionKind::Error]).is_none());
    }

    #[test]
    fn region_before_after_boundary_strictness() {
        let mut idx = RegionIndex::new();
        idx.regions.push(make_region(RegionKind::Output, 5, 10));
        idx.regions.push(make_region(RegionKind::Output, 15, 20));

        // Cursor exactly on a region's start_line: strictly-before excludes it,
        // strictly-after also excludes it.
        assert!(idx.region_before(5, &[]).is_none());
        assert_eq!(idx.region_after(5, &[]).unwrap().start_line, 15);

        assert_eq!(idx.region_before(15, &[]).unwrap().start_line, 5);
        assert!(idx.region_after(15, &[]).is_none());
    }

    #[test]
    fn region_before_after_past_last_and_before_first() {
        let mut idx = RegionIndex::new();
        idx.regions.push(make_region(RegionKind::Output, 10, 14));
        idx.regions.push(make_region(RegionKind::Output, 20, 24));

        // Past the last region: before finds the latest, after finds nothing.
        assert_eq!(idx.region_before(100, &[]).unwrap().start_line, 20);
        assert!(idx.region_after(100, &[]).is_none());

        // Before the first region: before finds nothing, after finds the earliest.
        assert!(idx.region_before(0, &[]).is_none());
        assert_eq!(idx.region_after(0, &[]).unwrap().start_line, 10);
    }

    #[test]
    fn osc133_works_same_as_osc633() {
        let mut idx = RegionIndex::new();

        idx.set_current_line(0);
        idx.push_event(&InterceptedEvent::Osc133(Osc633Mark::PromptStart));
        assert_eq!(idx.current_region().unwrap().kind, RegionKind::Prompt);

        idx.push_event(&InterceptedEvent::Osc133(Osc633Mark::PromptEnd));
        assert_eq!(idx.current_region().unwrap().kind, RegionKind::Command);
    }
}
