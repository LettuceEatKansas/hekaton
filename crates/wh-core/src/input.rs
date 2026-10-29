//! Typing text. Port of `computer._type_text`.
//!
//! Text longer than `paste_over` characters (6 by default) is pasted through the clipboard and the
//! owner's clipboard is put back afterwards; short text (a game hotkey, a digit) is typed key by key
//! with a small gap. 2026-09-20 side-by-side test: a fast burst of synthetic key events typed
//! "...winhands rrrrrrrr" into Windows 11 Notepad, while pasting came out clean.
//!
//! The owner's hard line applies first: this tool types into terminals, so a command typed here counts
//! as running it, pasted or keyed ([`Policy::check_text`]).

use std::fmt;
use std::thread;
use std::time::Duration;

use crate::backend::{Backend, BackendError, InputGuard, no_guard};
use crate::destructive::{DestructiveAction, Policy};
use crate::keys::{Key, Platform};

/// Gap between typed characters.
const KEY_GAP: Duration = Duration::from_millis(12);
/// Let the clipboard settle before pasting.
const BEFORE_PASTE: Duration = Duration::from_millis(30);
/// Let the app read the clipboard before the old contents go back.
const AFTER_PASTE: Duration = Duration::from_millis(120);

/// How `type_text` behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeOptions {
    /// Character count above which text is pasted instead of typed (config `paste_over`).
    pub paste_over: usize,
    /// Which paste shortcut to press.
    pub platform: Platform,
    /// Try the backend's direct insert first ([`Backend::insert_text`]).
    pub direct: bool,
}

impl TypeOptions {
    /// This build's platform and the given threshold.
    pub fn new(paste_over: usize) -> Self {
        Self {
            paste_over,
            platform: Platform::current(),
            direct: true,
        }
    }
}

/// How the text went in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeOutcome {
    /// Typed key by key.
    Typed,
    /// Pasted from the clipboard.
    Pasted,
    /// Written straight into the focused text control.
    Inserted,
}

impl TypeOutcome {
    /// The suffix Python appended to its note: "" or " (pasted)".
    pub fn note_suffix(self) -> &'static str {
        match self {
            TypeOutcome::Typed => "",
            TypeOutcome::Pasted => " (pasted)",
            TypeOutcome::Inserted => " (direct)",
        }
    }
}

/// Why text was not typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeError {
    /// The text is on the destructive list; nothing was sent.
    Refused(DestructiveAction),
    /// The OS failed while typing or pasting.
    Backend(BackendError),
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeError::Refused(d) => d.fmt(f),
            TypeError::Backend(b) => b.fmt(f),
        }
    }
}

impl std::error::Error for TypeError {}

impl From<BackendError> for TypeError {
    fn from(e: BackendError) -> Self {
        TypeError::Backend(e)
    }
}

/// Type `text` into whatever has focus, pasting when it is long.
pub fn type_text(
    backend: &dyn Backend,
    policy: Policy,
    text: &str,
    options: &TypeOptions,
) -> Result<TypeOutcome, TypeError> {
    type_text_guarded(backend, policy, text, options, &no_guard)
}

/// Type with a caller guard before each injected group. Clipboard restoration is unconditional.
pub fn type_text_guarded(
    backend: &dyn Backend,
    policy: Policy,
    text: &str,
    options: &TypeOptions,
    guard: InputGuard<'_>,
) -> Result<TypeOutcome, TypeError> {
    guard()?;
    policy
        .check_text(text, "the text to type")
        .map_err(TypeError::Refused)?;
    // A trailing newline means "and press Enter" (a file name, a search box, a prompt). Pasted, a
    // single-line box drops it: 2026-10-28, Explorer's rename box kept "New folder" for a pasted
    // "made-by-hekaton\n" and took the name once Enter was a real key press.
    let body = text.trim_end_matches(['\r', '\n']);
    let enters = match text[body.len()..].matches('\n').count() {
        0 if body.len() < text.len() => 1,
        n => n,
    };
    let outcome = type_body(backend, body, options, guard)?;
    if enters > 0 {
        guard()?;
        backend.key_chord(&[Key::Enter], u32::try_from(enters).unwrap_or(1), guard)?;
    }
    Ok(outcome)
}

fn type_body(
    backend: &dyn Backend,
    text: &str,
    options: &TypeOptions,
    guard: InputGuard<'_>,
) -> Result<TypeOutcome, TypeError> {
    if text.is_empty() {
        return Ok(TypeOutcome::Typed);
    }
    if options.direct && !text.contains('\r') && backend.insert_text(text)? {
        return Ok(TypeOutcome::Inserted);
    }
    if text.chars().count() <= options.paste_over {
        for ch in text.chars().filter(|&ch| ch != '\r') {
            if ch == '\n' {
                backend.key_chord(&[Key::Enter], 1, guard)?;
            } else {
                let mut buf = [0u8; 4];
                backend.type_text(ch.encode_utf8(&mut buf), guard)?;
            }
            thread::sleep(KEY_GAP);
        }
        return Ok(TypeOutcome::Typed);
    }
    let saved = backend.clipboard_text().ok();
    guard()?;
    backend.set_clipboard_text(text)?;
    thread::sleep(BEFORE_PASTE);
    let paste_modifier = match options.platform {
        Platform::Windows => Key::Ctrl,
        Platform::Mac => Key::Meta,
    };
    let pasted = backend.key_chord(&[paste_modifier, Key::Char('v')], 1, guard);
    if pasted.is_ok() {
        thread::sleep(AFTER_PASTE);
    }
    if let Some(saved) = saved {
        // Putting the old text back is best effort, as in Python.
        let _ = backend.set_clipboard_text(&saved);
    }
    pasted?;
    Ok(TypeOutcome::Pasted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ClipboardContent;
    use crate::backend::fake::{FakeBackend, InputEvent};

    const OFF: Policy = Policy::REFUSE;

    #[test]
    fn a_native_text_box_takes_the_text_directly_with_no_keys_or_clipboard() {
        let backend = FakeBackend::new();
        backend.set_direct_insert(true);
        let out = type_text(&backend, OFF, "a long sentence, no paste", &TypeOptions::new(6)).unwrap();
        assert_eq!(out, TypeOutcome::Inserted);
        assert_eq!(out.note_suffix(), " (direct)");
        assert_eq!(backend.inserted(), vec!["a long sentence, no paste".to_string()]);
        assert!(backend.events().is_empty(), "no key events, no paste chord");
    }

    #[test]
    fn a_trailing_newline_is_still_a_real_enter_after_a_direct_insert() {
        let backend = FakeBackend::new();
        backend.set_direct_insert(true);
        type_text(&backend, OFF, "query\n", &TypeOptions::new(6)).unwrap();
        assert_eq!(backend.inserted(), vec!["query".to_string()]);
        let events = backend.events();
        assert!(!events.is_empty(), "Enter was pressed");
        assert!(
            !events.iter().any(|e| matches!(e, InputEvent::Type(_))),
            "no characters typed: {events:?}"
        );
    }

    #[test]
    fn direct_insert_is_skipped_when_switched_off() {
        let backend = FakeBackend::new();
        backend.set_direct_insert(true);
        let mut opts = TypeOptions::new(6);
        opts.direct = false;
        assert_eq!(type_text(&backend, OFF, "abc", &opts).unwrap(), TypeOutcome::Typed);
        assert!(backend.inserted().is_empty());
    }

    #[test]
    fn short_text_is_typed_key_by_key() {
        let backend = FakeBackend::new();
        let out = type_text(&backend, OFF, "abc", &TypeOptions::new(6)).unwrap();
        assert_eq!(out, TypeOutcome::Typed);
        assert_eq!(out.note_suffix(), "");
        assert_eq!(backend.typed_text(), "abc");
        assert_eq!(backend.events().len(), 3, "one event per character");
    }

    #[test]
    fn the_threshold_is_in_characters_and_inclusive() {
        let backend = FakeBackend::new();
        let six = type_text(&backend, OFF, "123456", &TypeOptions::new(6)).unwrap();
        assert_eq!(six, TypeOutcome::Typed);
        let emoji = type_text(
            &backend,
            OFF,
            "\u{1F642}\u{1F642}\u{1F642}",
            &TypeOptions::new(3),
        )
        .unwrap();
        assert_eq!(
            emoji,
            TypeOutcome::Typed,
            "three characters, though six UTF-16 units"
        );
    }

    #[test]
    fn long_text_is_pasted_and_the_owners_clipboard_comes_back() {
        let backend = FakeBackend::new();
        backend.set_clipboard(ClipboardContent::Text("jon's clipboard".into()));
        // Pinned to Windows: this test asserts Ctrl+V, and `TypeOptions::new` follows the OS the
        // tests run on (a_mac_pastes_with_command covers the other half).
        let opts = TypeOptions {
            paste_over: 6,
            platform: Platform::Windows,
            direct: true,
        };
        let out = type_text(&backend, OFF, "a long sentence to paste", &opts).unwrap();
        assert_eq!(out, TypeOutcome::Pasted);
        assert_eq!(out.note_suffix(), " (pasted)");
        assert_eq!(backend.typed_text(), "", "nothing was keyed");
        let keys: Vec<InputEvent> = backend.events();
        assert_eq!(
            keys,
            [
                InputEvent::KeyDown(Key::Ctrl),
                InputEvent::KeyDown(Key::Char('v')),
                InputEvent::KeyUp(Key::Char('v')),
                InputEvent::KeyUp(Key::Ctrl),
            ]
        );
        assert_eq!(backend.clipboard_text().unwrap(), "jon's clipboard");
    }

    #[test]
    fn a_trailing_newline_is_pasted_as_a_real_enter_press() {
        let backend = FakeBackend::new();
        let opts = TypeOptions {
            paste_over: 6,
            platform: Platform::Windows,
            direct: true,
        };
        let out = type_text(&backend, OFF, "made-by-hekaton\n", &opts).unwrap();
        assert_eq!(out, TypeOutcome::Pasted);
        let keys: Vec<InputEvent> = backend.events();
        assert_eq!(
            &keys[4..],
            [
                InputEvent::KeyDown(Key::Enter),
                InputEvent::KeyUp(Key::Enter)
            ],
            "{keys:?}"
        );
        assert!(
            backend
                .clipboard_history()
                .iter()
                .any(|t| t == "made-by-hekaton"),
            "the newline is not pasted"
        );
    }

    #[test]
    fn short_text_presses_enter_for_a_newline() {
        let backend = FakeBackend::new();
        type_text(&backend, OFF, "ab\r\n", &TypeOptions::new(6)).unwrap();
        assert_eq!(backend.typed_text(), "ab");
        assert_eq!(
            backend.events().last(),
            Some(&InputEvent::KeyUp(Key::Enter))
        );
    }

    #[test]
    fn a_mac_pastes_with_command() {
        let backend = FakeBackend::new();
        let opts = TypeOptions {
            paste_over: 2,
            platform: Platform::Mac,
            direct: true,
        };
        type_text(&backend, OFF, "hello", &opts).unwrap();
        assert_eq!(backend.events()[0], InputEvent::KeyDown(Key::Meta));
    }

    #[test]
    fn an_empty_clipboard_is_not_restored_as_something() {
        let backend = FakeBackend::new();
        type_text(&backend, OFF, "paste this please", &TypeOptions::new(6)).unwrap();
        // the saved text was "" (empty clipboard) and goes back as ""
        assert_eq!(backend.clipboard_text().unwrap(), "");
    }

    // Ported from tests/test_destructive.py::test_the_typed_text_path_is_guarded.
    #[test]
    fn the_typed_text_path_is_guarded() {
        let backend = FakeBackend::new();
        let err = type_text(
            &backend,
            OFF,
            "vssadmin delete shadows /all",
            &TypeOptions::new(500),
        )
        .unwrap_err();
        assert!(matches!(err, TypeError::Refused(_)));
        assert!(err.to_string().contains("allow_destructive"));
        assert!(backend.typed_text().is_empty() && backend.events().is_empty());
        // a long refused command is not pasted either
        let err = type_text(
            &backend,
            OFF,
            "vssadmin delete shadows /all",
            &TypeOptions::new(3),
        )
        .unwrap_err();
        assert!(matches!(err, TypeError::Refused(_)));
        assert!(backend.events().is_empty());
        assert_eq!(backend.clipboard_text().unwrap(), "");
    }

    #[test]
    fn the_opt_in_lets_a_destructive_looking_text_through() {
        let backend = FakeBackend::new();
        type_text(
            &backend,
            Policy::allow(),
            "vssadmin delete shadows",
            &TypeOptions::new(500),
        )
        .unwrap();
        assert_eq!(backend.typed_text(), "vssadmin delete shadows");
    }
}
