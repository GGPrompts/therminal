//! Stateful ANSI escape sequence stripping.

/// Parser state for the ANSI escape sequence stripper.
///
/// Carried across `feed()` calls so that escape sequences split across PTY
/// read boundaries are handled correctly instead of leaking fragments into
/// the visible text buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripState {
    /// Normal text -- not inside any escape sequence.
    Normal,
    /// Saw ESC (0x1B), waiting for the next byte to determine sequence type.
    Escape,
    /// Inside a CSI sequence (ESC \[), consuming parameter/intermediate bytes
    /// until a final byte in 0x40..=0x7E.
    Csi,
    /// Inside an OSC sequence (ESC \]), consuming until BEL (0x07) or ST (ESC \\).
    Osc,
    /// Inside an OSC sequence, just saw ESC -- waiting for `\` to complete ST,
    /// or any other byte which continues the OSC body.
    OscEsc,
    /// Inside an APC sequence (ESC \_), consuming until ST (ESC \\).
    Apc,
    /// Inside an APC sequence, just saw ESC -- waiting for `\` to complete ST.
    ApcEsc,
    /// Saw ESC followed by a charset designator (one of `( ) * +`), waiting
    /// for the charset byte (e.g. `B`, `0`).
    Charset,
}

/// Stateful ANSI escape sequence stripper.
///
/// Strips CSI, OSC, APC, and other common escape sequences from a byte
/// stream, returning only the visible text. State is preserved across
/// calls to [`AnsiStripper::feed`] so that sequences split across PTY
/// read boundaries are consumed correctly.
pub struct AnsiStripper {
    state: StripState,
}

impl Default for AnsiStripper {
    fn default() -> Self {
        Self::new()
    }
}

impl AnsiStripper {
    pub fn new() -> Self {
        Self {
            state: StripState::Normal,
        }
    }

    /// Feed a chunk of raw bytes and return the visible text extracted from it.
    ///
    /// Escape-sequence parsing state is carried across calls, so a sequence
    /// that starts at the end of one chunk and finishes at the start of the
    /// next is handled without leaking control characters into the output.
    pub fn feed(&mut self, bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len());
        let mut i = 0;

        while i < bytes.len() {
            let b = bytes[i];

            match self.state {
                StripState::Normal => {
                    if b == 0x1B {
                        self.state = StripState::Escape;
                        i += 1;
                    } else if b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t' {
                        // Skip non-printable control characters.
                        i += 1;
                    } else {
                        // Visible character or whitespace -- handle UTF-8.
                        let remaining = &bytes[i..];
                        if let Some(ch) = decode_utf8_char(remaining) {
                            let char_len = ch.len_utf8();
                            out.push(ch);
                            i += char_len;
                        } else {
                            // Invalid UTF-8, skip byte.
                            i += 1;
                        }
                    }
                }

                StripState::Escape => {
                    // We saw ESC last; this byte determines the sequence type.
                    match b {
                        b'[' => {
                            self.state = StripState::Csi;
                            i += 1;
                        }
                        b']' => {
                            self.state = StripState::Osc;
                            i += 1;
                        }
                        b'(' | b')' | b'*' | b'+' => {
                            self.state = StripState::Charset;
                            i += 1;
                        }
                        b'_' => {
                            self.state = StripState::Apc;
                            i += 1;
                        }
                        _ => {
                            // Other 2-byte ESC sequences: skip this byte and done.
                            self.state = StripState::Normal;
                            i += 1;
                        }
                    }
                }

                StripState::Csi => {
                    // CSI sequence: consume until final byte 0x40..=0x7E.
                    if (0x40..=0x7E).contains(&b) {
                        self.state = StripState::Normal;
                    }
                    i += 1;
                }

                StripState::Osc => {
                    if b == 0x07 {
                        // BEL terminates OSC.
                        self.state = StripState::Normal;
                        i += 1;
                    } else if b == 0x1B {
                        // Possible ST (ESC \).
                        self.state = StripState::OscEsc;
                        i += 1;
                    } else {
                        // OSC body -- skip.
                        i += 1;
                    }
                }

                StripState::OscEsc => {
                    if b == b'\\' {
                        // ST complete -- OSC is done.
                        self.state = StripState::Normal;
                    } else {
                        // Not ST -- the ESC was part of the OSC body (rare).
                        // Stay in OSC and reprocess this byte.
                        self.state = StripState::Osc;
                        continue; // reprocess without advancing i
                    }
                    i += 1;
                }

                StripState::Apc => {
                    if b == 0x1B {
                        self.state = StripState::ApcEsc;
                    }
                    i += 1;
                }

                StripState::ApcEsc => {
                    if b == b'\\' {
                        // ST complete -- APC is done.
                        self.state = StripState::Normal;
                    } else {
                        // Not ST -- stay in APC.
                        self.state = StripState::Apc;
                        continue; // reprocess without advancing i
                    }
                    i += 1;
                }

                StripState::Charset => {
                    // Charset designation: consume the one charset byte.
                    self.state = StripState::Normal;
                    i += 1;
                }
            }
        }

        out
    }
}

/// Strip ANSI escape sequences from bytes, returning visible text.
///
/// Convenience wrapper that creates a one-shot [`AnsiStripper`]. For
/// streaming use (where sequences may be split across chunks), prefer
/// creating an `AnsiStripper` and calling [`AnsiStripper::feed`] repeatedly.
#[cfg(test)]
fn strip_ansi_visible(bytes: &[u8]) -> String {
    AnsiStripper::new().feed(bytes)
}

/// Decode a single UTF-8 character from the start of a byte slice.
pub(crate) fn decode_utf8_char(bytes: &[u8]) -> Option<char> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.chars().next())
        .or_else(|| {
            // Try progressively shorter slices (1-4 bytes) to handle partial
            // multi-byte sequences at chunk boundaries.
            for len in (1..=4.min(bytes.len())).rev() {
                if let Ok(s) = std::str::from_utf8(&bytes[..len]) {
                    return s.chars().next();
                }
            }
            None
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- ANSI stripping ------------------------------------------------------

    #[test]
    fn strip_plain_text() {
        let text = b"Hello, world!";
        assert_eq!(strip_ansi_visible(text), "Hello, world!");
    }

    #[test]
    fn strip_csi_color_codes() {
        // ESC[31m = red, ESC[0m = reset
        let text = b"\x1b[31mRed text\x1b[0m normal";
        assert_eq!(strip_ansi_visible(text), "Red text normal");
    }

    #[test]
    fn strip_osc_title() {
        // OSC 2 = set title, terminated by BEL
        let text = b"\x1b]2;My Title\x07Visible text";
        assert_eq!(strip_ansi_visible(text), "Visible text");
    }

    #[test]
    fn strip_preserves_newlines() {
        let text = b"line1\nline2\r\nline3";
        assert_eq!(strip_ansi_visible(text), "line1\nline2\r\nline3");
    }

    #[test]
    fn strip_mixed_escapes() {
        let text = b"\x1b[1;32m> \x1b[0mType a message\x1b]2;title\x07";
        assert_eq!(strip_ansi_visible(text), "> Type a message");
    }

    // -- Stateful ANSI stripper (split sequences) ----------------------------

    #[test]
    fn split_csi_across_chunks() {
        let mut stripper = AnsiStripper::new();
        let out1 = stripper.feed(b"hello\x1b");
        let out2 = stripper.feed(b"[0mworld");
        assert_eq!(format!("{out1}{out2}"), "helloworld");
    }

    #[test]
    fn split_csi_esc_bracket_then_params() {
        let mut stripper = AnsiStripper::new();
        let out1 = stripper.feed(b"before\x1b[");
        let out2 = stripper.feed(b"31mafter");
        assert_eq!(format!("{out1}{out2}"), "beforeafter");
    }

    #[test]
    fn split_osc_across_chunks() {
        let mut stripper = AnsiStripper::new();
        let out1 = stripper.feed(b"before\x1b]");
        let out2 = stripper.feed(b"2;My Title\x07after");
        assert_eq!(format!("{out1}{out2}"), "beforeafter");
    }

    #[test]
    fn split_osc_st_across_chunks() {
        let mut stripper = AnsiStripper::new();
        let out1 = stripper.feed(b"\x1b]633;some data");
        let out2 = stripper.feed(b" more data\x1b\\visible");
        assert_eq!(format!("{out1}{out2}"), "visible");
    }

    #[test]
    fn split_osc_st_esc_at_boundary() {
        let mut stripper = AnsiStripper::new();
        let out1 = stripper.feed(b"\x1b]2;title\x1b");
        let out2 = stripper.feed(b"\\after");
        assert_eq!(format!("{out1}{out2}"), "after");
    }

    #[test]
    fn split_normal_text_across_chunks() {
        let mut stripper = AnsiStripper::new();
        let out1 = stripper.feed(b"hello ");
        let out2 = stripper.feed(b"world");
        assert_eq!(format!("{out1}{out2}"), "hello world");
    }

    #[test]
    fn split_apc_across_chunks() {
        let mut stripper = AnsiStripper::new();
        let out1 = stripper.feed(b"before\x1b_apc body");
        let out2 = stripper.feed(b" continued\x1b\\after");
        assert_eq!(format!("{out1}{out2}"), "beforeafter");
    }

    #[test]
    fn split_charset_across_chunks() {
        let mut stripper = AnsiStripper::new();
        let out1 = stripper.feed(b"before\x1b(");
        let out2 = stripper.feed(b"Bafter");
        assert_eq!(format!("{out1}{out2}"), "beforeafter");
    }

    // -- Control characters ----------------------------------------------------

    #[test]
    fn strip_control_characters_except_whitespace() {
        // BEL (0x07), BS (0x08), and other C0 controls should be stripped,
        // but \n, \r, and \t should be preserved.
        let text = b"\x07hello\x08\tworld\x01\x02";
        assert_eq!(strip_ansi_visible(text), "hello\tworld");
    }

    #[test]
    fn strip_nul_bytes() {
        let text = b"abc\x00def";
        assert_eq!(strip_ansi_visible(text), "abcdef");
    }

    // -- UTF-8 handling -------------------------------------------------------

    #[test]
    fn strip_preserves_multibyte_utf8() {
        let text = "hello 世界 🌍".as_bytes();
        assert_eq!(strip_ansi_visible(text), "hello 世界 🌍");
    }

    #[test]
    fn strip_utf8_with_ansi_interleaved() {
        let text = "\x1b[32m日本語\x1b[0m テスト".as_bytes();
        assert_eq!(strip_ansi_visible(text), "日本語 テスト");
    }

    // -- Two-byte ESC sequences -----------------------------------------------

    #[test]
    fn strip_two_byte_esc_sequences() {
        // ESC 7 (save cursor), ESC 8 (restore cursor), ESC = (app keypad)
        let text = b"\x1b7hello\x1b8\x1b=world";
        assert_eq!(strip_ansi_visible(text), "helloworld");
    }

    // -- Charset designator variants ------------------------------------------

    #[test]
    fn strip_all_charset_designators() {
        // ESC (B, ESC )0, ESC *A, ESC +B
        let text = b"\x1b(Bhello\x1b)0\x1b*A\x1b+Bworld";
        assert_eq!(strip_ansi_visible(text), "helloworld");
    }

    // -- OSC terminated by BEL vs ST ------------------------------------------

    #[test]
    fn strip_osc_terminated_by_st() {
        let text = b"\x1b]2;title\x1b\\visible";
        assert_eq!(strip_ansi_visible(text), "visible");
    }

    // -- Empty inputs ---------------------------------------------------------

    #[test]
    fn strip_empty_input() {
        assert_eq!(strip_ansi_visible(b""), "");
    }

    #[test]
    fn strip_only_escape_sequences() {
        let text = b"\x1b[31m\x1b[0m\x1b]2;title\x07";
        assert_eq!(strip_ansi_visible(text), "");
    }

    // -- decode_utf8_char edge cases ------------------------------------------

    #[test]
    fn decode_utf8_char_ascii() {
        assert_eq!(decode_utf8_char(b"A"), Some('A'));
    }

    #[test]
    fn decode_utf8_char_multibyte() {
        assert_eq!(decode_utf8_char("é".as_bytes()), Some('é'));
        assert_eq!(decode_utf8_char("日".as_bytes()), Some('日'));
        assert_eq!(decode_utf8_char("🌍".as_bytes()), Some('🌍'));
    }

    #[test]
    fn decode_utf8_char_empty_returns_none() {
        assert_eq!(decode_utf8_char(b""), None);
    }

    #[test]
    fn decode_utf8_char_invalid_byte_returns_none() {
        // 0xFF is not a valid UTF-8 start byte
        assert_eq!(decode_utf8_char(&[0xFF]), None);
    }

    // -- Stateful: ESC at very end of input -----------------------------------

    #[test]
    fn esc_at_end_of_chunk_is_consumed_by_next() {
        let mut stripper = AnsiStripper::new();
        let out1 = stripper.feed(b"text\x1b");
        // Next chunk starts a CSI, OSC, or just a 2-byte ESC seq
        let out2 = stripper.feed(b"M more"); // ESC M = reverse index (2-byte)
        assert_eq!(format!("{out1}{out2}"), "text more");
    }
}
