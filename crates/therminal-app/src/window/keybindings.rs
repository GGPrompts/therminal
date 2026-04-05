//! Keybinding map: config-driven key-to-action mapping.
//!
//! Parses `[keybindings]` from therminal.toml into a `HashMap` for O(1) lookup
//! of incoming winit key events.

use std::collections::HashMap;

use tracing::warn;
use winit::event::{KeyEvent, Modifiers};
use winit::keyboard::{Key, NamedKey};

use therminal_core::config::{
    parse_binding, KeyAction, ParsedKey, ParsedNamedKey, TherminalConfig,
};

// ── Binding key types ──────────────────────────────────────────────────

/// A lookup key for the binding map: (modifiers, key).
///
/// The key is stored as a `BindingKey` which mirrors winit's `Key` enum
/// closely enough to look up incoming key events in O(1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum BindingKey {
    /// A single character (lowercase for case-insensitive matching with
    /// shift accounted for separately).
    Character(String),
    /// A named key (Enter, arrows, function keys, etc.).
    Named(NamedKey),
}

/// Full lookup key: modifiers + key.
pub(crate) type BindingLookup = (bool, bool, bool, bool, BindingKey);

// ── Conversion ─────────────────────────────────────────────────────────

/// Convert a parsed named key to winit's `NamedKey`.
fn to_winit_named(k: &ParsedNamedKey) -> NamedKey {
    match k {
        ParsedNamedKey::Enter => NamedKey::Enter,
        ParsedNamedKey::Tab => NamedKey::Tab,
        ParsedNamedKey::Escape => NamedKey::Escape,
        ParsedNamedKey::Backspace => NamedKey::Backspace,
        ParsedNamedKey::Delete => NamedKey::Delete,
        ParsedNamedKey::Insert => NamedKey::Insert,
        ParsedNamedKey::Home => NamedKey::Home,
        ParsedNamedKey::End => NamedKey::End,
        ParsedNamedKey::PageUp => NamedKey::PageUp,
        ParsedNamedKey::PageDown => NamedKey::PageDown,
        ParsedNamedKey::ArrowUp => NamedKey::ArrowUp,
        ParsedNamedKey::ArrowDown => NamedKey::ArrowDown,
        ParsedNamedKey::ArrowLeft => NamedKey::ArrowLeft,
        ParsedNamedKey::ArrowRight => NamedKey::ArrowRight,
        ParsedNamedKey::Space => NamedKey::Space,
        ParsedNamedKey::F1 => NamedKey::F1,
        ParsedNamedKey::F2 => NamedKey::F2,
        ParsedNamedKey::F3 => NamedKey::F3,
        ParsedNamedKey::F4 => NamedKey::F4,
        ParsedNamedKey::F5 => NamedKey::F5,
        ParsedNamedKey::F6 => NamedKey::F6,
        ParsedNamedKey::F7 => NamedKey::F7,
        ParsedNamedKey::F8 => NamedKey::F8,
        ParsedNamedKey::F9 => NamedKey::F9,
        ParsedNamedKey::F10 => NamedKey::F10,
        ParsedNamedKey::F11 => NamedKey::F11,
        ParsedNamedKey::F12 => NamedKey::F12,
    }
}

// ── Map building and lookup ────────────────────────────────────────────

/// Build a binding lookup map from the config's keybinding list.
///
/// Invalid bindings are logged and skipped.
pub(crate) fn build_binding_map(config: &TherminalConfig) -> HashMap<BindingLookup, KeyAction> {
    let mut map = HashMap::new();
    for binding in &config.keybindings.bindings {
        match parse_binding(&binding.key) {
            Some((mods, parsed_key)) => {
                let bk = match &parsed_key {
                    ParsedKey::Character(c) => BindingKey::Character(c.clone()),
                    ParsedKey::Named(n) => BindingKey::Named(to_winit_named(n)),
                };
                let lookup = (mods.ctrl, mods.shift, mods.alt, mods.super_key, bk);
                map.insert(lookup, binding.action.clone());
            }
            None => {
                warn!(
                    key = %binding.key,
                    action = ?binding.action,
                    "skipping invalid keybinding"
                );
            }
        }
    }
    map
}

/// Look up a winit key event in the binding map.
pub(crate) fn lookup_binding(
    map: &HashMap<BindingLookup, KeyAction>,
    modifiers: &Modifiers,
    key_event: &KeyEvent,
) -> Option<KeyAction> {
    let state = modifiers.state();
    let ctrl = state.control_key();
    let shift = state.shift_key();
    let alt = state.alt_key();
    let super_key = state.super_key();

    let bk = match &key_event.logical_key {
        Key::Character(s) => {
            // Normalize: winit reports uppercase when Shift is held.
            // The binding map stores lowercase characters.
            BindingKey::Character(s.to_lowercase().to_string())
        }
        Key::Named(n) => BindingKey::Named(*n),
        _ => return None,
    };

    let lookup = (ctrl, shift, alt, super_key, bk);
    map.get(&lookup).cloned()
}
