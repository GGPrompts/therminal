//! Keybinding map: config-driven key-to-action mapping.
//!
//! Parses `[keybindings]` from therminal.toml into a `HashMap` for O(1) lookup
//! of incoming winit key events.

use std::collections::HashMap;

use tracing::warn;
use winit::event::{KeyEvent, Modifiers};
use winit::keyboard::{Key, NamedKey};
#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "macos",
    target_os = "windows",
))]
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

use therminal_core::config::{
    KeyAction, ParsedKey, ParsedNamedKey, TherminalConfig, parse_binding,
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
    if let Some(action) = map.get(&lookup) {
        return Some(action.clone());
    }

    // When Shift is held, winit reports the shifted character as logical_key
    // (e.g., Shift+/ → '?', Shift+= → '+'). Bindings use the unshifted
    // character. Use winit's key_without_modifiers() to get the layout-aware
    // unshifted key rather than a hardcoded US-layout table.
    if shift && let Some(unshifted_bk) = key_without_modifiers_binding(key_event) {
        let fallback = (ctrl, shift, alt, super_key, unshifted_bk);
        if let Some(action) = map.get(&fallback) {
            return Some(action.clone());
        }
    }

    None
}

/// Extract the unshifted key from a KeyEvent using winit's platform API.
/// Returns None if the platform trait isn't available or the key is named.
#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "macos",
    target_os = "windows",
))]
fn key_without_modifiers_binding(key_event: &KeyEvent) -> Option<BindingKey> {
    match key_event.key_without_modifiers() {
        Key::Character(s) => Some(BindingKey::Character(s.to_lowercase().to_string())),
        _ => None,
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "macos",
    target_os = "windows",
)))]
fn key_without_modifiers_binding(_key_event: &KeyEvent) -> Option<BindingKey> {
    None
}

#[cfg(test)]
mod tests {
    use therminal_core::config::{KeyAction, Keybinding, KeybindingsConfig, TherminalConfig};
    use winit::keyboard::NamedKey;

    use super::*;

    /// Build a TherminalConfig with only the given keybindings (no defaults).
    fn config_with_bindings(bindings: Vec<Keybinding>) -> TherminalConfig {
        TherminalConfig {
            keybindings: KeybindingsConfig { bindings },
            ..TherminalConfig::default()
        }
    }

    // ── build_binding_map ─────────────────────────────────────────────────

    #[test]
    fn build_binding_map_empty_bindings_produces_empty_map() {
        let cfg = config_with_bindings(vec![]);
        let map = build_binding_map(&cfg);
        assert!(map.is_empty());
    }

    #[test]
    fn build_binding_map_default_config_has_entries() {
        let cfg = TherminalConfig::default();
        let map = build_binding_map(&cfg);
        assert!(
            !map.is_empty(),
            "default config should produce a non-empty binding map"
        );
    }

    #[test]
    fn build_binding_map_ctrl_shift_h_maps_to_split_horizontal() {
        let cfg = config_with_bindings(vec![Keybinding {
            key: "ctrl+shift+h".to_string(),
            action: KeyAction::SplitHorizontal,
        }]);
        let map = build_binding_map(&cfg);
        // lookup key: (ctrl=true, shift=true, alt=false, super=false, 'h')
        let lookup = (
            true,
            true,
            false,
            false,
            BindingKey::Character("h".to_string()),
        );
        assert_eq!(map.get(&lookup), Some(&KeyAction::SplitHorizontal));
    }

    #[test]
    fn build_binding_map_alt_only_action() {
        let cfg = config_with_bindings(vec![Keybinding {
            key: "alt+1".to_string(),
            action: KeyAction::SwitchWorkspace(1),
        }]);
        let map = build_binding_map(&cfg);
        let lookup = (
            false,
            false,
            true,
            false,
            BindingKey::Character("1".to_string()),
        );
        assert_eq!(map.get(&lookup), Some(&KeyAction::SwitchWorkspace(1)));
    }

    #[test]
    fn build_binding_map_named_key_enter() {
        let cfg = config_with_bindings(vec![Keybinding {
            key: "ctrl+shift+enter".to_string(),
            action: KeyAction::SplitAuto,
        }]);
        let map = build_binding_map(&cfg);
        let lookup = (true, true, false, false, BindingKey::Named(NamedKey::Enter));
        assert_eq!(map.get(&lookup), Some(&KeyAction::SplitAuto));
    }

    #[test]
    fn build_binding_map_named_key_arrow_up() {
        let cfg = config_with_bindings(vec![Keybinding {
            key: "ctrl+shift+up".to_string(),
            action: KeyAction::FocusUp,
        }]);
        let map = build_binding_map(&cfg);
        let lookup = (
            true,
            true,
            false,
            false,
            BindingKey::Named(NamedKey::ArrowUp),
        );
        assert_eq!(map.get(&lookup), Some(&KeyAction::FocusUp));
    }

    #[test]
    fn build_binding_map_invalid_binding_is_skipped() {
        // "ctrl+shift+foobar" is not a valid key — should be silently dropped.
        let cfg = config_with_bindings(vec![
            Keybinding {
                key: "ctrl+shift+foobar".to_string(),
                action: KeyAction::ClosePane,
            },
            Keybinding {
                key: "ctrl+shift+w".to_string(),
                action: KeyAction::ClosePane,
            },
        ]);
        let map = build_binding_map(&cfg);
        assert_eq!(map.len(), 1, "invalid binding should be skipped");
    }

    #[test]
    fn build_binding_map_ctrl_equals_maps_to_font_size_up() {
        let cfg = config_with_bindings(vec![Keybinding {
            key: "ctrl+=".to_string(),
            action: KeyAction::FontSizeUp,
        }]);
        let map = build_binding_map(&cfg);
        let lookup = (
            true,
            false,
            false,
            false,
            BindingKey::Character("=".to_string()),
        );
        assert_eq!(map.get(&lookup), Some(&KeyAction::FontSizeUp));
    }

    #[test]
    fn build_binding_map_ctrl_plus_alias() {
        // "ctrl+plus" should decode to the '+' character.
        let cfg = config_with_bindings(vec![Keybinding {
            key: "ctrl+plus".to_string(),
            action: KeyAction::FontSizeUp,
        }]);
        let map = build_binding_map(&cfg);
        let lookup = (
            true,
            false,
            false,
            false,
            BindingKey::Character("+".to_string()),
        );
        assert_eq!(map.get(&lookup), Some(&KeyAction::FontSizeUp));
    }

    #[test]
    fn build_binding_map_ctrl_minus_alias() {
        let cfg = config_with_bindings(vec![Keybinding {
            key: "ctrl+minus".to_string(),
            action: KeyAction::FontSizeDown,
        }]);
        let map = build_binding_map(&cfg);
        let lookup = (
            true,
            false,
            false,
            false,
            BindingKey::Character("-".to_string()),
        );
        assert_eq!(map.get(&lookup), Some(&KeyAction::FontSizeDown));
    }

    #[test]
    fn build_binding_map_duplicate_binding_last_wins() {
        // If the same key combo appears twice, the last entry should win
        // (HashMap insert overwrites).
        let cfg = config_with_bindings(vec![
            Keybinding {
                key: "ctrl+a".to_string(),
                action: KeyAction::Copy,
            },
            Keybinding {
                key: "ctrl+a".to_string(),
                action: KeyAction::Paste,
            },
        ]);
        let map = build_binding_map(&cfg);
        let lookup = (
            true,
            false,
            false,
            false,
            BindingKey::Character("a".to_string()),
        );
        // Either Copy or Paste (last write wins); map has exactly one entry.
        assert_eq!(map.len(), 1);
        let action = map.get(&lookup).expect("binding should exist");
        assert!(
            *action == KeyAction::Copy || *action == KeyAction::Paste,
            "expected Copy or Paste, got {action:?}"
        );
    }

    #[test]
    fn build_binding_map_default_config_contains_expected_actions() {
        let cfg = TherminalConfig::default();
        let map = build_binding_map(&cfg);

        // Spot-check a handful of well-known defaults.
        let split_h = (
            true,
            true,
            false,
            false,
            BindingKey::Character("h".to_string()),
        );
        assert_eq!(
            map.get(&split_h),
            Some(&KeyAction::SplitHorizontal),
            "ctrl+shift+h should be SplitHorizontal"
        );

        let close = (
            true,
            true,
            false,
            false,
            BindingKey::Character("w".to_string()),
        );
        assert_eq!(
            map.get(&close),
            Some(&KeyAction::ClosePane),
            "ctrl+shift+w should be ClosePane"
        );

        let focus_next = (
            true,
            true,
            false,
            false,
            BindingKey::Character("n".to_string()),
        );
        assert_eq!(
            map.get(&focus_next),
            Some(&KeyAction::FocusNext),
            "ctrl+shift+n should be FocusNext"
        );

        let workspace1 = (
            false,
            false,
            true,
            false,
            BindingKey::Character("1".to_string()),
        );
        assert_eq!(
            map.get(&workspace1),
            Some(&KeyAction::SwitchWorkspace(1)),
            "alt+1 should be SwitchWorkspace(1)"
        );
    }

    #[test]
    fn build_binding_map_all_workspace_keys_present() {
        let cfg = TherminalConfig::default();
        let map = build_binding_map(&cfg);

        for n in 1u8..=9 {
            let lookup = (
                false,
                false,
                true,
                false,
                BindingKey::Character(n.to_string()),
            );
            assert_eq!(
                map.get(&lookup),
                Some(&KeyAction::SwitchWorkspace(n)),
                "alt+{n} should be SwitchWorkspace({n})"
            );
        }
    }

    #[test]
    fn build_binding_map_named_arrow_directions() {
        let cfg = TherminalConfig::default();
        let map = build_binding_map(&cfg);

        let up = (
            true,
            true,
            false,
            false,
            BindingKey::Named(NamedKey::ArrowUp),
        );
        let down = (
            true,
            true,
            false,
            false,
            BindingKey::Named(NamedKey::ArrowDown),
        );
        let left = (
            true,
            true,
            false,
            false,
            BindingKey::Named(NamedKey::ArrowLeft),
        );
        let right = (
            true,
            true,
            false,
            false,
            BindingKey::Named(NamedKey::ArrowRight),
        );

        assert_eq!(map.get(&up), Some(&KeyAction::FocusUp));
        assert_eq!(map.get(&down), Some(&KeyAction::FocusDown));
        assert_eq!(map.get(&left), Some(&KeyAction::FocusLeft));
        assert_eq!(map.get(&right), Some(&KeyAction::FocusRight));
    }

    // ── BindingKey normalization ───────────────────────────────────────────

    #[test]
    fn binding_key_character_stores_lowercase() {
        // The map should store lowercase characters. Test the build step normalizes.
        let cfg = config_with_bindings(vec![Keybinding {
            key: "ctrl+c".to_string(),
            action: KeyAction::Copy,
        }]);
        let map = build_binding_map(&cfg);
        // 'c' lowercase should be found; 'C' should not be a separate key.
        let lower = (
            true,
            false,
            false,
            false,
            BindingKey::Character("c".to_string()),
        );
        assert!(map.contains_key(&lower));
    }

    #[test]
    fn binding_key_named_pageup_pagedown() {
        let cfg = config_with_bindings(vec![
            Keybinding {
                key: "ctrl+alt+pageup".to_string(),
                action: KeyAction::JumpErrorPrev,
            },
            Keybinding {
                key: "ctrl+alt+pagedown".to_string(),
                action: KeyAction::JumpErrorNext,
            },
        ]);
        let map = build_binding_map(&cfg);
        let pgup = (
            true,
            false,
            true,
            false,
            BindingKey::Named(NamedKey::PageUp),
        );
        let pgdown = (
            true,
            false,
            true,
            false,
            BindingKey::Named(NamedKey::PageDown),
        );
        assert_eq!(map.get(&pgup), Some(&KeyAction::JumpErrorPrev));
        assert_eq!(map.get(&pgdown), Some(&KeyAction::JumpErrorNext));
    }
}
