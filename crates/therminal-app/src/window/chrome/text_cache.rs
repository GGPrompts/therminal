//! Overlay text shaping cache helpers.
//!
//! `ensure_shaped` populates a keyed slot in the overlay cache with a shaped
//! `Buffer`. `cached_buf` retrieves it. Callers that may render before a slot
//! is populated should handle the `None` case by skipping that draw element.

use std::collections::HashMap;

use glyphon::{Attrs, Buffer, FontSystem, Metrics, Shaping};

/// Ensure a cached shaped Buffer exists for the given slot.
/// If the cache key matches, this is a no-op. Otherwise creates a new Buffer,
/// shapes it, and stores it in the cache.
#[allow(clippy::too_many_arguments)]
pub(super) fn ensure_shaped(
    slot: &str,
    cache_key: &str,
    metrics: Metrics,
    width: f32,
    height: f32,
    text: &str,
    attrs: Attrs<'_>,
    font_system: &mut FontSystem,
    cache: &mut HashMap<String, (String, Buffer)>,
) {
    let needs_reshape = cache
        .get(slot)
        .map(|(k, _)| k.as_str() != cache_key)
        .unwrap_or(true);

    if needs_reshape {
        let mut buf = Buffer::new(font_system, metrics);
        buf.set_size(font_system, Some(width), Some(height));
        buf.set_text(font_system, text, &attrs, Shaping::Basic, None);
        buf.shape_until_scroll(font_system, false);
        cache.insert(slot.to_string(), (cache_key.to_string(), buf));
    }
}

/// Get a reference to a cached Buffer, or `None` if the slot has not been
/// populated via `ensure_shaped`. Render callers that see `None` should log
/// and skip drawing that element rather than panic from a render hot path.
pub(super) fn cached_buf<'a>(
    cache: &'a HashMap<String, (String, Buffer)>,
    slot: &str,
) -> Option<&'a Buffer> {
    cache.get(slot).map(|(_, b)| b)
}
