//! Key combos. Port of the "Key combo parsing" section of `winhands/core.py` (`VK`,
//! `KEY_ALIASES`, `_split_combo`, `parse_key_combo`) and the key-name helpers in
//! `winhands/computer.py` (`_KEY_NAMES`, `_key_combo`, `_with_modifiers`, `OPENERS`).
//!
//! Python resolved a combo straight to Windows virtual-key codes. Here it resolves to the neutral
//! [`Key`] enum and each backend maps `Key` to its own codes ([`windows_vk`] is that table for
//! Windows, kept here as plain data so the parser tests can check the exact codes Python produced).
//!
//! The one platform difference is the alias table: a model habit from the other OS.
//!
//! | name | Windows | macOS |
//! |---|---|---|
//! | `cmd`, `command` | Ctrl (a Mac habit: `cmd+s` means `ctrl+s`) | Meta (the Command key) |
//! | `super` | Meta (the Windows key) | Meta (Command) |
//! | `win`, `windows`, `meta` | Meta | Meta |
//! | `option`, `opt` | Alt | Alt (the Option key) |

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::textutil::py_repr;

/// The OS whose key vocabulary a combo is parsed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    /// Windows: `cmd` is Ctrl, the Meta key is the Windows key.
    Windows,
    /// macOS: `cmd` is Meta, the Meta key is Command.
    Mac,
}

impl Platform {
    /// The platform this binary was built for. Linux builds (core unit tests only) read as Windows.
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::Mac
        } else {
            Platform::Windows
        }
    }
}

/// A key a backend can press. Letters are lowercase; `Char` also covers digits and the punctuation
/// keys the Python table knows (`; = , - . / ` [ \ ] '`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Key {
    /// Control.
    Ctrl,
    /// Alt (Option on a Mac).
    Alt,
    /// Shift.
    Shift,
    /// The Windows key; Command on a Mac.
    Meta,
    /// Backspace.
    Backspace,
    /// Tab.
    Tab,
    /// Enter / Return.
    Enter,
    /// Pause / Break.
    Pause,
    /// Caps Lock.
    CapsLock,
    /// Escape.
    Escape,
    /// Space bar.
    Space,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// End.
    End,
    /// Home.
    Home,
    /// Left arrow.
    Left,
    /// Up arrow.
    Up,
    /// Right arrow.
    Right,
    /// Down arrow.
    Down,
    /// Insert.
    Insert,
    /// Forward delete.
    Delete,
    /// Num Lock.
    NumLock,
    /// Scroll Lock.
    ScrollLock,
    /// A function key, 1 to 24.
    F(u8),
    /// A letter (lowercase), a digit, or one of `; = , - . / ` [ \ ] '`.
    Char(char),
}

impl Key {
    /// Is this one of Ctrl, Alt, Shift, Meta?
    pub fn is_modifier(self) -> bool {
        matches!(self, Key::Ctrl | Key::Alt | Key::Shift | Key::Meta)
    }

    /// The neutral name, the inverse of [`key_from_name`] for canonical names.
    pub fn name(self) -> String {
        match self {
            Key::Ctrl => "ctrl".into(),
            Key::Alt => "alt".into(),
            Key::Shift => "shift".into(),
            Key::Meta => "meta".into(),
            Key::Backspace => "backspace".into(),
            Key::Tab => "tab".into(),
            Key::Enter => "enter".into(),
            Key::Pause => "pause".into(),
            Key::CapsLock => "capslock".into(),
            Key::Escape => "esc".into(),
            Key::Space => "space".into(),
            Key::PageUp => "pageup".into(),
            Key::PageDown => "pagedown".into(),
            Key::End => "end".into(),
            Key::Home => "home".into(),
            Key::Left => "left".into(),
            Key::Up => "up".into(),
            Key::Right => "right".into(),
            Key::Down => "down".into(),
            Key::Insert => "insert".into(),
            Key::Delete => "delete".into(),
            Key::NumLock => "numlock".into(),
            Key::ScrollLock => "scrolllock".into(),
            Key::F(n) => format!("f{n}"),
            Key::Char(c) => c.to_string(),
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

/// The Windows virtual-key code of a key: the Python `VK` table. Letters are their uppercase
/// ASCII, digits their ASCII.
pub fn windows_vk(key: Key) -> u16 {
    match key {
        Key::Backspace => 0x08,
        Key::Tab => 0x09,
        Key::Enter => 0x0D,
        Key::Shift => 0x10,
        Key::Ctrl => 0x11,
        Key::Alt => 0x12,
        Key::Pause => 0x13,
        Key::CapsLock => 0x14,
        Key::Escape => 0x1B,
        Key::Space => 0x20,
        Key::PageUp => 0x21,
        Key::PageDown => 0x22,
        Key::End => 0x23,
        Key::Home => 0x24,
        Key::Left => 0x25,
        Key::Up => 0x26,
        Key::Right => 0x27,
        Key::Down => 0x28,
        Key::Insert => 0x2D,
        Key::Delete => 0x2E,
        Key::Meta => 0x5B,
        Key::NumLock => 0x90,
        Key::ScrollLock => 0x91,
        // VK_F1 is 0x70; Python used 0x6F + n above F12, which is the same sequence (F13 = 0x7C).
        Key::F(n) => 0x70 + u16::from(n.saturating_sub(1)),
        Key::Char(c) => match c {
            ';' => 0xBA,
            '=' => 0xBB,
            ',' => 0xBC,
            '-' => 0xBD,
            '.' => 0xBE,
            '/' => 0xBF,
            '`' => 0xC0,
            '[' => 0xDB,
            '\\' => 0xDC,
            ']' => 0xDD,
            '\'' => 0xDE,
            other => other.to_ascii_uppercase() as u16,
        },
    }
}

/// Python `MODIFIER_NAMES`.
pub const MODIFIER_NAMES: [&str; 8] = [
    "ctrl", "control", "alt", "menu", "shift", "win", "windows", "meta",
];

/// Names that read the same on every OS (Python `KEY_ALIASES`, minus the OS-specific rows).
const COMMON_ALIASES: [(&str, &str); 13] = [
    ("option", "alt"),
    ("opt", "alt"),
    ("pgup", "pageup"),
    ("pgdn", "pagedown"),
    ("ins", "insert"),
    ("bksp", "backspace"),
    ("spacebar", "space"),
    ("arrowleft", "left"),
    ("arrowright", "right"),
    ("arrowup", "up"),
    ("arrowdown", "down"),
    ("page_up", "pageup"),
    ("page_down", "pagedown"),
];

/// `page-up` and `page-down` also alias (they contain a dash, so they sit apart in the table).
const DASHED_ALIASES: [(&str, &str); 2] = [("page-up", "pageup"), ("page-down", "pagedown")];

/// What `name` is an alias for on `platform`, if it is one. Python `KEY_ALIASES.get(name)`.
pub fn alias(platform: Platform, name: &str) -> Option<&'static str> {
    let os_specific = match (platform, name) {
        (Platform::Windows, "cmd" | "command") => Some("ctrl"),
        (Platform::Windows, "super") => Some("win"),
        (Platform::Mac, "cmd" | "command" | "super") => Some("meta"),
        _ => None,
    };
    os_specific.or_else(|| {
        COMMON_ALIASES
            .iter()
            .chain(DASHED_ALIASES.iter())
            .find(|(from, _)| *from == name)
            .map(|(_, to)| *to)
    })
}

fn resolve(platform: Platform, name: &str) -> &str {
    alias(platform, name).unwrap_or(name)
}

fn is_modifier_name(name: &str) -> bool {
    MODIFIER_NAMES.contains(&name)
}

/// The key a (lowercase, alias-resolved) name stands for. Python `VK` lookup.
pub fn key_from_name(name: &str) -> Option<Key> {
    Some(match name {
        "ctrl" | "control" => Key::Ctrl,
        "alt" | "menu" => Key::Alt,
        "shift" => Key::Shift,
        "win" | "windows" | "meta" => Key::Meta,
        "backspace" => Key::Backspace,
        "tab" => Key::Tab,
        "enter" | "return" => Key::Enter,
        "pause" => Key::Pause,
        "capslock" => Key::CapsLock,
        "esc" | "escape" => Key::Escape,
        "space" => Key::Space,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "end" => Key::End,
        "home" => Key::Home,
        "left" => Key::Left,
        "up" => Key::Up,
        "right" => Key::Right,
        "down" => Key::Down,
        "insert" => Key::Insert,
        "delete" | "del" => Key::Delete,
        "numlock" => Key::NumLock,
        "scrolllock" => Key::ScrollLock,
        other => {
            if let Some(n) = other.strip_prefix('f').and_then(|d| d.parse::<u8>().ok()) {
                // Python built f1..f24 from f"f{i}", so "f01" and "f+1" are not keys.
                if (1..=24).contains(&n) && other == format!("f{n}") {
                    return Some(Key::F(n));
                }
            }
            let mut chars = other.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return None;
            };
            if c.is_ascii_digit()
                || c.is_ascii_lowercase()
                || matches!(
                    c,
                    ';' | '=' | ',' | '-' | '.' | '/' | '`' | '[' | '\\' | ']' | '\''
                )
            {
                Key::Char(c)
            } else {
                return None;
            }
        }
    })
}

/// A combo could not be parsed. The message is the text a caller sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyComboError(pub String);

impl fmt::Display for KeyComboError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KeyComboError {}

/// `'ctrl+a'`, `'ctrl-a'`, `'Ctrl + Shift + S'` and `'ctrl-shift-s'` all mean the same combo.
///
/// 2026-09-20: a Hermes run sent key('ctrl-a') and got 'unknown key: ctrl-a'. A dash only joins
/// keys when every piece before the last is a modifier, so a bare '-' and 'ctrl+-' (zoom out)
/// still mean the minus key. Python `_split_combo`.
fn split_combo(combo: &str, platform: Platform) -> Vec<String> {
    let text = combo.trim().to_lowercase();
    if text.trim_matches('+').contains('+') || text == "+" {
        return text
            .split('+')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
    }
    if text.contains('-') && text != "-" {
        let pieces: Vec<&str> = text.split('-').collect();
        let n = pieces.len();
        let (head, last): (&[&str], &str) = if pieces[n - 1].is_empty() && n >= 2 {
            // 'ctrl--' is ctrl plus the minus key
            (&pieces[..n - 2], "-")
        } else {
            (&pieces[..n - 1], pieces[n - 1])
        };
        let head: Vec<&str> = head.iter().map(|p| resolve(platform, p.trim())).collect();
        if !head.is_empty() && head.iter().all(|p| is_modifier_name(p)) && !last.trim().is_empty() {
            let mut out: Vec<String> = head.iter().map(|s| s.to_string()).collect();
            out.push(last.trim().to_string());
            return out;
        }
    }
    if text.contains(' ') {
        let pieces: Vec<&str> = text.split_whitespace().collect();
        let mapped: Vec<&str> = pieces.iter().map(|p| resolve(platform, p)).collect();
        if pieces.len() > 1
            && mapped[..mapped.len() - 1]
                .iter()
                .all(|p| is_modifier_name(p))
        {
            return pieces.iter().map(|s| s.to_string()).collect();
        }
    }
    vec![text]
}

/// `'ctrl+alt+shift+k'` to an ordered key list, modifiers first. Python `parse_key_combo`.
pub fn parse_key_combo(combo: &str, platform: Platform) -> Result<Vec<Key>, KeyComboError> {
    if combo.trim().is_empty() {
        return Err(KeyComboError("empty key combo".into()));
    }
    let parts: Vec<String> = split_combo(combo, platform)
        .iter()
        .map(|p| resolve(platform, p).to_string())
        .collect();
    if parts.is_empty() {
        return Err(KeyComboError(format!(
            "empty key combo: {}",
            py_repr(combo)
        )));
    }
    let (mods, rest): (Vec<&String>, Vec<&String>) =
        parts.iter().partition(|p| is_modifier_name(p));
    if rest.len() > 1 {
        return Err(KeyComboError(format!(
            "too many non-modifier keys in combo: {}",
            py_repr(combo)
        )));
    }
    let mut keys = Vec::with_capacity(parts.len());
    for p in mods {
        keys.push(
            key_from_name(p)
                .ok_or_else(|| KeyComboError(format!("unknown modifier: {}", py_repr(p))))?,
        );
    }
    for p in rest {
        keys.push(key_from_name(p).ok_or_else(|| {
            KeyComboError(format!(
                "unknown key: {}. Write combos like 'ctrl+a', 'shift+tab', 'alt+f4', 'esc', \
                 'f5' (modifiers ctrl/alt/shift/win joined by '+')",
                py_repr(p)
            ))
        })?);
    }
    if keys.is_empty() {
        return Err(KeyComboError(format!(
            "no keys parsed from combo: {}",
            py_repr(combo)
        )));
    }
    Ok(keys)
}

/// xdotool-style names that Claude-trained habits produce, mapped to winhands key names
/// (computer.py `_KEY_NAMES`).
pub const XDOTOOL_NAMES: [(&str, &str); 21] = [
    ("return", "enter"),
    ("kp_enter", "enter"),
    ("escape", "esc"),
    ("page_down", "pagedown"),
    ("page_up", "pageup"),
    ("super", "win"),
    ("super_l", "win"),
    ("control", "ctrl"),
    ("control_l", "ctrl"),
    ("shift_l", "shift"),
    ("alt_l", "alt"),
    ("backspace", "backspace"),
    ("prior", "pageup"),
    ("next", "pagedown"),
    ("delete", "delete"),
    ("minus", "-"),
    ("plus", "="),
    ("comma", ","),
    ("period", "."),
    ("slash", "/"),
    ("grave", "`"),
];

fn xdotool_name(name: &str) -> Option<&'static str> {
    XDOTOOL_NAMES
        .iter()
        .find(|(from, _)| *from == name)
        .map(|(_, to)| *to)
}

/// Normalise a `key` action's text to winhands names joined by `+`: spaces dropped, each piece
/// lowercased and run through the xdotool table. Python `computer._key_combo`.
pub fn normalize_key_text(text: &str) -> String {
    text.replace(' ', "")
        .split('+')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let lower = p.to_lowercase();
            xdotool_name(&lower).map(str::to_string).unwrap_or(lower)
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Hold modifier keys such as `shift` or `ctrl+shift` (a `text` field on click/scroll/drag).
/// Pieces split on `+` or `-`; each goes through the xdotool table, then the alias table, and must
/// be a known key. Python `computer._with_modifiers` (the key lookup half).
pub fn parse_modifiers(mods: &str, platform: Platform) -> Result<Vec<Key>, KeyComboError> {
    let mut keys = Vec::new();
    for raw in mods.to_lowercase().replace('-', "+").split('+') {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        let name = xdotool_name(name).unwrap_or_else(|| resolve(platform, name));
        match key_from_name(name) {
            Some(k) => keys.push(k),
            None => return Err(KeyComboError(format!("unknown modifier {}", py_repr(name)))),
        }
    }
    Ok(keys)
}

/// Shortcuts that open a tab, window or dialog: the next keystroke must wait for it
/// (computer.py `OPENERS`). `f2` is Explorer's rename box, which appears late.
pub const OPENERS: [&str; 15] = [
    "ctrl+n",
    "ctrl+t",
    "ctrl+o",
    "ctrl+s",
    "ctrl+shift+s",
    "ctrl+shift+n",
    "ctrl+p",
    "ctrl+f",
    "ctrl+h",
    "ctrl+l",
    "ctrl+g",
    "f12",
    "alt+d",
    "ctrl+w",
    "f2",
];

/// Is `combo` (already normalised) a shortcut that opens something?
pub fn is_opener(combo: &str) -> bool {
    OPENERS.contains(&combo)
}

/// Can `text` be a calculator-style expression such as `4309*82=`? Text sent as `key` that is not
/// a key name is typed as-is only when it looks like one; a typo'd key name must still raise the
/// unknown-key error (computer.py `_CALC_LIKE_CHARS`).
pub fn is_calc_like(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| "0123456789 .*/+-=()%".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: Platform = Platform::Windows;

    fn vks(combo: &str) -> Vec<u16> {
        parse_key_combo(combo, WIN)
            .unwrap()
            .into_iter()
            .map(windows_vk)
            .collect()
    }

    fn a() -> u16 {
        u16::from(b'A')
    }

    fn s() -> u16 {
        u16::from(b'S')
    }

    // ---- ported from tests/test_core.py (key combo parsing) ----

    #[test]
    fn parse_simple_key() {
        assert_eq!(vks("f5"), [windows_vk(Key::F(5))]);
        assert_eq!(vks("f5"), [0x74]);
    }

    #[test]
    fn parse_modifiers_order() {
        assert_eq!(
            parse_key_combo("ctrl+alt+shift+k", WIN).unwrap(),
            [Key::Ctrl, Key::Alt, Key::Shift, Key::Char('k')]
        );
        assert_eq!(vks("ctrl+alt+shift+k"), [0x11, 0x12, 0x10, u16::from(b'K')]);
    }

    #[test]
    fn parse_win_combo() {
        assert_eq!(vks("win+r"), [windows_vk(Key::Meta), u16::from(b'R')]);
    }

    #[test]
    fn parse_case_insensitive_and_spaces() {
        assert_eq!(vks(" Ctrl + S "), [0x11, s()]);
    }

    #[test]
    fn parse_empty_raises() {
        assert!(parse_key_combo("", WIN).is_err());
        assert!(parse_key_combo("   ", WIN).is_err());
    }

    #[test]
    fn parse_unknown_key_raises() {
        assert!(parse_key_combo("ctrl+notakey", WIN).is_err());
    }

    #[test]
    fn parse_too_many_nonmodifiers_raises() {
        assert!(parse_key_combo("a+b", WIN).is_err());
    }

    // ---- ported from tests/test_20260920_game_fixes.py (key combo forms) ----

    #[test]
    fn key_combo_forms() {
        let cases: [(&str, Vec<u16>); 11] = [
            ("ctrl+a", vec![0x11, a()]),
            ("ctrl-a", vec![0x11, a()]),
            ("Ctrl + Shift + S", vec![0x11, 0x10, s()]),
            ("ctrl-shift-s", vec![0x11, 0x10, s()]),
            ("ctrl shift s", vec![0x11, 0x10, s()]),
            ("cmd+s", vec![0x11, s()]),
            ("-", vec![0xBD]),
            ("ctrl+-", vec![0x11, 0xBD]),
            ("ctrl--", vec![0x11, 0xBD]),
            ("esc", vec![0x1B]),
            ("pgdn", vec![0x22]),
        ];
        for (combo, expected) in cases {
            assert_eq!(vks(combo), expected, "combo {combo:?}");
        }
    }

    #[test]
    fn key_combo_error_shows_the_format() {
        let err = parse_key_combo("ctrl+nosuchkey", WIN).unwrap_err();
        assert!(err.0.contains("ctrl+a"), "{err}");
        assert_eq!(
            err.0,
            "unknown key: 'nosuchkey'. Write combos like 'ctrl+a', 'shift+tab', 'alt+f4', 'esc', \
             'f5' (modifiers ctrl/alt/shift/win joined by '+')"
        );
    }

    // ---- the platform difference ----

    #[test]
    fn cmd_is_ctrl_on_windows_and_command_on_mac() {
        assert_eq!(
            parse_key_combo("cmd+s", Platform::Windows).unwrap(),
            [Key::Ctrl, Key::Char('s')]
        );
        assert_eq!(
            parse_key_combo("cmd+s", Platform::Mac).unwrap(),
            [Key::Meta, Key::Char('s')]
        );
        assert_eq!(
            parse_key_combo("command-c", Platform::Mac).unwrap(),
            [Key::Meta, Key::Char('c')]
        );
        assert_eq!(
            parse_key_combo("command c", Platform::Windows).unwrap(),
            [Key::Ctrl, Key::Char('c')]
        );
        // super is the Windows key on Windows and Command on a Mac; both end up as Meta.
        assert_eq!(
            parse_key_combo("super+e", Platform::Windows).unwrap(),
            [Key::Meta, Key::Char('e')]
        );
        assert_eq!(
            parse_key_combo("super+e", Platform::Mac).unwrap(),
            [Key::Meta, Key::Char('e')]
        );
        // option is Alt on both.
        assert_eq!(
            parse_key_combo("option+left", Platform::Mac).unwrap(),
            [Key::Alt, Key::Left]
        );
        assert_eq!(
            parse_key_combo("opt+left", Platform::Windows).unwrap(),
            [Key::Alt, Key::Left]
        );
        assert_eq!(alias(Platform::Windows, "cmd"), Some("ctrl"));
        assert_eq!(alias(Platform::Mac, "cmd"), Some("meta"));
        assert_eq!(alias(Platform::Mac, "nope"), None);
    }

    #[test]
    fn the_current_platform_matches_the_build() {
        assert_eq!(
            Platform::current() == Platform::Mac,
            cfg!(target_os = "macos")
        );
    }

    // ---- extra coverage of the table ----

    #[test]
    fn every_python_vk_entry_has_the_same_code() {
        let table: [(&str, u16); 31] = [
            ("backspace", 0x08),
            ("tab", 0x09),
            ("enter", 0x0D),
            ("return", 0x0D),
            ("shift", 0x10),
            ("ctrl", 0x11),
            ("control", 0x11),
            ("alt", 0x12),
            ("menu", 0x12),
            ("pause", 0x13),
            ("capslock", 0x14),
            ("esc", 0x1B),
            ("escape", 0x1B),
            ("space", 0x20),
            ("pageup", 0x21),
            ("pagedown", 0x22),
            ("end", 0x23),
            ("home", 0x24),
            ("left", 0x25),
            ("up", 0x26),
            ("right", 0x27),
            ("down", 0x28),
            ("insert", 0x2D),
            ("delete", 0x2E),
            ("del", 0x2E),
            ("win", 0x5B),
            ("windows", 0x5B),
            ("meta", 0x5B),
            ("numlock", 0x90),
            ("scrolllock", 0x91),
            ("f12", 0x7B),
        ];
        for (name, code) in table {
            assert_eq!(windows_vk(key_from_name(name).unwrap()), code, "{name}");
        }
        let punct: [(&str, u16); 11] = [
            (";", 0xBA),
            ("=", 0xBB),
            (",", 0xBC),
            ("-", 0xBD),
            (".", 0xBE),
            ("/", 0xBF),
            ("`", 0xC0),
            ("[", 0xDB),
            ("\\", 0xDC),
            ("]", 0xDD),
            ("'", 0xDE),
        ];
        for (name, code) in punct {
            assert_eq!(windows_vk(key_from_name(name).unwrap()), code, "{name}");
        }
    }

    #[test]
    fn function_keys_run_f1_to_f24_with_python_codes() {
        for n in 1..=24u8 {
            let key = key_from_name(&format!("f{n}")).unwrap();
            assert_eq!(key, Key::F(n));
            let python = if n > 12 {
                0x6F + u16::from(n)
            } else {
                0x70 + u16::from(n - 1)
            };
            assert_eq!(windows_vk(key), python, "f{n}");
        }
        assert_eq!(key_from_name("f0"), None);
        assert_eq!(key_from_name("f25"), None);
        assert_eq!(key_from_name("f01"), None);
        assert_eq!(key_from_name("f"), Some(Key::Char('f')));
    }

    #[test]
    fn letters_and_digits_are_their_ascii_codes() {
        for c in 'a'..='z' {
            assert_eq!(
                windows_vk(key_from_name(&c.to_string()).unwrap()),
                c.to_ascii_uppercase() as u16
            );
        }
        for c in '0'..='9' {
            assert_eq!(windows_vk(key_from_name(&c.to_string()).unwrap()), c as u16);
        }
        assert_eq!(key_from_name("é"), None);
        assert_eq!(
            key_from_name("A"),
            None,
            "names are lowercased before lookup"
        );
    }

    #[test]
    fn every_alias_resolves() {
        for (from, to) in [
            ("pgup", "pageup"),
            ("pgdn", "pagedown"),
            ("ins", "insert"),
            ("bksp", "backspace"),
            ("spacebar", "space"),
            ("arrowleft", "left"),
            ("arrowright", "right"),
            ("arrowup", "up"),
            ("arrowdown", "down"),
            ("page_up", "pageup"),
            ("page_down", "pagedown"),
            ("page-up", "pageup"),
            ("page-down", "pagedown"),
            ("option", "alt"),
            ("opt", "alt"),
        ] {
            assert_eq!(alias(WIN, from), Some(to), "{from}");
            assert_eq!(alias(Platform::Mac, from), Some(to), "{from}");
        }
        assert_eq!(
            parse_key_combo("ctrl+page-up", WIN).unwrap(),
            [Key::Ctrl, Key::PageUp]
        );
        assert_eq!(parse_key_combo("page_down", WIN).unwrap(), [Key::PageDown]);
    }

    #[test]
    fn dash_and_plus_edge_cases_match_python() {
        // a dash only joins when every piece before the last is a modifier
        assert!(parse_key_combo("a-b", WIN).is_err());
        assert_eq!(
            parse_key_combo("shift-tab", WIN).unwrap(),
            [Key::Shift, Key::Tab]
        );
        // a bare plus has no pieces left
        assert_eq!(
            parse_key_combo("+", WIN).unwrap_err().0,
            "empty key combo: '+'"
        );
        assert!(parse_key_combo("ctrl++", WIN).is_err());
        // modifiers only: nothing but modifiers is a valid (if odd) combo, as in Python
        assert_eq!(
            parse_key_combo("ctrl+shift", WIN).unwrap(),
            [Key::Ctrl, Key::Shift]
        );
        // a space-separated pair where the first is not a modifier stays one (invalid) name
        assert!(parse_key_combo("a b", WIN).is_err());
        // repeated modifiers are kept in order
        assert_eq!(parse_key_combo("ctrl+ctrl+a", WIN).unwrap().len(), 3);
    }

    #[test]
    fn modifiers_come_first_whatever_the_input_order() {
        assert_eq!(
            parse_key_combo("k+ctrl+alt", WIN).unwrap(),
            [Key::Ctrl, Key::Alt, Key::Char('k')]
        );
    }

    #[test]
    fn xdotool_names_normalise_to_winhands_names() {
        assert_eq!(normalize_key_text("Return"), "enter");
        assert_eq!(normalize_key_text("Control_L + Page_Down"), "ctrl+pagedown");
        assert_eq!(normalize_key_text("super+e"), "win+e");
        assert_eq!(normalize_key_text("ctrl+minus"), "ctrl+-");
        assert_eq!(normalize_key_text("KP_Enter"), "enter");
        assert_eq!(normalize_key_text("A"), "a");
        assert_eq!(normalize_key_text(""), "");
    }

    #[test]
    fn modifier_strings_for_click_and_drag() {
        assert_eq!(parse_modifiers("shift", WIN).unwrap(), [Key::Shift]);
        assert_eq!(
            parse_modifiers("Ctrl+Shift", WIN).unwrap(),
            [Key::Ctrl, Key::Shift]
        );
        assert_eq!(
            parse_modifiers("ctrl-alt", WIN).unwrap(),
            [Key::Ctrl, Key::Alt]
        );
        assert_eq!(parse_modifiers("super", WIN).unwrap(), [Key::Meta]);
        assert_eq!(parse_modifiers("", WIN).unwrap(), []);
        assert_eq!(
            parse_modifiers("bogus", WIN).unwrap_err().0,
            "unknown modifier 'bogus'"
        );
    }

    #[test]
    fn openers_and_calculator_text() {
        assert!(is_opener("ctrl+n") && is_opener("f2") && !is_opener("ctrl+c"));
        assert!(is_calc_like("4309*82=") && is_calc_like("(1+2)%"));
        assert!(!is_calc_like("Retun") && !is_calc_like(""));
    }

    #[test]
    fn key_names_round_trip() {
        for name in [
            "ctrl", "alt", "shift", "meta", "enter", "esc", "pageup", "f5", "a", "7", "-",
        ] {
            assert_eq!(key_from_name(name).unwrap().name(), name);
        }
        assert_eq!(Key::F(12).to_string(), "f12");
    }
}
