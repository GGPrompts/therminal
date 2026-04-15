#![allow(dead_code)]
/// Clipboard support: arboard wrapper + OSC 52 parsing.
///
/// OSC 52 sequence format: `\x1b]52;c;{base64-encoded-text}\a`
/// The `c` parameter selects the clipboard selection (we treat all as the
/// system clipboard). The payload is standard base64-encoded UTF-8 text.
///
/// On WSL2, arboard's Wayland backend (`wlr-data-control`) fails because
/// WSLg doesn't support that protocol. We detect WSL2 via `WSL_DISTRO_NAME`
/// and temporarily unset `WAYLAND_DISPLAY` so arboard skips straight to the
/// X11 backend (WSLg provides an X11 server via XWayland).
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use tracing::{debug, warn};

/// Cached clipboard instance. Created once, reused for all operations.
/// This avoids repeated Wayland → X11 fallback attempts and the associated
/// log noise on WSL2.
static CLIPBOARD: OnceLock<Option<Mutex<arboard::Clipboard>>> = OnceLock::new();

/// Detect WSL2 via the `WSL_DISTRO_NAME` environment variable.
fn is_wsl2() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
}

/// Eagerly initialize the clipboard.
///
/// **Must be called from `main()` before any other threads are spawned**
/// (before winit `EventLoop`, wgpu `Instance`, or tokio runtime creation).
/// On WSL2 this temporarily unsets `WAYLAND_DISPLAY` while arboard probes
/// backends so it picks X11 instead of the broken WSLg `wlr-data-control`
/// path, then restores it. Because this runs single-threaded at startup,
/// the env mutation is sound — no other thread can observe the gap.
///
/// Safe to call multiple times; subsequent calls are no-ops.
pub fn init() {
    let _ = get_clipboard();
}

/// Get or create the cached clipboard instance.
///
/// Prefer calling [`init`] from `main()` so this runs before threads exist.
/// If it's first triggered lazily from the event loop on WSL2 the X11
/// backend selection still works (arboard re-reads env on each `new()`),
/// but the env mutation would then race other threads — which is why
/// [`init`] exists.
fn get_clipboard() -> Option<&'static Mutex<arboard::Clipboard>> {
    CLIPBOARD
        .get_or_init(|| {
            // On WSL2, force X11 backend by hiding WAYLAND_DISPLAY during init.
            // WSLg ships a Wayland compositor that doesn't implement
            // `wlr-data-control`, which arboard's Wayland backend requires;
            // XWayland works fine, so we prefer it.
            let wayland_display = if is_wsl2() {
                let val = std::env::var_os("WAYLAND_DISPLAY");
                if val.is_some() {
                    debug!("WSL2 detected: forcing X11 clipboard backend");
                    // SAFETY: `clipboard::init()` is called from `main()` before
                    // any winit/wgpu/tokio threads are spawned, so no other
                    // thread can observe this env mutation. If `get_clipboard`
                    // is instead first hit lazily, the OnceLock still
                    // serializes this block — but the ordering invariant
                    // above is what makes the env write sound.
                    unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
                }
                val
            } else {
                None
            };

            let result = arboard::Clipboard::new();

            // Restore WAYLAND_DISPLAY so other code (wgpu, winit) still sees it.
            if let Some(val) = wayland_display {
                // SAFETY: Same single-threaded-at-startup invariant as above;
                // this merely restores the value we just removed.
                unsafe { std::env::set_var("WAYLAND_DISPLAY", val) };
            }

            match result {
                Ok(cb) => {
                    debug!("clipboard initialized");
                    Some(Mutex::new(cb))
                }
                Err(e) => {
                    warn!("clipboard unavailable: {e}");
                    // Try one more time without any display vars as last resort.
                    match arboard::Clipboard::new() {
                        Ok(cb) => {
                            debug!("clipboard initialized on retry");
                            Some(Mutex::new(cb))
                        }
                        Err(e2) => {
                            warn!("clipboard completely unavailable: {e2}");
                            None
                        }
                    }
                }
            }
        })
        .as_ref()
}

/// Copy `text` to the system clipboard.
///
/// Logs a warning and returns `()` on failure so callers don't need to handle
/// clipboard errors as fatal.
pub fn copy_to_clipboard(text: &str) {
    let Some(cb_mutex) = get_clipboard() else {
        warn!("clipboard unavailable, cannot copy");
        return;
    };
    let Ok(mut cb) = cb_mutex.lock() else {
        warn!("clipboard mutex poisoned");
        return;
    };
    if let Err(e) = cb.set_text(text) {
        warn!("clipboard write failed: {e}");
    } else {
        debug!("copied {} bytes to clipboard", text.len());
    }
}

/// Read a UTF-8 string from the system clipboard.
///
/// Returns an empty string if the clipboard is unavailable or contains
/// non-text data.
pub fn paste_from_clipboard() -> String {
    let Some(cb_mutex) = get_clipboard() else {
        warn!("clipboard unavailable, cannot paste");
        return String::new();
    };
    let Ok(mut cb) = cb_mutex.lock() else {
        warn!("clipboard mutex poisoned");
        return String::new();
    };
    match cb.get_text() {
        Ok(text) => {
            debug!("pasted {} bytes from clipboard", text.len());
            text
        }
        Err(e) => {
            warn!("clipboard read failed: {e}");
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

    fn osc52(text: &str) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        format!("\x1b]52;c;{encoded}\x07")
    }

    #[test]
    fn parse_osc52_recognised() {
        // handle_osc52 should return true for a well-formed OSC 52 sequence
        // even when the clipboard is unavailable (e.g. headless CI).
        // The clipboard write is best-effort; only the parsing matters here.
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

    #[test]
    fn wsl2_detection_reads_env() {
        // Without WSL_DISTRO_NAME set, should not detect WSL2.
        let had_var = std::env::var_os("WSL_DISTRO_NAME");
        // SAFETY: test-only env var mutation.
        unsafe { std::env::remove_var("WSL_DISTRO_NAME") };
        assert!(!is_wsl2());
        // Restore if it was set.
        if let Some(val) = had_var {
            unsafe { std::env::set_var("WSL_DISTRO_NAME", val) };
        }
    }
}
