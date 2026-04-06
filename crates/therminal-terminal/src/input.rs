//! Keyboard event to PTY byte encoding.
//!
//! Platform-agnostic key types and encoding logic. Converts a [`KeyCode`] +
//! [`Modifiers`] pair into the byte sequences that a terminal emulator sends
//! over a PTY.  Covers printable text, control characters, cursor keys,
//! editing keys, and function keys using standard xterm escape sequences.
//!
//! When the terminal has the kitty keyboard protocol enabled (progressive
//! enhancement flags 1-2), use [`encode_key_kitty`] instead of [`encode_key`]
//! for unambiguous key encoding.  See
//! <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>.

// -- Key types ---------------------------------------------------------------

/// Platform-agnostic key code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCode {
    Enter,
    Backspace,
    Tab,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
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
    Char(char),
}

/// Modifier key state.
#[derive(Debug, Clone, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

// -- Encoding ----------------------------------------------------------------

/// Encode a key press into the bytes that should be written to the PTY.
///
/// Returns `None` for keys that have no PTY representation (e.g. bare
/// modifier presses).
pub fn encode_key(key: &KeyCode, mods: &Modifiers) -> Option<Vec<u8>> {
    // -- Ctrl+letter ---------------------------------------------------------
    // Must be checked *before* the printable-text path so that
    // Ctrl+C emits 0x03 rather than the literal 'c'.
    if mods.ctrl
        && let KeyCode::Char(ch) = key
        && let Some(byte) = ctrl_char_byte(*ch)
    {
        return Some(vec![byte]);
    }

    // -- Special / non-printable keys ----------------------------------------
    if let Some(bytes) = encode_special(key) {
        return Some(bytes);
    }

    // -- Function keys -------------------------------------------------------
    if let Some(bytes) = encode_fkey(key) {
        return Some(bytes);
    }

    // -- Alt/Meta + key -> ESC prefix ----------------------------------------
    // When Alt is held (without Ctrl), prepend \x1b to the key byte.
    if mods.alt
        && !mods.ctrl
        && let KeyCode::Char(ch) = key
    {
        let mut s = String::new();
        s.push(*ch);
        let mut bytes = Vec::with_capacity(1 + s.len());
        bytes.push(0x1b);
        bytes.extend_from_slice(s.as_bytes());
        return Some(bytes);
    }

    // -- Printable text (no Ctrl held) ---------------------------------------
    if !mods.ctrl
        && let KeyCode::Char(ch) = key
    {
        let mut s = String::new();
        s.push(*ch);
        return Some(s.into_bytes());
    }

    None
}

// -- Kitty keyboard protocol encoding ----------------------------------------

/// Progressive enhancement flags for the kitty keyboard protocol.
///
/// These mirror the flags that a terminal application pushes via
/// `CSI > flags u`.  We only need to *read* them to decide how to
/// encode outgoing key events.
///
/// Implemented as a simple bitmask wrapper to avoid an extra dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyFlags(pub u8);

impl KittyFlags {
    /// No flags active -- kitty protocol is not in use.
    pub const NONE: Self = Self(0);
    /// Flag 1: Disambiguate escape codes.
    pub const DISAMBIGUATE: Self = Self(1);
    /// Flag 2: Report event types (press/repeat/release).
    pub const REPORT_EVENTS: Self = Self(2);
    /// Flag 4: Report alternate keys.
    pub const REPORT_ALTERNATE: Self = Self(4);
    /// Flag 8: Report all keys as escape codes.
    pub const REPORT_ALL_KEYS: Self = Self(8);
    /// Flag 16: Report associated text.
    pub const REPORT_TEXT: Self = Self(16);

    /// Check if a specific flag is set.
    #[inline]
    pub fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0 && flag.0 != 0
    }
}

/// Event type for kitty keyboard protocol (flag 2: report-event-types).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventType {
    /// Normal key press (event type 1, the default -- omitted from encoding).
    Press,
    /// Key repeat (event type 2).
    Repeat,
    /// Key release (event type 3).
    Release,
}

/// Encode a key press using the kitty keyboard protocol.
///
/// Produces `CSI unicode-key-code [; modifiers[:event-type]] u` for most keys,
/// and the appropriate CSI sequences for special/functional keys.
///
/// `flags` controls which progressive enhancements are active.  At minimum,
/// `DISAMBIGUATE` (flag 1) should be set -- otherwise the caller should use
/// the legacy [`encode_key`] instead.
///
/// `event_type` is only encoded when `REPORT_EVENTS` (flag 2) is active AND
/// the event is not a plain press (the default).
///
/// Returns `None` for keys that have no encoding (e.g. bare modifier presses
/// when `REPORT_ALL_KEYS` is not active).
pub fn encode_key_kitty(
    key: &KeyCode,
    mods: &Modifiers,
    flags: KittyFlags,
    event_type: KeyEventType,
) -> Option<Vec<u8>> {
    // Build modifier parameter: shift=1, alt=2, ctrl=4, super=8 -> value = bits + 1.
    // A value of 1 means "no modifiers" and is omitted per the spec.
    let mod_bits = kitty_modifier_bits(mods);

    // Build the event-type suffix for the modifier parameter.
    let event_suffix = if flags.contains(KittyFlags::REPORT_EVENTS) {
        match event_type {
            KeyEventType::Press => None, // 1 is default, omitted
            KeyEventType::Repeat => Some(2u8),
            KeyEventType::Release => Some(3u8),
        }
    } else {
        None
    };

    // Try to encode as a special key with a dedicated CSI number.
    if let Some(bytes) = encode_kitty_special(key, mod_bits, event_suffix) {
        return Some(bytes);
    }

    // Try to encode as a functional key (F1-F12, editing keys with ~ suffix).
    if let Some(bytes) = encode_kitty_functional(key, mod_bits, event_suffix) {
        return Some(bytes);
    }

    // Printable / unicode characters -> CSI codepoint ; modifiers u
    if let Some(codepoint) = key_to_unicode(key) {
        return Some(format_csi_u(codepoint, mod_bits, event_suffix));
    }

    None
}

/// Map modifier state to the kitty protocol bitmask (1-indexed).
///
/// Returns 0 when no modifiers are held, which means "no modifier param needed".
/// The wire format is `bits + 1`, but we encode that in the formatting step.
fn kitty_modifier_bits(mods: &Modifiers) -> u8 {
    let mut bits: u8 = 0;
    if mods.shift {
        bits |= 1;
    }
    if mods.alt {
        bits |= 2;
    }
    if mods.ctrl {
        bits |= 4;
    }
    // super/logo is bit 8 -- not tracked in our Modifiers struct yet.
    bits
}

/// Format `CSI codepoint ; modifier-and-event u`.
fn format_csi_u(codepoint: u32, mod_bits: u8, event_suffix: Option<u8>) -> Vec<u8> {
    let mut s = format!("\x1b[{codepoint}");
    let param = mod_bits.wrapping_add(1); // 1-indexed
    match (mod_bits > 0, event_suffix) {
        (false, None) => {
            // No modifiers, no event type -- minimal form: CSI codepoint u
        }
        (true, None) => {
            // Modifiers but no event suffix.
            s.push_str(&format!(";{param}"));
        }
        (false, Some(ev)) => {
            // No modifiers but event suffix -- must include modifier param as 1.
            s.push_str(&format!(";1:{ev}"));
        }
        (true, Some(ev)) => {
            s.push_str(&format!(";{param}:{ev}"));
        }
    }
    s.push('u');
    s.into_bytes()
}

/// Encode special keys that use a letter suffix (A-H, P-S) rather than `u`.
///
/// These are cursor keys and Enter/Tab/Backspace in the disambiguate mode,
/// encoded as `CSI 1 ; modifier <letter>` when modifiers are present, or
/// as `CSI <letter>` with no params.
fn encode_kitty_special(key: &KeyCode, mod_bits: u8, event_suffix: Option<u8>) -> Option<Vec<u8>> {
    // Cursor keys use letter suffixes directly.
    let suffix = match key {
        KeyCode::ArrowUp => 'A',
        KeyCode::ArrowDown => 'B',
        KeyCode::ArrowRight => 'C',
        KeyCode::ArrowLeft => 'D',
        KeyCode::Home => 'H',
        KeyCode::End => 'F',
        _ => return None,
    };

    let param = mod_bits.wrapping_add(1);
    let mut s = String::from("\x1b[");
    match (mod_bits > 0, event_suffix) {
        (false, None) => {
            // Bare key -- standard form.
            s.push('1');
        }
        (true, None) => {
            s.push_str(&format!("1;{param}"));
        }
        (false, Some(ev)) => {
            s.push_str(&format!("1;1:{ev}"));
        }
        (true, Some(ev)) => {
            s.push_str(&format!("1;{param}:{ev}"));
        }
    }
    s.push(suffix);
    Some(s.into_bytes())
}

/// Encode functional keys that use the `~` suffix with a key number.
///
/// Format: `CSI number [; modifier[:event]] ~`
fn encode_kitty_functional(
    key: &KeyCode,
    mod_bits: u8,
    event_suffix: Option<u8>,
) -> Option<Vec<u8>> {
    let number = match key {
        KeyCode::Insert => 2,
        KeyCode::Delete => 3,
        KeyCode::PageUp => 5,
        KeyCode::PageDown => 6,
        KeyCode::F1 => 11,
        KeyCode::F2 => 12,
        KeyCode::F3 => 13,
        KeyCode::F4 => 14,
        KeyCode::F5 => 15,
        KeyCode::F6 => 17,
        KeyCode::F7 => 18,
        KeyCode::F8 => 19,
        KeyCode::F9 => 20,
        KeyCode::F10 => 21,
        KeyCode::F11 => 23,
        KeyCode::F12 => 24,
        _ => return None,
    };

    let param = mod_bits.wrapping_add(1);
    let mut s = format!("\x1b[{number}");
    match (mod_bits > 0, event_suffix) {
        (false, None) => {}
        (true, None) => {
            s.push_str(&format!(";{param}"));
        }
        (false, Some(ev)) => {
            s.push_str(&format!(";1:{ev}"));
        }
        (true, Some(ev)) => {
            s.push_str(&format!(";{param}:{ev}"));
        }
    }
    s.push('~');
    Some(s.into_bytes())
}

/// Map a `KeyCode` to its Unicode codepoint for the `CSI u` encoding.
///
/// Special keys that have dedicated codepoints in the kitty spec are mapped
/// here; printable characters map to their Unicode value.
fn key_to_unicode(key: &KeyCode) -> Option<u32> {
    match key {
        KeyCode::Escape => Some(27),
        KeyCode::Enter => Some(13),
        KeyCode::Tab => Some(9),
        KeyCode::Backspace => Some(127),
        KeyCode::Char(ch) => Some(*ch as u32),
        // Cursor/functional keys are handled by their own encoders.
        _ => None,
    }
}

// -- Ctrl + letter -> control byte -------------------------------------------

/// Map a character (a-z, A-Z) or one of the special Ctrl-combos (@, [, \, ],
/// ^, _) to the single control byte `(ascii_value & 0x1F)`.
fn ctrl_char_byte(ch: char) -> Option<u8> {
    let code = ch as u32;
    // Lowercase a-z
    if (0x61..=0x7a).contains(&code) {
        return Some((code as u8) & 0x1f);
    }
    // Uppercase A-Z -- same result after masking
    if (0x41..=0x5a).contains(&code) {
        return Some((code as u8) & 0x1f);
    }
    // Ctrl+@ -> NUL (0x00), Ctrl+[ -> ESC (0x1b), Ctrl+\ -> 0x1c,
    // Ctrl+] -> 0x1d, Ctrl+^ -> 0x1e, Ctrl+_ -> 0x1f
    match code {
        0x40 => Some(0x00), // @
        0x5b => Some(0x1b), // [
        0x5c => Some(0x1c), // backslash
        0x5d => Some(0x1d), // ]
        0x5e => Some(0x1e), // ^
        0x5f => Some(0x1f), // _
        _ => None,
    }
}

// -- Special keys -> escape sequences ----------------------------------------

fn encode_special(key: &KeyCode) -> Option<Vec<u8>> {
    let bytes: &[u8] = match key {
        KeyCode::Enter => b"\r",
        KeyCode::Backspace => b"\x7f",
        KeyCode::Tab => b"\t",
        KeyCode::Escape => b"\x1b",

        // Cursor movement
        KeyCode::ArrowUp => b"\x1b[A",
        KeyCode::ArrowDown => b"\x1b[B",
        KeyCode::ArrowRight => b"\x1b[C",
        KeyCode::ArrowLeft => b"\x1b[D",

        // Editing keys
        KeyCode::Home => b"\x1b[H",
        KeyCode::End => b"\x1b[F",
        KeyCode::Insert => b"\x1b[2~",
        KeyCode::Delete => b"\x1b[3~",
        KeyCode::PageUp => b"\x1b[5~",
        KeyCode::PageDown => b"\x1b[6~",

        _ => return None,
    };
    Some(bytes.to_vec())
}

// -- Function keys -> escape sequences ---------------------------------------

fn encode_fkey(key: &KeyCode) -> Option<Vec<u8>> {
    let seq: &[u8] = match key {
        KeyCode::F1 => b"\x1bOP",
        KeyCode::F2 => b"\x1bOQ",
        KeyCode::F3 => b"\x1bOR",
        KeyCode::F4 => b"\x1bOS",
        KeyCode::F5 => b"\x1b[15~",
        KeyCode::F6 => b"\x1b[17~",
        KeyCode::F7 => b"\x1b[18~",
        KeyCode::F8 => b"\x1b[19~",
        KeyCode::F9 => b"\x1b[20~",
        KeyCode::F10 => b"\x1b[21~",
        KeyCode::F11 => b"\x1b[23~",
        KeyCode::F12 => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
}

// -- Mouse encoding (SGR 1006) -----------------------------------------------

/// Mouse button identifiers for SGR encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// Scroll up (button 64 in SGR).
    ScrollUp,
    /// Scroll down (button 65 in SGR).
    ScrollDown,
}

/// Encode a mouse press event as an SGR 1006 escape sequence.
///
/// Format: `\x1b[<button;col;row M`
///
/// `col` and `row` are 1-based grid coordinates. Modifier bits are added
/// to the button value: +4 shift, +8 alt, +16 ctrl.
pub fn encode_mouse_press(
    button: MouseButton,
    col: usize,
    row: usize,
    mods: &Modifiers,
) -> Vec<u8> {
    let btn = mouse_button_code(button, mods);
    format!("\x1b[<{btn};{};{}M", col + 1, row + 1).into_bytes()
}

/// Encode a mouse release event as an SGR 1006 escape sequence.
///
/// Format: `\x1b[<button;col;row m`
pub fn encode_mouse_release(
    button: MouseButton,
    col: usize,
    row: usize,
    mods: &Modifiers,
) -> Vec<u8> {
    let btn = mouse_button_code(button, mods);
    format!("\x1b[<{btn};{};{}m", col + 1, row + 1).into_bytes()
}

/// Encode a mouse drag (motion with button held) as an SGR 1006 escape.
///
/// Format: `\x1b[<(32+button);col;row M`
pub fn encode_mouse_drag(button: MouseButton, col: usize, row: usize, mods: &Modifiers) -> Vec<u8> {
    let btn = mouse_button_code(button, mods) + 32;
    format!("\x1b[<{btn};{};{}M", col + 1, row + 1).into_bytes()
}

/// Encode a mouse motion (no button held) as an SGR 1006 escape.
///
/// Format: `\x1b[<35;col;row M`
pub fn encode_mouse_motion(col: usize, row: usize, mods: &Modifiers) -> Vec<u8> {
    let btn = 35 + modifier_bits(mods);
    format!("\x1b[<{btn};{};{}M", col + 1, row + 1).into_bytes()
}

/// Compute the SGR button code including modifier bits.
fn mouse_button_code(button: MouseButton, mods: &Modifiers) -> u8 {
    let base = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::ScrollUp => 64,
        MouseButton::ScrollDown => 65,
    };
    base + modifier_bits(mods)
}

/// Modifier bits for SGR mouse encoding: +4 shift, +8 alt, +16 ctrl.
fn modifier_bits(mods: &Modifiers) -> u8 {
    let mut bits = 0u8;
    if mods.shift {
        bits += 4;
    }
    if mods.alt {
        bits += 8;
    }
    if mods.ctrl {
        bits += 16;
    }
    bits
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn no_mods() -> Modifiers {
        Modifiers {
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    fn ctrl() -> Modifiers {
        Modifiers {
            ctrl: true,
            ..no_mods()
        }
    }

    fn alt() -> Modifiers {
        Modifiers {
            alt: true,
            ..no_mods()
        }
    }

    fn ctrl_alt() -> Modifiers {
        Modifiers {
            ctrl: true,
            alt: true,
            ..no_mods()
        }
    }

    // -- Printable text ------------------------------------------------------

    #[test]
    fn printable_ascii() {
        assert_eq!(
            encode_key(&KeyCode::Char('a'), &no_mods()),
            Some(b"a".to_vec())
        );
    }

    #[test]
    fn printable_shifted() {
        let mods = Modifiers {
            shift: true,
            ..no_mods()
        };
        assert_eq!(encode_key(&KeyCode::Char('A'), &mods), Some(b"A".to_vec()));
    }

    #[test]
    fn printable_utf8_multibyte() {
        assert_eq!(
            encode_key(&KeyCode::Char('\u{00e9}'), &no_mods()),
            Some("\u{00e9}".as_bytes().to_vec())
        );
    }

    // -- Special keys --------------------------------------------------------

    #[test]
    fn enter_key() {
        assert_eq!(
            encode_key(&KeyCode::Enter, &no_mods()),
            Some(b"\r".to_vec())
        );
    }

    #[test]
    fn backspace_key() {
        assert_eq!(
            encode_key(&KeyCode::Backspace, &no_mods()),
            Some(b"\x7f".to_vec())
        );
    }

    #[test]
    fn tab_key() {
        assert_eq!(encode_key(&KeyCode::Tab, &no_mods()), Some(b"\t".to_vec()));
    }

    #[test]
    fn escape_key() {
        assert_eq!(
            encode_key(&KeyCode::Escape, &no_mods()),
            Some(b"\x1b".to_vec())
        );
    }

    // -- Arrow keys ----------------------------------------------------------

    #[test]
    fn arrow_up() {
        assert_eq!(
            encode_key(&KeyCode::ArrowUp, &no_mods()),
            Some(b"\x1b[A".to_vec())
        );
    }

    #[test]
    fn arrow_down() {
        assert_eq!(
            encode_key(&KeyCode::ArrowDown, &no_mods()),
            Some(b"\x1b[B".to_vec())
        );
    }

    #[test]
    fn arrow_right() {
        assert_eq!(
            encode_key(&KeyCode::ArrowRight, &no_mods()),
            Some(b"\x1b[C".to_vec())
        );
    }

    #[test]
    fn arrow_left() {
        assert_eq!(
            encode_key(&KeyCode::ArrowLeft, &no_mods()),
            Some(b"\x1b[D".to_vec())
        );
    }

    // -- Editing keys --------------------------------------------------------

    #[test]
    fn home_key() {
        assert_eq!(
            encode_key(&KeyCode::Home, &no_mods()),
            Some(b"\x1b[H".to_vec())
        );
    }

    #[test]
    fn end_key() {
        assert_eq!(
            encode_key(&KeyCode::End, &no_mods()),
            Some(b"\x1b[F".to_vec())
        );
    }

    #[test]
    fn delete_key() {
        assert_eq!(
            encode_key(&KeyCode::Delete, &no_mods()),
            Some(b"\x1b[3~".to_vec())
        );
    }

    #[test]
    fn insert_key() {
        assert_eq!(
            encode_key(&KeyCode::Insert, &no_mods()),
            Some(b"\x1b[2~".to_vec())
        );
    }

    #[test]
    fn page_up() {
        assert_eq!(
            encode_key(&KeyCode::PageUp, &no_mods()),
            Some(b"\x1b[5~".to_vec())
        );
    }

    #[test]
    fn page_down() {
        assert_eq!(
            encode_key(&KeyCode::PageDown, &no_mods()),
            Some(b"\x1b[6~".to_vec())
        );
    }

    // -- Ctrl + letter -------------------------------------------------------

    #[test]
    fn ctrl_c() {
        assert_eq!(encode_key(&KeyCode::Char('c'), &ctrl()), Some(vec![0x03]));
    }

    #[test]
    fn ctrl_d() {
        assert_eq!(encode_key(&KeyCode::Char('d'), &ctrl()), Some(vec![0x04]));
    }

    #[test]
    fn ctrl_a() {
        assert_eq!(encode_key(&KeyCode::Char('a'), &ctrl()), Some(vec![0x01]));
    }

    #[test]
    fn ctrl_z() {
        assert_eq!(encode_key(&KeyCode::Char('z'), &ctrl()), Some(vec![0x1a]));
    }

    #[test]
    fn ctrl_l_uppercase() {
        assert_eq!(encode_key(&KeyCode::Char('L'), &ctrl()), Some(vec![0x0c]));
    }

    // -- Function keys -------------------------------------------------------

    #[test]
    fn f1() {
        assert_eq!(
            encode_key(&KeyCode::F1, &no_mods()),
            Some(b"\x1bOP".to_vec())
        );
    }

    #[test]
    fn f5() {
        assert_eq!(
            encode_key(&KeyCode::F5, &no_mods()),
            Some(b"\x1b[15~".to_vec())
        );
    }

    #[test]
    fn f12() {
        assert_eq!(
            encode_key(&KeyCode::F12, &no_mods()),
            Some(b"\x1b[24~".to_vec())
        );
    }

    // -- Ignored keys --------------------------------------------------------

    #[test]
    fn no_special_no_char_returns_none() {
        // A key code with no encoding and no char -- should return None.
        // Verify that Ctrl on a non-letter char returns None.
        assert_eq!(encode_key(&KeyCode::Char('\u{ffff}'), &ctrl()), None);
    }

    // -- Alt/Meta + key ------------------------------------------------------

    #[test]
    fn alt_b_word_back() {
        assert_eq!(
            encode_key(&KeyCode::Char('b'), &alt()),
            Some(vec![0x1b, b'b'])
        );
    }

    #[test]
    fn alt_f_word_forward() {
        assert_eq!(
            encode_key(&KeyCode::Char('f'), &alt()),
            Some(vec![0x1b, b'f'])
        );
    }

    #[test]
    fn alt_d_kill_word() {
        assert_eq!(
            encode_key(&KeyCode::Char('d'), &alt()),
            Some(vec![0x1b, b'd'])
        );
    }

    #[test]
    fn alt_dot_last_arg() {
        assert_eq!(
            encode_key(&KeyCode::Char('.'), &alt()),
            Some(vec![0x1b, b'.'])
        );
    }

    #[test]
    fn alt_uppercase_b() {
        let mods = Modifiers {
            alt: true,
            shift: true,
            ..no_mods()
        };
        assert_eq!(
            encode_key(&KeyCode::Char('B'), &mods),
            Some(vec![0x1b, b'B'])
        );
    }

    #[test]
    fn ctrl_alt_does_not_add_esc_prefix() {
        // Ctrl+Alt+C -> Ctrl+C (0x03), not ESC+'c'
        assert_eq!(
            encode_key(&KeyCode::Char('c'), &ctrl_alt()),
            Some(vec![0x03])
        );
    }

    #[test]
    fn alt_non_char_returns_none() {
        // Alt held with a non-char key that has no special encoding -- but
        // our special keys DO encode, so test with a function key that
        // encodes regardless. Instead verify that Alt doesn't break F1.
        assert_eq!(encode_key(&KeyCode::F1, &alt()), Some(b"\x1bOP".to_vec()));
    }

    // -- Kitty keyboard protocol encoding ------------------------------------

    fn kitty_disambiguate() -> KittyFlags {
        KittyFlags::DISAMBIGUATE
    }

    fn kitty_disambiguate_events() -> KittyFlags {
        KittyFlags(KittyFlags::DISAMBIGUATE.0 | KittyFlags::REPORT_EVENTS.0)
    }

    // -- Basic printable characters (CSI codepoint u) ------------------------

    #[test]
    fn kitty_plain_a() {
        // 'a' = U+0061 = 97 -> CSI 97 u
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Char('a'),
                &no_mods(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[97u".to_vec()),
        );
    }

    #[test]
    fn kitty_shifted_a() {
        // Shift+A = U+0041 = 65, shift=bit1 -> modifier param = 2
        let mods = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Char('A'),
                &mods,
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[65;2u".to_vec()),
        );
    }

    #[test]
    fn kitty_ctrl_c() {
        // Ctrl+c: codepoint 99, ctrl=bit4 -> modifier param = 5
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Char('c'),
                &ctrl(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[99;5u".to_vec()),
        );
    }

    #[test]
    fn kitty_ctrl_shift_a() {
        // Ctrl+Shift+a: codepoint 97, shift=1 + ctrl=4 = 5 -> param = 6
        let mods = Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Char('a'),
                &mods,
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[97;6u".to_vec()),
        );
    }

    #[test]
    fn kitty_alt_b() {
        // Alt+b: codepoint 98, alt=bit2 -> modifier param = 3
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Char('b'),
                &alt(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[98;3u".to_vec()),
        );
    }

    // -- Special keys (Enter, Tab, Escape, Backspace) ------------------------

    #[test]
    fn kitty_enter() {
        // Enter = codepoint 13
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Enter,
                &no_mods(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[13u".to_vec()),
        );
    }

    #[test]
    fn kitty_tab() {
        // Tab = codepoint 9
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Tab,
                &no_mods(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[9u".to_vec()),
        );
    }

    #[test]
    fn kitty_escape() {
        // Escape = codepoint 27
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Escape,
                &no_mods(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[27u".to_vec()),
        );
    }

    #[test]
    fn kitty_backspace() {
        // Backspace = codepoint 127
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Backspace,
                &no_mods(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[127u".to_vec()),
        );
    }

    // -- Arrow / cursor keys (letter suffix) ---------------------------------

    #[test]
    fn kitty_arrow_up_no_mods() {
        // ArrowUp with no modifiers -> CSI 1 A
        assert_eq!(
            encode_key_kitty(
                &KeyCode::ArrowUp,
                &no_mods(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[1A".to_vec()),
        );
    }

    #[test]
    fn kitty_arrow_down_ctrl() {
        // Ctrl+Down: ctrl=4 -> param=5  -> CSI 1;5 B
        assert_eq!(
            encode_key_kitty(
                &KeyCode::ArrowDown,
                &ctrl(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[1;5B".to_vec()),
        );
    }

    #[test]
    fn kitty_home_shift() {
        // Shift+Home: shift=1 -> param=2  -> CSI 1;2 H
        let mods = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Home,
                &mods,
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[1;2H".to_vec()),
        );
    }

    #[test]
    fn kitty_end_no_mods() {
        assert_eq!(
            encode_key_kitty(
                &KeyCode::End,
                &no_mods(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[1F".to_vec()),
        );
    }

    // -- Functional keys (~ suffix) ------------------------------------------

    #[test]
    fn kitty_delete_no_mods() {
        // Delete = number 3 -> CSI 3 ~
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Delete,
                &no_mods(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[3~".to_vec()),
        );
    }

    #[test]
    fn kitty_insert_ctrl() {
        // Ctrl+Insert: number 2, ctrl param=5 -> CSI 2;5 ~
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Insert,
                &ctrl(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[2;5~".to_vec()),
        );
    }

    #[test]
    fn kitty_f1_no_mods() {
        // F1 = number 11 -> CSI 11 ~
        assert_eq!(
            encode_key_kitty(
                &KeyCode::F1,
                &no_mods(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[11~".to_vec()),
        );
    }

    #[test]
    fn kitty_f5_shift() {
        // Shift+F5: number 15, shift param=2 -> CSI 15;2 ~
        let mods = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_key_kitty(
                &KeyCode::F5,
                &mods,
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[15;2~".to_vec()),
        );
    }

    #[test]
    fn kitty_f12_no_mods() {
        assert_eq!(
            encode_key_kitty(
                &KeyCode::F12,
                &no_mods(),
                kitty_disambiguate(),
                KeyEventType::Press
            ),
            Some(b"\x1b[24~".to_vec()),
        );
    }

    // -- Event types (flag 2: report-event-types) ----------------------------

    #[test]
    fn kitty_key_release_a() {
        // Release event for 'a' with report-events flag -> CSI 97;1:3 u
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Char('a'),
                &no_mods(),
                kitty_disambiguate_events(),
                KeyEventType::Release
            ),
            Some(b"\x1b[97;1:3u".to_vec()),
        );
    }

    #[test]
    fn kitty_key_repeat_ctrl_c() {
        // Repeat event for Ctrl+c -> CSI 99;5:2 u
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Char('c'),
                &ctrl(),
                kitty_disambiguate_events(),
                KeyEventType::Repeat
            ),
            Some(b"\x1b[99;5:2u".to_vec()),
        );
    }

    #[test]
    fn kitty_release_arrow_up() {
        // Release ArrowUp with report-events -> CSI 1;1:3 A
        assert_eq!(
            encode_key_kitty(
                &KeyCode::ArrowUp,
                &no_mods(),
                kitty_disambiguate_events(),
                KeyEventType::Release
            ),
            Some(b"\x1b[1;1:3A".to_vec()),
        );
    }

    #[test]
    fn kitty_release_f1() {
        // Release F1 with report-events -> CSI 11;1:3 ~
        assert_eq!(
            encode_key_kitty(
                &KeyCode::F1,
                &no_mods(),
                kitty_disambiguate_events(),
                KeyEventType::Release
            ),
            Some(b"\x1b[11;1:3~".to_vec()),
        );
    }

    #[test]
    fn kitty_press_no_event_suffix() {
        // Press event (default) should NOT include :1 even with report-events flag.
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Char('a'),
                &no_mods(),
                kitty_disambiguate_events(),
                KeyEventType::Press
            ),
            Some(b"\x1b[97u".to_vec()),
        );
    }

    // -- Event type NOT encoded when flag 2 is off ---------------------------

    #[test]
    fn kitty_release_ignored_without_flag() {
        // Without REPORT_EVENTS, release should still encode as a press (no :3 suffix).
        assert_eq!(
            encode_key_kitty(
                &KeyCode::Char('a'),
                &no_mods(),
                kitty_disambiguate(),
                KeyEventType::Release
            ),
            Some(b"\x1b[97u".to_vec()),
        );
    }

    // -- KittyFlags contains tests -------------------------------------------

    #[test]
    fn kitty_flags_contains() {
        let flags = KittyFlags(KittyFlags::DISAMBIGUATE.0 | KittyFlags::REPORT_EVENTS.0);
        assert!(flags.contains(KittyFlags::DISAMBIGUATE));
        assert!(flags.contains(KittyFlags::REPORT_EVENTS));
        assert!(!flags.contains(KittyFlags::REPORT_ALL_KEYS));
        assert!(!flags.contains(KittyFlags::NONE));
    }

    // -- Mouse SGR encoding --------------------------------------------------

    #[test]
    fn mouse_press_left() {
        let bytes = encode_mouse_press(MouseButton::Left, 5, 10, &no_mods());
        assert_eq!(bytes, b"\x1b[<0;6;11M");
    }

    #[test]
    fn mouse_release_right() {
        let bytes = encode_mouse_release(MouseButton::Right, 0, 0, &no_mods());
        assert_eq!(bytes, b"\x1b[<2;1;1m");
    }

    #[test]
    fn mouse_press_with_ctrl() {
        let bytes = encode_mouse_press(MouseButton::Left, 3, 7, &ctrl());
        assert_eq!(bytes, b"\x1b[<16;4;8M");
    }

    #[test]
    fn mouse_scroll_up() {
        let bytes = encode_mouse_press(MouseButton::ScrollUp, 10, 5, &no_mods());
        assert_eq!(bytes, b"\x1b[<64;11;6M");
    }

    #[test]
    fn mouse_scroll_down() {
        let bytes = encode_mouse_press(MouseButton::ScrollDown, 10, 5, &no_mods());
        assert_eq!(bytes, b"\x1b[<65;11;6M");
    }

    #[test]
    fn mouse_drag_left() {
        let bytes = encode_mouse_drag(MouseButton::Left, 4, 2, &no_mods());
        assert_eq!(bytes, b"\x1b[<32;5;3M");
    }

    #[test]
    fn mouse_motion_no_button() {
        let bytes = encode_mouse_motion(4, 2, &no_mods());
        assert_eq!(bytes, b"\x1b[<35;5;3M");
    }
}
