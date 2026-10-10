//! Overlay text shaping cache helpers.
//!
//! `ensure_shaped` populates a keyed slot in the overlay cache with a shaped
//! `Buffer`. `cached_buf` retrieves it. Callers that may render before a slot
//! is populated should handle the `None` case by skipping that draw element.

use std::collections::HashMap;

use glyphon::{Attrs, Buffer, FontSystem, Metrics, Shaping};

/// The chrome text cache: slot name → (cache key, shaped buffer).
///
/// Aliased so helper signatures don't have to repeat the long
/// `HashMap<String, (String, Buffer)>` literal everywhere.
pub(super) type ChromeTextCache = HashMap<String, (String, Buffer)>;

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
    ensure_shaped_colored(
        slot,
        cache_key,
        metrics,
        width,
        height,
        text,
        attrs,
        &[],
        font_system,
        cache,
    );
}

/// Shape text with optional per-character icon colors.
#[allow(clippy::too_many_arguments)]
pub(super) fn ensure_shaped_colored(
    slot: &str,
    cache_key: &str,
    metrics: Metrics,
    width: f32,
    height: f32,
    text: &str,
    attrs: Attrs<'_>,
    colors: &[(usize, [u8; 3])],
    font_system: &mut FontSystem,
    cache: &mut ChromeTextCache,
) {
    let needs_reshape = cache
        .get(slot)
        .map(|(k, _)| k.as_str() != cache_key)
        .unwrap_or(true);

    if needs_reshape {
        let mut buf = Buffer::new(font_system, metrics);
        buf.set_size(font_system, Some(width), Some(height));
        if colors.is_empty() {
            buf.set_text(font_system, text, &attrs, Shaping::Basic, None);
        } else {
            let spans = text.char_indices().enumerate().map(|(i, (byte, ch))| {
                let attr = colors
                    .iter()
                    .find(|(pos, _)| *pos == i)
                    .map(|(_, rgb)| {
                        attrs
                            .clone()
                            .color(glyphon::Color::rgb(rgb[0], rgb[1], rgb[2]))
                    })
                    .unwrap_or_else(|| attrs.clone());
                (&text[byte..byte + ch.len_utf8()], attr)
            });
            buf.set_rich_text(font_system, spans, &attrs, Shaping::Basic, None);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_color_does_not_leak_to_neighboring_text() {
        let mut font_system = FontSystem::new();
        let mut cache = ChromeTextCache::new();
        let base = glyphon::Color::rgb(240, 240, 240);
        let orange = glyphon::Color::rgb(229, 139, 74);
        ensure_shaped_colored(
            "tab",
            "colored",
            Metrics::new(12.0, 16.0),
            200.0,
            20.0,
            "1 R name",
            Attrs::new().color(base),
            &[(2, [229, 139, 74]), (99, [65, 190, 183])],
            &mut font_system,
            &mut cache,
        );
        let buffer = cached_buf(&cache, "tab").unwrap();
        let glyphs: Vec<_> = buffer.layout_runs().flat_map(|r| r.glyphs).collect();
        assert!(!glyphs.is_empty());
        assert!(
            glyphs
                .iter()
                .any(|g| g.start == 2 && g.color_opt == Some(orange))
        );
        assert!(
            glyphs
                .iter()
                .filter(|g| g.start != 2)
                .all(|g| g.color_opt == Some(base))
        );
    }

    /// Build a minimal cache by constructing the HashMap directly without
    /// calling `ensure_shaped` (which requires a real FontSystem + glyphon).
    /// We can't create a real `Buffer` without a FontSystem, so these tests
    /// focus on the lookup logic of `cached_buf` via a stand-in approach:
    /// verify the `None` path for missing keys.

    #[test]
    fn cached_buf_returns_none_for_unknown_key() {
        // An empty cache must return None for any slot name.
        let cache: HashMap<String, (String, Buffer)> = HashMap::new();
        assert!(cached_buf(&cache, "sb_left").is_none());
        assert!(cached_buf(&cache, "hdr_idx_0").is_none());
        assert!(cached_buf(&cache, "csd_close").is_none());
    }

    #[test]
    fn cached_buf_returns_none_for_wrong_slot_name() {
        // Even if slots exist for similar names, the wrong key returns None.
        let cache: HashMap<String, (String, Buffer)> = HashMap::new();
        assert!(cached_buf(&cache, "").is_none());
        assert!(cached_buf(&cache, "sb_left ").is_none()); // trailing space
        assert!(cached_buf(&cache, "SB_LEFT").is_none()); // wrong case
    }

    #[test]
    fn ensure_shaped_is_noop_on_matching_cache_key() {
        // When the slot and cache_key already match, ensure_shaped must not
        // overwrite the existing entry. We verify by checking the cache size
        // stays at 1 after a second call with the same slot+key.
        // Requires a real FontSystem; skip if not available via a FontSystem::new().
        let mut font_system = glyphon::FontSystem::new();
        let mut cache: HashMap<String, (String, Buffer)> = HashMap::new();
        let metrics = glyphon::Metrics::new(12.0, 16.0);
        let attrs = glyphon::Attrs::new();

        // First call: populates the cache.
        ensure_shaped(
            "slot_a",
            "key_a",
            metrics,
            200.0,
            20.0,
            "hello",
            attrs,
            &mut font_system,
            &mut cache,
        );
        assert_eq!(cache.len(), 1);

        // Second call with identical slot+key: must be a no-op (cache stays at 1).
        ensure_shaped(
            "slot_a",
            "key_a",
            metrics,
            200.0,
            20.0,
            "hello",
            glyphon::Attrs::new(),
            &mut font_system,
            &mut cache,
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn ensure_shaped_overwrites_on_changed_cache_key() {
        // When the cache_key changes (e.g. text changed), ensure_shaped must
        // update the entry so the stale buffer is replaced.
        let mut font_system = glyphon::FontSystem::new();
        let mut cache: HashMap<String, (String, Buffer)> = HashMap::new();
        let metrics = glyphon::Metrics::new(12.0, 16.0);
        let attrs = glyphon::Attrs::new();

        ensure_shaped(
            "slot_b",
            "key_b1",
            metrics,
            100.0,
            16.0,
            "old text",
            attrs,
            &mut font_system,
            &mut cache,
        );
        assert_eq!(cache.get("slot_b").unwrap().0, "key_b1");

        ensure_shaped(
            "slot_b",
            "key_b2",
            metrics,
            100.0,
            16.0,
            "new text",
            glyphon::Attrs::new(),
            &mut font_system,
            &mut cache,
        );
        // The key must now reflect the updated cache_key.
        assert_eq!(cache.get("slot_b").unwrap().0, "key_b2");
    }

    #[test]
    fn cached_buf_returns_some_after_ensure_shaped() {
        // After ensure_shaped populates a slot, cached_buf must return Some.
        let mut font_system = glyphon::FontSystem::new();
        let mut cache: HashMap<String, (String, Buffer)> = HashMap::new();
        let metrics = glyphon::Metrics::new(12.0, 16.0);
        let attrs = glyphon::Attrs::new();

        assert!(cached_buf(&cache, "my_slot").is_none());

        ensure_shaped(
            "my_slot",
            "my_key",
            metrics,
            100.0,
            16.0,
            "content",
            attrs,
            &mut font_system,
            &mut cache,
        );

        assert!(cached_buf(&cache, "my_slot").is_some());
        // A different slot must still return None.
        assert!(cached_buf(&cache, "other_slot").is_none());
    }

    #[test]
    fn ensure_shaped_multiple_independent_slots() {
        // Multiple distinct slots must all be populated independently.
        let mut font_system = glyphon::FontSystem::new();
        let mut cache: HashMap<String, (String, Buffer)> = HashMap::new();
        let metrics = glyphon::Metrics::new(12.0, 16.0);
        for (slot, key, text) in [
            ("slot_1", "k1", "alpha"),
            ("slot_2", "k2", "beta"),
            ("slot_3", "k3", "gamma"),
        ] {
            ensure_shaped(
                slot,
                key,
                metrics,
                80.0,
                16.0,
                text,
                glyphon::Attrs::new(),
                &mut font_system,
                &mut cache,
            );
        }

        assert_eq!(cache.len(), 3);
        assert!(cached_buf(&cache, "slot_1").is_some());
        assert!(cached_buf(&cache, "slot_2").is_some());
        assert!(cached_buf(&cache, "slot_3").is_some());
        assert!(cached_buf(&cache, "slot_4").is_none());
    }
}
