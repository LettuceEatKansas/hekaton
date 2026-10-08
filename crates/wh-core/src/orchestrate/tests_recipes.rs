//! Recipes through `run_many`, over a fake desktop. Ported from `tests/test_20261008b_recipes.py`
//! (the sections "running a recipe through run_many", "the safety layer is still in the way of every
//! recipe step" and "audit log"). The Python tests replaced `run()` with a recorder; here the steps are
//! real, cheap ones (`clipboard_set`, `wait`, `key`) whose effect the fake backend records.

use serde_json::{Value, json};

use super::StepError;
use super::testkit::Kit;
use crate::backend::fake::InputEvent;
use crate::backend::{Backend, ClipboardContent, Element, Rect, Role, WindowRef};
use crate::env::MapEnv;
use crate::keys::Key;

fn no_shot_env() -> MapEnv {
    MapEnv::new().with("WINHANDS_AUTO_SHOT", "0")
}

/// A recipe object with the Python tests' defaults, overridden by `over`.
fn good(over: Value) -> Value {
    let mut rec = json!({
        "name": "demo",
        "description": "Open a file",
        "params": {"path": {"required": true, "description": "full path"}, "n": {"default": "3"}},
        "steps": [
            {"action": "clipboard_set", "text": "open {path}"},
            {"action": "clipboard_get"},
            {"action": "clipboard_set", "text": "x{n}"}
        ]
    });
    for (k, v) in over.as_object().expect("overrides") {
        rec[k] = v.clone();
    }
    rec
}

fn kit() -> Kit {
    let mut kit = Kit::new();
    kit.empty_recipes();
    kit
}

fn clip(kit: &Kit) -> Option<String> {
    match kit.fake.clipboard().unwrap() {
        ClipboardContent::Text(t) => Some(t),
        _ => None,
    }
}

/// A front window whose accessibility tree names these things, so `expect` and `when` can find them.
fn front_with(kit: &Kit, app: &str, title: &str, names: &[&str]) -> WindowRef {
    let w = kit.front(title, app, 7);
    let mut root = Element::new("root", Role::Window, title);
    for (i, n) in names.iter().enumerate() {
        let i = i as i32;
        root = root.with_child(
            Element::new(&format!("e{i}"), Role::Button, n).at(Rect::new(
                i * 40,
                0,
                i * 40 + 30,
                20,
            )),
        );
    }
    kit.fake.set_tree(w, root);
    w
}

fn err_text(r: Result<super::ManyResult, StepError>) -> String {
    r.expect_err("the call should fail").message().to_string()
}

// ---------------------------------------------------------------------------- running

// Ported from test_a_recipe_runs_as_one_batch_with_one_trailing_screenshot.
#[test]
fn a_recipe_runs_as_one_batch_with_one_trailing_screenshot() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({})));
    let out = kit
        .run(json!({"action": "recipe", "name": "demo", "params": {"path": "C:\\a.txt", "n": 4}}))
        .unwrap();
    let lines: Vec<&str> = out.text.lines().collect();
    assert_eq!(lines[0], "recipe demo(path, n=3): 3 steps");
    assert!(!lines[0].contains("a.txt"), "names, never values");
    assert_eq!(lines[1], "clipboard set to 13 chars");
    assert!(
        lines[2].starts_with("clipboard (13 chars): 'open C:"),
        "{}",
        lines[2]
    );
    assert!(lines[2].contains("a.txt'"), "{}", lines[2]);
    assert_eq!(lines[3], "clipboard set to 2 chars");
    assert_eq!(clip(&kit).as_deref(), Some("x4"));
    assert_eq!(out.images.len(), 1);
    assert!(lines.last().unwrap().starts_with("screen settled"));
}

// Ported from test_shot_false_drops_the_trailing_screenshot.
#[test]
fn shot_false_drops_the_trailing_screenshot() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({})));
    let out = kit
        .run(json!({"action": "recipe", "name": "demo", "params": {"path": "p"}, "shot": false}))
        .unwrap();
    assert!(
        out.images.is_empty() && !out.text.contains("[image"),
        "{}",
        out.text
    );
}

// Ported from test_a_failing_step_stops_the_recipe_and_names_the_step.
#[test]
fn a_failing_step_stops_the_recipe_and_names_the_step() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "three",
        "params": {},
        "steps": [
            {"action": "clipboard_set", "text": "a"},
            {"action": "wait", "duration": "abc"},
            {"action": "clipboard_set", "text": "b"}
        ]
    })));
    let out = kit
        .run(json!({"action": "recipe", "name": "three"}))
        .unwrap();
    assert!(
        out.text
            .contains("step 2 (wait) failed: could not convert string to float: 'abc'"),
        "{}",
        out.text
    );
    assert_eq!(clip(&kit).as_deref(), Some("a"), "step 3 must not run");
}

// Ported from test_a_failed_expect_stops_the_recipe.
#[test]
fn a_failed_expect_stops_the_recipe_and_says_so_on_a_step_line() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "chk",
        "params": {},
        "steps": [
            {"action": "clipboard_set", "text": "a", "expect": {"text": "zzz"}},
            {"action": "clipboard_set", "text": "b"}
        ]
    })));
    let out = kit.run(json!({"action": "recipe", "name": "chk"})).unwrap();
    assert_eq!(clip(&kit).as_deref(), Some("a"));
    assert!(out.text.contains("expect: FAIL"), "{}", out.text);
    // 2026-10-08 live run: wh exited 0 after a recipe step's expect failed
    assert!(
        out.text
            .contains("step 1 (clipboard_set) failed: expect not met, batch stopped"),
        "{}",
        out.text
    );
    assert_eq!(
        super::many::batch_failure_fields(&out.text).map(|(n, _)| n),
        Some(1)
    );
}

// Ported from test_a_batch_stopped_by_expect_reports_its_failed_step.
#[test]
fn a_batch_stopped_by_expect_reports_its_failed_step() {
    let text = "recipe note(text): 3 steps\nok\npressed ctrl+n; expect: FAIL (title is 'x')\nstep 2 (key) failed: expect not met, batch stopped";
    assert_eq!(
        super::many::batch_failure_fields(text),
        Some((2, "expect not met, batch stopped".to_string()))
    );
}

#[test]
fn a_lone_step_whose_expect_fails_gets_no_extra_failed_line() {
    let kit = kit();
    let out = kit
        .run(json!({"action": "clipboard_set", "text": "a", "expect": {"text": "zzz"}}))
        .unwrap();
    assert!(out.text.contains("expect: FAIL"), "{}", out.text);
    assert!(!out.text.contains("batch stopped"), "{}", out.text);
}

// Ported from test_on_fail_skip_carries_on_after_an_error_and_is_not_reported_as_a_failed_batch.
#[test]
fn on_fail_skip_carries_on_after_an_error_and_is_not_a_failed_batch() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "soft",
        "params": {},
        "steps": [
            {"action": "wait", "duration": "abc", "on_fail": "skip"},
            {"action": "clipboard_set", "text": "b"}
        ]
    })));
    let out = kit
        .run(json!({"action": "recipe", "name": "soft"}))
        .unwrap();
    assert_eq!(clip(&kit).as_deref(), Some("b"));
    assert!(
        out.text.contains(
            "step 1 (wait) errored and was skipped: could not convert string to float: 'abc'"
        ),
        "{}",
        out.text
    );
    assert!(!out.text.contains("failed:"), "{}", out.text);
    assert_eq!(super::many::batch_failure_fields(&out.text), None);
}

// Ported from test_on_fail_skip_also_covers_a_failed_expect.
#[test]
fn on_fail_skip_also_covers_a_failed_expect() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "soft2",
        "params": {},
        "steps": [
            {"action": "clipboard_set", "text": "a", "expect": {"text": "zzz"}, "on_fail": "skip"},
            {"action": "clipboard_set", "text": "b"}
        ]
    })));
    let out = kit
        .run(json!({"action": "recipe", "name": "soft2"}))
        .unwrap();
    assert_eq!(clip(&kit).as_deref(), Some("b"));
    assert!(!out.text.contains("batch stopped"), "{}", out.text);
}

// Ported from test_on_fail_stop_is_the_default_and_can_be_said_out_loud.
#[test]
fn on_fail_stop_is_the_default_and_can_be_said_out_loud() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "hard",
        "params": {},
        "steps": [
            {"action": "wait", "duration": "abc", "on_fail": "stop"},
            {"action": "clipboard_set", "text": "b"}
        ]
    })));
    let out = kit
        .run(json!({"action": "recipe", "name": "hard"}))
        .unwrap();
    assert_eq!(clip(&kit), None);
    assert!(out.text.contains("step 1 (wait) failed:"), "{}", out.text);
}

// Ported from test_when_runs_a_step_only_if_the_condition_holds_right_now.
#[test]
fn when_runs_a_step_only_if_the_condition_holds_right_now() {
    let mut kit = kit();
    front_with(&kit, "notepad.exe", "Notepad", &["Save changes"]);
    kit.put_bundled(&good(json!({
        "name": "branch",
        "params": {},
        "steps": [
            {"action": "key", "text": "Return", "when": {"text": "Save changes"}},
            {"action": "key", "text": "Escape", "when": {"gone_text": "Save changes"}},
            {"action": "type", "text": "done"}
        ]
    })));
    let out = kit
        .run(json!({"action": "recipe", "name": "branch"}))
        .unwrap();
    let pressed: Vec<Key> = kit
        .fake
        .events()
        .into_iter()
        .filter_map(|e| match e {
            InputEvent::KeyDown(k) => Some(k),
            _ => None,
        })
        .collect();
    assert!(
        pressed.contains(&Key::Enter) && !pressed.contains(&Key::Escape),
        "{pressed:?}"
    );
    assert_eq!(kit.fake.typed_text(), "done");
    assert!(
        out.text
            .contains("step 2 (key) skipped: when not met (text 'Save changes' is still there)"),
        "{}",
        out.text
    );
}

// Ported from test_when_works_in_a_hand_written_batch_too.
#[test]
fn when_works_in_a_hand_written_batch_too() {
    let kit = kit();
    front_with(&kit, "notepad.exe", "Notepad", &[]);
    let out = kit
        .run(json!({"actions": [
            {"action": "type", "text": "a", "when": {"title": "other app"}},
            {"action": "type", "text": "b"}
        ]}))
        .unwrap();
    assert_eq!(kit.fake.typed_text(), "b");
    assert!(
        out.text
            .contains("skipped: when not met (title is 'notepad', wanted 'other app')"),
        "{}",
        out.text
    );
}

// Ported from test_a_malformed_when_fails_that_step.
#[test]
fn a_malformed_when_fails_that_step() {
    let kit = kit();
    let out = kit
        .run(json!({"actions": [{"action": "type", "text": "a", "when": "now"}]}))
        .unwrap();
    assert!(
        out.text
            .contains("step 1 (type) failed: when: when must be an object"),
        "{}",
        out.text
    );
    assert_eq!(kit.fake.typed_text(), "");
}

// Ported from test_recipe_steps_mix_with_ordinary_steps_in_one_batch.
#[test]
fn recipe_steps_mix_with_ordinary_steps_in_one_batch() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "mid",
        "params": {},
        "steps": [{"action": "type", "text": "m"}]
    })));
    let out = kit
        .run(json!({"actions": [
            {"action": "type", "text": "first"},
            {"action": "recipe", "name": "mid"},
            {"action": "type", "text": "last"}
        ]}))
        .unwrap();
    assert_eq!(kit.fake.typed_text(), "firstmlast");
    assert!(out.text.contains("recipe mid(): 1 steps"), "{}", out.text);
}

// Ported from test_an_unknown_recipe_or_bad_params_fail_before_any_step_runs.
#[test]
fn an_unknown_recipe_or_bad_params_fail_before_any_step_runs() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({})));
    for (call, why) in [
        (
            json!({"action": "recipe", "name": "ghost"}),
            "no recipe named 'ghost'",
        ),
        (json!({"action": "recipe", "name": "demo"}), "needs param"),
        (
            json!({"action": "recipe", "name": "demo", "params": {"path": "p", "bad": 1}}),
            "has no param bad",
        ),
        (json!({"action": "recipe"}), "recipe name"),
    ] {
        let msg = err_text(kit.run(call));
        assert!(msg.contains(why), "{msg}");
    }
    let msg = err_text(kit.run(json!({"actions": [
        {"action": "clipboard_set", "text": "x"},
        {"action": "recipe", "name": "ghost"}
    ]})));
    assert!(msg.contains("no recipe named"), "{msg}");
    assert_eq!(clip(&kit), None, "not even the first step ran");
}

// Ported from test_a_recipe_called_directly_through_run_explains_itself.
#[test]
fn a_recipe_called_directly_through_run_explains_itself() {
    let kit = kit();
    let err = kit
        .step(json!({"action": "recipe", "name": "x"}))
        .expect_err("recipe needs a batch");
    assert!(err.message().contains("runs inside a batch"), "{err}");
}

// Ported from test_a_text_answer_ends_without_a_screenshot_and_a_window_op_keeps_it (the listing half).
#[test]
fn a_text_answer_ends_without_a_screenshot() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "ask",
        "params": {},
        "steps": [{"action": "list_windows"}]
    })));
    assert!(
        kit.run(json!({"action": "recipe", "name": "ask"}))
            .unwrap()
            .images
            .is_empty()
    );
    assert!(
        kit.run(json!({"action": "recipe_list"}))
            .unwrap()
            .images
            .is_empty()
    );
}

// Ported from test_a_user_recipe_overrides_a_bundled_one_of_the_same_name (the run half).
#[test]
fn a_users_own_recipe_wins_when_the_name_is_run() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "same", "params": {}, "steps": [{"action": "clipboard_set", "text": "bundled"}]
    })));
    kit.put_user(&good(json!({
        "name": "same", "params": {}, "steps": [{"action": "clipboard_set", "text": "mine"}]
    })));
    kit.run(json!({"action": "recipe", "name": "same", "shot": false}))
        .unwrap();
    assert_eq!(clip(&kit).as_deref(), Some("mine"));
    let listed = kit.run(json!({"action": "recipe_list"})).unwrap();
    assert!(
        listed.text.contains("same(): Open a file [yours]"),
        "{}",
        listed.text
    );
}

// Ported from test_a_text_answer_ends_without_a_screenshot_and_a_window_op_keeps_it (the window half):
// an acting step last means the caller gets the picture of what it did.
#[test]
fn a_window_op_as_the_last_step_keeps_the_screenshot() {
    let mut kit = kit();
    kit.front("x - Notepad", "notepad.exe", 7);
    kit.put_bundled(&good(json!({
        "name": "act", "params": {},
        "steps": [{"action": "window", "op": "focus", "window": "notepad.exe"}]
    })));
    let out = kit.run(json!({"action": "recipe", "name": "act"})).unwrap();
    assert_eq!(out.images.len(), 1, "{}", out.text);
}

// ---------------------------------------------------------------------------- the safety layer

// Ported from test_a_refusal_inside_a_recipe_stops_it_like_any_batch.
#[test]
fn a_refusal_inside_a_recipe_stops_it_like_any_batch() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "evil",
        "params": {"t": {"required": true}},
        "steps": [{"action": "type", "text": "{t}"}, {"action": "key", "text": "Return"}]
    })));
    let out = kit
        .run(json!({"action": "recipe", "name": "evil", "params": {"t": "format c: /q"}}))
        .unwrap();
    assert!(
        out.text
            .contains("step 1 (type) failed: refused: a format command"),
        "{}",
        out.text
    );
    assert!(
        out.text.contains(crate::destructive::OPT_IN),
        "{}",
        out.text
    );
    assert!(
        kit.fake.events().is_empty(),
        "nothing was sent, and Return never ran"
    );
}

// Ported from test_a_recipe_cannot_carry_allow_irreversible_but_the_caller_can_grant_it_for_one_call.
#[test]
fn a_recipe_cannot_carry_allow_irreversible_but_the_caller_can_grant_it_for_one_call() {
    let mut kit = kit();
    let w = kit.front("Shop", "notepad.exe", 7);
    kit.fake.set_tree(
        w,
        Element::new("root", Role::Window, "Shop")
            .with_child(Element::new("b", Role::Button, "Send").at(Rect::new(0, 0, 50, 30))),
    );
    kit.put_bundled(&good(json!({
        "name": "press",
        "params": {},
        "steps": [{"action": "left_click", "coordinate": [20, 20]}]
    })));
    let call = |extra: Value| {
        let mut c = json!({"action": "recipe", "name": "press", "only_apps": ["notepad.exe"], "shot": false});
        for (k, v) in extra.as_object().unwrap() {
            c[k] = v.clone();
        }
        c
    };
    let clicks = |kit: &Kit| {
        kit.fake
            .events()
            .iter()
            .filter(|e| matches!(e, InputEvent::Click { .. }))
            .count()
    };
    let refused = kit.run(call(json!({}))).unwrap();
    assert!(
        refused.text.contains("looks like an irreversible action"),
        "{}",
        refused.text
    );
    assert_eq!(clicks(&kit), 0);
    kit.run(call(json!({"allow_irreversible": true}))).unwrap();
    assert_eq!(clicks(&kit), 1, "the caller granted it for this call");
    let again = kit.run(call(json!({}))).unwrap();
    assert!(
        again.text.contains("looks like an irreversible action"),
        "not sticky: {}",
        again.text
    );
    assert_eq!(clicks(&kit), 1);
    kit.put_bundled(&good(json!({
        "name": "sneaky",
        "params": {},
        "steps": [{"action": "left_click", "allow_irreversible": true}]
    })));
    let msg = err_text(kit.run(json!({"action": "recipe", "name": "sneaky"})));
    assert!(msg.contains("cannot be stored in a recipe"), "{msg}");
}

fn key_step_refusal(kit: &Kit, call: Value) -> String {
    kit.run(call).unwrap().text
}

// Ported from test_the_call_allowlist_applies_to_recipe_steps_and_a_recipe_can_only_narrow_it.
#[test]
fn the_call_allowlist_applies_to_recipe_steps_and_a_recipe_can_only_narrow_it() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "open",
        "params": {},
        "only_apps": ["notepad.exe"],
        "steps": [{"action": "key", "text": "a"}]
    })));
    kit.put_bundled(&good(json!({
        "name": "free",
        "params": {},
        "steps": [{"action": "key", "text": "a"}]
    })));
    kit.front("Mail", "olk.exe", 8);
    // no call allowlist: the recipe's own is used for its steps
    let t = key_step_refusal(
        &kit,
        json!({"action": "recipe", "name": "open", "shot": false}),
    );
    assert!(
        t.contains("belongs to olk.exe, which this task may not touch (allowed: notepad.exe)"),
        "{t}"
    );
    // the call's allowlist is what a recipe without one runs under
    let t = key_step_refusal(
        &kit,
        json!({"action": "recipe", "name": "free", "only_apps": ["Notepad.EXE", "calc.exe"], "shot": false}),
    );
    assert!(t.contains("(allowed: calc.exe, notepad.exe)"), "{t}");
    // both: only what the two share
    let t = key_step_refusal(
        &kit,
        json!({"action": "recipe", "name": "open", "only_apps": ["notepad.exe", "calc.exe"], "shot": false}),
    );
    assert!(t.contains("(allowed: notepad.exe)"), "{t}");
    // nothing shared is refused outright, never silently widened
    let msg =
        err_text(kit.run(json!({"action": "recipe", "name": "open", "only_apps": ["calc.exe"]})));
    assert!(
        msg.contains(
            "limited to ['notepad.exe'], which this call's allowlist ['calc.exe'] does not allow"
        ),
        "{msg}"
    );
}

// Ported from test_title_scoped_allowlist_entries_keep_the_stricter_side.
#[test]
fn title_scoped_allowlist_entries_keep_the_stricter_side() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "scoped",
        "params": {},
        "only_apps": ["msedge.exe:inprivate"],
        "steps": [{"action": "key", "text": "a"}]
    })));
    kit.put_bundled(&good(json!({
        "name": "plain",
        "params": {},
        "only_apps": ["msedge.exe"],
        "steps": [{"action": "key", "text": "a"}]
    })));
    kit.front("Work - Edge", "msedge.exe", 9);
    // the recipe is stricter: the call allows all of Edge, the recipe only its InPrivate windows
    let t = key_step_refusal(
        &kit,
        json!({"action": "recipe", "name": "scoped", "only_apps": ["msedge.exe"], "shot": false}),
    );
    assert!(t.contains("(allowed: msedge.exe:inprivate)"), "{t}");
    // the call is stricter
    let t = key_step_refusal(
        &kit,
        json!({"action": "recipe", "name": "plain", "only_apps": ["msedge.exe:inprivate"], "shot": false}),
    );
    assert!(t.contains("(allowed: msedge.exe:inprivate)"), "{t}");
    // a window that fits the scope is allowed
    kit.front("InPrivate - Edge", "msedge.exe", 9);
    let t = key_step_refusal(
        &kit,
        json!({"action": "recipe", "name": "scoped", "only_apps": ["msedge.exe"], "shot": false}),
    );
    assert!(t.contains("pressed a"), "{t}");
    let msg = err_text(
        kit.run(json!({"action": "recipe", "name": "scoped", "only_apps": ["msedge.exe:work"]})),
    );
    assert!(msg.contains("does not allow"), "{msg}");
}

// Ported from test_the_call_allowlist_is_restored_even_when_a_recipe_step_fails: the call's own
// allowlist belongs to the call and a recipe step runs in a context of its own, so the step after the
// recipe sees the call's list again however the recipe ended.
#[test]
fn the_call_allowlist_is_back_after_a_recipe_step_even_when_it_fails() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "boom",
        "params": {},
        "only_apps": ["notepad.exe"],
        "steps": [{"action": "wait", "duration": "abc", "on_fail": "skip"}]
    })));
    kit.front("Mail", "olk.exe", 8);
    let out = kit
        .run(json!({"actions": [
            {"action": "recipe", "name": "boom"},
            {"action": "key", "text": "a"}
        ], "only_apps": ["notepad.exe", "calc.exe"], "shot": false}))
        .unwrap();
    assert!(out.text.contains("errored and was skipped"), "{}", out.text);
    assert!(
        out.text.contains("(allowed: calc.exe, notepad.exe)"),
        "{}",
        out.text
    );
}

// Ported from test_every_recipe_step_goes_through_run: the human-input wait runs per acting step.
#[test]
fn every_recipe_step_goes_through_the_normal_checks() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "s",
        "params": {},
        "steps": [{"action": "key", "text": "a"}, {"action": "type", "text": "b"}]
    })));
    kit.fake.set_human_idle(std::time::Duration::from_secs(60));
    kit.run(json!({"action": "recipe", "name": "s", "shot": false}))
        .unwrap();
    assert!(
        kit.fake.human_idle_reads() >= 2,
        "{}",
        kit.fake.human_idle_reads()
    );
    assert_eq!(kit.fake.typed_text(), "b");
}

// ---------------------------------------------------------------------------- shortcuts in a batch

// Ported from test_a_window_step_settles_before_the_next_step.
#[test]
fn a_window_step_settles_before_the_next_step() {
    let mut kit = Kit::with_env(no_shot_env());
    kit.empty_recipes();
    let w = kit.front("a - Notepad", "notepad.exe", 7);
    let before = kit.fake.capture_count();
    kit.run(json!({"actions": [
        {"action": "window", "op": "focus", "window": "notepad.exe"},
        {"action": "key", "text": "a"}
    ]}))
    .unwrap();
    assert!(
        kit.fake.capture_count() > before,
        "the screen was watched between the steps"
    );
    assert_eq!(kit.fake.foreground_window().unwrap(), Some(w));
}

// ---------------------------------------------------------------------------- acknowledgement

// Ported from test_recipe_save_and_recipe_need_the_acknowledgement_recipe_list_does_not and
// test_window_and_folder_need_the_acknowledgement.
#[test]
fn the_new_actions_need_the_acknowledgement_except_recipe_list() {
    use crate::config::Config;
    use crate::paths::Paths;
    use crate::testutil::TempDir;
    let tmp = TempDir::new("ack-new");
    let cfg = Config::new(Paths::in_dir(tmp.path()), MapEnv::new());
    for action in ["recipe", "recipe_save", "window", "folder"] {
        let err = crate::ack::check_acknowledged(&cfg, "computer", &json!({"action": action}))
            .expect_err(action);
        assert!(err.to_string().contains("not been acknowledged"), "{err}");
    }
    // a recipe hidden inside a batch of reads is still a recipe
    let err = crate::ack::check_acknowledged(
        &cfg,
        "computer",
        &json!({"actions": [{"action": "list_windows"}, {"action": "recipe", "name": "x"}]}),
    )
    .expect_err("batch");
    assert!(err.to_string().contains("not been acknowledged"), "{err}");
    assert!(
        crate::ack::check_acknowledged(&cfg, "computer", &json!({"action": "recipe_list"})).is_ok()
    );
    assert!(
        crate::ack::check_acknowledged(&cfg, "computer", &json!({"action": "list_windows"}))
            .is_ok()
    );
}

// ---------------------------------------------------------------------------- audit

// Ported from test_the_daemon_writes_those_fields_into_its_audit_entry.
#[test]
fn the_audit_entry_names_the_recipe_and_the_param_names_never_the_values() {
    let mut kit = kit();
    kit.put_bundled(&good(json!({
        "name": "note",
        "params": {"text": {"required": true}},
        "steps": [{"action": "clipboard_set", "text": "{text}"}]
    })));
    let out = kit.computer.execute(
        &json!({"action": "recipe", "name": "note", "params": {"text": "hunter2"}}),
        &kit.who,
    );
    assert!(out.ok, "{:?}", out.error);
    let raw = std::fs::read_to_string(kit.computer.cfg.paths().audit_log()).unwrap();
    assert!(!raw.contains("hunter2"), "{raw}");
    let entry: Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
    let args = &entry["args"];
    assert_eq!(args["action"], "recipe");
    assert_eq!(args["steps"], 1);
    assert_eq!(
        args["recipes"],
        json!([{"action": "recipe", "name": "note", "params": ["text"]}])
    );
}

// Ported from test_a_refused_recipe_call_is_logged_with_its_params_masked.
#[test]
fn a_refused_recipe_call_is_logged_with_its_params_masked() {
    let kit = kit();
    let out = kit.computer.execute(
        &json!({"action": "recipe", "name": "ghost", "params": {"text": "hunter2"}}),
        &kit.who,
    );
    assert!(!out.ok);
    let raw = std::fs::read_to_string(kit.computer.cfg.paths().audit_log()).unwrap();
    assert!(!raw.contains("hunter2"), "{raw}");
    let entry: Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
    assert!(
        entry["args"]["params"]["text"]
            .as_str()
            .unwrap()
            .starts_with("<7 chars "),
        "{raw}"
    );
    assert_eq!(entry["args"]["name"], "ghost");
}

// A batch that fails to start logs what each step typed redacted too.
#[test]
fn a_refused_batch_is_logged_with_each_steps_text_masked() {
    let kit = kit();
    let out = kit.computer.execute(
        &json!({"actions": [
            {"action": "type", "text": "hunter2"},
            {"action": "recipe", "name": "ghost", "params": {"p": "s3cret"}}
        ]}),
        &kit.who,
    );
    assert!(!out.ok);
    let raw = std::fs::read_to_string(kit.computer.cfg.paths().audit_log()).unwrap();
    assert!(!raw.contains("hunter2") && !raw.contains("s3cret"), "{raw}");
}
