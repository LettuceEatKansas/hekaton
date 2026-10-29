//! The actions, one at a time, over a fake desktop. Ported from `tests/test_computer.py`,
//! `tests/test_20260921_live_next.py` (right_click and copy reports, h-copyfile, h-contextmenu) and
//! `tests/test_20261008_speed_fixes.py` (W5, W10).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::StepError;
use super::testkit::{H, Kit, W, cmd_key, png_size};
use crate::backend::fake::InputEvent;
use crate::backend::{
    ClipboardContent, Element, MenuItem, MouseButton, OcrLine, OcrResult, OcrWord, Point, Rect,
    Role, ScrollDirection, WindowRef,
};
use crate::clock::Clock;
use crate::env::MapEnv;
use crate::keys::Key;

/// A front window whose accessibility tree has these named controls.
fn app_with(kit: &Kit, controls: &[(&str, Role, Rect)]) -> WindowRef {
    let w = kit.front("App", "app.exe", 11);
    let mut root = Element::new("root", Role::Window, "App");
    for (i, (name, role, rect)) in controls.iter().enumerate() {
        root = root.with_child(Element::new(&format!("c{i}"), role.clone(), name).at(*rect));
    }
    kit.fake.set_tree(w, root);
    w
}

/// A display and screen big enough for a 320 x 60 grab around a point.
fn big_screen(kit: &Kit) {
    kit.fake.set_displays(vec![crate::backend::Display {
        id: 1,
        name: String::new(),
        rect: Rect::from_xywh(0, 0, 1000, 300),
        scale: 1.0,
        primary: true,
    }]);
    kit.fake.set_screen(super::testkit::textured(1000, 300));
}

/// Remember every chord the fake presses.
fn record_chords(kit: &Kit) -> Arc<Mutex<Vec<Vec<Key>>>> {
    let seen: Arc<Mutex<Vec<Vec<Key>>>> = Arc::default();
    let sink = seen.clone();
    kit.fake
        .set_chord_hook(move |_, keys| sink.lock().unwrap().push(keys.to_vec()));
    seen
}

fn ctrl(c: char) -> Vec<Key> {
    vec![Key::Ctrl, Key::Char(c)]
}

fn clicks(kit: &Kit) -> Vec<(Point, MouseButton, u8)> {
    kit.fake
        .events()
        .into_iter()
        .filter_map(|e| match e {
            InputEvent::Click { at, button, count } => Some((at, button, count)),
            _ => None,
        })
        .collect()
}

fn word(text: &str, l: i32, t: i32, r: i32, b: i32) -> OcrWord {
    OcrWord {
        text: text.into(),
        rect: Rect::new(l, t, r, b),
        confidence: 1.0,
    }
}

fn ocr_line(words: Vec<OcrWord>) -> OcrLine {
    OcrLine {
        text: words
            .iter()
            .map(|w| w.text.clone())
            .collect::<Vec<_>>()
            .join(" "),
        rect: Rect::default(),
        words,
        confidence: 1.0,
    }
}

fn err_of(r: Result<(String, Option<String>), StepError>) -> StepError {
    r.expect_err("the step should fail")
}

// ---------------------------------------------------------------------------- screenshot, zoom

#[test]
fn a_screenshot_says_what_space_its_coordinates_are_in() {
    let kit = Kit::new();
    let (note, img) = kit.step(json!({"action": "screenshot"})).unwrap();
    assert_eq!(
        note,
        format!("screenshot; give coordinates in pixels of this {W}x{H} image")
    );
    assert!(img.is_some());
    let norm = kit.ctx(json!({"coords": "norm1000"}));
    let (note, _) = norm
        .run(&Kit::args(json!({"action": "screenshot"})))
        .unwrap();
    assert_eq!(note, "screenshot; give coordinates in 0-1000 on both axes");
}

// Ported from test_zoom_accepts_box_sent_as_coordinate (h-clockdate) and
// test_zoom_without_a_box_says_what_it_needs.
#[test]
fn a_zoom_box_may_arrive_as_coordinate_and_must_exist() {
    let kit = Kit::new();
    let (note, img) = kit
        .step(json!({"action": "zoom", "coordinate": [W - 6, H - 5, W, H]}))
        .unwrap();
    assert!(
        note.starts_with("zoomed view (coordinates stay in the full screenshot's space)"),
        "{note}"
    );
    assert!(img.is_some());
    let err = err_of(kit.step(json!({"action": "zoom"})));
    assert!(matches!(err, StepError::Value(_)));
    assert_eq!(
        err.message(),
        "zoom needs region: [x0, y0, x1, y1], the box to magnify"
    );
    let err = err_of(kit.step(json!({"action": "zoom", "region": [1, 2, 3]})));
    assert!(err.message().starts_with("zoom needs region"));
}

// ---------------------------------------------------------------------------- keys

// Ported from test_lone_symbol_key_is_typed.
#[test]
fn a_lone_symbol_key_is_typed_and_names_are_pressed() {
    let kit = Kit::new();
    kit.front("Editor", "editor.exe", 30);
    let chords = record_chords(&kit);
    kit.step(json!({"action": "key", "text": "*"})).unwrap();
    kit.step(json!({"action": "key", "text": "ctrl+a"}))
        .unwrap();
    kit.step(json!({"action": "key", "text": "Return"}))
        .unwrap();
    let (note, _) = kit
        .step(json!({"action": "key", "text": "4309*82="}))
        .unwrap();
    // arithmetic goes key by key: a calculator ignores a pasted "4309*82="
    assert_eq!(kit.fake.typed_text(), "*4309*82=");
    assert_eq!(note, "typed '4309*82=' (not a key name, so typed as text)");
    let seen = chords.lock().unwrap().clone();
    assert_eq!(seen, vec![ctrl('a'), vec![Key::Enter]]);
}

// Ported from test_key_text_fallback_only_for_calculator_like_text (R1).
#[test]
fn a_mistyped_key_name_is_an_error_not_typed_text() {
    let kit = Kit::new();
    let err = err_of(kit.step(json!({"action": "key", "text": "Retun"})));
    assert!(err.message().starts_with("unknown key: 'retun'"), "{err}");
    assert_eq!(kit.fake.typed_text(), "");
    let err = err_of(kit.step(json!({"action": "key", "text": "1+2"})));
    assert_eq!(err.message(), "too many non-modifier keys in combo: '1+2'");
}

#[test]
fn space_separated_keys_are_pressed_in_turn_and_modifier_words_stay_one_chord() {
    let kit = Kit::new();
    kit.front("Editor", "editor.exe", 30);
    let chords = record_chords(&kit);
    let (note, _) = kit
        .step(json!({"action": "key", "text": "ctrl+a Delete Return"}))
        .unwrap();
    assert_eq!(note, "pressed ctrl+a, delete, enter in turn");
    let seen = chords.lock().unwrap().clone();
    assert_eq!(seen, vec![ctrl('a'), vec![Key::Delete], vec![Key::Enter]]);
    chords.lock().unwrap().clear();
    kit.step(json!({"action": "key", "text": "ctrl shift s"}))
        .unwrap();
    let seen = chords.lock().unwrap().clone();
    assert_eq!(seen, vec![vec![Key::Ctrl, Key::Shift, Key::Char('s')]]);
    chords.lock().unwrap().clear();
    let (note, _) = kit
        .step(json!({"action": "key", "text": "1 2 * 3 Return"}))
        .unwrap();
    assert_eq!(note, "pressed 1, 2, *, 3, enter in turn");
    assert!(kit.fake.typed_text().contains('*'));
    let err = err_of(kit.step(json!({"action": "key", "text": "Home Retun"})));
    assert!(err.message().starts_with("unknown key"), "{err}");
}

#[test]
fn key_repeat_is_clamped_and_a_lone_symbol_repeats() {
    let kit = Kit::new();
    kit.step(json!({"action": "key", "text": "%", "repeat": 3}))
        .unwrap();
    assert_eq!(kit.fake.typed_text(), "%%%");
    let chords = record_chords(&kit);
    kit.step(json!({"action": "key", "text": "down", "repeat": 500}))
        .unwrap();
    assert_eq!(chords.lock().unwrap().len(), 100);
}

// Ported from test_windows_key_combo_refused_under_allowlist (S1) and
// test_windows_key_combo_still_allowed_with_no_allowlist.
#[test]
fn windows_key_combos_are_refused_under_an_allowlist_only() {
    let kit = Kit::new();
    for call in [
        json!({"action": "key", "text": "win+r"}),
        json!({"action": "hold_key", "text": "win+r", "duration": 0.01}),
    ] {
        let err = err_of(kit.step_only(&["notepad.exe"], call));
        assert!(matches!(err, StepError::Target(_)));
        assert!(err.message().contains("Windows-key"), "{err}");
    }
    assert!(kit.fake.events().is_empty());
    let chords = record_chords(&kit);
    kit.step(json!({"action": "key", "text": "win+r"})).unwrap();
    assert_eq!(
        chords.lock().unwrap().clone(),
        vec![vec![Key::Meta, Key::Char('r')]]
    );
}

#[test]
fn hold_key_holds_then_releases_in_reverse() {
    let kit = Kit::new();
    let (note, _) = kit
        .step(json!({"action": "hold_key", "text": "ctrl+shift", "duration": 0.25}))
        .unwrap();
    assert_eq!(note, "held ctrl+shift 0.25s");
    assert_eq!(
        kit.fake.events(),
        vec![
            InputEvent::KeyDown(Key::Ctrl),
            InputEvent::KeyDown(Key::Shift),
            InputEvent::KeyUp(Key::Shift),
            InputEvent::KeyUp(Key::Ctrl)
        ]
    );
    assert!(kit.clock.now() >= Duration::from_millis(250));
    let (note, _) = kit
        .step(json!({"action": "hold_key", "text": "a", "duration": 999}))
        .unwrap();
    assert_eq!(note, "held a 30s");
}

#[test]
fn typing_needs_text_and_reports_how_it_went_in() {
    let kit = Kit::new();
    kit.front("Editor", "editor.exe", 30);
    assert_eq!(
        err_of(kit.step(json!({"action": "type"}))).message(),
        "type needs text"
    );
    let (note, _) = kit.step(json!({"action": "type", "text": "abc"})).unwrap();
    assert_eq!(note, "typed 3 chars");
    let (note, _) = kit
        .step(json!({"action": "type", "text": "a longer sentence"}))
        .unwrap();
    assert_eq!(note, "typed 17 chars (pasted)");
}

// Ported from test_keys_without_a_window_refuse_the_agents_own_app (caller-relative since 2026-09-24)
// and test_20260924_caller_guard: keystrokes into the process driving winhands are refused.
#[test]
fn keys_and_text_into_the_callers_own_window_are_refused() {
    let mut kit = Kit::new();
    kit.front("Claude", "claude.exe", 100);
    kit.who = crate::caller::Caller::new(
        vec![
            (100, "claude.exe".into()),
            (200, "windowsterminal.exe".into()),
        ],
        false,
    );
    for call in [
        json!({"action": "type", "text": "hello"}),
        json!({"action": "key", "text": "enter"}),
    ] {
        let err = err_of(kit.step(call));
        assert!(matches!(err, StepError::Target(_)));
        assert!(
            err.message()
                .starts_with("refused: that window belongs to claude.exe (pid 100)"),
            "{err}"
        );
        assert!(
            err.message().contains("calling winhands right now"),
            "{err}"
        );
    }
    assert!(kit.fake.events().is_empty());
    // another window is ordinary work, whoever is calling
    kit.front("Notepad", "notepad.exe", 5);
    kit.step(json!({"action": "type", "text": "ok"})).unwrap();
    assert_eq!(kit.fake.typed_text(), "ok");
}

#[test]
fn an_owner_protected_window_is_refused_for_keys_and_clicks() {
    let kit = Kit::new();
    kit.front("Vault", "keepass.exe", 8);
    std::fs::create_dir_all(kit.computer.config().paths().dir()).unwrap();
    std::fs::write(kit.computer.config().paths().protected(), "KeePass.exe\n\n").unwrap();
    let err = err_of(kit.step(json!({"action": "type", "text": "x"})));
    assert!(matches!(err, StepError::Permission(_)), "{err:?}");
    assert_eq!(
        err.message(),
        "process 'keepass.exe' is protected, action refused"
    );
    let err = err_of(kit.step(json!({"action": "left_click", "coordinate": [50, 50]})));
    assert!(err.message().contains("is protected"), "{err}");
    assert!(kit.fake.events().is_empty());
}

#[test]
fn a_click_on_the_uac_consent_window_is_refused_whatever_the_allowlist() {
    let kit = Kit::new();
    kit.front("User Account Control", "consent.exe", 1);
    let err = err_of(kit.step(json!({"action": "left_click", "coordinate": [50, 50]})));
    assert!(matches!(err, StepError::Permission(_)), "{err:?}");
    assert!(
        err.message()
            .contains("elevation or credential prompt (consent.exe)"),
        "{err}"
    );
    assert!(err.message().contains("allow_destructive"));
    assert!(kit.fake.events().is_empty());
}

// ---------------------------------------------------------------------------- scroll

// Ported from test_scroll_reads_misplaced_amount_and_spot.
#[test]
fn scroll_reads_a_misplaced_amount_and_spot() {
    let kit = Kit::new();
    let ctx = kit.ctx(json!({"coords": "norm1000"}));
    let (note, _) = ctx
        .run(&Kit::args(
            json!({"action": "scroll", "start_coordinate": [500, 500],
                               "text": "10", "scroll_direction": "down"}),
        ))
        .unwrap();
    assert_eq!(note, "scrolled down 10");
    assert_eq!(
        kit.fake.events(),
        vec![InputEvent::Scroll {
            at: Point::new(159, 89),
            direction: ScrollDirection::Down,
            notches: 10
        }]
    );
}

#[test]
fn scroll_defaults_clamps_and_holds_modifiers() {
    let kit = Kit::new();
    kit.step(json!({"action": "scroll", "coordinate": [10, 20]}))
        .unwrap();
    kit.step(
        json!({"action": "scroll", "coordinate": [10, 20], "scroll_direction": "up",
                    "scroll_amount": 99, "text": "shift"}),
    )
    .unwrap();
    let ev = kit.fake.events();
    assert_eq!(
        ev[0],
        InputEvent::Scroll {
            at: Point::new(10, 20),
            direction: ScrollDirection::Down,
            notches: 3
        }
    );
    assert_eq!(ev[1], InputEvent::KeyDown(Key::Shift));
    assert_eq!(
        ev[2],
        InputEvent::Scroll {
            at: Point::new(10, 20),
            direction: ScrollDirection::Up,
            notches: 30
        }
    );
    assert_eq!(ev[3], InputEvent::KeyUp(Key::Shift));
}

#[test]
fn a_modifier_is_released_even_when_the_action_fails() {
    let kit = Kit::new();
    let err = err_of(kit.step(json!({"action": "scroll", "coordinate": [10, 20],
                                     "scroll_direction": "sideways", "text": "ctrl"})));
    assert_eq!(err.message(), "bad direction: 'sideways'");
    let ev = kit.fake.events();
    assert_eq!(ev.first(), Some(&InputEvent::KeyDown(Key::Ctrl)));
    assert_eq!(ev.last(), Some(&InputEvent::KeyUp(Key::Ctrl)));
}

#[test]
fn an_unknown_modifier_is_a_value_error() {
    let kit = Kit::new();
    let err =
        err_of(kit.step(json!({"action": "left_click", "coordinate": [5, 5], "text": "hyper"})));
    assert!(matches!(err, StepError::Value(_)));
    assert_eq!(err.message(), "unknown modifier 'hyper'");
    assert!(kit.fake.events().is_empty());
}

// ---------------------------------------------------------------------------- pointer actions

#[test]
fn the_click_family_reports_what_it_did() {
    let kit = Kit::new();
    app_with(&kit, &[]);
    let (note, img) = kit
        .step(json!({"action": "left_click", "coordinate": [30, 20]}))
        .unwrap();
    assert_eq!(
        (note.as_str(), img.is_none()),
        ("left click at [30, 20]", true)
    );
    assert_eq!(
        kit.step(json!({"action": "middle_click", "coordinate": [31, 20]}))
            .unwrap()
            .0,
        "middle click at [31, 20]"
    );
    assert_eq!(
        kit.step(json!({"action": "double_click", "coordinate": [32, 20]}))
            .unwrap()
            .0,
        "double click at [32, 20]"
    );
    assert_eq!(
        kit.step(json!({"action": "triple_click", "coordinate": [33, 20]}))
            .unwrap()
            .0,
        "triple click at [33, 20]"
    );
    assert_eq!(
        clicks(&kit),
        vec![
            (Point::new(30, 20), MouseButton::Left, 1),
            (Point::new(31, 20), MouseButton::Middle, 1),
            (Point::new(32, 20), MouseButton::Left, 2),
            (Point::new(33, 20), MouseButton::Left, 1),
            (Point::new(33, 20), MouseButton::Left, 1),
            (Point::new(33, 20), MouseButton::Left, 1),
        ]
    );
}

#[test]
fn a_click_without_a_coordinate_uses_the_pointer_and_says_where() {
    use crate::backend::Backend;
    let kit = Kit::new();
    kit.fake.mouse_move(Point::new(100, 90)).unwrap();
    let (note, _) = kit.step(json!({"action": "left_click"})).unwrap();
    assert_eq!(note, "left click at [100, 90]");
    let (note, _) = kit.step(json!({"action": "cursor_position"})).unwrap();
    assert_eq!(note, "cursor at [100, 90]");
    let (note, _) = kit.step(json!({"action": "mouse_move"})).unwrap();
    assert_eq!(note, "moved to None");
}

#[test]
fn mouse_buttons_and_moves_report_in_python_words() {
    let kit = Kit::new();
    assert_eq!(
        kit.step(json!({"action": "left_mouse_down", "coordinate": [5, 6]}))
            .unwrap()
            .0,
        "left mouse down"
    );
    assert_eq!(
        kit.step(json!({"action": "left_mouse_up", "coordinate": [5, 6]}))
            .unwrap()
            .0,
        "left mouse up"
    );
    assert_eq!(
        kit.step(json!({"action": "mouse_move", "coordinate": [7, 8]}))
            .unwrap()
            .0,
        "moved to [7, 8]"
    );
    let ev = kit.fake.events();
    assert_eq!(ev[0], InputEvent::MouseMove(Point { x: 5, y: 6 }));
    assert_eq!(
        ev[1],
        InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true
        }
    );
    assert_eq!(
        ev[2],
        InputEvent::MouseButton {
            button: MouseButton::Left,
            down: false
        }
    );
    assert_eq!(ev[3], InputEvent::MouseMove(Point::new(7, 8)));
}

#[test]
fn a_coordinate_outside_the_screenshot_is_refused_before_any_input() {
    let kit = Kit::new();
    let err = err_of(kit.step(json!({"action": "left_click", "coordinate": [W, 5]})));
    assert_eq!(
        err.message(),
        format!("coordinate [{W}, 5] is outside the {W}x{H} screenshot")
    );
    let err = err_of(kit.step(json!({"action": "left_click", "coordinate": "10,10"})));
    assert_eq!(err.message(), "coordinate must be [x, y]");
    assert!(kit.fake.events().is_empty());
}

// Ported from test_cross_window_drag_reports_and_stays_allowed and
// test_same_window_drag_no_cross_window_tag.
#[test]
fn a_drag_between_two_windows_says_so_and_a_drag_inside_one_does_not() {
    let kit = Kit::new();
    kit.window("Left", "a.exe", 1, Rect::new(0, 0, 150, 180));
    kit.window("Right", "b.exe", 2, Rect::new(150, 0, 320, 180));
    let (note, _) = kit
        .step(json!({"action": "left_click_drag", "start_coordinate": [10, 10], "coordinate": [200, 10]}))
        .unwrap();
    assert_eq!(note, "dragged [10, 10] -> [200, 10] (cross-window)");
    let (note, _) = kit
        .step(json!({"action": "left_click_drag", "start_coordinate": [10, 10], "coordinate": [20, 10]}))
        .unwrap();
    assert_eq!(note, "dragged [10, 10] -> [20, 10]");
    let drags: Vec<InputEvent> = kit
        .fake
        .events()
        .into_iter()
        .filter(|e| matches!(e, InputEvent::Drag { .. }))
        .collect();
    assert_eq!(
        drags[0],
        InputEvent::Drag {
            from: Point::new(10, 10),
            to: Point::new(200, 10)
        }
    );
    let err = err_of(kit.step(json!({"action": "left_click_drag", "coordinate": [20, 10]})));
    assert_eq!(err.message(), "coordinate must be [x, y]");
}

// ---------------------------------------------------------------------------- allowlist on clicks

#[test]
fn input_under_an_allowlist_only_reaches_allowed_apps() {
    let kit = Kit::new();
    kit.window("Notes", "notepad.exe", 1, Rect::new(0, 0, 150, 180));
    kit.window("Game", "slaythespire2.exe", 2, Rect::new(150, 0, 320, 180));
    kit.step_only(
        &["notepad.exe"],
        json!({"action": "left_click", "coordinate": [10, 10]}),
    )
    .unwrap();
    let err = err_of(kit.step_only(
        &["notepad.exe"],
        json!({"action": "left_click", "coordinate": [200, 10]}),
    ));
    assert_eq!(
        err.message(),
        "refused: that window belongs to slaythespire2.exe, which this task may not touch (allowed: notepad.exe)"
    );
    // no allowlist: no limit
    kit.step(json!({"action": "left_click", "coordinate": [200, 10]}))
        .unwrap();
}

// Ported from test_irreversible_click_refused_unless_allowed, test_irreversible_match_is_whole_word,
// test_irreversible_gate_off_without_allowlist and test_irreversible_name_lookup_handles_failure.
#[test]
fn irreversible_looking_buttons_are_refused_under_an_allowlist() {
    let kit = Kit::new();
    app_with(
        &kit,
        &[
            ("Submit Order", Role::Button, Rect::new(20, 20, 60, 40)),
            ("Getting Started", Role::Button, Rect::new(100, 20, 160, 40)),
        ],
    );
    let only = &["app.exe"];
    let err = err_of(kit.step_only(
        only,
        json!({"action": "left_click", "coordinate": [30, 30]}),
    ));
    assert_eq!(
        err.message(),
        "refused: 'Submit Order' looks like an irreversible action ('submit'); pass \"allow_irreversible\": true to click it anyway"
    );
    assert!(clicks(&kit).is_empty());
    kit.step_only(
        only,
        json!({"action": "left_click", "coordinate": [30, 30], "allow_irreversible": true}),
    )
    .unwrap();
    assert_eq!(clicks(&kit).len(), 1);
    // whole word only: "Getting Started" must not trip the "get" rule
    kit.step_only(
        only,
        json!({"action": "left_click", "coordinate": [110, 30]}),
    )
    .unwrap();
    // double clicks are checked too, right clicks are not
    let err = err_of(kit.step_only(
        only,
        json!({"action": "double_click", "coordinate": [30, 30]}),
    ));
    assert!(err.message().contains("irreversible"));
    kit.step_only(
        only,
        json!({"action": "right_click", "coordinate": [30, 30]}),
    )
    .unwrap();
    // no allowlist: the gate does not even look
    kit.step(json!({"action": "left_click", "coordinate": [30, 30]}))
        .unwrap();
    // nothing under the point, or an app that does not answer: no refusal
    kit.step_only(
        only,
        json!({"action": "left_click", "coordinate": [300, 170]}),
    )
    .unwrap();
}

#[test]
fn strict_does_not_take_the_models_allow_irreversible() {
    let kit = Kit::with_env(crate::env::MapEnv::new().with("WINHANDS_SAFETY_PROFILE", "strict"));
    app_with(
        &kit,
        &[("Submit Order", Role::Button, Rect::new(20, 20, 60, 40))],
    );
    let err = err_of(kit.step_only(
        &["app.exe"],
        json!({"action": "left_click", "coordinate": [30, 30], "allow_irreversible": true}),
    ));
    assert!(err.message().contains("irreversible"), "{}", err.message());
    assert!(clicks(&kit).is_empty());
}

#[test]
fn the_irreversible_words_are_the_documented_set() {
    for (name, hit) in [
        ("Send", "send"),
        ("Place Order", "place order"),
        ("Uninstall", "uninstall"),
        ("Pay now", "pay"),
        ("Delete all", "delete"),
    ] {
        let kit = Kit::new();
        app_with(&kit, &[(name, Role::Button, Rect::new(20, 20, 60, 40))]);
        let err = err_of(kit.step_only(
            &["app.exe"],
            json!({"action": "left_click", "coordinate": [30, 30]}),
        ));
        assert!(
            err.message().contains(&format!("('{hit}')")),
            "{name}: {err}"
        );
    }
}

// ---------------------------------------------------------------------------- describe the landing

// Ported from test_click_on_a_row_names_the_row, test_click_on_empty_list_says_no_item and
// test_click_on_a_game_canvas_says_nothing (h-copyfile).
#[test]
fn a_click_says_what_it_landed_on() {
    let kit = Kit::new();
    let w = kit.front("Explorer", "explorer.exe", 4);
    let cell = Element::new("cell", Role::Edit, "Name").at(Rect::new(20, 20, 100, 40));
    let row = Element::new("row", Role::ListItem, "invoice-541.txt")
        .at(Rect::new(10, 20, 200, 40))
        .with_child(cell);
    let list = Element::new("list", Role::List, "Items View")
        .at(Rect::new(10, 10, 300, 150))
        .with_child(row);
    let pane = Element::new("pane", Role::Pane, "Slay the Spire 2").at(Rect::new(0, 0, 10, 10));
    kit.fake.set_tree(
        w,
        Element::new("root", Role::Window, "Explorer")
            .with_child(list)
            .with_child(pane),
    );
    let (note, _) = kit
        .step(json!({"action": "left_click", "coordinate": [30, 30]}))
        .unwrap();
    assert_eq!(
        note,
        "left click at [30, 30] (on list item 'invoice-541.txt')"
    );
    // the empty part of the list
    let (note, _) = kit
        .step(json!({"action": "left_click", "coordinate": [250, 100]}))
        .unwrap();
    assert_eq!(
        note,
        "left click at [250, 100] (on list 'Items View', no item there)"
    );
    // a bare canvas says nothing
    let (note, _) = kit
        .step(json!({"action": "left_click", "coordinate": [5, 5]}))
        .unwrap();
    assert_eq!(note, "left click at [5, 5]");
    // middle and triple clicks do not describe
    let (note, _) = kit
        .step(json!({"action": "middle_click", "coordinate": [30, 30]}))
        .unwrap();
    assert_eq!(note, "middle click at [30, 30]");
}

// ---------------------------------------------------------------------------- right-click menus

fn copy_paste_menu(kit: &Kit) {
    kit.fake.set_popup_menu(
        vec![WindowRef::from_raw(4242)],
        vec![
            MenuItem {
                name: "Copy".into(),
                rect: Rect::new(0, 0, 20, 10),
            },
            MenuItem {
                name: "Paste".into(),
                rect: Rect::new(0, 20, 20, 30),
            },
            MenuItem {
                name: "Delete".into(),
                rect: Rect::new(0, 0, 0, 0),
            },
        ],
    );
}

// Ported from test_right_click_reports_menu_items.
#[test]
fn right_click_reports_the_menu_items_with_click_points() {
    let kit = Kit::new();
    copy_paste_menu(&kit);
    let (note, _) = kit
        .step(json!({"action": "right_click", "coordinate": [10, 10]}))
        .unwrap();
    assert_eq!(
        note,
        "right click at [10, 10]; menu: Copy @[10, 5], Paste @[10, 25], Delete"
    );
}

// Ported from test_right_click_with_no_menu_returns_early (W5).
#[test]
fn right_click_with_no_menu_returns_early() {
    let kit = Kit::new();
    let (note, _) = kit
        .step(json!({"action": "right_click", "coordinate": [10, 10]}))
        .unwrap();
    assert!(!note.contains("menu"), "{note}");
    assert!(
        kit.clock.now() <= Duration::from_millis(250),
        "about 200 ms of polling, not 1.03 s: {:?}",
        kit.clock.now()
    );
}

// Ported from test_right_click_with_a_menu_keeps_the_patient_read (W5).
#[test]
fn right_click_with_a_menu_keeps_the_patient_read() {
    let kit = Kit::new();
    copy_paste_menu(&kit);
    kit.fake.set_popup_delay(2); // the menu window shows up on the third look
    kit.fake.set_menu_delay(5); // and builds its items slowly (shell extensions)
    let (note, _) = kit
        .step(json!({"action": "right_click", "coordinate": [10, 10]}))
        .unwrap();
    assert!(
        note.ends_with("menu: Copy @[10, 5], Paste @[10, 25], Delete"),
        "{note}"
    );
}

// Ported from test_right_click_still_finds_a_menu_in_a_non_popup_window (W5): Chromium and Electron
// draw menus in plain top-level windows, one quick look catches them.
#[test]
fn right_click_still_finds_a_menu_in_a_non_popup_window() {
    let kit = Kit::new();
    kit.fake.set_popup_menu(
        vec![],
        vec![MenuItem {
            name: "Back".into(),
            rect: Rect::new(0, 0, 20, 10),
        }],
    );
    let (note, _) = kit
        .step(json!({"action": "right_click", "coordinate": [10, 10]}))
        .unwrap();
    assert!(note.ends_with("menu: Back @[10, 5]"), "{note}");
    assert!(kit.clock.now() <= Duration::from_millis(250));
}

// Ported from test_menu_items_waits_for_a_late_menu.
#[test]
fn menu_items_waits_for_a_late_menu() {
    let kit = Kit::new();
    copy_paste_menu(&kit);
    kit.fake.set_menu_delay(2);
    let ctx = kit.ctx(json!({}));
    let items = ctx.menu_items(5.0, 0.4);
    assert_eq!(items.len(), 3);
    assert!(kit.clock.now() <= Duration::from_millis(200));
    // no menu at all: it gives up after the wait
    let empty = Kit::new();
    assert!(empty.ctx(json!({})).menu_items(0.3, 0.4).is_empty());
}

// Ported from test_shift_f10_lists_the_menu.
#[test]
fn shift_f10_lists_the_menu_it_opens() {
    let kit = Kit::new();
    kit.fake.set_popup_menu(
        vec![],
        vec![MenuItem {
            name: "Copy".into(),
            rect: Rect::new(0, 0, 20, 10),
        }],
    );
    let (note, _) = kit
        .step(json!({"action": "key", "text": "shift+F10"}))
        .unwrap();
    assert_eq!(note, "pressed shift+f10; menu: Copy @[10, 5]");
}

#[test]
fn a_click_inside_an_open_menu_says_what_is_showing_now() {
    let kit = Kit::new();
    copy_paste_menu(&kit);
    let (note, _) = kit
        .step(json!({"action": "left_click", "coordinate": [10, 5]}))
        .unwrap();
    assert_eq!(
        note,
        "left click at [10, 5]; menu now: Copy @[10, 5], Paste @[10, 25], Delete"
    );
}

// ---------------------------------------------------------------------------- clipboard

// Ported from test_ctrl_c_with_nothing_selected_says_so, test_ctrl_c_reports_the_copied_files and
// test_other_keys_do_not_touch_the_clipboard_check.
#[test]
fn ctrl_c_says_whether_anything_reached_the_clipboard() {
    let kit = Kit::new();
    let (note, _) = kit
        .step(json!({"action": "key", "text": "ctrl+c"}))
        .unwrap();
    assert_eq!(
        note,
        "pressed ctrl+c; clipboard unchanged: nothing was copied (is the item or text really selected? click it once and check it is highlighted)"
    );
    kit.fake.set_chord_hook(|fake, keys| {
        if keys == [Key::Ctrl, Key::Char('c')] {
            fake.set_clipboard(ClipboardContent::Files(vec![
                "C:\\docs\\invoice-541.txt".into(),
            ]));
        }
    });
    let (note, _) = kit
        .step(json!({"action": "key", "text": "ctrl+c"}))
        .unwrap();
    assert_eq!(
        note,
        "pressed ctrl+c; clipboard now holds 1 file(s): invoice-541.txt"
    );
    let (note, _) = kit
        .step(json!({"action": "key", "text": "ctrl+v"}))
        .unwrap();
    assert_eq!(
        note, "pressed ctrl+v",
        "paste does not look at the clipboard"
    );
}

#[test]
fn a_copy_of_text_or_many_files_is_summarised() {
    let kit = Kit::new();
    kit.fake.set_chord_hook(|fake, keys| {
        if keys == [Key::Ctrl, Key::Char('x')] {
            fake.set_clipboard(ClipboardContent::Text("x".repeat(100)));
        }
        if keys == [Key::Ctrl, Key::Insert] {
            fake.set_clipboard(ClipboardContent::Files(
                (1..=7).map(|i| format!("/d/f{i}.txt")).collect(),
            ));
        }
    });
    let (note, _) = kit
        .step(json!({"action": "key", "text": "ctrl+x"}))
        .unwrap();
    assert_eq!(
        note,
        format!(
            "pressed ctrl+x; clipboard now holds text (100 chars): '{}'",
            "x".repeat(60)
        )
    );
    let (note, _) = kit
        .step(json!({"action": "key", "text": "ctrl+insert"}))
        .unwrap();
    assert_eq!(
        note,
        "pressed ctrl+insert; clipboard now holds 7 file(s): f1.txt, f2.txt, f3.txt, f4.txt, f5.txt and 2 more"
    );
}

// Ported from test_clipboard_get_and_set, test_clipboard_set_over_cap_refused and
// test_clipboard_set_needs_text.
#[test]
fn clipboard_get_and_set() {
    let kit = Kit::new();
    kit.fake.set_clipboard(ClipboardContent::Text("old".into()));
    let (note, _) = kit
        .step(json!({"action": "clipboard_set", "text": "hello"}))
        .unwrap();
    assert_eq!(note, "clipboard set to 5 chars");
    let (note, _) = kit.step(json!({"action": "clipboard_get"})).unwrap();
    assert_eq!(note, "clipboard (5 chars): 'hello'");
    kit.fake.set_clipboard(ClipboardContent::Empty);
    assert_eq!(
        kit.step(json!({"action": "clipboard_get"})).unwrap().0,
        "clipboard (0 chars): ''"
    );
}

#[test]
fn clipboard_set_is_capped_and_needs_text() {
    let kit = Kit::with_env(MapEnv::new().with("WINHANDS_CLIP_MAX", "5"));
    let err = err_of(kit.step(json!({"action": "clipboard_set", "text": "way too long"})));
    assert_eq!(
        err.message(),
        "clipboard_set text is 12 chars, over the 5-char cap"
    );
    assert_eq!(
        err_of(kit.step(json!({"action": "clipboard_set"}))).message(),
        "clipboard_set needs text"
    );
}

// ---------------------------------------------------------------------------- waiting

#[test]
fn wait_asks_the_screen_not_the_clock() {
    let kit = Kit::new();
    let (note, _) = kit
        .step(json!({"action": "wait", "duration": 2.5}))
        .unwrap();
    assert!(
        note.starts_with("waited ") && note.ends_with("s (asked 2.5s, screen settled)"),
        "{note}"
    );
    assert!(
        kit.clock.now() < Duration::from_millis(1100),
        "a static screen settles after the 40 percent grace"
    );
    let (note, _) = kit
        .step(json!({"action": "wait", "duration": 0.2}))
        .unwrap();
    assert_eq!(note, "waited 0.2s (asked 0.2s, screen settled)");
    let (note, _) = kit.step(json!({"action": "wait", "duration": 99})).unwrap();
    assert!(note.contains("(asked 30s,"), "{note}");
    let (note, _) = kit.step(json!({"action": "wait"})).unwrap();
    assert!(note.contains("(asked 1s,"), "{note}");
}

// Ported from test_wait_for_text_found and test_wait_for_text_timeout.
#[test]
fn wait_for_text_finds_it_or_times_out() {
    let kit = Kit::new();
    app_with(&kit, &[("Done", Role::Text, Rect::new(10, 10, 50, 30))]);
    let (note, _) = kit
        .step(json!({"action": "wait_for_text", "text": "Done", "timeout": 5}))
        .unwrap();
    assert_eq!(note, "wait_for_text: 'Done' appeared after 0.0s (uia)");
    let start = kit.clock.now();
    let (note, _) = kit
        .step(json!({"action": "wait_for_text", "text": "Never", "timeout": 1}))
        .unwrap();
    assert_eq!(note, "wait_for_text: 'Never' did not appear within 1s");
    assert!(kit.clock.now() - start >= Duration::from_secs(1));
    assert_eq!(
        err_of(kit.step(json!({"action": "wait_for_text"}))).message(),
        "wait_for_text needs text: the words to wait for"
    );
    // the timeout is clamped to 0.1..120
    let (note, _) = kit
        .step(json!({"action": "wait_for_text", "text": "Never", "timeout": 9999}))
        .unwrap();
    assert!(note.ends_with("did not appear within 120s"), "{note}");
}

// Ported from test_wait_for_file_found and the missing-file case.
#[test]
fn wait_for_file_finds_it_or_times_out() {
    let kit = Kit::new();
    let f = kit.tmp.path().join("out.txt");
    std::fs::write(&f, "x").unwrap();
    let p = f.to_string_lossy().to_string();
    let (note, _) = kit
        .step(json!({"action": "wait_for_file", "text": p}))
        .unwrap();
    assert_eq!(note, format!("wait_for_file: {p} appeared after 0.0s"));
    let gone = kit
        .tmp
        .path()
        .join("nope.txt")
        .to_string_lossy()
        .to_string();
    let (note, _) = kit
        .step(json!({"action": "wait_for_file", "path": gone, "timeout": 0.5}))
        .unwrap();
    assert_eq!(
        note,
        format!("wait_for_file: {gone} did not appear within 0.5s")
    );
    assert_eq!(
        err_of(kit.step(json!({"action": "wait_for_file"}))).message(),
        "wait_for_file needs text: the file path to watch for"
    );
}

// Ported from test_wait_for_file_under_temp_required_when_allowlisted.
#[test]
fn under_an_allowlist_files_are_only_watched_under_temp() {
    let kit = Kit::new();
    let temp = kit.tmp.path().to_string_lossy().to_string();
    let outside = kit
        .tmp
        .path()
        .parent()
        .unwrap()
        .join("elsewhere")
        .join("out.txt")
        .to_string_lossy()
        .to_string();
    let err = err_of(kit.step_only(
        &["notepad.exe"],
        json!({"action": "wait_for_file", "text": outside, "timeout": 0.1}),
    ));
    assert_eq!(
        err.message(),
        format!(
            "refused: under this task's allowlist, wait_for_file may only watch paths under {temp} (got {outside})"
        )
    );
    let inside = kit.tmp.path().join("out.txt").to_string_lossy().to_string();
    kit.step_only(
        &["notepad.exe"],
        json!({"action": "wait_for_file", "text": inside, "timeout": 0.1}),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------- list_files

// Ported from test_list_files_confined_to_temp_under_allowlist.
#[test]
fn list_files_is_confined_to_temp_under_an_allowlist() {
    let kit = Kit::new();
    let dir = kit.tmp.path().join("work");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    std::fs::write(dir.join("b.log"), "y").unwrap();
    let only = &["notepad.exe"];
    let outside = kit
        .tmp
        .path()
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let err = err_of(kit.step_only(only, json!({"action": "list_files", "text": outside})));
    assert!(
        err.message().starts_with(
            "refused: under this task's allowlist, list_files may only list paths under "
        ),
        "{err}"
    );
    let (out, img) = kit
        .step_only(
            only,
            json!({"action": "list_files", "text": dir.to_string_lossy(), "filter": "*.txt"}),
        )
        .unwrap();
    assert!(
        out.contains("a.txt") && !out.contains("b.log") && img.is_none(),
        "{out}"
    );
    assert_eq!(
        err_of(kit.step(json!({"action": "list_files"}))).message(),
        "list_files needs text: the folder path"
    );
    // path= works as well as text=, and limit is honoured
    let (out, _) = kit
        .step(json!({"action": "list_files", "path": dir.to_string_lossy(), "limit": 1}))
        .unwrap();
    assert!(
        out.contains("2 items, by modified desc")
            && out.contains("... 1 more, raise limit to see them"),
        "{out}"
    );
}

// ---------------------------------------------------------------------------- click_text, replace_word

// Ported from test_click_text_clicks_the_only_match.
#[test]
fn click_text_clicks_the_only_match() {
    let kit = Kit::new();
    app_with(&kit, &[("Save", Role::Button, Rect::new(10, 10, 50, 30))]);
    let (note, img) = kit
        .step(json!({"action": "click_text", "text": "save"}))
        .unwrap();
    assert_eq!(note, "clicked 'Save' (uia) at [30, 20]");
    assert!(img.is_none());
    assert_eq!(
        clicks(&kit),
        vec![(Point::new(30, 20), MouseButton::Left, 1)]
    );
    kit.step(json!({"action": "click_text", "text": "save", "button": "right", "double": true}))
        .unwrap();
    assert_eq!(clicks(&kit)[1], (Point::new(30, 20), MouseButton::Right, 2));
}

// Ported from test_click_text_ambiguous_refuses_and_lists_candidates and
// test_click_text_nth_picks_candidate.
#[test]
fn click_text_with_two_matches_lists_them_and_nth_picks_one() {
    let kit = Kit::new();
    app_with(
        &kit,
        &[
            ("Save", Role::Button, Rect::new(10, 10, 50, 30)),
            ("Save As", Role::Button, Rect::new(60, 40, 100, 60)),
        ],
    );
    let err = err_of(kit.step(json!({"action": "click_text", "text": "save"})));
    assert!(matches!(err, StepError::Target(_)));
    assert_eq!(
        err.message(),
        "2 matches for 'save', ambiguous: 1: 'Save'@[30, 20], 2: 'Save As'@[80, 50]. Pass nth to pick one"
    );
    kit.step(json!({"action": "click_text", "text": "save", "nth": 2}))
        .unwrap();
    assert_eq!(
        clicks(&kit),
        vec![(Point::new(80, 50), MouseButton::Left, 1)]
    );
    let err = err_of(kit.step(json!({"action": "click_text", "text": "save", "nth": 3})));
    assert_eq!(
        err.message(),
        "nth=3 out of range; found 2 match(es) for 'save'"
    );
}

// Ported from test_click_text_no_match_refused.
#[test]
fn click_text_with_no_match_is_refused() {
    let kit = Kit::new();
    let err = err_of(kit.step(json!({"action": "click_text", "text": "nowhere"})));
    assert!(matches!(err, StepError::Target(_)));
    assert_eq!(
        err.message(),
        "no text on screen matches 'nowhere' (tried UIA then OCR); zoom or find_text first to see what is actually there"
    );
    assert_eq!(
        err_of(kit.step(json!({"action": "click_text"}))).message(),
        "click_text needs text: the words to click"
    );
}

// Ported from test_click_text_prefers_the_open_menu: the menu is its own window, so the walk of the
// window behind it must not run.
#[test]
fn click_text_prefers_the_open_menu() {
    let kit = Kit::new();
    app_with(&kit, &[("Copy", Role::Button, Rect::new(1, 1, 5, 5))]);
    kit.fake.set_popup_menu(
        vec![WindowRef::from_raw(99)],
        vec![
            MenuItem {
                name: "Copy as path".into(),
                rect: Rect::new(60, 170, 300, 200),
            },
            MenuItem {
                name: "Copy\tCtrl+C".into(),
                rect: Rect::new(60, 200, 300, 230),
            },
        ],
    );
    let ctx = kit.ctx(json!({}));
    let found = ctx.locate_text("copy").unwrap();
    assert_eq!(
        found.len(),
        1,
        "the exact label before the tab wins over a substring"
    );
    assert_eq!(
        (
            found[0].x,
            found[0].y,
            found[0].source,
            found[0].label.as_str()
        ),
        (180, 215, "menu", "Copy\tCtrl+C")
    );
    // no exact label: substring matches
    let ctx2 = kit.ctx(json!({}));
    let found = ctx2.locate_text("as path").unwrap();
    assert_eq!(found[0].label, "Copy as path");
}

// Ported from test_replace_word_double_clicks_and_types.
#[test]
fn replace_word_double_clicks_and_types() {
    let kit = Kit::new();
    app_with(&kit, &[("draft", Role::Text, Rect::new(40, 50, 60, 70))]);
    let (note, _) = kit
        .step(json!({"action": "replace_word", "old": "draft", "new": "final"}))
        .unwrap();
    assert_eq!(
        clicks(&kit),
        vec![(Point::new(50, 60), MouseButton::Left, 2)]
    );
    assert_eq!(kit.fake.typed_text(), "final");
    assert_eq!(note, "replaced 'draft' (uia) with 'final'");
    // old falls back to text; a long replacement is pasted
    let (note, _) = kit
        .step(json!({"action": "replace_word", "text": "draft", "new": "a much longer word"}))
        .unwrap();
    assert!(
        note.ends_with("with 'a much longer word' (pasted)"),
        "{note}"
    );
}

// Ported from test_replace_word_needs_old_and_new.
#[test]
fn replace_word_needs_old_and_new() {
    let kit = Kit::new();
    let err = err_of(kit.step(json!({"action": "replace_word", "old": "draft"})));
    assert!(matches!(err, StepError::Value(_)));
    assert_eq!(err.message(), "replace_word needs old and new");
    let err = err_of(kit.step(json!({"action": "replace_word", "old": "ghost", "new": "x"})));
    assert!(
        err.message()
            .starts_with("no text on screen matches 'ghost'"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------- double-click aim

// Ported from test_double_click_snaps_from_a_gap_onto_the_word and test_snap_is_off_unless_asked.
#[test]
fn a_double_click_in_a_gap_snaps_onto_the_nearest_word_when_asked() {
    let kit = Kit::new();
    big_screen(&kit);
    app_with(&kit, &[]);
    // the grab box is x-160..x+160, y-30..y+30 upscaled 3x; words sit in that 960x180 picture
    kit.fake.set_ocr(OcrResult {
        lines: vec![ocr_line(vec![
            word("status:", 280, 70, 440, 110),
            word("draft", 495, 70, 625, 110),
        ])],
        engine: "fake".into(),
    });
    let ctx = kit.ctx(json!({"snap": true}));
    let (x, y, w) = ctx.snap_to_word(300, 100);
    assert_eq!(w, "draft");
    assert_eq!(x, (300.0 - 160.0 + (495.0 + 625.0) / 6.0) as i32);
    assert_eq!(y, (100.0 - 30.0 + (70.0 + 110.0) / 6.0) as i32);
    // a word 60 px away is out of reach
    kit.fake.set_ocr(OcrResult {
        lines: vec![ocr_line(vec![word("status:", 280, 70, 300, 110)])],
        engine: "fake".into(),
    });
    assert_eq!(ctx.snap_to_word(300, 100).2, "");
    // off unless asked: no capture, no OCR
    let off = kit.ctx(json!({}));
    let calls = kit.fake.ocr_calls();
    assert_eq!(off.snap_to_word(300, 100), (300, 100, String::new()));
    assert_eq!(kit.fake.ocr_calls(), calls);
}

#[test]
fn a_double_click_with_snap_reports_the_word_it_moved_onto() {
    let kit = Kit::new();
    big_screen(&kit);
    app_with(&kit, &[]);
    kit.fake.set_ocr(OcrResult {
        lines: vec![ocr_line(vec![word("draft", 495, 70, 625, 110)])],
        engine: "fake".into(),
    });
    let ctx = kit.ctx(json!({"snap": true}));
    let (note, _) = ctx
        .run(&Kit::args(
            json!({"action": "double_click", "coordinate": [300, 100]}),
        ))
        .unwrap();
    assert!(
        note.starts_with("(aim: moved onto the word 'draft') double click at [300, 100]"),
        "{note}"
    );
    let at = clicks(&kit)[0].0;
    assert_ne!(at, Point::new(300, 100));
    // a point already on a word stays
    kit.fake.set_ocr(OcrResult {
        lines: vec![ocr_line(vec![word("here", 400, 70, 560, 130)])],
        engine: "fake".into(),
    });
    let (x, y, w) = ctx.snap_to_word(300, 100);
    assert_eq!((x, y, w.as_str()), (300, 100, ""));
}

// ---------------------------------------------------------------------------- read, list_windows

// Ported from test_read_lists_elements_and_respects_the_allowlist.
#[test]
fn read_lists_elements_and_respects_the_allowlist() {
    let kit = Kit::new();
    let w = kit.window("Notepad", "notepad.exe", 5, Rect::from_xywh(0, 0, 100, 100));
    kit.fake.set_tree(
        w,
        Element::new("r", Role::Window, "Notepad")
            .with_child(Element::new("a", Role::Button, "Save").at(Rect::new(1, 1, 9, 9)))
            .with_child(Element::new("b", Role::Button, "Open").at(Rect::new(11, 1, 19, 9))),
    );
    kit.window("Mail", "mail.exe", 6, Rect::from_xywh(100, 0, 100, 100));
    let (out, img) = kit
        .step(json!({"action": "read", "text": "Notepad", "filter": "save", "limit": 5}))
        .unwrap();
    assert_eq!(out, "window \"Notepad\"\n0 Button \"Save\"");
    assert!(img.is_none());
    let err = err_of(kit.step_only(&["notepad.exe"], json!({"action": "read", "text": "Mail"})));
    assert_eq!(
        err.message(),
        "refused: that window belongs to mail.exe, which this task may not touch (allowed: notepad.exe)"
    );
    // read of an allowed window works under the allowlist
    kit.step_only(
        &["notepad.exe"],
        json!({"action": "read", "text": "Notepad"}),
    )
    .unwrap();
}

// Ported from test_read_is_passive_by_default_and_says_how_to_wake and
// test_read_with_wake_true_allows_the_raise (W10).
#[test]
fn read_does_not_raise_a_window_unless_asked() {
    let kit = Kit::new();
    let w = kit.window("Steam", "steam.exe", 5, Rect::from_xywh(0, 0, 100, 100));
    kit.fake.set_tree(
        w,
        Element::new("r", Role::Window, "Steam")
            .with_child(Element::new("a", Role::Button, "Store").at(Rect::new(1, 1, 9, 9))),
    );
    kit.fake.set_chromium_empty(w);
    let (note, img) = kit
        .step(json!({"action": "read", "text": "Steam"}))
        .unwrap();
    assert!(img.is_none());
    assert!(note.ends_with(super::snapshot::WAKE_HINT), "{note}");
    assert_eq!(
        note.matches("wake=true").count(),
        1,
        "one hint line, not a repeat"
    );
    assert!(kit.fake.wakes().is_empty(), "read is passive");
    for value in [json!(true), json!("true")] {
        let kit = Kit::new();
        let w = kit.window("Steam", "steam.exe", 5, Rect::from_xywh(0, 0, 100, 100));
        kit.fake.set_tree(
            w,
            Element::new("r", Role::Window, "Steam")
                .with_child(Element::new("a", Role::Button, "Store").at(Rect::new(1, 1, 9, 9))),
        );
        kit.fake.set_chromium_empty(w);
        let (note, _) = kit
            .step(json!({"action": "read", "text": "Steam", "wake": value}))
            .unwrap();
        assert!(
            !note.contains("wake=true") && note.contains("0 Button \"Store\""),
            "{note}"
        );
        assert_eq!(kit.fake.wakes(), vec![w]);
    }
}

#[test]
fn read_with_no_window_in_front_names_the_problem() {
    let kit = Kit::new();
    let err = err_of(kit.step(json!({"action": "read"})));
    assert_eq!(err.message(), "window handle unavailable; refresh windows");
    let err = err_of(kit.step(json!({"action": "read", "limit": "many"})));
    let _ = err;
}

#[test]
fn list_windows_lists_hidden_ones_on_request() {
    let kit = Kit::new();
    kit.window(
        "Calculator",
        "ApplicationFrameHost.exe",
        1,
        Rect::from_xywh(0, 0, 10, 10),
    );
    let mut ghost = crate::backend::WindowInfo::basic(
        "Calculator",
        "CalculatorApp.exe",
        2,
        Rect::from_xywh(0, 0, 10, 10),
    );
    ghost.hidden = true;
    kit.fake.add_window(ghost);
    let (plain, _) = kit.step(json!({"action": "list_windows"})).unwrap();
    assert_eq!(plain.lines().count(), 1);
    let (all, _) = kit
        .step(json!({"action": "list_windows", "all": true}))
        .unwrap();
    assert_eq!(all.lines().count(), 2);
    assert!(all.contains("[cloaked]"));
}

// 2026-10-29 leads run: the owner's mail window title showed up in a task scoped to two apps.
#[test]
fn list_windows_under_only_apps_names_only_allowed_windows() {
    let kit = Kit::new();
    kit.window(
        "Untitled - Notepad",
        "notepad.exe",
        1,
        Rect::from_xywh(0, 0, 10, 10),
    );
    kit.window(
        "Inbox - Mail",
        "outlook.exe",
        2,
        Rect::from_xywh(0, 0, 10, 10),
    );
    let (out, _) = kit
        .step_only(&["notepad.exe"], json!({"action": "list_windows"}))
        .unwrap();
    assert!(out.contains("Untitled - Notepad"), "{out}");
    assert!(!out.contains("Inbox"), "{out}");
    assert!(
        out.ends_with("(1 windows of apps outside only_apps not shown)"),
        "{out}"
    );
    let (plain, _) = kit.step(json!({"action": "list_windows"})).unwrap();
    assert!(plain.contains("Inbox"), "{plain}");
}

// ---------------------------------------------------------------------------- expect

fn expect_kit() -> Kit {
    let kit = Kit::new();
    let w = app_with(&kit, &[("Saved", Role::Text, Rect::new(10, 10, 50, 30))]);
    kit.fake
        .update_window(w, |i| i.title = "Untitled - Notepad".into());
    kit
}

// Ported from test_expect_pass_when_text_found, test_expect_fail_when_text_missing,
// test_expect_fail_when_gone_text_still_present and test_expect_title_check.
#[test]
fn expect_checks_text_gone_text_and_title() {
    let kit = expect_kit();
    let ctx = kit.ctx(json!({}));
    let check = |v: Value| ctx.check_expect(&v).unwrap();
    assert_eq!(check(json!({"text": "Saved"})), "expect: PASS");
    assert_eq!(
        check(json!({"text": "Missing"})),
        "expect: FAIL (text 'Missing' not found)"
    );
    assert_eq!(check(json!({"gone_text": "Missing"})), "expect: PASS");
    assert_eq!(
        check(json!({"gone_text": "Saved"})),
        "expect: FAIL (text 'Saved' is still there)"
    );
    assert_eq!(check(json!({"title": "Notepad"})), "expect: PASS");
    assert_eq!(
        check(json!({"title": "Calculator"})),
        "expect: FAIL (title is 'untitled - notepad', wanted 'Calculator')"
    );
    assert_eq!(
        check(json!({"text": "Missing", "title": "Calculator"})),
        "expect: FAIL (title is 'untitled - notepad', wanted 'Calculator'; text 'Missing' not found)"
    );
    assert_eq!(
        check(json!({})),
        "expect: FAIL (expect needs at least one of text, gone_text, title)"
    );
    assert_eq!(
        check(json!("Saved")),
        "expect: FAIL (expect must be an object with text/gone_text/title)"
    );
}

// Ported from test_expect_wired_into_run_appends_to_note.
#[test]
fn expect_is_appended_to_the_steps_note() {
    let kit = expect_kit();
    let (note, _) = kit
        .step(json!({"action": "wait", "duration": 0.1, "expect": {"text": "Saved"}}))
        .unwrap();
    assert_eq!(
        note,
        "waited 0.1s (asked 0.1s, screen settled); expect: PASS"
    );
    let error = kit
        .step(json!({"action": "left_click", "coordinate": [1, 1], "expect": {"text": "Nope"}}))
        .unwrap_err();
    assert!(matches!(error, StepError::Expectation(_)));
    assert!(
        error
            .message()
            .ends_with("; expect: FAIL (text 'Nope' not found)"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------- misc wiring

#[test]
fn actions_that_send_input_mark_it_as_ours() {
    let kit = Kit::with_env(MapEnv::new());
    kit.fake.set_human_idle(Duration::from_millis(5));
    // first click: the person-at-the-keyboard wait fires and gives up
    assert!(
        kit.step(json!({"action": "left_click", "coordinate": [5, 5]}))
            .is_err()
    );
    // after our own input, the same low idle time is just the echo of it
    let kit = Kit::with_env(MapEnv::new().with("WINHANDS_HUMAN_PAUSE", "0"));
    kit.fake.set_human_idle(Duration::from_millis(5));
    kit.step(json!({"action": "left_click", "coordinate": [5, 5]}))
        .unwrap();
    kit.step(json!({"action": "left_click", "coordinate": [6, 6]}))
        .unwrap();
}

#[test]
fn the_last_point_is_ringed_on_the_next_screenshot() {
    let kit = Kit::with_env(MapEnv::new().with("WINHANDS_IMG", "png"));
    kit.step(json!({"action": "left_click", "coordinate": [100, 90]}))
        .unwrap();
    let (_, with_ring) = kit.step(json!({"action": "screenshot"})).unwrap();
    let off = Kit::with_env(
        MapEnv::new()
            .with("WINHANDS_IMG", "png")
            .with("WINHANDS_MARK", "0"),
    );
    off.step(json!({"action": "left_click", "coordinate": [100, 90]}))
        .unwrap();
    let (_, plain) = off.step(json!({"action": "screenshot"})).unwrap();
    assert_ne!(with_ring, plain, "the marker changes the picture");
    // a click long ago is not drawn on a fresh screenshot
    kit.clock.advance(std::time::Duration::from_secs(31));
    let (_, stale) = kit.step(json!({"action": "screenshot"})).unwrap();
    let never = Kit::with_env(MapEnv::new().with("WINHANDS_IMG", "png"));
    let (_, unclicked) = never.step(json!({"action": "screenshot"})).unwrap();
    assert_eq!(stale, unclicked, "a stale marker is dropped");
}

// Ported from test_zoom_never_upscales_more_than_2x, test_zoom_never_exceeds_shot_long and
// test_zoom_still_downscales_a_big_region (W3), through the zoom action rather than the maths.
#[test]
fn a_zoom_picture_is_capped_at_twice_its_pixels_and_at_shot_long() {
    // shot space is screen space here (1280 long edge), so a 400 x 300 box used to become 1280 x 960
    let ctx_size = |long: i64, region: [i32; 4]| {
        let kit = Kit::with_env(
            MapEnv::new()
                .with("WINHANDS_IMG", "png")
                .with("WINHANDS_ZOOM_OCR", "0")
                .with("WINHANDS_SHOT_LONG", &long.to_string()),
        );
        kit.fake.set_displays(vec![crate::backend::Display {
            id: 1,
            name: String::new(),
            rect: Rect::from_xywh(0, 0, 1280, 720),
            scale: 1.0,
            primary: true,
        }]);
        kit.fake.clear_screen();
        let (_, img) = kit
            .step(json!({"action": "zoom", "region": region}))
            .unwrap();
        png_size(&img.unwrap())
    };
    assert_eq!(ctx_size(1280, [0, 0, 400, 300]), (800, 600));
    let (w, h) = ctx_size(500, [0, 0, 200, 150]); // 400 x 300 screen px
    assert_eq!(w.max(h), 500);
    assert!(h.abs_diff(375) <= 2, "{h}");
    assert_eq!(ctx_size(640, [0, 0, 600, 300]), (640, 320));
}

#[test]
fn a_click_into_the_callers_own_window_is_refused_before_any_input() {
    let mut kit = Kit::new();
    kit.window("Claude", "claude.exe", 100, Rect::new(0, 0, 150, 180));
    kit.window("Notepad", "notepad.exe", 5, Rect::new(150, 0, 320, 180));
    kit.who = crate::caller::Caller::new(vec![(100, "claude.exe".into())], false);
    for call in [
        json!({"action": "left_click", "coordinate": [10, 10]}),
        json!({"action": "scroll", "coordinate": [10, 10]}),
        json!({"action": "left_click_drag", "start_coordinate": [200, 10], "coordinate": [10, 10]}),
        json!({"action": "left_mouse_down", "coordinate": [10, 10]}),
    ] {
        let err = err_of(kit.step(call.clone()));
        assert!(
            err.message()
                .starts_with("refused: that window belongs to claude.exe (pid 100)"),
            "{call}: {err}"
        );
    }
    assert!(kit.fake.events().is_empty());
    kit.step(json!({"action": "left_click", "coordinate": [200, 10]}))
        .unwrap();
}

// Ported from test_keys_without_a_window_refuse_the_agents_own_app (2026-09-19): when the caller
// cannot be identified, the old fixed list of agent, terminal and editor processes stands in.
#[test]
fn keys_into_an_agent_host_are_refused_when_the_caller_is_unknown() {
    for host in [
        "claude.exe",
        "hermes.exe",
        "codex.exe",
        "windowsterminal.exe",
        "code.exe",
    ] {
        let kit = Kit::new();
        kit.front("Agent", host, 7);
        let err = err_of(kit.step(json!({"action": "type", "text": "hello"})));
        assert!(matches!(err, StepError::Target(_)));
        assert!(err.message().contains(host), "{host}: {err}");
        assert!(
            err.message()
                .contains("could not work out which process is calling"),
            "{err}"
        );
        assert!(kit.fake.events().is_empty());
    }
    let kit = Kit::new();
    kit.front("Notes", "notepad.exe", 7);
    kit.step(json!({"action": "type", "text": "hello"}))
        .unwrap();
    assert_eq!(kit.fake.typed_text(), "hello");
}

// 2026-10-28 gauntlet L1: a scoped task saved a file through file_dialog, then was refused when it
// waited for that same file. Its own save may be watched; any other path outside %TEMP% may not.
#[test]
fn under_an_allowlist_the_file_this_call_saved_may_be_watched() {
    let kit = Kit::new();
    let base = kit.tmp.path().parent().unwrap().join("elsewhere");
    let saved = base.join("saved.txt").to_string_lossy().to_string();
    let other = base.join("other.txt").to_string_lossy().to_string();
    let ctx = kit.ctx(json!({"only_apps": ["notepad.exe"]}));
    assert!(
        ctx.check_wait_path_allowed(&saved, "wait_for_file may only watch")
            .is_err()
    );
    ctx.o.saved_paths.lock().unwrap().push(saved.clone());
    ctx.check_wait_path_allowed(&saved, "wait_for_file may only watch")
        .unwrap();
    assert!(
        ctx.check_wait_path_allowed(&other, "wait_for_file may only watch")
            .is_err()
    );
}

// 2026-10-11 (macOS, Brave address bar): a pasted trailing newline never submitted a one-line
// field. The body is typed, then Enter is pressed as a real key.
#[test]
fn a_trailing_newline_is_a_real_enter_press() {
    let kit = Kit::new();
    app_with(&kit, &[]);
    kit.step(json!({"action": "type", "text": "go\n"})).unwrap();
    assert_eq!(kit.fake.typed_text(), "go");
    let enters = kit
        .fake
        .events()
        .iter()
        .filter(|e| matches!(e, InputEvent::KeyDown(Key::Enter)))
        .count();
    assert_eq!(enters, 1);
}

// 2026-10-28 (macOS bench): read(window="Safari") silently read the front window, and "Safari" never
// matched because the window title is the page and the process is com.apple.Safari.
#[test]
fn read_finds_a_window_by_its_app_and_takes_window_as_well_as_text() {
    let kit = Kit::new();
    let front = kit.front("Untitled", "TextEdit.exe", 5);
    kit.fake
        .set_tree(front, Element::new("r", Role::Window, "Untitled"));
    let mail = kit.window(
        "Compose Mail - Gmail",
        "com.apple.Safari",
        6,
        Rect::from_xywh(100, 0, 100, 100),
    );
    kit.fake.set_tree(
        mail,
        Element::new("r", Role::Window, "Compose Mail - Gmail"),
    );
    let (out, _) = kit
        .step(json!({"action": "read", "window": "Safari"}))
        .unwrap();
    assert!(out.starts_with("window \"Compose Mail - Gmail\""), "{out}");
    let (out, _) = kit
        .step(json!({"action": "read", "text": "textedit"}))
        .unwrap();
    assert!(out.starts_with("window \"Untitled\""), "{out}");
    // two windows of one app stay ambiguous: never guess which one
    kit.window(
        "Inbox - Gmail",
        "com.apple.Safari",
        6,
        Rect::from_xywh(300, 0, 100, 100),
    );
    let err = err_of(kit.step(json!({"action": "read", "text": "Safari"})));
    assert!(
        err.message().contains("ambiguous window"),
        "{}",
        err.message()
    );
}

// ---------------------------------------------------------------------------- press (2026-10-29)

fn pressable_app(kit: &Kit) -> WindowRef {
    let w = kit.front("Calc", "calc.exe", 41);
    let mut seven = Element::new("seven", Role::Button, "7").at(Rect::new(10, 10, 50, 50));
    seven.affordances.push(crate::backend::Affordance::Invoke);
    let label = Element::new("pic", Role::Button, "Logo").at(Rect::new(100, 10, 140, 50));
    kit.fake.set_tree(
        w,
        Element::new("root", Role::Window, "Calc")
            .with_child(seven)
            .with_child(label),
    );
    w
}

#[test]
fn press_does_the_elements_own_action_without_the_mouse() {
    let kit = Kit::new();
    pressable_app(&kit);
    kit.step(json!({"action": "read", "text": "Calc"})).unwrap();
    let (note, img) = kit.step(json!({"action": "press", "text": "0"})).unwrap();
    assert!(note.starts_with("pressed 0 "), "{note}");
    assert!(note.ends_with("'7'"), "{note}");
    assert!(img.is_none());
    assert!(clicks(&kit).is_empty(), "no mouse click");
    assert_eq!(kit.fake.performed().len(), 1);
}

#[test]
fn press_clicks_the_centre_of_an_element_with_no_action() {
    let kit = Kit::new();
    pressable_app(&kit);
    kit.step(json!({"action": "read", "text": "Calc"})).unwrap();
    let (note, _) = kit.step(json!({"action": "press", "text": "1"})).unwrap();
    assert!(note.contains("it has no press action"), "{note}");
    let c = clicks(&kit);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].0, Point { x: 120, y: 30 });
}

#[test]
fn press_needs_a_number_from_a_read() {
    let kit = Kit::new();
    pressable_app(&kit);
    let err = err_of(kit.step(json!({"action": "press", "text": "Save"})));
    assert!(
        err.message().starts_with("press needs text"),
        "{}",
        err.message()
    );
    let err = err_of(kit.step(json!({"action": "press", "text": "5"})));
    assert!(err.message().contains("read it first"), "{}", err.message());
}

#[test]
fn typing_into_the_same_box_skips_the_settle_between_steps() {
    let kit = Kit::new();
    kit.front("Editor", "editor.exe", 30);
    let before = kit.fake.preview_count() + kit.fake.capture_count();
    kit.run(json!({"actions": [
        {"action": "type", "text": "ab"}, {"action": "type", "text": "cd"}, {"action": "key", "text": "End"}
    ], "shot": false}))
    .unwrap();
    let typed_looks = kit.fake.preview_count() + kit.fake.capture_count() - before;
    let before = kit.fake.preview_count() + kit.fake.capture_count();
    kit.run(json!({"actions": [
        {"action": "type", "text": "ab\n"}, {"action": "type", "text": "cd"}, {"action": "key", "text": "End"}
    ], "shot": false}))
    .unwrap();
    let enter_looks = kit.fake.preview_count() + kit.fake.capture_count() - before;
    assert!(
        typed_looks < enter_looks,
        "{typed_looks} vs {enter_looks}: Enter still waits"
    );
}
