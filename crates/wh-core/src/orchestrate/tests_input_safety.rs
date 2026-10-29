//! Regression coverage uses only the in-memory desktop backend.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::testkit::Kit;
use crate::backend::fake::InputEvent;
use crate::backend::{
    Backend, BackendError, ClipboardContent, Element, MouseButton, Point, Rect, Role,
};
use crate::destructive::Policy;
use crate::input::{TypeOptions, type_text_guarded};
use crate::keys::Key;

#[test]
fn resumed_requests_keep_their_stop_generation() {
    let kit = Kit::new();
    let ctx = kit.ctx(json!({}));
    kit.computer.pause_flag().set(true);
    kit.computer.pause_flag().set(false);
    assert!(ctx.check_cancelled().is_err());
    assert!(kit.ctx(json!({})).check_cancelled().is_ok());
}

#[test]
fn repeated_key_injection_stops_after_hotkey_even_if_resumed() {
    let kit = Kit::new();
    kit.front("Editor", "editor.exe", 30);
    let pause = kit.computer.pause.clone();
    kit.fake.set_chord_hook(move |_, _| {
        pause.set(true);
        pause.set(false);
    });
    let result = kit.step(json!({"action": "key", "text": "ctrl+a", "repeat": 10}));
    assert!(result.is_err());
    assert_eq!(
        kit.fake
            .events()
            .iter()
            .filter(|event| matches!(event, InputEvent::KeyDown(Key::Ctrl)))
            .count(),
        1
    );
    assert!(kit.fake.events().contains(&InputEvent::KeyUp(Key::Ctrl)));
}

#[test]
fn holding_releases_keys_after_body_error_and_unwind() {
    let kit = Kit::new();
    let ctx = kit.ctx(json!({}));
    let result: super::error::StepResult<()> = ctx.holding(&[Key::Ctrl, Key::Shift], || {
        Err(super::error::StepError::runtime("body failed"))
    });
    assert!(result.is_err());
    assert_eq!(
        kit.fake.events(),
        vec![
            InputEvent::KeyDown(Key::Ctrl),
            InputEvent::KeyDown(Key::Shift),
            InputEvent::KeyUp(Key::Shift),
            InputEvent::KeyUp(Key::Ctrl)
        ]
    );
    kit.fake.clear_events();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: super::error::StepResult<()> = ctx.holding(&[Key::Ctrl], || panic!("body panicked"));
    }));
    assert_eq!(
        kit.fake.events(),
        vec![InputEvent::KeyDown(Key::Ctrl), InputEvent::KeyUp(Key::Ctrl)]
    );
}

#[test]
fn interruption_during_hold_releases_pressed_keys() {
    let kit = Kit::new();
    let ctx = kit.ctx(json!({}));
    let result = ctx.holding(&[Key::Ctrl], || {
        kit.computer.pause_flag().set(true);
        ctx.sleep_checked(30.0)
    });
    assert!(result.is_err());
    assert_eq!(
        kit.fake.events(),
        vec![InputEvent::KeyDown(Key::Ctrl), InputEvent::KeyUp(Key::Ctrl)]
    );
}

#[test]
fn mouse_down_moves_to_the_validated_coordinate() {
    let kit = Kit::new();
    kit.step(json!({"action": "left_mouse_down", "coordinate": [80, 60]}))
        .unwrap();
    assert_eq!(kit.fake.cursor_position().unwrap(), Point::new(80, 60));
    assert_eq!(
        kit.fake.events(),
        vec![
            InputEvent::MouseMove(Point::new(80, 60)),
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true
            }
        ]
    );
}

#[test]
fn mouse_release_ignores_pause_and_target_resolution() {
    let kit = Kit::new();
    let ctx = kit.ctx(json!({}));
    kit.computer.pause_flag().set(true);
    ctx.run(&Kit::args(
        json!({"action": "left_mouse_up", "coordinate": "invalid"}),
    ))
    .unwrap();
    assert_eq!(
        kit.fake.events(),
        vec![InputEvent::MouseButton {
            button: MouseButton::Left,
            down: false
        }]
    );
}

#[test]
fn text_targeted_delete_requires_the_same_opt_in_as_coordinate_click() {
    let kit = Kit::new();
    let window = kit.front("Editor", "editor.exe", 30);
    kit.fake.set_tree(
        window,
        Element::new("root", Role::Window, "Editor").with_child(
            Element::new("delete", Role::Button, "Delete").at(Rect::new(20, 20, 100, 50)),
        ),
    );
    let result = kit.step_only(
        &["editor.exe"],
        json!({"action": "click_text", "text": "Delete"}),
    );
    assert!(result.unwrap_err().message().contains("irreversible"));
    assert!(kit.fake.events().is_empty());
    kit.step_only(
        &["editor.exe"],
        json!({"action": "click_text", "text": "Delete", "allow_irreversible": true}),
    )
    .unwrap();
    assert!(
        kit.fake
            .events()
            .iter()
            .any(|event| matches!(event, InputEvent::Click { .. }))
    );
}

#[test]
fn clipboard_is_restored_when_paste_guard_cancels() {
    let kit = Kit::new();
    kit.fake
        .set_clipboard(ClipboardContent::Text("owner clipboard".into()));
    let checks = AtomicUsize::new(0);
    let guard = || {
        if checks.fetch_add(1, Ordering::SeqCst) >= 2 {
            Err(BackendError::Denied("cancelled".into()))
        } else {
            Ok(())
        }
    };
    let result = type_text_guarded(
        kit.fake.as_ref(),
        Policy::REFUSE,
        "long text to paste",
        &TypeOptions::new(6),
        &guard,
    );
    assert!(result.is_err());
    assert_eq!(kit.fake.clipboard_text().unwrap(), "owner clipboard");
    assert!(kit.fake.events().is_empty());
}

#[test]
fn edge_profile_prefix_sibling_is_not_a_temp_child() {
    let kit = Kit::new();
    let temp = kit.computer.config().env().var("TEMP").unwrap();
    let ctx = kit.ctx(json!({"only_apps": ["msedge.exe:inprivate"]}));
    let command = format!(
        "msedge.exe --inprivate --user-data-dir={}evil/profile",
        temp
    );
    assert!(ctx.check_launch_allowed(&command).is_err());
}

#[test]
fn expectation_mismatch_is_typed_after_action_execution() {
    let kit = Kit::new();
    let error = kit.step(json!({"action": "mouse_move", "coordinate": [20, 30], "expect": {"title": "missing window"}})).unwrap_err();
    assert!(matches!(error, super::error::StepError::Expectation(_)));
    assert!(
        !error.is_validation(),
        "an executed action needs failure-state evidence"
    );
    assert_eq!(kit.fake.cursor_position().unwrap(), Point::new(20, 30));
}

#[test]
fn expectation_verdict_is_not_affected_by_failure_words_in_window_titles() {
    let kit = Kit::new();
    kit.front("step 9 (key) failed: example", "editor.exe", 30);
    let ctx = kit.ctx(json!({}));
    assert!(
        ctx.check_expect_result(&json!({"title": "step 9 (key) failed:"}))
            .unwrap()
            .is_none()
    );
    assert!(
        ctx.check_expect_result(&json!({"title": "different"}))
            .unwrap()
            .is_some()
    );
}

#[test]
fn read_only_direct_action_cannot_move_pointer_or_replace_clipboard() {
    let kit = Kit::new();
    kit.fake
        .set_clipboard(ClipboardContent::Text("owner clipboard".into()));
    let ctx = kit.ctx(json!({"read_only": true}));
    for action in [
        json!({"action": "left_mouse_down", "coordinate": [20, 30]}),
        json!({"action": "clipboard_set", "text": "changed"}),
    ] {
        assert!(ctx.run(&Kit::args(action)).is_err());
    }
    assert!(kit.fake.events().is_empty());
    assert_eq!(kit.fake.clipboard_text().unwrap(), "owner clipboard");
}

#[test]
fn cancelled_menu_poll_returns_without_advancing_a_frozen_clock() {
    let kit = Kit::new();
    let ctx = kit.ctx(json!({}));
    kit.computer.pause_flag().set(true);
    assert!(ctx.menu_items(10.0, 0.1).is_empty());
    assert!(!ctx.wait_popup(10.0, 0.1));
}

#[test]
fn pause_releases_only_the_tools_owned_split_mouse_press() {
    let kit = Kit::new();
    kit.computer.pause_and_release().unwrap();
    assert!(
        kit.fake.events().is_empty(),
        "pause must not release the person's mouse button"
    );
    kit.computer.pause_flag().set(false);
    kit.step(json!({"action": "left_mouse_down", "coordinate": [20, 30]}))
        .unwrap();
    kit.computer.pause_and_release().unwrap();
    assert!(kit.computer.pause_flag().is_paused());
    assert!(!*kit.computer.owned_mouse.lock().unwrap());
    assert_eq!(
        kit.fake.events().last(),
        Some(&InputEvent::MouseButton {
            button: MouseButton::Left,
            down: false
        })
    );
    let count = kit.fake.events().len();
    kit.computer.pause_and_release().unwrap();
    assert_eq!(
        kit.fake.events().len(),
        count,
        "already released input is not released twice"
    );
}

#[test]
fn explicit_mouse_release_clears_split_press_ownership() {
    let kit = Kit::new();
    kit.step(json!({"action": "left_mouse_down", "coordinate": [20, 30]}))
        .unwrap();
    kit.step(json!({"action": "left_mouse_up"})).unwrap();
    let count = kit.fake.events().len();
    kit.computer.pause_and_release().unwrap();
    assert_eq!(kit.fake.events().len(), count);
}

#[test]
fn split_mouse_hold_requires_release_before_another_acting_action() {
    let kit = Kit::new();
    kit.step(json!({"action": "left_mouse_down", "coordinate": [20, 30]}))
        .unwrap();
    let count = kit.fake.events().len();
    for action in [
        json!({"action": "left_click", "coordinate": [40, 50]}),
        json!({"action": "left_click_drag", "start_coordinate": [20, 30], "coordinate": [40, 50]}),
        json!({"action": "key", "text": "ctrl+a"}),
        json!({"action": "window", "op": "focus", "window": "front"}),
    ] {
        let error = kit.step(action).unwrap_err();
        assert!(error.message().contains("send left_mouse_up"));
        assert_eq!(kit.fake.events().len(), count);
    }
    kit.step(json!({"action": "mouse_move", "coordinate": [40, 50]}))
        .unwrap();
    kit.step(json!({"action": "left_mouse_up"})).unwrap();
    kit.step(json!({"action": "left_click", "coordinate": [40, 50]}))
        .unwrap();
    assert!(!*kit.computer.owned_mouse.lock().unwrap());
}

#[test]
fn typing_stops_before_the_next_character_when_foreground_changes() {
    let kit = Kit::new();
    kit.front("First editor", "editor.exe", 30);
    let second = kit.window(
        "Second editor",
        "editor.exe",
        31,
        Rect::from_xywh(10, 10, 200, 100),
    );
    let ctx = kit.ctx(json!({}));
    let target = ctx.typing_target().unwrap();
    let calls = AtomicUsize::new(0);
    let guard = || {
        if calls.fetch_add(1, Ordering::SeqCst) == 2 {
            kit.fake.set_foreground(Some(second));
        }
        ctx.typing_target_guard(target)
    };
    let result = type_text_guarded(
        kit.fake.as_ref(),
        Policy::REFUSE,
        "abc",
        &TypeOptions::new(6),
        &guard,
    );
    assert!(result.is_err());
    assert_eq!(kit.fake.typed_text(), "a");
}

#[test]
fn typing_stops_when_a_foreground_handle_has_a_different_process_owner() {
    let kit = Kit::new();
    let window = kit.front("Editor", "editor.exe", 30);
    let ctx = kit.ctx(json!({}));
    let target = ctx.typing_target().unwrap();
    let calls = AtomicUsize::new(0);
    let guard = || {
        if calls.fetch_add(1, Ordering::SeqCst) == 2 {
            kit.fake.update_window(window, |info| info.pid = 99);
        }
        ctx.typing_target_guard(target)
    };
    let result = type_text_guarded(
        kit.fake.as_ref(),
        Policy::REFUSE,
        "abc",
        &TypeOptions::new(6),
        &guard,
    );
    assert!(result.is_err());
    assert_eq!(kit.fake.typed_text(), "a");
}

#[test]
fn unrestricted_typing_reduces_full_metadata_reads_without_caching_across_calls() {
    let kit = Kit::new();
    kit.front("Editor", "editor.exe", 30);
    let before = kit.fake.window_info_reads();
    kit.ctx(json!({})).type_text("abcdef").unwrap();
    let optimized = kit.fake.window_info_reads() - before;
    let before = kit.fake.window_info_reads();
    kit.ctx(json!({"only_apps": ["editor.exe"]}))
        .type_text("abcdef")
        .unwrap();
    let restricted = kit.fake.window_info_reads() - before;
    assert!(
        optimized < restricted,
        "full reads: optimized={optimized}, restricted={restricted}"
    );
    let before = kit.fake.window_info_reads();
    kit.ctx(json!({})).type_text("a").unwrap();
    assert!(
        kit.fake.window_info_reads() > before,
        "next call must validate policy again"
    );
}

#[test]
fn typing_refuses_missing_foreground_or_unknown_owner_pid() {
    let kit = Kit::new();
    assert!(kit.ctx(json!({})).type_text("a").is_err());
    kit.front("Unknown owner", "editor.exe", 0);
    assert!(kit.ctx(json!({})).type_text("a").is_err());
    assert!(kit.fake.events().is_empty());
}
