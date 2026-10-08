//! Neutral [`Key`] to macOS virtual keycodes (`kVK_*` from HIToolbox `Events.h`, ANSI US layout)
//! and `CGEventFlags` bits. Plain data, so the table is unit-tested on every OS.

use wh_core::keys::Key;

/// `kCGEventFlagMaskAlphaShift` (Caps Lock on).
pub(crate) const FLAG_CAPS_LOCK: u64 = 0x0001_0000;
/// `kCGEventFlagMaskShift`.
pub(crate) const FLAG_SHIFT: u64 = 0x0002_0000;
/// `kCGEventFlagMaskControl`.
pub(crate) const FLAG_CONTROL: u64 = 0x0004_0000;
/// `kCGEventFlagMaskAlternate` (Option).
pub(crate) const FLAG_ALTERNATE: u64 = 0x0008_0000;
/// `kCGEventFlagMaskCommand`.
pub(crate) const FLAG_COMMAND: u64 = 0x0010_0000;
/// `kCGEventFlagMaskNumericPad`, set on arrow keys by real hardware.
pub(crate) const FLAG_NUMERIC_PAD: u64 = 0x0020_0000;
/// `kCGEventFlagMaskSecondaryFn`, set on arrows, F-keys and the navigation cluster.
pub(crate) const FLAG_SECONDARY_FN: u64 = 0x0080_0000;

/// `kVK_Return`.
pub(crate) const VK_RETURN: u16 = 0x24;
/// `kVK_Tab`.
pub(crate) const VK_TAB: u16 = 0x30;
/// `kVK_ANSI_V`.
pub(crate) const VK_V: u16 = 0x09;
/// `kVK_Command`.
pub(crate) const VK_COMMAND: u16 = 0x37;

/// The `CGEventFlags` bit a modifier key sets while it is held; `None` for any other key.
pub(crate) fn modifier_flag(key: Key) -> Option<u64> {
    match key {
        Key::Shift => Some(FLAG_SHIFT),
        Key::Ctrl => Some(FLAG_CONTROL),
        Key::Alt => Some(FLAG_ALTERNATE),
        Key::Meta => Some(FLAG_COMMAND),
        Key::CapsLock => Some(FLAG_CAPS_LOCK),
        _ => None,
    }
}

/// Flags real hardware adds to non-modifier keys of the arrow, function and navigation groups.
pub(crate) fn implied_flags(key: Key) -> u64 {
    match key {
        Key::Left | Key::Right | Key::Up | Key::Down => FLAG_SECONDARY_FN | FLAG_NUMERIC_PAD,
        Key::F(_)
        | Key::Home
        | Key::End
        | Key::PageUp
        | Key::PageDown
        | Key::Delete
        | Key::Insert => FLAG_SECONDARY_FN,
        _ => 0,
    }
}

/// The virtual keycode for a key, or `None` when the ANSI layout has none (an F-key above 20, a
/// character outside the table).
pub(crate) fn keycode(key: Key) -> Option<u16> {
    Some(match key {
        Key::Ctrl => 0x3B,
        Key::Alt => 0x3A,
        Key::Shift => 0x38,
        Key::Meta => VK_COMMAND,
        Key::Backspace => 0x33,
        Key::Tab => VK_TAB,
        Key::Enter => VK_RETURN,
        // No Pause key on a Mac: F15 sits where Pause/Break does on a PC keyboard.
        Key::Pause => 0x71,
        Key::CapsLock => 0x39,
        Key::Escape => 0x35,
        Key::Space => 0x31,
        Key::PageUp => 0x74,
        Key::PageDown => 0x79,
        Key::End => 0x77,
        Key::Home => 0x73,
        Key::Left => 0x7B,
        Key::Up => 0x7E,
        Key::Right => 0x7C,
        Key::Down => 0x7D,
        // The Help key is where Insert lives on a PC keyboard.
        Key::Insert => 0x72,
        Key::Delete => 0x75,
        // Keypad Clear stands in for Num Lock, F14 for Scroll Lock (their PC positions).
        Key::NumLock => 0x47,
        Key::ScrollLock => 0x6B,
        Key::F(n) => return function_key(n),
        Key::Char(c) => return char_key(c),
    })
}

fn function_key(n: u8) -> Option<u16> {
    Some(match n {
        1 => 0x7A,
        2 => 0x78,
        3 => 0x63,
        4 => 0x76,
        5 => 0x60,
        6 => 0x61,
        7 => 0x62,
        8 => 0x64,
        9 => 0x65,
        10 => 0x6D,
        11 => 0x67,
        12 => 0x6F,
        13 => 0x69,
        14 => 0x6B,
        15 => 0x71,
        16 => 0x6A,
        17 => 0x40,
        18 => 0x4F,
        19 => 0x50,
        20 => 0x5A,
        _ => return None,
    })
}

fn char_key(c: char) -> Option<u16> {
    Some(match c.to_ascii_lowercase() {
        'a' => 0x00,
        's' => 0x01,
        'd' => 0x02,
        'f' => 0x03,
        'h' => 0x04,
        'g' => 0x05,
        'z' => 0x06,
        'x' => 0x07,
        'c' => 0x08,
        'v' => VK_V,
        'b' => 0x0B,
        'q' => 0x0C,
        'w' => 0x0D,
        'e' => 0x0E,
        'r' => 0x0F,
        'y' => 0x10,
        't' => 0x11,
        '1' => 0x12,
        '2' => 0x13,
        '3' => 0x14,
        '4' => 0x15,
        '6' => 0x16,
        '5' => 0x17,
        '=' => 0x18,
        '9' => 0x19,
        '7' => 0x1A,
        '-' => 0x1B,
        '8' => 0x1C,
        '0' => 0x1D,
        ']' => 0x1E,
        'o' => 0x1F,
        'u' => 0x20,
        '[' => 0x21,
        'i' => 0x22,
        'p' => 0x23,
        'l' => 0x25,
        'j' => 0x26,
        '\'' => 0x27,
        'k' => 0x28,
        ';' => 0x29,
        '\\' => 0x2A,
        ',' => 0x2B,
        '/' => 0x2C,
        'n' => 0x2D,
        'm' => 0x2E,
        '.' => 0x2F,
        '`' => 0x32,
        _ => return None,
    })
}

/// The US-layout key that types an ASCII character, with whether Shift is part of it. `None`
/// for anything else (accents, emoji, control characters).
pub(crate) fn ascii_key(c: char) -> Option<(u16, bool)> {
    if c == ' ' {
        return Some((0x31, false));
    }
    if !c.is_ascii_graphic() {
        return None;
    }
    if c.is_ascii_uppercase() {
        return char_key(c).map(|code| (code, true));
    }
    let unshifted = match c {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        other => return char_key(other).map(|code| (code, false)),
    };
    char_key(unshifted).map(|code| (code, true))
}

/// The W3C `KeyboardEvent.code` name of a key (what the hotkey crate parses), `None` for the
/// modifiers and for keys with no such name.
pub(crate) fn w3c_code_name(key: Key) -> Option<String> {
    Some(match key {
        Key::Backspace => "Backspace".into(),
        Key::Tab => "Tab".into(),
        Key::Enter => "Enter".into(),
        Key::Pause => "Pause".into(),
        Key::CapsLock => "CapsLock".into(),
        Key::Escape => "Escape".into(),
        Key::Space => "Space".into(),
        Key::PageUp => "PageUp".into(),
        Key::PageDown => "PageDown".into(),
        Key::End => "End".into(),
        Key::Home => "Home".into(),
        Key::Left => "ArrowLeft".into(),
        Key::Up => "ArrowUp".into(),
        Key::Right => "ArrowRight".into(),
        Key::Down => "ArrowDown".into(),
        Key::Insert => "Insert".into(),
        Key::Delete => "Delete".into(),
        Key::NumLock => "NumLock".into(),
        Key::ScrollLock => "ScrollLock".into(),
        Key::F(n) if (1..=24).contains(&n) => format!("F{n}"),
        Key::F(_) => return None,
        Key::Char(c) => match c.to_ascii_lowercase() {
            l @ 'a'..='z' => format!("Key{}", l.to_ascii_uppercase()),
            d @ '0'..='9' => format!("Digit{d}"),
            ';' => "Semicolon".into(),
            '=' => "Equal".into(),
            ',' => "Comma".into(),
            '-' => "Minus".into(),
            '.' => "Period".into(),
            '/' => "Slash".into(),
            '`' => "Backquote".into(),
            '[' => "BracketLeft".into(),
            '\\' => "Backslash".into(),
            ']' => "BracketRight".into(),
            '\'' => "Quote".into(),
            _ => return None,
        },
        Key::Ctrl | Key::Alt | Key::Shift | Key::Meta => return None,
    })
}

/// Name of the System Settings pane for a missing grant, used in error text.
pub(crate) const PANE_ACCESSIBILITY: &str = "System Settings > Privacy & Security > Accessibility";
/// Pane for screen capture.
pub(crate) const PANE_SCREEN_RECORDING: &str =
    "System Settings > Privacy & Security > Screen & System Audio Recording";

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn well_known_codes() {
        assert_eq!(keycode(Key::Char('a')), Some(0x00));
        assert_eq!(keycode(Key::Char('c')), Some(0x08));
        assert_eq!(keycode(Key::Char('v')), Some(0x09));
        assert_eq!(keycode(Key::Char('z')), Some(0x06));
        assert_eq!(keycode(Key::Char('q')), Some(0x0C));
        assert_eq!(keycode(Key::Enter), Some(0x24));
        assert_eq!(keycode(Key::Tab), Some(0x30));
        assert_eq!(keycode(Key::Space), Some(0x31));
        assert_eq!(keycode(Key::Backspace), Some(0x33));
        assert_eq!(keycode(Key::Escape), Some(0x35));
        assert_eq!(keycode(Key::Meta), Some(0x37));
        assert_eq!(keycode(Key::Shift), Some(0x38));
        assert_eq!(keycode(Key::Alt), Some(0x3A));
        assert_eq!(keycode(Key::Ctrl), Some(0x3B));
        assert_eq!(keycode(Key::Left), Some(0x7B));
        assert_eq!(keycode(Key::Down), Some(0x7D));
        assert_eq!(keycode(Key::Delete), Some(0x75));
        assert_eq!(keycode(Key::F(1)), Some(0x7A));
        assert_eq!(keycode(Key::F(12)), Some(0x6F));
    }

    #[test]
    fn digits_are_not_in_numeric_order() {
        let codes: Vec<u16> = "0123456789"
            .chars()
            .map(|c| keycode(Key::Char(c)).unwrap())
            .collect();
        assert_eq!(
            codes,
            [0x1D, 0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19]
        );
    }

    #[test]
    fn letters_and_punctuation_all_map_and_never_collide() {
        let mut seen = HashSet::new();
        for c in ('a'..='z').chain("0123456789;=,-./`[\\]'".chars()) {
            let code = keycode(Key::Char(c)).unwrap_or_else(|| panic!("no code for {c:?}"));
            assert!(seen.insert(code), "{c:?} reuses code {code:#x}");
        }
        assert_eq!(seen.len(), 26 + 10 + 11);
    }

    #[test]
    fn uppercase_maps_like_lowercase_and_unknown_chars_do_not_map() {
        assert_eq!(keycode(Key::Char('A')), keycode(Key::Char('a')));
        assert_eq!(keycode(Key::Char('é')), None);
        assert_eq!(keycode(Key::F(21)), None);
        assert_eq!(keycode(Key::F(0)), None);
    }

    #[test]
    fn function_keys_up_to_twenty_are_distinct() {
        let mut seen = HashSet::new();
        for n in 1..=20 {
            assert!(seen.insert(keycode(Key::F(n)).unwrap()), "F{n}");
        }
        assert!(!seen.contains(&0x37));
    }

    #[test]
    fn named_keys_do_not_collide_with_characters() {
        let named = [
            Key::Ctrl,
            Key::Alt,
            Key::Shift,
            Key::Meta,
            Key::Backspace,
            Key::Tab,
            Key::Enter,
            Key::Pause,
            Key::CapsLock,
            Key::Escape,
            Key::Space,
            Key::PageUp,
            Key::PageDown,
            Key::End,
            Key::Home,
            Key::Left,
            Key::Up,
            Key::Right,
            Key::Down,
            Key::Insert,
            Key::Delete,
            Key::NumLock,
            Key::ScrollLock,
        ];
        let mut seen: HashSet<u16> = ('a'..='z')
            .chain("0123456789;=,-./`[\\]'".chars())
            .map(|c| keycode(Key::Char(c)).unwrap())
            .collect();
        // Pause (F15) and Scroll Lock (F14) deliberately share codes with those function keys.
        for k in named {
            let code = keycode(k).unwrap_or_else(|| panic!("no code for {k:?}"));
            assert!(seen.insert(code), "{k:?} collides: {code:#x}");
        }
    }

    #[test]
    fn modifier_flags_match_cgeventflags() {
        assert_eq!(modifier_flag(Key::Shift), Some(0x20000));
        assert_eq!(modifier_flag(Key::Ctrl), Some(0x40000));
        assert_eq!(modifier_flag(Key::Alt), Some(0x80000));
        assert_eq!(modifier_flag(Key::Meta), Some(0x100000));
        assert_eq!(modifier_flag(Key::Char('a')), None);
        let cmd_shift = modifier_flag(Key::Meta).unwrap() | modifier_flag(Key::Shift).unwrap();
        assert_eq!(cmd_shift, 0x120000);
    }

    #[test]
    fn w3c_names_for_the_hotkey_crate() {
        assert_eq!(w3c_code_name(Key::F(12)).as_deref(), Some("F12"));
        assert_eq!(w3c_code_name(Key::Char('k')).as_deref(), Some("KeyK"));
        assert_eq!(w3c_code_name(Key::Char('5')).as_deref(), Some("Digit5"));
        assert_eq!(w3c_code_name(Key::Pause).as_deref(), Some("Pause"));
        assert_eq!(w3c_code_name(Key::Left).as_deref(), Some("ArrowLeft"));
        assert_eq!(w3c_code_name(Key::Char('\\')).as_deref(), Some("Backslash"));
        assert_eq!(w3c_code_name(Key::Char('\'')).as_deref(), Some("Quote"));
        assert_eq!(w3c_code_name(Key::Ctrl), None);
        assert_eq!(w3c_code_name(Key::F(30)), None);
        assert_eq!(w3c_code_name(Key::Char('é')), None);
    }

    #[test]
    fn arrows_and_function_keys_carry_the_fn_flag() {
        assert_eq!(
            implied_flags(Key::Left),
            FLAG_SECONDARY_FN | FLAG_NUMERIC_PAD
        );
        assert_eq!(implied_flags(Key::F(5)), FLAG_SECONDARY_FN);
        assert_eq!(implied_flags(Key::Home), FLAG_SECONDARY_FN);
        assert_eq!(implied_flags(Key::Char('a')), 0);
        assert_eq!(implied_flags(Key::Enter), 0);
    }

    #[test]
    fn ascii_characters_map_to_a_key_and_shift() {
        assert_eq!(ascii_key('a'), Some((0x00, false)));
        assert_eq!(ascii_key('A'), Some((0x00, true)));
        assert_eq!(ascii_key('5'), Some((0x17, false)));
        assert_eq!(ascii_key('%'), Some((0x17, true)));
        assert_eq!(ascii_key('='), Some((0x18, false)));
        assert_eq!(ascii_key('+'), Some((0x18, true)));
        assert_eq!(ascii_key(' '), Some((0x31, false)));
        assert_eq!(ascii_key('?'), Some((0x2C, true)));
        assert_eq!(ascii_key('"'), Some((0x27, true)));
        assert_eq!(ascii_key('|'), Some((0x2A, true)));
        assert_eq!(ascii_key('~'), Some((0x32, true)));
        assert_eq!(ascii_key('\n'), None);
        assert_eq!(ascii_key('é'), None);
        assert_eq!(ascii_key('😀'), None);
    }

    #[test]
    fn every_printable_ascii_character_has_a_key() {
        for c in (0x20u8..0x7F).map(char::from) {
            assert!(ascii_key(c).is_some(), "{c:?}");
        }
    }
}
