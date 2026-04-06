//! Keybinding types, default bindings, and binding string parser.

use serde::{Deserialize, Serialize};
use tracing::warn;

// ── Section: Keybindings ─────────────────────────────────────────────────

/// Typed action for a keybinding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    /// Copy selected text to the clipboard.
    Copy,
    /// Paste text from the clipboard.
    Paste,
    /// Increase the font size by one step.
    FontSizeUp,
    /// Decrease the font size by one step.
    FontSizeDown,
    /// Reset the font size to the configured default.
    FontSizeReset,
    /// Split the focused pane horizontally (side-by-side).
    SplitHorizontal,
    /// Split the focused pane vertically (top/bottom).
    SplitVertical,
    /// Split the focused pane in auto-detected direction.
    SplitAuto,
    /// Close the currently focused pane.
    ClosePane,
    /// Grow the focused pane's split ratio.
    ResizeGrow,
    /// Shrink the focused pane's split ratio.
    ResizeShrink,
    /// Reset all split ratios to 50/50.
    ResizeReset,
    /// Move focus to the next pane.
    FocusNext,
    /// Move focus to the previous pane.
    FocusPrev,
    /// Move focus up.
    FocusUp,
    /// Move focus down.
    FocusDown,
    /// Move focus left.
    FocusLeft,
    /// Move focus right.
    FocusRight,
    /// Toggle focused pane fullscreen (zoom).
    ZoomPane,
    /// Show the keybinding help overlay.
    ShowHelp,
    /// Close all panes (batch kill).
    CloseAllPanes,
    /// Restore the last closed-all layout.
    RestoreLayout,
    /// Swap focused pane with the next pane.
    SwapNext,
    /// Swap focused pane with the previous pane.
    SwapPrev,
    /// Switch to workspace N (1-9).
    SwitchWorkspace(u8),
    /// Send focused pane to workspace N (1-9).
    SendToWorkspace(u8),
    /// Create a new workspace tab.
    NewWorkspace,
    /// Rename the current (or targeted) workspace tab.
    RenameWorkspace,
    /// Jump scrollback to the previous semantic region.
    JumpRegionPrev,
    /// Jump scrollback to the next semantic region.
    JumpRegionNext,
    /// Jump scrollback to the previous error region.
    JumpErrorPrev,
    /// Jump scrollback to the next error region.
    JumpErrorNext,
    // ── Hotspot actions (used by action palette, not keybindable) ────────
    /// Copy a hotspot text to the clipboard.
    HotspotCopy(String),
    /// Open a file path in the user's `$EDITOR` or via `xdg-open`.
    HotspotOpenInEditor(String),
    /// Open an issue/URL via `xdg-open`.
    HotspotOpenExternal(String),
}

impl KeyAction {
    /// Human-readable description of what this action does.
    pub fn description(&self) -> &'static str {
        match self {
            KeyAction::Copy => "Copy selection",
            KeyAction::Paste => "Paste from clipboard",
            KeyAction::FontSizeUp => "Increase font size",
            KeyAction::FontSizeDown => "Decrease font size",
            KeyAction::FontSizeReset => "Reset font size",
            KeyAction::SplitHorizontal => "Split pane horizontally",
            KeyAction::SplitVertical => "Split pane vertically",
            KeyAction::SplitAuto => "Auto split pane",
            KeyAction::ClosePane => "Close focused pane",
            KeyAction::ResizeGrow => "Grow pane ratio",
            KeyAction::ResizeShrink => "Shrink pane ratio",
            KeyAction::ResizeReset => "Reset pane ratios",
            KeyAction::FocusNext => "Focus next pane",
            KeyAction::FocusPrev => "Focus previous pane",
            KeyAction::FocusUp => "Focus pane above",
            KeyAction::FocusDown => "Focus pane below",
            KeyAction::FocusLeft => "Focus pane left",
            KeyAction::FocusRight => "Focus pane right",
            KeyAction::ZoomPane => "Toggle pane zoom",
            KeyAction::ShowHelp => "Show keybinding help",
            KeyAction::CloseAllPanes => "Close all panes",
            KeyAction::RestoreLayout => "Restore last layout",
            KeyAction::SwapNext => "Swap pane with next",
            KeyAction::SwapPrev => "Swap pane with previous",
            KeyAction::SwitchWorkspace(_) => "Switch workspace",
            KeyAction::SendToWorkspace(_) => "Send pane to workspace",
            KeyAction::NewWorkspace => "New workspace tab",
            KeyAction::RenameWorkspace => "Rename workspace tab",
            KeyAction::JumpRegionPrev => "Jump to previous region",
            KeyAction::JumpRegionNext => "Jump to next region",
            KeyAction::JumpErrorPrev => "Jump to previous error",
            KeyAction::JumpErrorNext => "Jump to next error",
            KeyAction::HotspotCopy(_) => "Copy to clipboard",
            KeyAction::HotspotOpenInEditor(_) => "Open in editor",
            KeyAction::HotspotOpenExternal(_) => "Open externally",
        }
    }

    /// Which section this action belongs to in the help overlay.
    pub fn section(&self) -> &'static str {
        match self {
            KeyAction::SplitHorizontal
            | KeyAction::SplitVertical
            | KeyAction::SplitAuto
            | KeyAction::ClosePane
            | KeyAction::ResizeGrow
            | KeyAction::ResizeShrink
            | KeyAction::ResizeReset
            | KeyAction::FocusNext
            | KeyAction::FocusPrev
            | KeyAction::FocusUp
            | KeyAction::FocusDown
            | KeyAction::FocusLeft
            | KeyAction::FocusRight
            | KeyAction::ZoomPane
            | KeyAction::CloseAllPanes
            | KeyAction::RestoreLayout
            | KeyAction::SwapNext
            | KeyAction::SwapPrev
            | KeyAction::SwitchWorkspace(_)
            | KeyAction::SendToWorkspace(_)
            | KeyAction::NewWorkspace
            | KeyAction::RenameWorkspace => "Pane Management",
            KeyAction::JumpRegionPrev
            | KeyAction::JumpRegionNext
            | KeyAction::JumpErrorPrev
            | KeyAction::JumpErrorNext => "Navigation",
            KeyAction::FontSizeUp | KeyAction::FontSizeDown | KeyAction::FontSizeReset => "Font",
            KeyAction::Copy | KeyAction::Paste | KeyAction::ShowHelp => "General",
            KeyAction::HotspotCopy(_)
            | KeyAction::HotspotOpenInEditor(_)
            | KeyAction::HotspotOpenExternal(_) => "Hotspot",
        }
    }
}

/// A single keybinding entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybinding {
    /// Key combination (e.g. "ctrl+shift+c", "ctrl+plus").
    pub key: String,
    /// Action to perform when this keybinding is triggered.
    pub action: KeyAction,
}

/// Keybinding configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeybindingsConfig {
    /// List of keybinding overrides. These are merged on top of defaults.
    pub bindings: Vec<Keybinding>,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            bindings: vec![
                // Clipboard
                Keybinding {
                    key: "ctrl+shift+c".to_string(),
                    action: KeyAction::Copy,
                },
                Keybinding {
                    key: "ctrl+shift+v".to_string(),
                    action: KeyAction::Paste,
                },
                // Font sizing
                Keybinding {
                    key: "ctrl+=".to_string(),
                    action: KeyAction::FontSizeUp,
                },
                Keybinding {
                    key: "ctrl+minus".to_string(),
                    action: KeyAction::FontSizeDown,
                },
                Keybinding {
                    key: "ctrl+0".to_string(),
                    action: KeyAction::FontSizeReset,
                },
                // Pane splits
                Keybinding {
                    key: "ctrl+shift+h".to_string(),
                    action: KeyAction::SplitHorizontal,
                },
                Keybinding {
                    key: "ctrl+shift+d".to_string(),
                    action: KeyAction::SplitVertical,
                },
                Keybinding {
                    key: "ctrl+shift+enter".to_string(),
                    action: KeyAction::SplitAuto,
                },
                Keybinding {
                    key: "alt+enter".to_string(),
                    action: KeyAction::SplitAuto,
                },
                // Pane management
                Keybinding {
                    key: "ctrl+shift+w".to_string(),
                    action: KeyAction::ClosePane,
                },
                Keybinding {
                    key: "ctrl+shift+=".to_string(),
                    action: KeyAction::ResizeGrow,
                },
                Keybinding {
                    key: "ctrl+shift+-".to_string(),
                    action: KeyAction::ResizeShrink,
                },
                Keybinding {
                    key: "ctrl+shift+0".to_string(),
                    action: KeyAction::ResizeReset,
                },
                // Focus movement (directional arrows)
                Keybinding {
                    key: "ctrl+shift+up".to_string(),
                    action: KeyAction::FocusUp,
                },
                Keybinding {
                    key: "ctrl+shift+down".to_string(),
                    action: KeyAction::FocusDown,
                },
                Keybinding {
                    key: "ctrl+shift+left".to_string(),
                    action: KeyAction::FocusLeft,
                },
                Keybinding {
                    key: "ctrl+shift+right".to_string(),
                    action: KeyAction::FocusRight,
                },
                // Focus cycling
                Keybinding {
                    key: "ctrl+shift+n".to_string(),
                    action: KeyAction::FocusNext,
                },
                Keybinding {
                    key: "ctrl+shift+p".to_string(),
                    action: KeyAction::FocusPrev,
                },
                // Zoom
                Keybinding {
                    key: "ctrl+shift+z".to_string(),
                    action: KeyAction::ZoomPane,
                },
                // Help overlay (both / and ? for cross-platform compat —
                // Windows may not unshift the key in key_without_modifiers)
                Keybinding {
                    key: "ctrl+shift+/".to_string(),
                    action: KeyAction::ShowHelp,
                },
                Keybinding {
                    key: "ctrl+shift+?".to_string(),
                    action: KeyAction::ShowHelp,
                },
                // Batch pane operations
                Keybinding {
                    key: "ctrl+shift+q".to_string(),
                    action: KeyAction::CloseAllPanes,
                },
                Keybinding {
                    key: "ctrl+shift+u".to_string(),
                    action: KeyAction::RestoreLayout,
                },
                // New workspace tab
                Keybinding {
                    key: "ctrl+shift+t".to_string(),
                    action: KeyAction::NewWorkspace,
                },
                // Pane swap
                Keybinding {
                    key: "alt+shift+right".to_string(),
                    action: KeyAction::SwapNext,
                },
                Keybinding {
                    key: "alt+shift+left".to_string(),
                    action: KeyAction::SwapPrev,
                },
                // Workspace switching (Alt+1 through Alt+9)
                Keybinding {
                    key: "alt+1".to_string(),
                    action: KeyAction::SwitchWorkspace(1),
                },
                Keybinding {
                    key: "alt+2".to_string(),
                    action: KeyAction::SwitchWorkspace(2),
                },
                Keybinding {
                    key: "alt+3".to_string(),
                    action: KeyAction::SwitchWorkspace(3),
                },
                Keybinding {
                    key: "alt+4".to_string(),
                    action: KeyAction::SwitchWorkspace(4),
                },
                Keybinding {
                    key: "alt+5".to_string(),
                    action: KeyAction::SwitchWorkspace(5),
                },
                Keybinding {
                    key: "alt+6".to_string(),
                    action: KeyAction::SwitchWorkspace(6),
                },
                Keybinding {
                    key: "alt+7".to_string(),
                    action: KeyAction::SwitchWorkspace(7),
                },
                Keybinding {
                    key: "alt+8".to_string(),
                    action: KeyAction::SwitchWorkspace(8),
                },
                Keybinding {
                    key: "alt+9".to_string(),
                    action: KeyAction::SwitchWorkspace(9),
                },
                // Send pane to workspace (Alt+Shift+1 through Alt+Shift+9)
                Keybinding {
                    key: "alt+shift+1".to_string(),
                    action: KeyAction::SendToWorkspace(1),
                },
                Keybinding {
                    key: "alt+shift+2".to_string(),
                    action: KeyAction::SendToWorkspace(2),
                },
                Keybinding {
                    key: "alt+shift+3".to_string(),
                    action: KeyAction::SendToWorkspace(3),
                },
                Keybinding {
                    key: "alt+shift+4".to_string(),
                    action: KeyAction::SendToWorkspace(4),
                },
                Keybinding {
                    key: "alt+shift+5".to_string(),
                    action: KeyAction::SendToWorkspace(5),
                },
                Keybinding {
                    key: "alt+shift+6".to_string(),
                    action: KeyAction::SendToWorkspace(6),
                },
                Keybinding {
                    key: "alt+shift+7".to_string(),
                    action: KeyAction::SendToWorkspace(7),
                },
                Keybinding {
                    key: "alt+shift+8".to_string(),
                    action: KeyAction::SendToWorkspace(8),
                },
                Keybinding {
                    key: "alt+shift+9".to_string(),
                    action: KeyAction::SendToWorkspace(9),
                },
                // Semantic scrollback navigation
                Keybinding {
                    key: "ctrl+alt+up".to_string(),
                    action: KeyAction::JumpRegionPrev,
                },
                Keybinding {
                    key: "ctrl+alt+down".to_string(),
                    action: KeyAction::JumpRegionNext,
                },
                Keybinding {
                    key: "ctrl+alt+pageup".to_string(),
                    action: KeyAction::JumpErrorPrev,
                },
                Keybinding {
                    key: "ctrl+alt+pagedown".to_string(),
                    action: KeyAction::JumpErrorNext,
                },
            ],
        }
    }
}

// ── Binding parser ──────────────────────────────────────────────────────

/// Modifier flags produced by [`parse_binding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ParsedModifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_key: bool,
}

/// Key identifier produced by [`parse_binding`].
///
/// This is a platform-independent representation that maps 1:1 to
/// `winit::keyboard::Key` variants.  The conversion happens in the app
/// crate so that `therminal-core` stays free of windowing dependencies.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ParsedKey {
    /// A single printable character (a-z, 0-9, punctuation).
    Character(String),
    /// A named key (Enter, Tab, Escape, Arrow*, F1-F12, etc.).
    Named(ParsedNamedKey),
}

/// Named (non-character) keys recognized by the binding parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParsedNamedKey {
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Space,
}

/// Parse a keybinding string like `"ctrl+shift+h"` into modifiers and key.
///
/// Returns `None` (and logs a warning) if the binding string is invalid.
///
/// Supported modifier names: `ctrl`, `shift`, `alt`, `super`.
/// Supported key names: `a`-`z`, `0`-`9`, `f1`-`f12`, arrow keys,
/// `enter`, `tab`, `escape`, `backspace`, `delete`, `insert`, `home`,
/// `end`, `pageup`, `pagedown`, `space`, and single-character
/// punctuation (`+`, `-`, `=`, `[`, `]`, `\`, `;`, `'`, `,`, `.`, `/`).
///
/// The special token `plus` is treated as the `+` character key, and
/// `minus` as the `-` character key, so they can coexist with the `+`
/// separator.
pub fn parse_binding(binding: &str) -> Option<(ParsedModifiers, ParsedKey)> {
    let parts: Vec<&str> = binding.split('+').collect();
    if parts.is_empty() {
        warn!(binding, "empty keybinding string");
        return None;
    }

    let mut mods = ParsedModifiers::default();
    let mut key_string: Option<String> = None;

    for (i, part) in parts.iter().enumerate() {
        let lower = part.trim().to_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => mods.ctrl = true,
            "shift" => mods.shift = true,
            "alt" | "option" => mods.alt = true,
            "super" | "meta" | "cmd" | "win" => mods.super_key = true,
            _ => {
                // Must be the key component — should be the last part.
                if i != parts.len() - 1 {
                    // There are more parts after this non-modifier.
                    // Join everything from here with '+' (handles e.g.
                    // accidental extra '+' in the binding string).
                    key_string = Some(parts[i..].join("+"));
                    break;
                }
                key_string = Some(part.trim().to_string());
            }
        }
    }

    let key_str = match key_string.as_deref() {
        Some(k) if !k.is_empty() => k,
        _ => {
            warn!(binding, "keybinding has no key component");
            return None;
        }
    };

    let key_lower = key_str.to_lowercase();
    let parsed_key = match key_lower.as_str() {
        // Named keys
        "enter" | "return" => ParsedKey::Named(ParsedNamedKey::Enter),
        "tab" => ParsedKey::Named(ParsedNamedKey::Tab),
        "escape" | "esc" => ParsedKey::Named(ParsedNamedKey::Escape),
        "backspace" => ParsedKey::Named(ParsedNamedKey::Backspace),
        "delete" | "del" => ParsedKey::Named(ParsedNamedKey::Delete),
        "insert" | "ins" => ParsedKey::Named(ParsedNamedKey::Insert),
        "home" => ParsedKey::Named(ParsedNamedKey::Home),
        "end" => ParsedKey::Named(ParsedNamedKey::End),
        "pageup" | "page_up" => ParsedKey::Named(ParsedNamedKey::PageUp),
        "pagedown" | "page_down" => ParsedKey::Named(ParsedNamedKey::PageDown),
        "up" | "arrowup" => ParsedKey::Named(ParsedNamedKey::ArrowUp),
        "down" | "arrowdown" => ParsedKey::Named(ParsedNamedKey::ArrowDown),
        "left" | "arrowleft" => ParsedKey::Named(ParsedNamedKey::ArrowLeft),
        "right" | "arrowright" => ParsedKey::Named(ParsedNamedKey::ArrowRight),
        "space" => ParsedKey::Named(ParsedNamedKey::Space),
        "f1" => ParsedKey::Named(ParsedNamedKey::F1),
        "f2" => ParsedKey::Named(ParsedNamedKey::F2),
        "f3" => ParsedKey::Named(ParsedNamedKey::F3),
        "f4" => ParsedKey::Named(ParsedNamedKey::F4),
        "f5" => ParsedKey::Named(ParsedNamedKey::F5),
        "f6" => ParsedKey::Named(ParsedNamedKey::F6),
        "f7" => ParsedKey::Named(ParsedNamedKey::F7),
        "f8" => ParsedKey::Named(ParsedNamedKey::F8),
        "f9" => ParsedKey::Named(ParsedNamedKey::F9),
        "f10" => ParsedKey::Named(ParsedNamedKey::F10),
        "f11" => ParsedKey::Named(ParsedNamedKey::F11),
        "f12" => ParsedKey::Named(ParsedNamedKey::F12),
        // Aliases for punctuation that conflicts with the '+' separator
        "plus" => ParsedKey::Character("+".to_string()),
        "minus" => ParsedKey::Character("-".to_string()),
        // Single-character keys (letters, digits, punctuation)
        s if s.len() == 1 => ParsedKey::Character(s.to_string()),
        _ => {
            warn!(binding, key = key_str, "unrecognized key in keybinding");
            return None;
        }
    };

    Some((mods, parsed_key))
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keybindings_default_has_copy_paste() {
        let kb = KeybindingsConfig::default();
        assert!(kb.bindings.iter().any(|b| b.action == KeyAction::Copy));
        assert!(kb.bindings.iter().any(|b| b.action == KeyAction::Paste));
    }

    #[test]
    fn keybindings_default_has_pane_actions() {
        let kb = KeybindingsConfig::default();
        assert!(
            kb.bindings
                .iter()
                .any(|b| b.action == KeyAction::SplitHorizontal)
        );
        assert!(kb.bindings.iter().any(|b| b.action == KeyAction::ClosePane));
        assert!(kb.bindings.iter().any(|b| b.action == KeyAction::ZoomPane));
        assert!(kb.bindings.iter().any(|b| b.action == KeyAction::FocusNext));
    }

    #[test]
    fn parse_binding_ctrl_shift_h() {
        let (mods, key) = parse_binding("ctrl+shift+h").unwrap();
        assert!(mods.ctrl);
        assert!(mods.shift);
        assert!(!mods.alt);
        assert!(!mods.super_key);
        assert_eq!(key, ParsedKey::Character("h".to_string()));
    }

    #[test]
    fn parse_binding_ctrl_plus() {
        let (mods, key) = parse_binding("ctrl+plus").unwrap();
        assert!(mods.ctrl);
        assert!(!mods.shift);
        assert_eq!(key, ParsedKey::Character("+".to_string()));
    }

    #[test]
    fn parse_binding_ctrl_shift_enter() {
        let (mods, key) = parse_binding("ctrl+shift+enter").unwrap();
        assert!(mods.ctrl);
        assert!(mods.shift);
        assert_eq!(key, ParsedKey::Named(ParsedNamedKey::Enter));
    }

    #[test]
    fn parse_binding_arrow_keys() {
        let (mods, key) = parse_binding("ctrl+shift+up").unwrap();
        assert!(mods.ctrl);
        assert!(mods.shift);
        assert_eq!(key, ParsedKey::Named(ParsedNamedKey::ArrowUp));
    }

    #[test]
    fn parse_binding_function_keys() {
        let (_, key) = parse_binding("f12").unwrap();
        assert_eq!(key, ParsedKey::Named(ParsedNamedKey::F12));
    }

    #[test]
    fn parse_binding_equals_sign() {
        let (mods, key) = parse_binding("ctrl+shift+=").unwrap();
        assert!(mods.ctrl);
        assert!(mods.shift);
        assert_eq!(key, ParsedKey::Character("=".to_string()));
    }

    #[test]
    fn parse_binding_minus_sign() {
        let (mods, key) = parse_binding("ctrl+shift+-").unwrap();
        assert!(mods.ctrl);
        assert!(mods.shift);
        assert_eq!(key, ParsedKey::Character("-".to_string()));
    }

    #[test]
    fn parse_binding_invalid_returns_none() {
        assert!(parse_binding("").is_none());
        assert!(parse_binding("ctrl+shift+foobar").is_none());
    }

    #[test]
    fn parse_binding_alt_enter() {
        let (mods, key) = parse_binding("alt+enter").unwrap();
        assert!(!mods.ctrl);
        assert!(!mods.shift);
        assert!(mods.alt);
        assert!(!mods.super_key);
        assert_eq!(key, ParsedKey::Named(ParsedNamedKey::Enter));
    }

    #[test]
    fn parse_binding_alt_super() {
        let (mods, key) = parse_binding("alt+super+a").unwrap();
        assert!(!mods.ctrl);
        assert!(!mods.shift);
        assert!(mods.alt);
        assert!(mods.super_key);
        assert_eq!(key, ParsedKey::Character("a".to_string()));
    }
}
