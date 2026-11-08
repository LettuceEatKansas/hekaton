//! `run_many` and the daemon's `computer` route, over a fake desktop. Ported from
//! `tests/test_computer.py` (batching, per-call options, expect), `tests/test_20261008_speed_fixes.py`
//! (W2 shot and shot_long, W3 no image before input, W4 settle floor), `tests/test_execution.py`,
//! `tests/test_daemon_batch_status.py` and `tests/test_20260924_decisions.py` (the gate, as it reaches
//! the `computer` route).

use std::time::Duration;

use serde_json::{Value, json};

use super::testkit::Kit;
use super::{CallOpts, StepError};
use crate::backend::fake::InputEvent;
use crate::backend::{Element, Rect, Role, WindowRef};
use crate::clock::Clock;
use crate::destructive::Policy;
use crate::env::MapEnv;

fn no_shot_env() -> MapEnv {
    MapEnv::new().with("HEKATON_AUTO_SHOT", "0")
}

/// A front window that exposes real accessibility elements, so the settle floor is the ordinary 130 ms.
fn rich_front(kit: &Kit) -> WindowRef {
    let w = kit.front("Notepad", "notepad.exe", 7);
    let mut root = Element::new("root", Role::Window, "Notepad");
    for i in 0..5 {
        root = root.with_child(
            Element::new(&format!("b{i}"), Role::Button, &format!("B{i}")).at(Rect::new(
                i * 10,
                0,
                i * 10 + 8,
                8,
            )),
        );
    }
    kit.fake.set_tree(w, root);
    w
}

fn click(x: i32, y: i32) -> Value {
    json!({"action": "left_click", "coordinate": [x, y]})
}

// ---------------------------------------------------------------------------- batching

// Ported from test_batch_stops_at_first_error_and_names_the_step.
#[test]
fn a_batch_stops_at_the_first_error_and_names_the_step() {
    let kit = Kit::with_env(no_shot_env());
    let out = kit
        .run(json!({"actions": [
            click(10, 10),
            {"action": "key", "text": "definitelynotakey"},
            {"action": "type", "text": "never typed"}
        ]}))
        .unwrap();
    assert!(
        out.text
            .contains("step 2 (key) failed: unknown key: 'definitelynotakey'"),
        "{}",
        out.text
    );
    assert_eq!(kit.fake.typed_text(), "", "step 3 must not run");
    assert!(
        kit.fake
            .events()
            .iter()
            .any(|e| matches!(e, InputEvent::Click { .. }))
    );
    assert!(out.images.is_empty());
}

// Ported from test_every_zoom_in_a_batch_comes_back.
#[test]
fn every_zoom_in_a_batch_comes_back() {
    let kit = Kit::with_env(no_shot_env());
    let out = kit
        .run(json!({"actions": [
            {"action": "mouse_move", "coordinate": [5, 5]},
            {"action": "zoom", "region": [0, 0, 100, 60]},
            {"action": "mouse_move", "coordinate": [6, 6]},
            {"action": "zoom", "region": [100, 60, 200, 120]}
        ]}))
        .unwrap();
    assert_eq!(out.images.len(), 2);
    assert!(
        out.text.contains("[image 1]") && out.text.contains("[image 2]"),
        "{}",
        out.text
    );
    assert_eq!(out.mime, "image/jpeg");
}

// Ported from test_opener_shortcut_waits_for_focus.
#[test]
fn an_opener_shortcut_waits_for_focus_before_the_next_step() {
    let kit = Kit::with_env(no_shot_env());
    kit.front("Notepad", "notepad.exe", 7);
    let out = kit
        .run(json!({"actions": [
            {"action": "key", "text": "ctrl+n"},
            {"action": "type", "text": "hi"},
            {"action": "key", "text": "ctrl+shift+s"}
        ]}))
        .unwrap();
    assert!(
        out.text.contains("pressed ctrl+n") && out.text.contains("typed 2 chars"),
        "{}",
        out.text
    );
    // ctrl+n waits for focus (a settle that watches 3 looks); type settles on one calm read (2 looks);
    // the last step settles nothing.
    assert_eq!(kit.fake.capture_count(), 5);
    assert_eq!(kit.fake.typed_text(), "hi");
}

// Ported from test_f2_without_rename_box_stops_the_batch_in_explorer.
#[test]
fn f2_without_a_rename_box_stops_the_batch_in_explorer() {
    let kit = Kit::with_env(no_shot_env());
    let w = kit.front("Docs", "explorer.exe", 9);
    kit.fake
        .update_window(w, |i| i.class = "CabinetWClass".into());
    kit.fake.set_has_caret(false);
    let out = kit
        .run(json!({"actions": [
            {"action": "key", "text": "F2"},
            {"action": "type", "text": "new"},
            {"action": "key", "text": "Return"}
        ]}))
        .unwrap();
    assert_eq!(kit.fake.typed_text(), "", "nothing after F2 may run");
    assert!(out.text.contains("opened no rename box"), "{}", out.text);
    assert!(
        out.text.contains("step 2 and later skipped:"),
        "{}",
        out.text
    );
}

// Ported from test_f2_without_rename_box_outside_explorer_just_continues (R1).
#[test]
fn f2_outside_explorer_just_continues() {
    let kit = Kit::with_env(no_shot_env());
    let w = kit.front("Book1", "excel.exe", 9);
    kit.fake.update_window(w, |i| i.class = "XLMAIN".into());
    let out = kit
        .run(json!({"actions": [
            {"action": "key", "text": "F2"},
            {"action": "type", "text": "new"}
        ]}))
        .unwrap();
    assert_eq!(kit.fake.typed_text(), "new");
    assert!(!out.text.contains("opened no rename box"), "{}", out.text);
}

#[test]
fn f2_with_a_rename_box_continues_in_explorer() {
    let kit = Kit::with_env(no_shot_env());
    let w = kit.front("Docs", "explorer.exe", 9);
    kit.fake
        .update_window(w, |i| i.class = "CabinetWClass".into());
    kit.fake.set_has_caret(true);
    kit.run(json!({"actions": [
        {"action": "key", "text": "F2"},
        {"action": "type", "text": "new"}
    ]}))
    .unwrap();
    assert_eq!(kit.fake.typed_text(), "new");
}

#[test]
fn a_key_that_selects_something_then_a_click_waits_a_beat_not_a_settle() {
    let kit = Kit::with_env(no_shot_env());
    kit.run(json!({"actions": [{"action": "key", "text": "down"}, click(5, 5)]}))
        .unwrap();
    assert_eq!(kit.fake.capture_count(), 0);
    assert!(kit.clock.now() >= Duration::from_millis(80));
}

#[test]
fn a_step_that_is_not_an_object_is_refused_up_front() {
    let kit = Kit::new();
    let err = kit
        .run(json!({"actions": [click(1, 1), "oops"]}))
        .unwrap_err();
    assert!(matches!(err, StepError::Value(_)));
    assert_eq!(err.message(), "step 2 must be an object with an action");
}

// Ported from test_batch_stops_on_expect_fail.
#[test]
fn a_batch_stops_on_an_expect_fail() {
    let kit = Kit::with_env(no_shot_env());
    kit.front("Notepad", "notepad.exe", 7);
    let out = kit
        .run(json!({"actions": [
            {"action": "mouse_move", "coordinate": [1, 1], "expect": {"text": "Saved"}},
            {"action": "type", "text": "never"}
        ]}))
        .unwrap();
    assert!(
        out.text.contains("expect: FAIL (text 'Saved' not found)"),
        "{}",
        out.text
    );
    assert_eq!(kit.fake.typed_text(), "");
}

// Ported from test_expect_fail_keeps_the_image.
#[test]
fn an_expect_fail_keeps_the_image() {
    let kit = Kit::new();
    let out = kit
        .run(
            json!({"actions": [{"action": "mouse_move", "coordinate": [1, 1],
                                 "expect": {"text": "x"}}]}),
        )
        .unwrap();
    assert_eq!(out.images.len(), 1);
}

// Ported from test_text_answers_skip_the_trailing_screenshot.
#[test]
fn text_answers_skip_the_trailing_screenshot() {
    let kit = Kit::new();
    kit.front("Notepad", "notepad.exe", 7);
    let out = kit.run(json!({"action": "list_windows"})).unwrap();
    assert!(
        out.text
            .starts_with("w0 \"Notepad\" notepad.exe [focused] "),
        "{}",
        out.text
    );
    assert!(out.images.is_empty());
    assert_eq!(kit.fake.capture_count(), 0);
}

// Ported from test_list_windows_is_a_passive_text_action: a passive action never waits for a person.
#[test]
fn a_passive_action_does_not_wait_for_a_person() {
    let kit = Kit::new();
    kit.fake.set_human_idle(Duration::from_millis(10));
    let out = kit
        .run(json!({"action": "list_windows", "all": true}))
        .unwrap();
    assert!(out.images.is_empty());
    assert_eq!(
        kit.fake.human_idle_reads(),
        0,
        "a passive action must not read the input idle time"
    );
}

#[test]
fn an_acting_action_waits_for_a_person_to_stop_then_refuses() {
    let kit = Kit::with_env(no_shot_env());
    // a person is at the mouse right now and never stops
    kit.fake.set_human_idle(Duration::from_millis(10));
    let err = kit.run(click(5, 5)).unwrap();
    // our own first action is also "newer input than ours", so it is a person's: the call waited
    assert!(
        err.text
            .contains("step 1 (left_click) failed: a person is using the mouse or keyboard"),
        "{}",
        err.text
    );
    assert!(kit.clock.now() >= Duration::from_secs(6));
    assert!(
        kit.fake.events().is_empty(),
        "no input may go out while a person is active"
    );
}

#[test]
fn human_pause_can_be_turned_off_for_a_benchmark_harness() {
    let kit = Kit::with_env(no_shot_env().with("HEKATON_HUMAN_PAUSE", "0"));
    kit.fake.set_human_idle(Duration::from_millis(10));
    let out = kit.run(click(5, 5)).unwrap();
    assert!(out.text.starts_with("left click at [5, 5]"), "{}", out.text);
}

// 2026-10-30: an unattended run was refused with "a person is using the mouse" and nobody there.
// Input of ours that the gate was never told about reads as a person's.
#[test]
fn a_step_that_sent_input_and_then_failed_still_tells_the_gate_it_was_ours() {
    let kit = Kit::with_env(no_shot_env());
    assert!(!kit.computer.human.has_marked());
    let err = kit
        .step(json!({"action": "left_click", "coordinate": [1, 1], "expect": {"text": "Nope"}}))
        .unwrap_err();
    assert!(matches!(err, StepError::Expectation(_)), "{err}");
    assert!(kit.computer.human.has_marked(), "the click went out");
}

#[test]
fn a_refused_step_that_sent_nothing_does_not_mark_the_gate() {
    let kit = Kit::with_env(no_shot_env());
    kit.step(json!({"action": "key"})).unwrap_err();
    assert!(
        !kit.computer.human.has_marked(),
        "a bad argument must not hide a person's input"
    );
}

#[test]
fn left_mouse_up_marks_the_gate_even_though_it_skips_the_pause() {
    let kit = Kit::with_env(no_shot_env());
    kit.step(json!({"action": "left_mouse_up"})).unwrap();
    assert!(kit.computer.human.has_marked());
}

#[test]
fn the_refusal_names_the_human_pause_switch() {
    let kit = Kit::with_env(no_shot_env());
    kit.fake.set_human_idle(Duration::from_millis(10));
    let err = kit.run(click(5, 5)).unwrap().text;
    assert!(err.contains("human_pause"), "{err}");
    assert!(err.contains("HEKATON_HUMAN_PAUSE=0"), "{err}");
}

#[test]
fn with_the_pause_off_the_input_idle_time_is_never_read() {
    let kit = Kit::with_env(no_shot_env().with("HEKATON_HUMAN_PAUSE", "0"));
    kit.fake.set_human_idle(Duration::from_millis(10));
    kit.run(click(5, 5)).unwrap();
    assert_eq!(kit.fake.human_idle_reads(), 0);
}

// ---------------------------------------------------------------------------- per-call options

// Ported from test_only_apps_is_set_fresh_every_call and test_only_apps_keeps_regex_scope_case.
#[test]
fn only_apps_is_set_fresh_every_call_and_keeps_the_scope_case() {
    let kit = Kit::new();
    let opts =
        |v: Value| CallOpts::from_args(&kit.computer, &Kit::args(v), Policy::REFUSE).unwrap();
    let one = opts(json!({"actions": [], "only_apps": ["Notepad.exe"]}));
    assert_eq!(one.only_apps.iter().collect::<Vec<_>>(), ["notepad.exe"]);
    assert!(
        opts(json!({"actions": []})).only_apps.is_empty(),
        "absent means no allowlist for THIS call"
    );
    assert!(opts(json!({"only_apps": []})).only_apps.is_empty());
    let scoped = opts(json!({"only_apps": ["MSEdge.exe:re:\\bWiki\\b"]}));
    assert_eq!(
        scoped.only_apps.iter().collect::<Vec<_>>(),
        ["msedge.exe:re:\\bWiki\\b"]
    );
}

// Ported from test_coords_does_not_leak_between_calls, test_shot_long_does_not_leak_between_calls
// and test_display_does_not_leak_between_calls (D1.1).
#[test]
fn coords_shot_long_and_display_do_not_leak_between_calls() {
    let kit = Kit::with_env(MapEnv::new().with("HEKATON_SHOT_LONG", "1280"));
    kit.fake.set_displays(vec![
        crate::backend::Display {
            id: 1,
            name: String::new(),
            rect: Rect::from_xywh(0, 0, 1920, 1080),
            scale: 1.0,
            primary: true,
        },
        crate::backend::Display {
            id: 2,
            name: String::new(),
            rect: Rect::from_xywh(1920, 0, 1280, 720),
            scale: 1.0,
            primary: false,
        },
    ]);
    let opts =
        |v: Value| CallOpts::from_args(&kit.computer, &Kit::args(v), Policy::REFUSE).unwrap();
    let first = opts(json!({"coords": "norm1000", "shot_long": 640, "display": 2}));
    assert_eq!(first.coords, crate::scale::Coords::Norm1000);
    assert_eq!(first.long_edge, 640);
    assert_eq!(first.display, Some(Rect::from_xywh(1920, 0, 1280, 720)));
    let second = opts(json!({}));
    assert_eq!(second.coords, crate::scale::Coords::Pixels);
    assert_eq!(
        second.long_edge, 1280,
        "the configured default, not the previous call's 640"
    );
    assert_eq!(second.display, None);
    let err = CallOpts::from_args(
        &kit.computer,
        &Kit::args(json!({"display": 3})),
        Policy::REFUSE,
    )
    .unwrap_err();
    assert_eq!(
        err.message(),
        "display 3 does not exist; this PC has 2 monitor(s)"
    );
}

// Ported from test_run_many_resets_scale_to_the_new_default and test_shot_long_is_clamped.
#[test]
fn the_default_scale_is_1024_and_a_calls_shot_long_is_clamped() {
    let kit = Kit::new();
    let opts =
        |v: Value| CallOpts::from_args(&kit.computer, &Kit::args(v), Policy::REFUSE).unwrap();
    assert_eq!(opts(json!({"action": "mouse_move"})).long_edge, 1024);
    assert_eq!(opts(json!({"shot_long": 3})).long_edge, 128);
    assert_eq!(opts(json!({"shot_long": 99999})).long_edge, 4096);
}

// Ported from test_coordinates_use_the_calls_own_shot_long.
#[test]
fn coordinates_are_read_in_the_calls_own_scale() {
    let kit = Kit::new();
    kit.fake.set_displays(vec![crate::backend::Display {
        id: 1,
        name: String::new(),
        rect: Rect::from_xywh(0, 0, 2560, 1440),
        scale: 1.0,
        primary: true,
    }]);
    let moved = |v: Value| {
        kit.fake.clear_events();
        kit.run(v).unwrap();
        kit.fake.events()
    };
    let mv = |long: Option<i64>| {
        let mut call = json!({"action": "mouse_move", "coordinate": [100, 100], "shot": false});
        if let Some(l) = long {
            call["shot_long"] = json!(l);
        }
        call
    };
    use crate::backend::Point;
    assert_eq!(
        moved(mv(Some(640)))[0],
        InputEvent::MouseMove(Point::new(400, 400))
    );
    assert_eq!(
        moved(mv(None))[0],
        InputEvent::MouseMove(Point::new(250, 250))
    );
    assert_eq!(
        moved(mv(Some(1280)))[0],
        InputEvent::MouseMove(Point::new(200, 200))
    );
}

// ---------------------------------------------------------------------------- W2: shot, shot_long

// Ported from test_shot_default_still_attaches_the_screenshot.
#[test]
fn the_default_attaches_a_screenshot() {
    let kit = Kit::new();
    let out = kit.run(click(1, 1)).unwrap();
    assert_eq!(out.images.len(), 1);
    assert!(
        out.text.contains(
            "[image 1: full screen; click frame: 320x180 px, the same as the screen's points]"
        ),
        "{}",
        out.text
    );
    assert!(out.text.starts_with("left click at [1, 1]"), "{}", out.text);
}

// Ported from test_shot_false_skips_the_screenshot_but_still_settles.
#[test]
fn shot_false_skips_the_picture_but_an_acting_call_still_settles() {
    let kit = Kit::new();
    rich_front(&kit);
    let mut call = click(1, 1);
    call["shot"] = json!(false);
    let out = kit.run(call).unwrap();
    assert!(out.images.is_empty());
    assert!(!out.text.contains("[image"), "{}", out.text);
    // an ordinary window: the 130 ms floor, so two calm looks end it at 40 + 2 x 70 ms
    assert!(
        out.text.ends_with("screen settled in 180 ms"),
        "{}",
        out.text
    );
    assert_eq!(kit.fake.capture_count(), 3);
}

// Ported from test_shot_false_spellings.
#[test]
fn shot_false_has_several_spellings() {
    for value in [
        json!(false),
        json!("false"),
        json!("0"),
        json!("no"),
        json!("off"),
    ] {
        let kit = Kit::new();
        let mut call = click(1, 1);
        call["shot"] = value.clone();
        assert!(kit.run(call).unwrap().images.is_empty(), "{value}");
    }
}

// Ported from test_shot_false_in_a_batch_keeps_explicit_zoom_images.
#[test]
fn shot_false_in_a_batch_keeps_explicit_zoom_images() {
    let kit = Kit::new();
    let out = kit
        .run(json!({"actions": [
            {"action": "key", "text": "a"},
            {"action": "zoom", "region": [0, 0, 100, 60]}
        ], "shot": false}))
        .unwrap();
    assert_eq!(out.images.len(), 1);
}

// Ported from test_shot_false_with_a_passive_last_step_does_no_settle_at_all.
#[test]
fn shot_false_with_a_passive_last_step_does_no_settle_at_all() {
    let kit = Kit::new();
    let out = kit
        .run(json!({"action": "wait", "duration": 0.1, "shot": false}))
        .unwrap();
    assert!(out.images.is_empty());
    assert_eq!(kit.fake.capture_count(), 0);
    assert!(
        out.text.starts_with("waited 0.1s (asked 0.1s)"),
        "{}",
        out.text
    );
}

#[test]
fn a_passive_last_step_with_a_picture_wanted_settles_without_a_floor() {
    let kit = Kit::with_env(MapEnv::new().with("HEKATON_SETTLE_FLOOR_MS", "400"));
    let out = kit.run(json!({"action": "wait", "duration": 0.1})).unwrap();
    assert_eq!(out.images.len(), 1);
    assert!(
        out.text.contains("screen settled in 180 ms"),
        "{}",
        out.text
    );
}

// Ported from test_trailing_settle_floor_applies_to_acting_calls_only.
#[test]
fn the_settle_floor_applies_to_acting_calls_only() {
    let kit = Kit::with_env(MapEnv::new().with("HEKATON_SETTLE_FLOOR_MS", "400"));
    // a floor above 450 ms is honoured as is; 400 is below it, so an ordinary window gets 400
    rich_front(&kit);
    let acting = kit.run(click(1, 1)).unwrap();
    assert!(
        acting.text.contains("screen settled in 460 ms"),
        "{}",
        acting.text
    );
    let passive = kit.run(json!({"action": "wait", "duration": 0.1})).unwrap();
    assert!(
        passive.text.contains("screen settled in 180 ms"),
        "{}",
        passive.text
    );
}

#[test]
fn a_window_with_no_elements_keeps_the_long_floor() {
    let kit = Kit::new();
    kit.front("Game", "game.exe", 3); // no accessibility tree: a game draws everything itself
    let out = kit.run(click(1, 1)).unwrap();
    assert!(
        out.text.contains("screen settled in 460 ms"),
        "{}",
        out.text
    );
}

// Ported from test_mid_batch_steps_settle_on_one_calm_read.
#[test]
fn mid_batch_steps_settle_on_one_calm_read() {
    let kit = Kit::new();
    rich_front(&kit);
    kit.run(json!({"actions": [click(1, 1), click(2, 2)], "shot": false}))
        .unwrap();
    // 2 looks between the steps (one calm read), then 3 for the trailing settle (two calm reads)
    assert_eq!(kit.fake.capture_count(), 5);
}

#[test]
fn auto_shot_off_skips_the_trailing_settle_and_picture() {
    let kit = Kit::with_env(no_shot_env());
    let out = kit.run(click(1, 1)).unwrap();
    assert!(out.images.is_empty());
    assert!(!out.text.contains("screen settled"), "{}", out.text);
    assert_eq!(kit.fake.capture_count(), 0);
}

// ---------------------------------------------------------------------------- W3: no image before input

// Ported from test_unknown_action_returns_text_only.
#[test]
fn an_unknown_action_returns_text_only() {
    let kit = Kit::new();
    let out = kit.run(json!({"action": "bogus"})).unwrap();
    assert!(
        out.text.contains("failed") && out.text.contains("unknown action 'bogus'"),
        "{}",
        out.text
    );
    assert!(
        out.text.contains("one of: click_text, clipboard_get,"),
        "{}",
        out.text
    );
    assert!(out.images.is_empty());
    assert_eq!(kit.fake.capture_count(), 0);
}

#[test]
fn a_missing_action_names_none() {
    let kit = Kit::new();
    let out = kit.run(json!({"text": "x"})).unwrap();
    assert!(
        out.text
            .starts_with("step 1 (None) failed: unknown action ''; one of: "),
        "{}",
        out.text
    );
}

// Ported from test_validation_and_refusals_before_input_carry_no_image.
#[test]
fn validation_and_refusals_before_input_carry_no_image() {
    let kit = Kit::new();
    let cases = [
        json!({"action": "zoom"}),                                    // ValueError
        json!({"action": "read", "text": "no such window anywhere"}), // ValueError
        json!({"action": "left_click", "coordinate": [9999, 9999]}),  // out of the screenshot
        json!({"action": "type", "text": "rm -rf C:\\Windows\\System32"}), // PermissionError
        json!({"action": "key", "text": "definitelynotakey"}),        // KeyComboError
    ];
    for case in cases {
        let out = kit.run(json!({"actions": [case.clone()]})).unwrap();
        assert!(out.text.contains("step 1 ("), "{case}: {}", out.text);
        assert!(out.text.contains("failed"), "{case}: {}", out.text);
        assert!(out.images.is_empty(), "{case}");
    }
    assert_eq!(kit.fake.capture_count(), 0);
    assert!(kit.fake.events().is_empty());
}

// Ported from test_failure_in_a_passive_first_step_has_no_image.
#[test]
fn a_failure_after_only_passive_steps_has_no_image() {
    let kit = Kit::new();
    let out = kit
        .run(json!({"actions": [{"action": "list_windows"}, {"action": "zoom"}]}))
        .unwrap();
    assert!(out.images.is_empty());
    assert_eq!(kit.fake.capture_count(), 0);
}

// Ported from test_failure_after_an_earlier_step_acted_keeps_the_image.
#[test]
fn a_failure_after_an_earlier_step_acted_keeps_the_image() {
    let kit = Kit::new();
    let out = kit
        .run(
            json!({"actions": [click(3, 3), {"action": "key", "text": "definitelynotakey"},
                                {"action": "type", "text": "x"}]}),
        )
        .unwrap();
    assert!(out.text.contains("step 2 (key) failed"), "{}", out.text);
    assert_eq!(
        out.images.len(),
        1,
        "the screen changed under step 1: the caller needs to see it"
    );
}

// Ported from test_failure_after_input_was_sent_keeps_the_image: a step that got as far as sending
// input, then failed (a scroll aims the pointer, then refuses the direction).
#[test]
fn a_failure_after_input_was_sent_keeps_the_image() {
    let kit = Kit::new();
    let out = kit
        .run(json!({"action": "scroll", "coordinate": [5, 5], "scroll_direction": "sideways"}))
        .unwrap();
    assert!(
        out.text.contains("failed: bad direction: 'sideways'"),
        "{}",
        out.text
    );
    assert_eq!(out.images.len(), 1);
    assert_eq!(
        kit.fake.events(),
        vec![InputEvent::MouseMove(crate::backend::Point::new(5, 5))]
    );
}

// Ported from test_unexpected_error_keeps_the_image: anything that is not a ValueError or a
// PermissionError keeps the picture. A bad button name was a KeyError in Python.
#[test]
fn an_unexpected_error_keeps_the_image() {
    let kit = Kit::new();
    let w = kit.front("App", "app.exe", 1);
    kit.fake.set_tree(
        w,
        Element::new("r", Role::Window, "App")
            .with_child(Element::new("s", Role::Button, "Save").at(Rect::new(10, 10, 50, 30))),
    );
    let out = kit
        .run(json!({"action": "click_text", "text": "save", "button": "wheel"}))
        .unwrap();
    assert!(
        out.text.contains("step 1 (click_text) failed: 'wheel'"),
        "{}",
        out.text
    );
    assert_eq!(out.images.len(), 1);
}

// ---------------------------------------------------------------------------- the daemon's route

fn ack_off() -> MapEnv {
    MapEnv::new().with("HEKATON_ACKNOWLEDGED", "0")
}

fn kit_unacknowledged() -> Kit {
    Kit::with_env(ack_off())
}

#[test]
fn the_computer_route_returns_text_and_a_picture() {
    let kit = Kit::new();
    let out = kit.computer.execute(
        &json!({"action": "mouse_move", "coordinate": [4, 4]}),
        &kit.who,
    );
    assert!(out.ok);
    let j = out.to_json();
    assert_eq!(j["ok"], json!(true));
    let text = j["result"].as_str().unwrap();
    assert!(text.starts_with("moved to [4, 4]"), "{text}");
    let content = j["content"].as_array().unwrap();
    assert_eq!(content[0], json!({"type": "text", "text": text}));
    assert_eq!(content[1]["type"], json!("image"));
    assert_eq!(content[1]["mimeType"], json!("image/jpeg"));
    assert!(j.get("failed_step").is_none());
}

// Ported from tests/test_daemon_batch_status.py.
#[test]
fn a_failing_step_adds_machine_readable_fields_without_touching_the_text() {
    assert_eq!(
        super::many::batch_failure_fields("moved\nstep 3 (key) failed: unknown key: 'zzz'\nmore"),
        Some((3, "unknown key: 'zzz'".to_string()))
    );
    assert_eq!(super::many::batch_failure_fields("all fine"), None);
    let kit = Kit::new();
    let out = kit.computer.execute(
        &json!({"actions": [{"action": "list_windows"}, {"action": "key", "text": "definitelynotakey"}]}),
        &kit.who,
    );
    let j = out.to_json();
    assert_eq!(
        j["ok"],
        json!(false),
        "transport completion does not imply action success"
    );
    assert_eq!(j["failed_step"], json!(2));
    assert!(
        j["error"]
            .as_str()
            .unwrap()
            .starts_with("unknown key: 'definitelynotakey'")
    );
    assert!(
        j["result"]
            .as_str()
            .unwrap()
            .contains("step 2 (key) failed")
    );
    let fine = kit
        .computer
        .execute(&json!({"action": "list_windows"}), &kit.who)
        .to_json();
    assert!(fine.get("failed_step").is_none() && fine.get("error").is_none());
}

#[test]
fn a_bad_call_comes_back_as_not_ok_and_never_raises() {
    let kit = Kit::new();
    let j = kit.computer.execute(&json!([1, 2]), &kit.who).to_json();
    assert_eq!(
        j,
        json!({"ok": false, "error": "tool args must be an object"})
    );
    let j = kit
        .computer
        .execute(&json!({"actions": [click(1, 1), 7]}), &kit.who)
        .to_json();
    assert_eq!(j["ok"], json!(false));
    assert_eq!(j["error"], json!("step 2 must be an object with an action"));
}

// Ported from test_20260924_decisions.py::test_computer_reads_work_and_computer_input_refuses and
// test_a_batch_refuses_if_any_step_acts.
#[test]
fn before_acknowledgement_reads_work_and_input_refuses() {
    let kit = kit_unacknowledged();
    let read = kit
        .computer
        .execute(&json!({"action": "list_windows"}), &kit.who);
    assert!(read.ok, "{read:?}");
    let shot = kit
        .computer
        .execute(&json!({"action": "screenshot"}), &kit.who);
    assert!(shot.ok && shot.images.len() == 1, "{shot:?}");
    let click = kit.computer.execute(&click(1, 1), &kit.who);
    assert!(!click.ok);
    let why = click.error.unwrap();
    assert!(
        why.contains("left_click") && why.contains("hekaton acknowledge"),
        "{why}"
    );
    let batch = kit.computer.execute(
        &json!({"actions": [{"action": "list_windows"}, {"action": "type", "text": "x"}]}),
        &kit.who,
    );
    assert!(!batch.ok);
    assert!(batch.error.unwrap().contains("the type action"));
    assert!(kit.fake.events().is_empty());
    // clipboard_set changes state the owner owns: not a read
    let clip = kit
        .computer
        .execute(&json!({"action": "clipboard_set", "text": "x"}), &kit.who);
    assert!(!clip.ok);
}

// Ported from tests/test_execution.py::test_pause_rechecked_each_step (the computer route reads the
// pause flag on every call).
#[test]
fn a_paused_daemon_refuses_every_computer_call() {
    let kit = Kit::new();
    kit.computer.pause_flag().set(true);
    for call in [click(1, 1), json!({"action": "list_windows"})] {
        let out = kit.computer.execute(&call, &kit.who);
        assert!(!out.ok);
        assert_eq!(out.error.as_deref(), Some("paused by user hotkey"));
    }
    assert!(kit.fake.events().is_empty());
    kit.computer.pause_flag().set(false);
    assert!(
        kit.computer
            .execute(&json!({"action": "list_windows"}), &kit.who)
            .ok
    );
}

#[test]
fn the_rate_cap_refuses_the_fifty_first_call_in_a_second() {
    let kit = Kit::new();
    for _ in 0..50 {
        assert!(
            kit.computer
                .execute(&json!({"action": "list_windows"}), &kit.who)
                .ok
        );
    }
    let over = kit
        .computer
        .execute(&json!({"action": "list_windows"}), &kit.who);
    assert!(!over.ok);
    assert_eq!(over.error.as_deref(), Some("rate limit exceeded"));
    kit.clock.advance(Duration::from_secs(2));
    assert!(
        kit.computer
            .execute(&json!({"action": "list_windows"}), &kit.who)
            .ok
    );
}

#[test]
fn the_destructive_opt_in_is_per_call_and_never_forwarded() {
    let kit = Kit::with_env(no_shot_env());
    kit.front("Terminal", "term.exe", 4);
    let refused = kit.computer.execute(
        &json!({"action": "type", "text": "rm -rf C:\\Windows\\System32"}),
        &kit.who,
    );
    let text = refused.result.clone().unwrap();
    assert!(
        text.contains("refused:") && text.contains("allow_destructive"),
        "{text}"
    );
    assert_eq!(kit.fake.typed_text(), "");
    let allowed = kit.computer.execute(
        &json!({"action": "type", "text": "rm -rf C:\\Windows\\System32", "allow_destructive": true}),
        &kit.who,
    );
    assert!(
        allowed.result.as_ref().unwrap().starts_with("typed "),
        "{allowed:?}"
    );
    // the next call starts from refusing again
    let again = kit.computer.execute(
        &json!({"action": "type", "text": "rm -rf C:\\Windows\\System32"}),
        &kit.who,
    );
    assert!(again.result.unwrap().contains("refused:"));
}

#[test]
fn every_call_leaves_an_audit_entry_without_the_typed_text() {
    let kit = Kit::with_env(no_shot_env());
    kit.front("Notepad", "notepad.exe", 7);
    kit.computer
        .execute(&json!({"action": "type", "text": "hunter2"}), &kit.who);
    kit.computer.execute(
        &json!({"actions": [{"action": "list_windows"}, {"action": "key", "text": "definitelynotakey"}]}),
        &kit.who,
    );
    kit.computer.execute(&json!(5), &kit.who);
    let log = std::fs::read_to_string(kit.computer.config().paths().audit_log()).unwrap();
    assert!(!log.contains("hunter2"), "{log}");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 3, "{log}");
    assert!(
        lines[0].contains("\"action\": \"computer\"") && lines[0].contains("\"steps\": 1"),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains("\"ok\": true"), "{}", lines[0]);
    assert!(
        lines[1].contains("\"steps\": 2") && lines[1].contains("\"ok\": false"),
        "{}",
        lines[1]
    );
    assert!(lines[2].contains("\"ok\": false"), "{}", lines[2]);
}

#[test]
fn three_identical_failing_calls_get_a_hint_to_change_approach() {
    let kit = Kit::new();
    let bad = json!({"actions": [click(1, 1), 7]});
    let first = kit.computer.execute(&bad, &kit.who).error.unwrap();
    assert_eq!(first, "step 2 must be an object with an action");
    kit.computer.execute(&bad, &kit.who);
    let third = kit.computer.execute(&bad, &kit.who).error.unwrap();
    assert!(
        third.ends_with("(failed 3 times with these arguments. try a different approach)"),
        "{third}"
    );
}

#[test]
fn an_unwritable_audit_log_never_fails_the_call() {
    let kit = Kit::new();
    // the data directory is a file: every audit write fails
    std::fs::write(kit.tmp.path().join("data"), "not a directory").unwrap();
    assert!(
        kit.computer
            .execute(&json!({"action": "list_windows"}), &kit.who)
            .ok
    );
}

#[test]
fn execute_touches_the_idle_timer() {
    let kit = Kit::new();
    let before = kit.computer.idle_timer().remaining(kit.clock.as_ref());
    kit.clock.advance(Duration::from_secs(120));
    assert!(kit.computer.idle_timer().remaining(kit.clock.as_ref()) < before);
    kit.computer
        .execute(&json!({"action": "list_windows"}), &kit.who);
    assert!(kit.computer.idle_timer().remaining(kit.clock.as_ref()) >= before - 1.0);
}

// Ported from test_display_follows_foreground_monitor (2026-09-21): Search Console was focused in a
// browser on monitor 2 while every screenshot showed monitor 1, so the model never saw it.
#[test]
fn the_display_follows_the_foreground_window() {
    let kit = Kit::new();
    kit.fake.set_displays(vec![
        crate::backend::Display {
            id: 1,
            name: String::new(),
            rect: Rect::from_xywh(0, 0, 320, 180),
            scale: 1.0,
            primary: true,
        },
        crate::backend::Display {
            id: 2,
            name: String::new(),
            rect: Rect::from_xywh(320, 0, 192, 108),
            scale: 1.0,
            primary: false,
        },
    ]);
    kit.fake.clear_screen();
    let ctx = kit.ctx(json!({}));
    assert_eq!(
        ctx.display_rect(),
        Rect::from_xywh(0, 0, 320, 180),
        "nothing in front: the primary"
    );
    let w = kit.window("Console", "brave.exe", 4, Rect::new(332, 52, 500, 100));
    kit.fake.set_foreground(Some(w));
    kit.clock.advance(Duration::from_secs(3)); // the monitor list is cached for 2 s
    assert_eq!(ctx.display_rect(), Rect::from_xywh(320, 0, 192, 108));
    let (note, _) = ctx
        .run(&Kit::args(json!({"action": "screenshot"})))
        .unwrap();
    assert_eq!(
        note,
        "screenshot; give coordinates in pixels of this 192x108 image\n\
         click frame: 192x108 px, the same as the screen's points"
    );
    // a call's own display wins, and so does a configured one
    let chosen = kit.ctx(json!({"display": 1}));
    assert_eq!(chosen.display_rect(), Rect::from_xywh(0, 0, 320, 180));
    let pinned = Kit::with_env(MapEnv::new().with("HEKATON_DISPLAY", "5, 6, 700, 400"));
    assert_eq!(
        pinned.ctx(json!({})).display_rect(),
        Rect::from_xywh(5, 6, 700, 400)
    );
}

#[test]
fn the_tool_can_be_shared_between_the_daemons_threads() {
    fn needs_send_sync<T: Send + Sync>() {}
    needs_send_sync::<super::Computer>();
    needs_send_sync::<super::ToolOutcome>();
}

#[test]
fn a_read_outside_a_computer_call_wakes_an_empty_chromium_tree_by_default() {
    // Ported from test_snapshot_wakes_by_default_as_before and
    // test_snapshot_under_no_wake_policy_leaves_the_window_alone_and_asks_for_a_hint (W10).
    let kit = Kit::new();
    let w = kit.window("Steam", "steam.exe", 5, Rect::from_xywh(0, 0, 100, 100));
    kit.fake.set_tree(
        w,
        Element::new("r", Role::Window, "Steam")
            .with_child(Element::new("a", Role::Button, "Store").at(Rect::new(1, 1, 9, 9))),
    );
    kit.fake.set_chromium_empty(w);
    let quiet = kit
        .computer
        .read_snapshot(Some("Steam"), 40, "", false, false)
        .unwrap();
    assert!(quiet.ends_with(super::snapshot::WAKE_HINT), "{quiet}");
    assert!(kit.fake.wakes().is_empty());
    let woken = kit
        .computer
        .read_snapshot(Some("Steam"), 40, "", false, true)
        .unwrap();
    assert_eq!(woken, "window \"Steam\"\n0 Button \"Store\"");
    assert_eq!(kit.fake.wakes(), vec![w]);
    // the tree stays built after the raise, so a quiet read now needs no hint
    let after = kit
        .computer
        .read_snapshot(Some("Steam"), 40, "", false, false)
        .unwrap();
    assert!(!after.contains("wake=true"), "{after}");
}

#[test]
fn the_files_route_lists_newest_first() {
    let kit = Kit::new();
    std::fs::write(kit.tmp.path().join("a.txt"), "x").unwrap();
    kit.fake.set_utc_offset(3600);
    let listing = kit
        .computer
        .list_files(
            &kit.tmp.path().to_string_lossy(),
            "modified",
            true,
            20,
            "*.txt",
        )
        .unwrap();
    assert!(
        listing.contains("(1 items, by modified desc)") && listing.contains("a.txt"),
        "{listing}"
    );
}

#[test]
fn a_refused_batch_does_not_write_what_its_steps_typed_to_the_audit_log() {
    // The Python audit log redacted only the top level, so a batch refused at the gate (here: the
    // owner has not acknowledged) wrote the text of its type steps to disk.
    let kit = kit_unacknowledged();
    let out = kit.computer.execute(
        &json!({"actions": [{"action": "type", "text": "hunter2"}, {"action": "key", "text": "enter"}]}),
        &kit.who,
    );
    assert!(!out.ok);
    let log = std::fs::read_to_string(kit.computer.config().paths().audit_log()).unwrap();
    assert!(!log.contains("hunter2"), "{log}");
    assert!(log.contains("<7 chars"), "{log}");
}

// A realistic batch end to end: start Notepad, type into it, Save As, fill the dialog.
#[test]
fn a_whole_batch_runs_from_launch_to_a_saved_file() {
    use crate::backend::{Backend, ClipboardContent, ElementId, WindowInfo};
    use crate::keys::Key;
    let kit = Kit::new();
    let field = ElementId("filename".into());
    // launching Notepad opens a blank window; ctrl+s opens Save As in front of it; pasting fills the
    // box; Enter closes the dialog
    kit.fake.set_launch_hook(|fake, _| {
        fake.add_window(WindowInfo::basic(
            "Untitled - Notepad",
            "Notepad.exe",
            30,
            Rect::from_xywh(10, 10, 300, 160),
        ));
    });
    let dialog_field = field.clone();
    kit.fake.set_chord_hook(move |fake, keys| {
        let cmd = super::testkit::cmd_key();
        if keys == [Key::Ctrl, Key::Char('s')] {
            let dlg = fake.add_window(WindowInfo {
                class: "#32770".into(),
                ..WindowInfo::basic(
                    "Save As",
                    "Notepad.exe",
                    30,
                    Rect::from_xywh(40, 40, 200, 100),
                )
            });
            fake.set_tree(
                dlg,
                Element::new("root", Role::Window, "Save As").with_child(
                    Element::new("filename", Role::Edit, "File name:")
                        .at(Rect::new(50, 50, 150, 70))
                        .valued("Untitled.txt"),
                ),
            );
            fake.set_file_dialog_field(dlg, dialog_field.clone());
            fake.set_foreground(Some(dlg));
        }
        if keys == [cmd, Key::Char('v')] {
            let pasted = fake.clipboard_text().unwrap();
            fake.set_element_value(&dialog_field, &pasted);
        }
        if keys == [Key::Enter] {
            for w in fake.list_windows(true).unwrap() {
                if w.title == "Save As" {
                    fake.close_window(w.window);
                }
            }
        }
    });
    kit.fake
        .set_clipboard(ClipboardContent::Text("owner's text".into()));
    let out = kit
        .run(json!({"actions": [
            {"action": "open_app", "text": "Notepad"},
            {"action": "type", "text": "hello from hekaton"},
            {"action": "key", "text": "ctrl+s"},
            {"action": "file_dialog", "text": "C:/Users/jon/out.txt"},
            {"action": "list_windows"}
        ]}))
        .unwrap();
    let lines: Vec<&str> = out.text.lines().collect();
    assert_eq!(
        lines[0], "ok focused w0 \"Untitled - Notepad\" pid:30",
        "{}",
        out.text
    );
    assert_eq!(lines[1], "typed 18 chars (pasted)", "{}", out.text);
    assert_eq!(lines[2], "pressed ctrl+s", "{}", out.text);
    assert_eq!(
        lines[3], r"file dialog: entered C:\Users\jon\out.txt and confirmed",
        "{}",
        out.text
    );
    assert!(
        lines[4].starts_with("w0 \"Untitled - Notepad\" Notepad.exe 10,10,300,160"),
        "{}",
        out.text
    );
    assert_eq!(
        lines.len(),
        5,
        "a text answer last: no settle line, no picture"
    );
    assert!(out.images.is_empty());
    assert_eq!(kit.fake.clipboard_text().unwrap(), "owner's text");
}

// 2026-10-28 gauntlet L2: "wait 1.5" returned after 0.6 s on a calm screen and the table pasted next
// landed on Excel's start screen. A wait before blind input runs its full time.
#[test]
fn a_wait_before_blind_input_runs_its_full_time() {
    let kit = Kit::new();
    kit.front("Editor", "notepad.exe", 7);
    let start = kit.clock.now();
    let out = kit.computer.execute(
        &json!({"actions": [{"action": "wait"}, {"action": "key", "text": "a"}], "shot": false}),
        &kit.who,
    );
    assert!(out.ok, "{:?}", out.error);
    assert!(
        kit.clock.now() - start >= Duration::from_secs(1),
        "{:?}",
        out.result
    );

    let start = kit.clock.now();
    let out = kit.computer.execute(
        &json!({"actions": [{"action": "wait"}, {"action": "screenshot"}]}),
        &kit.who,
    );
    assert!(out.ok, "{:?}", out.error);
    assert!(
        kit.clock.now() - start < Duration::from_millis(1600),
        "a wait before a look may still end early: {:?}",
        out.result
    );

    // read_text sends select-all + copy, but only to look
    let out = kit.computer.execute(
        &json!({"actions": [{"action": "wait"}, {"action": "read_text"}], "shot": false}),
        &kit.who,
    );
    assert!(out.ok, "{:?}", out.error);
    let said = out.result.unwrap_or_default();
    assert!(
        !said.starts_with("waited 1.0s"),
        "a wait before read_text may end early: {said}"
    );
}

// 2026-10-29: the owner's open profile lets the model judge destructive text itself and drops the
// rate limit; the human-activity pause and the gaming lock stay theirs.
#[test]
fn the_open_profile_drops_the_hard_line_and_the_rate_limit() {
    let kit = Kit::with_env(no_shot_env().with("HEKATON_SAFETY_PROFILE", "open"));
    kit.front("Terminal", "term.exe", 4);
    let typed = kit.computer.execute(
        &json!({"action": "type", "text": "rm -rf C:\\Windows\\System32"}),
        &kit.who,
    );
    assert!(
        typed.result.as_ref().unwrap().starts_with("typed "),
        "{typed:?}"
    );
    for _ in 0..40 {
        let r = kit
            .computer
            .execute(&json!({"action": "list_windows"}), &kit.who);
        assert!(r.ok, "{r:?}");
    }
}

// 2026-10-29 (form job): a click opened Brave's "This page says" alert, and the batch's next click
// landed behind it. The batch now stops and names the dialog, unless the next click aims inside it.
fn alert_on_first_click(kit: &Kit) {
    kit.front("Vendor form", "brave.exe", 7);
    let opened = std::sync::Arc::new(std::sync::Mutex::new(false));
    kit.fake.set_click_hook(move |fake, _| {
        let mut o = opened.lock().unwrap();
        if !*o {
            *o = true;
            let info = crate::backend::WindowInfo::basic(
                "This page says",
                "brave.exe",
                7,
                Rect::from_xywh(100, 60, 120, 50),
            );
            let w = fake.add_window(info);
            fake.set_window_text(w, "This page says Choose a country and a category OK");
            fake.set_foreground(Some(w));
        }
    });
}

#[test]
fn a_batch_stops_when_a_click_opens_a_dialog_in_front() {
    let kit = Kit::with_env(no_shot_env());
    alert_on_first_click(&kit);
    let out = kit
        .run(json!({"actions": [click(50, 50), click(60, 60), {"action": "type", "text": "x"}]}))
        .unwrap();
    assert!(
        out.text.contains(
            "step 2 not run: a dialog opened in front after step 1: \"This page says\" saying \"This page says Choose a country and a category OK\""
        ),
        "{}",
        out.text
    );
    assert_eq!(out.failure.as_ref().map(|f| f.step), Some(2));
    assert_eq!(kit.fake.typed_text(), "");

    // a next click on the dialog's own words goes ahead
    let kit = Kit::with_env(no_shot_env());
    alert_on_first_click(&kit);
    let out = kit
        .run(json!({"actions": [click(50, 50), {"action": "click_text", "text": "OK"}]}))
        .unwrap();
    assert!(!out.text.contains("dialog opened"), "{}", out.text);
}
