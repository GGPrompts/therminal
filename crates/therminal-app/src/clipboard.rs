#![allow(dead_code)]

/// Clipboard support: arboard wrapper + OSC 52 parsing.
///
/// OSC 52 sequence format: `\x1b]52;c;{base64-encoded-text}\a`
/// The `c` parameter selects the clipboard selection (we treat all as the
/// system clipboard). The payload is standard base64-encoded UTF-8 text.
use base64::Engine as _;
use tracing::{debug, warn};

/// Copy `text` to the system clipboard.
///
/// Logs a warning and returns `()` on failure so callers don't need to handle
/// clipboard errors as fatal.
pub fn copy_to_clipboard(text: &str) {
    match arboard::Clipboard::new() {
        Ok(mut cb) => {
            if let Err(e) = cb.set_text(text) {
                warn!("clipboard write failed: {e}");
            } else {
                debug!("copied {} bytes to clipboard", text.len());
            }
        }
        Err(e) => warn!("clipboard unavailable: {e}"),
    }
}

/// Read a UTF-8 string from the system clipboard.
///
/// Returns an empty string if the clipboard is unavailable or contains
/// non-text data.
pub fn paste_from_clipboard() -> String {
    match arboard::Clipboard::new() {
        Ok(mut cb) => match cb.get_text() {
            Ok(text) => {
                debug!("pasted {} bytes from clipboard", text.len());
                text
            }
            Err(e) => {
                warn!("clipboard read failed: {e}");
                String::new()
            }
        },
        Err(e) => {
            warn!("clipboard unavailable: {e}");
            String::new()
        }
    }
}

/// Parse an OSC 52 escape sequence and write the decoded text to the clipboard.
///
/// Expected format (bytes as a Rust string literal):
/// ```text
/// \x1b]52;c;<base64>\x07
/// ```
/// or with the ST terminator `\x1b\\` instead of BEL `\x07`.
///
/// Returns `true` when the sequence was recognised and acted on (even if the
/// clipboard write ultimately failed), `false` when the input does not match
/// the OSC 52 pattern at all.
pub fn handle_osc52(sequence: &str) -> bool {
    // Strip the OSC introducer and terminator so we can split on `;`.
    let inner = sequence
        .strip_prefix("\x1b]")
        .unwrap_or(sequence)
        .trim_end_matches(['\x07', '\\', '\x1b']);

    let mut parts = inner.splitn(3, ';');
    let ps = parts.next().unwrap_or("");
    let _selection = parts.next().unwrap_or("");
    let payload = parts.next().unwrap_or("");

    if ps != "52" {
        return false;
    }

    // A payload of `?` is a clipboard-query request; we don't respond to
    // those yet (responding requires writing back to the PTY).
    if payload == "?" {
        debug!("OSC 52 clipboard query ignored (not yet implemented)");
        return true;
    }

    match base64::engine::general_purpose::STANDARD.decode(payload) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => {
                debug!("OSC 52 setting clipboard ({} chars)", text.len());
                copy_to_clipboard(&text);
            }
            Err(e) => warn!("OSC 52 payload is not valid UTF-8: {e}"),
        },
        Err(e) => warn!("OSC 52 base64 decode failed: {e}"),
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn osc52(text: &str) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        format!("\x1b]52;c;{encoded}\x07")
    }

    #[test]
    fn parse_osc52_recognised() {
        // handle_osc52 should return true for a well-formed OSC 52 sequence
        // even if the clipboard call fails in a headless CI environment.
        assert!(handle_osc52(&osc52("hello")));
    }

    #[test]
    fn parse_non_osc52_rejected() {
        assert!(!handle_osc52("\x1b]0;window title\x07"));
    }

    #[test]
    fn parse_osc52_query_recognised() {
        assert!(handle_osc52("\x1b]52;c;?\x07"));
    }
}
