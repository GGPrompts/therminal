//! OSC 633 shell integration parser.
//!
//! VS Code shell integration protocol (OSC 633) tracks command boundaries so
//! that the terminal can visually segment the scrollback into discrete command
//! blocks.  alacritty_terminal silently drops unknown OSC codes, so we
//! intercept the raw byte stream *before* it reaches the Term and extract the
//! 633 marks ourselves.
//!
//! ## Sequence format
//!
//! ```text
//! ESC ] 633 ; <mark> [ ; <data> ] BEL|ST
//! ```
//!
//! | Mark | Meaning                          | Extra data            |
//! |------|----------------------------------|-----------------------|
//! | A    | Prompt start                     | ---                   |
//! | B    | Prompt end (command input starts) | ---                   |
//! | C    | Pre-execution (command submitted) | ---                   |
//! | D    | Execution finished               | exit code (`i32`)     |
//! | E    | Explicit command line            | command text (`String`) |
//!
//! ## Usage
//!
//! Call [`Osc633Parser::feed`] with every byte chunk from the PTY.  The
//! returned [`Vec<Osc633Mark>`] contains any 633 marks found in that chunk.
//! The bytes are *not* consumed --- the full chunk should still be forwarded to
//! `alacritty_terminal::Term` for normal rendering.

// -- Public types ------------------------------------------------------------

/// A parsed OSC 633 mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc633Mark {
    /// `A` --- prompt output is about to be written.
    PromptStart,
    /// `B` --- prompt has been written; user is now typing.
    PromptEnd,
    /// `C` --- user pressed Enter; command is about to execute.
    PreExec,
    /// `D` --- command finished.  `exit_code` is `None` when the shell did not
    /// provide one.
    CommandFinished { exit_code: Option<i32> },
    /// `E` --- explicit command-line text provided by the shell.
    CommandLine { command: String },
}

/// The state of a tracked command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandState {
    /// Waiting for the first `B` mark after a prompt was started.
    PromptStart,
    /// Prompt written; cursor is in the user-input region.
    Input,
    /// User submitted the command (`C` mark received).
    Executing,
    /// Execution finished (`D` mark received).
    Finished,
}

/// A single command captured in the terminal scrollback.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CommandBlock {
    /// Grid line where the prompt started (`A` mark).
    pub start_line: usize,
    /// Grid line where execution finished (`D` mark). `None` while running.
    pub end_line: Option<usize>,
    /// The command text, populated from an `E` mark or left `None`.
    pub command: Option<String>,
    /// Exit code from the `D` mark.
    pub exit_code: Option<i32>,
    /// Current lifecycle state.
    pub state: CommandState,
}

/// Tracks the sequence of `CommandBlock`s produced by OSC 633 marks.
#[derive(Debug, Default)]
pub struct CommandTracker {
    /// All completed and in-progress blocks, oldest first.
    pub blocks: Vec<CommandBlock>,
    /// The line counter passed in from outside (incremented by the caller as
    /// lines are written to the grid).  Updated by [`CommandTracker::apply`].
    current_line: usize,
}

#[allow(dead_code)]
impl CommandTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance the "current line" cursor.  Call this whenever you know that
    /// the terminal cursor has moved to a new grid line (e.g. after a
    /// newline).  In practice the caller can simply pass `term.grid().cursor
    /// .point.line` each time `apply` is called.
    pub fn set_current_line(&mut self, line: usize) {
        self.current_line = line;
    }

    /// Apply a single parsed mark to the tracker.
    pub fn apply(&mut self, mark: &Osc633Mark) {
        match mark {
            Osc633Mark::PromptStart => {
                self.blocks.push(CommandBlock {
                    start_line: self.current_line,
                    end_line: None,
                    command: None,
                    exit_code: None,
                    state: CommandState::PromptStart,
                });
            }

            Osc633Mark::PromptEnd => {
                if let Some(block) = self.current_block_mut() {
                    block.state = CommandState::Input;
                }
            }

            Osc633Mark::PreExec => {
                if let Some(block) = self.current_block_mut() {
                    block.state = CommandState::Executing;
                }
            }

            Osc633Mark::CommandFinished { exit_code } => {
                let line = self.current_line;
                if let Some(block) = self.current_block_mut() {
                    block.state = CommandState::Finished;
                    block.end_line = Some(line);
                    block.exit_code = *exit_code;
                }
            }

            Osc633Mark::CommandLine { command } => {
                if let Some(block) = self.current_block_mut() {
                    block.command = Some(command.clone());
                }
            }
        }
    }

    /// Return a reference to the most recent (in-progress) block, if any.
    pub fn current_block(&self) -> Option<&CommandBlock> {
        self.blocks.last()
    }

    fn current_block_mut(&mut self) -> Option<&mut CommandBlock> {
        self.blocks.last_mut()
    }
}

// -- Byte-level OSC 633 scanner ----------------------------------------------

/// Stateful scanner that finds OSC 633 sequences in a raw PTY byte stream.
///
/// The scanner does not modify the byte stream; it only extracts marks.
/// Feed each PTY chunk through [`Osc633Parser::feed`]; the alacritty
/// `Term` should receive the same bytes unchanged.
#[derive(Debug, Default)]
pub struct Osc633Parser {
    /// Accumulator for the bytes of an in-flight OSC sequence.
    buf: Vec<u8>,
    /// Whether we are currently inside an OSC sequence.
    in_osc: bool,
    /// Set to `true` once we have seen `ESC ]` and confirmed the code is
    /// `633`.  Before confirmation we buffer everything so we can abandon
    /// non-633 OSC sequences cheaply.
    confirmed_633: bool,
    /// Set when ESC (0x1B) is seen inside an OSC, cleared on the next byte.
    /// Only `\` (0x5C) immediately after ESC should terminate via ST.
    after_esc: bool,
}

impl Osc633Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of raw PTY bytes and return any OSC 633 marks found.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Osc633Mark> {
        let mut marks = Vec::new();

        for &byte in bytes {
            if self.in_osc {
                match byte {
                    // BEL terminates an OSC sequence.
                    0x07 => {
                        if self.confirmed_633
                            && let Some(mark) = parse_633_body(&self.buf)
                        {
                            marks.push(mark);
                        }
                        self.reset();
                    }
                    // ESC inside an OSC signals the start of ST (`ESC \`).
                    0x1B => {
                        self.after_esc = true;
                        continue;
                    }
                    // `\` after ESC completes ST terminator.
                    0x5C if self.after_esc => {
                        self.after_esc = false;
                        if self.confirmed_633
                            && let Some(mark) = parse_633_body(&self.buf)
                        {
                            marks.push(mark);
                        }
                        self.reset();
                    }
                    _ => {
                        self.after_esc = false;
                        if !self.confirmed_633 {
                            // Still accumulating the OSC code prefix to see
                            // if it is `633`.
                            self.buf.push(byte);
                            // Minimum: "633;" is 4 bytes.
                            if self.buf.len() >= 4 {
                                if self.buf.starts_with(b"633;") {
                                    // Confirmed -- strip the "633;" prefix and
                                    // keep only the body.
                                    let body = self.buf[4..].to_vec();
                                    self.buf = body;
                                    self.confirmed_633 = true;
                                } else if !b"633;".starts_with(&self.buf) {
                                    // Definitely not 633 -- abandon early.
                                    self.reset();
                                }
                            }
                        } else {
                            self.buf.push(byte);
                        }
                    }
                }
            } else if byte == 0x1B {
                // Potential start of OSC (`ESC ]` = 0x1B 0x5D).
                // We enter a one-byte look-ahead state by starting the
                // buffer but not yet setting `in_osc`.
                self.buf.clear();
                self.buf.push(byte);
            } else if byte == 0x5D && self.buf.last() == Some(&0x1B) {
                // Confirmed `ESC ]` -- beginning of an OSC sequence.
                self.buf.clear();
                self.in_osc = true;
                self.confirmed_633 = false;
            } else {
                // Unrelated byte -- clear any partial ESC look-ahead.
                if !self.buf.is_empty() {
                    self.buf.clear();
                }
            }
        }

        marks
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.in_osc = false;
        self.confirmed_633 = false;
        self.after_esc = false;
    }
}

// -- Body parser -------------------------------------------------------------

/// Parse the body of a confirmed `633;...` sequence (the part after `633;`).
fn parse_633_body(body: &[u8]) -> Option<Osc633Mark> {
    if body.is_empty() {
        return None;
    }

    let mark_byte = body[0];
    // The rest after the mark letter (skip optional leading `;`).
    let rest = if body.len() > 1 && body[1] == b';' {
        &body[2..]
    } else {
        &body[1..]
    };

    match mark_byte {
        b'A' => Some(Osc633Mark::PromptStart),
        b'B' => Some(Osc633Mark::PromptEnd),
        b'C' => Some(Osc633Mark::PreExec),
        b'D' => {
            let exit_code = if rest.is_empty() {
                None
            } else {
                std::str::from_utf8(rest)
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok())
            };
            Some(Osc633Mark::CommandFinished { exit_code })
        }
        b'E' => {
            let command = std::str::from_utf8(rest).ok()?.to_owned();
            Some(Osc633Mark::CommandLine { command })
        }
        _ => None,
    }
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Build an OSC 633 sequence terminated by BEL.
    fn osc(mark: &str) -> Vec<u8> {
        let mut v = b"\x1b]633;".to_vec();
        v.extend_from_slice(mark.as_bytes());
        v.push(0x07); // BEL
        v
    }

    // Build an OSC 633 sequence terminated by ST (ESC \).
    fn osc_st(mark: &str) -> Vec<u8> {
        let mut v = b"\x1b]633;".to_vec();
        v.extend_from_slice(mark.as_bytes());
        v.extend_from_slice(b"\x1b\\"); // ST
        v
    }

    #[test]
    fn prompt_start() {
        let mut parser = Osc633Parser::new();
        let marks = parser.feed(&osc("A"));
        assert_eq!(marks, vec![Osc633Mark::PromptStart]);
    }

    #[test]
    fn prompt_end() {
        let mut parser = Osc633Parser::new();
        let marks = parser.feed(&osc("B"));
        assert_eq!(marks, vec![Osc633Mark::PromptEnd]);
    }

    #[test]
    fn pre_exec() {
        let mut parser = Osc633Parser::new();
        let marks = parser.feed(&osc("C"));
        assert_eq!(marks, vec![Osc633Mark::PreExec]);
    }

    #[test]
    fn command_finished_with_exit_code() {
        let mut parser = Osc633Parser::new();
        let marks = parser.feed(&osc("D;0"));
        assert_eq!(
            marks,
            vec![Osc633Mark::CommandFinished { exit_code: Some(0) }]
        );
    }

    #[test]
    fn command_finished_nonzero_exit() {
        let mut parser = Osc633Parser::new();
        let marks = parser.feed(&osc("D;1"));
        assert_eq!(
            marks,
            vec![Osc633Mark::CommandFinished { exit_code: Some(1) }]
        );
    }

    #[test]
    fn command_finished_no_exit_code() {
        let mut parser = Osc633Parser::new();
        let marks = parser.feed(&osc("D"));
        assert_eq!(marks, vec![Osc633Mark::CommandFinished { exit_code: None }]);
    }

    #[test]
    fn command_line() {
        let mut parser = Osc633Parser::new();
        let marks = parser.feed(&osc("E;cargo build"));
        assert_eq!(
            marks,
            vec![Osc633Mark::CommandLine {
                command: "cargo build".to_owned()
            }]
        );
    }

    #[test]
    fn st_terminator() {
        let mut parser = Osc633Parser::new();
        let marks = parser.feed(&osc_st("A"));
        assert_eq!(marks, vec![Osc633Mark::PromptStart]);
    }

    #[test]
    fn multiple_marks_in_one_chunk() {
        let mut parser = Osc633Parser::new();
        let mut bytes = osc("A");
        bytes.extend(b"$ ");
        bytes.extend(osc("B"));
        let marks = parser.feed(&bytes);
        assert_eq!(marks, vec![Osc633Mark::PromptStart, Osc633Mark::PromptEnd]);
    }

    #[test]
    fn split_across_chunks() {
        let mut parser = Osc633Parser::new();
        let full = osc("A");
        // Feed first half then second half.
        let mid = full.len() / 2;
        let mut marks = parser.feed(&full[..mid]);
        marks.extend(parser.feed(&full[mid..]));
        assert_eq!(marks, vec![Osc633Mark::PromptStart]);
    }

    #[test]
    fn unrelated_osc_is_ignored() {
        let mut parser = Osc633Parser::new();
        // OSC 2 (window title) should produce no marks.
        let title_osc = b"\x1b]2;my title\x07";
        let marks = parser.feed(title_osc);
        assert!(marks.is_empty());
    }

    #[test]
    fn unknown_633_mark_ignored() {
        let mut parser = Osc633Parser::new();
        let marks = parser.feed(&osc("Z"));
        assert!(marks.is_empty());
    }

    // -- CommandTracker integration ------------------------------------------

    #[allow(dead_code)]
    fn run_sequence(seq: &[(&str, usize)]) -> Vec<CommandBlock> {
        let mut tracker = CommandTracker::new();
        let mut parser = Osc633Parser::new();

        for (raw, line) in seq {
            let marks = parser.feed(raw.as_bytes());
            tracker.set_current_line(*line);
            for mark in &marks {
                tracker.apply(mark);
            }
        }

        tracker.blocks
    }

    #[test]
    fn tracker_full_command_flow() {
        // Simulate: prompt(line 0) -> input -> execute(line 1) -> finish(line 2)
        let mut tracker = CommandTracker::new();
        let mut parser = Osc633Parser::new();

        let seqs: &[(&[u8], usize)] = &[
            (&osc("A"), 0),
            (&osc("B"), 0),
            (&osc("E;git status"), 0),
            (&osc("C"), 1),
            (&osc("D;0"), 2),
        ];

        for (bytes, line) in seqs {
            let marks = parser.feed(bytes);
            tracker.set_current_line(*line);
            for mark in &marks {
                tracker.apply(mark);
            }
        }

        assert_eq!(tracker.blocks.len(), 1);
        let block = &tracker.blocks[0];
        assert_eq!(block.start_line, 0);
        assert_eq!(block.end_line, Some(2));
        assert_eq!(block.command, Some("git status".to_owned()));
        assert_eq!(block.exit_code, Some(0));
        assert_eq!(block.state, CommandState::Finished);
    }

    #[test]
    fn tracker_multiple_commands() {
        let mut tracker = CommandTracker::new();
        let mut parser = Osc633Parser::new();

        // First command
        for (bytes, line) in [
            (osc("A"), 0_usize),
            (osc("B"), 0),
            (osc("C"), 1),
            (osc("D;0"), 1),
        ] {
            let marks = parser.feed(&bytes);
            tracker.set_current_line(line);
            for mark in &marks {
                tracker.apply(mark);
            }
        }

        // Second command
        for (bytes, line) in [
            (osc("A"), 2_usize),
            (osc("B"), 2),
            (osc("E;ls"), 2),
            (osc("C"), 3),
            (osc("D;127"), 4),
        ] {
            let marks = parser.feed(&bytes);
            tracker.set_current_line(line);
            for mark in &marks {
                tracker.apply(mark);
            }
        }

        assert_eq!(tracker.blocks.len(), 2);
        assert_eq!(tracker.blocks[0].exit_code, Some(0));
        assert_eq!(tracker.blocks[0].state, CommandState::Finished);
        assert_eq!(tracker.blocks[1].command, Some("ls".to_owned()));
        assert_eq!(tracker.blocks[1].exit_code, Some(127));
    }

    #[test]
    fn tracker_no_marks_no_blocks() {
        let tracker = CommandTracker::new();
        assert!(tracker.blocks.is_empty());
        assert!(tracker.current_block().is_none());
    }
}
