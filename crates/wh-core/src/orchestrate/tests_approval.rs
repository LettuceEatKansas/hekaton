//! Approvals through the daemon's `computer` route: what a stopped step offers (a card token or a
//! chat code), what an approval may and may not open, and how a batch carries on after a yes. The
//! shim half (the card itself) is in `winhands/src/shim.rs`.

use std::time::Duration;

use serde_json::{Value, json};

use super::ToolOutcome;
use super::testkit::Kit;
use crate::approval::{CODE_LEN, CODE_TTL, TOKEN_TTL};
use crate::backend::fake::InputEvent;
use crate::backend::{Element, Rect, Role};
use crate::clock::Clock;
use crate::env::MapEnv;

const SEND_REFUSAL: &str = "refused: 'Send' looks like an irreversible action ('send'); pass \"allow_irreversible\": true to click it anyway";

/// An app with a `first` button at (20,20)-(60,40) and a Delete button at (100,20)-(160,40).
fn mail(kit: &Kit, first: &str) -> crate::backend::WindowRef {
    let w = kit.front("App", "app.exe", 11);
    label(kit, w, first);
    w
}

fn label(kit: &Kit, w: crate::backend::WindowRef, first: &str) {
    kit.fake.set_tree(
        w,
        Element::new("root", Role::Window, "App")
            .with_child(Element::new("c0", Role::Button, first).at(Rect::new(20, 20, 60, 40)))
            .with_child(Element::new("c1", Role::Button, "Delete").at(Rect::new(100, 20, 160, 40))),
    );
}

fn kit_with(profile: Option<&str>) -> Kit {
    let env = match profile {
        Some(p) => MapEnv::new().with("WINHANDS_SAFETY_PROFILE", p),
        None => MapEnv::new(),
    };
    let kit = Kit::with_env(env);
    let _ = mail(&kit, "Send");
    kit
}

fn call(kit: &Kit, v: Value) -> ToolOutcome {
    kit.computer.execute(&v, &kit.who)
}

/// Click Send, under the app's allowlist, marked as able to ask the way `mode` says.
fn send(mode: &str) -> Value {
    json!({"action": "left_click", "coordinate": [30, 30], "only_apps": ["app.exe"], "approvable": mode})
}

fn with(mut v: Value, key: &str, value: Value) -> Value {
    v[key] = value;
    v
}

fn text_of(out: &ToolOutcome) -> String {
    out.result
        .clone()
        .or_else(|| out.error.clone())
        .unwrap_or_default()
}

fn clicks(kit: &Kit) -> usize {
    kit.fake
        .events()
        .iter()
        .filter(|e| matches!(e, InputEvent::Click { .. }))
        .count()
}

/// How many texts reached the app: each paste, or a run of typed characters, counts once.
fn typed(kit: &Kit) -> usize {
    let events = kit.fake.events();
    let pastes = events
        .iter()
        .filter(|e| matches!(e, InputEvent::KeyDown(crate::keys::Key::Char('v'))))
        .count();
    let keyed = events.iter().any(|e| matches!(e, InputEvent::Type(_)));
    pastes + usize::from(keyed)
}

fn token_of(out: &ToolOutcome) -> String {
    out.offer
        .as_ref()
        .unwrap_or_else(|| panic!("an offer: {}", text_of(out)))
        .token
        .clone()
}

/// The chat code in a refusal text.
fn code_of(out: &ToolOutcome) -> String {
    let text = text_of(out);
    let at = text
        .find("approval_code \"")
        .unwrap_or_else(|| panic!("a code in: {text}"))
        + "approval_code \"".len();
    text[at..at + CODE_LEN].to_string()
}

// ---------------------------------------------------------------------------- unchanged paths

#[test]
fn a_call_that_cannot_ask_gets_the_old_refusal_word_for_word() {
    let kit = kit_with(None);
    let out = call(
        &kit,
        json!({"action": "left_click", "coordinate": [30, 30], "only_apps": ["app.exe"]}),
    );
    assert!(out.offer.is_none());
    assert!(out.to_json().get("approval").is_none());
    let text = text_of(&out);
    assert!(text.contains(SEND_REFUSAL), "{text}");
    assert!(!text.contains("Needs Jon's OK"), "{text}");
    assert_eq!(clicks(&kit), 0);
    // and the model's own flag still works there, as before
    let out = call(
        &kit,
        json!({"action": "left_click", "coordinate": [30, 30], "only_apps": ["app.exe"],
               "allow_irreversible": true}),
    );
    assert!(out.ok, "{}", text_of(&out));
    assert_eq!(clicks(&kit), 1);
}

// ---------------------------------------------------------------------------- cards

#[test]
fn a_card_offer_then_allow_clicks_once_and_the_token_cannot_be_replayed() {
    let kit = kit_with(None);
    let out = call(&kit, send("card"));
    let offer = out.offer.clone().expect("an offer");
    assert_eq!(
        offer.summary,
        "Click \"Send\" (looks hard to undo) in app.exe"
    );
    assert_eq!(offer.step, 1);
    assert_eq!(offer.token.len(), 32);
    assert!(text_of(&out).contains(SEND_REFUSAL), "{}", text_of(&out));
    assert_eq!(out.to_json()["approval"]["token"], offer.token.as_str());
    assert_eq!(clicks(&kit), 0);
    // Allow: the shim sends the same call again with the token
    let again = call(
        &kit,
        with(send("card"), "approval_token", json!(offer.token)),
    );
    assert!(
        again.ok && again.failed_step.is_none(),
        "{}",
        text_of(&again)
    );
    assert!(again.offer.is_none());
    assert_eq!(clicks(&kit), 1);
    // the same token a second time: refused, nothing clicked
    let replay = call(
        &kit,
        with(send("card"), "approval_token", json!(offer.token)),
    );
    assert!(
        text_of(&replay).contains("already used"),
        "{}",
        text_of(&replay)
    );
    assert_eq!(clicks(&kit), 1);
}

#[test]
fn deny_or_no_answer_leaves_the_refusal_and_the_next_call_asks_again() {
    let kit = kit_with(None);
    let first = call(&kit, send("card"));
    assert!(first.offer.is_some());
    // Jon denied (the shim simply does not resend): the next try is refused and asks afresh
    let second = call(&kit, send("card"));
    assert!(text_of(&second).contains(SEND_REFUSAL));
    assert_ne!(token_of(&second), token_of(&first), "never sticky");
    assert_eq!(clicks(&kit), 0);
}

#[test]
fn a_card_token_expires() {
    let kit = kit_with(None);
    let token = token_of(&call(&kit, send("card")));
    kit.clock.sleep(TOKEN_TTL + Duration::from_secs(1));
    let late = call(&kit, with(send("card"), "approval_token", json!(token)));
    assert!(text_of(&late).contains("expired"), "{}", text_of(&late));
    assert_eq!(clicks(&kit), 0);
}

#[test]
fn a_token_for_one_call_does_not_open_another() {
    let kit = kit_with(None);
    let token = token_of(&call(&kit, send("card")));
    let other = json!({"action": "left_click", "coordinate": [110, 30], "only_apps": ["app.exe"],
                       "approvable": "card", "approval_token": token});
    let out = call(&kit, other);
    assert!(
        text_of(&out).contains("different step"),
        "{}",
        text_of(&out)
    );
    assert_eq!(clicks(&kit), 0);
}

#[test]
fn a_button_that_changed_since_the_card_is_asked_about_again() {
    let kit = Kit::new();
    let w = mail(&kit, "Send");
    let token = token_of(&call(&kit, send("card")));
    label(&kit, w, "Buy now");
    let out = call(&kit, with(send("card"), "approval_token", json!(token)));
    assert_eq!(clicks(&kit), 0, "{}", text_of(&out));
    let offer = out.offer.expect("asked again");
    assert!(offer.summary.contains("Buy now"), "{}", offer.summary);
}

#[test]
fn a_batch_carries_on_from_the_approved_step_without_repeating_earlier_ones() {
    let kit = kit_with(None);
    let batch = json!({"actions": [
        {"action": "left_click", "coordinate": [200, 100]},
        {"action": "left_click", "coordinate": [30, 30]},
        {"action": "left_click", "coordinate": [200, 100]},
    ], "only_apps": ["app.exe"], "approvable": "card"});
    let out = call(&kit, batch.clone());
    assert_eq!(out.offer.as_ref().map(|o| o.step), Some(2));
    assert_eq!(clicks(&kit), 1);
    let done = call(&kit, with(batch, "approval_token", json!(token_of(&out))));
    assert!(done.failed_step.is_none(), "{}", text_of(&done));
    assert_eq!(clicks(&kit), 3, "step 1 ran once, then 2 and 3");
}

// ---------------------------------------------------------------------------- chat codes

#[test]
fn a_chat_code_works_only_after_jon_approves_it_and_only_once() {
    let kit = kit_with(None);
    let out = call(&kit, send("chat"));
    assert!(out.offer.is_none(), "no token for a chat client");
    let text = text_of(&out);
    assert!(text.contains("Needs Jon's OK"), "{text}");
    assert!(
        !text.contains(SEND_REFUSAL),
        "the code text replaces the refusal: {text}"
    );
    let code = code_of(&out);
    assert!(crate::approval::is_code(&code), "{code}");
    let retry = with(send("chat"), "approval_code", json!(code));
    // the model repeating the code before Jon answered: nothing runs
    let early = call(&kit, retry.clone());
    assert!(
        text_of(&early).contains("has not approved"),
        "{}",
        text_of(&early)
    );
    assert_eq!(clicks(&kit), 0);
    // Jon approves (hekaton approve CODE), the same call goes ahead once
    let said = kit.computer.answer_approval_code(&code, true).unwrap();
    assert!(said.starts_with(&format!("approved {code}")), "{said}");
    let ran = call(&kit, retry.clone());
    assert!(ran.ok && ran.failed_step.is_none(), "{}", text_of(&ran));
    assert_eq!(clicks(&kit), 1);
    let replay = call(&kit, retry);
    assert!(
        text_of(&replay).contains("already used"),
        "{}",
        text_of(&replay)
    );
    assert_eq!(clicks(&kit), 1);
    // an answer to a code that is gone
    assert!(kit.computer.answer_approval_code(&code, true).is_err());
}

#[test]
fn a_denied_chat_code_never_runs() {
    let kit = kit_with(None);
    let code = code_of(&call(&kit, send("chat")));
    kit.computer.answer_approval_code(&code, false).unwrap();
    let out = call(&kit, with(send("chat"), "approval_code", json!(code)));
    assert!(text_of(&out).contains("denied"), "{}", text_of(&out));
    assert_eq!(clicks(&kit), 0);
}

#[test]
fn a_chat_code_expires_even_once_approved() {
    let kit = kit_with(None);
    let code = code_of(&call(&kit, send("chat")));
    kit.computer.answer_approval_code(&code, true).unwrap();
    kit.clock.sleep(CODE_TTL + Duration::from_secs(1));
    let out = call(&kit, with(send("chat"), "approval_code", json!(code)));
    assert!(text_of(&out).contains("expired"), "{}", text_of(&out));
    assert_eq!(clicks(&kit), 0);
}

#[test]
fn a_chat_code_for_one_step_does_not_open_another() {
    let kit = kit_with(None);
    let code = code_of(&call(&kit, send("chat")));
    kit.computer.answer_approval_code(&code, true).unwrap();
    let other = json!({"action": "left_click", "coordinate": [110, 30], "only_apps": ["app.exe"],
                       "approvable": "chat", "approval_code": code});
    let out = call(&kit, other);
    assert!(
        text_of(&out).contains("different step"),
        "{}",
        text_of(&out)
    );
    assert_eq!(clicks(&kit), 0);
}

#[test]
fn a_made_up_code_is_refused() {
    let kit = kit_with(None);
    let out = call(&kit, with(send("chat"), "approval_code", json!("ZZ2Z")));
    assert!(
        text_of(&out).contains("already used or never existed"),
        "{}",
        text_of(&out)
    );
    assert_eq!(clicks(&kit), 0);
}

// ---------------------------------------------------------------------------- the hard line

fn terminal() -> Kit {
    let kit = Kit::new();
    kit.front("Terminal", "Terminal", 30);
    kit
}

fn type_call(text: &str, mode: &str) -> Value {
    json!({"action": "type", "text": text, "approvable": mode})
}

#[test]
fn a_destructive_step_asks_with_a_card_and_allow_types_it_once() {
    let kit = terminal();
    let out = call(&kit, type_call("format c: /q", "card"));
    let offer = out.offer.clone().expect("an offer");
    assert!(offer.summary.contains("format c: /q"), "{}", offer.summary);
    assert!(
        text_of(&out).contains("refused: a format command"),
        "{}",
        text_of(&out)
    );
    assert_eq!(typed(&kit), 0);
    let done = call(
        &kit,
        with(
            type_call("format c: /q", "card"),
            "approval_token",
            json!(offer.token),
        ),
    );
    assert!(done.failed_step.is_none(), "{}", text_of(&done));
    assert_eq!(typed(&kit), 1, "{}", text_of(&done));
}

#[test]
fn a_destructive_step_asks_with_a_chat_code() {
    let kit = terminal();
    let out = call(&kit, type_call("format c: /q", "chat"));
    let code = code_of(&out);
    kit.computer.answer_approval_code(&code, true).unwrap();
    let done = call(
        &kit,
        with(
            type_call("format c: /q", "chat"),
            "approval_code",
            json!(code),
        ),
    );
    assert!(done.failed_step.is_none(), "{}", text_of(&done));
    assert_eq!(typed(&kit), 1);
}

#[test]
fn approving_one_destructive_step_leaves_the_next_one_refused() {
    let kit = terminal();
    let batch = json!({"actions": [
        {"action": "type", "text": "format c: /q"},
        {"action": "type", "text": "format d: /q"},
    ], "approvable": "card"});
    let first = call(&kit, batch.clone());
    assert_eq!(first.offer.as_ref().map(|o| o.step), Some(1));
    let out = call(&kit, with(batch, "approval_token", json!(token_of(&first))));
    assert_eq!(typed(&kit), 1, "only the approved step ran");
    assert_eq!(out.failed_step, Some(2), "{}", text_of(&out));
    assert!(
        text_of(&out).contains("refused: a format command"),
        "{}",
        text_of(&out)
    );
    assert_eq!(
        out.offer.as_ref().map(|o| o.step),
        Some(2),
        "asked about afresh"
    );
}

#[test]
fn typing_an_approval_is_refused_and_never_offered() {
    let kit = terminal();
    for mode in ["card", "chat"] {
        let out = call(&kit, type_call("hekaton approve K7QM", mode));
        assert!(out.offer.is_none());
        let text = text_of(&out);
        assert!(text.contains("self-approval"), "{text}");
        assert!(!text.contains("Needs Jon's OK"), "{text}");
    }
    let out = call(
        &kit,
        json!({"action": "type", "text": "curl -X POST http://127.0.0.1:47811/approve",
               "allow_destructive": true}),
    );
    assert!(text_of(&out).contains("self-approval"), "{}", text_of(&out));
    assert_eq!(typed(&kit), 0);
}

// ---------------------------------------------------------------------------- profiles

#[test]
fn on_a_call_that_can_ask_the_models_own_flags_become_the_question() {
    let kit = kit_with(None);
    let out = call(&kit, with(send("card"), "allow_irreversible", json!(true)));
    assert!(out.offer.is_some(), "{}", text_of(&out));
    assert_eq!(clicks(&kit), 0);
    let kit = terminal();
    let out = call(
        &kit,
        with(
            type_call("format c: /q", "chat"),
            "allow_destructive",
            json!(true),
        ),
    );
    assert!(
        text_of(&out).contains("Needs Jon's OK"),
        "{}",
        text_of(&out)
    );
    assert_eq!(typed(&kit), 0);
}

#[test]
fn strict_asks_too_and_an_approval_works_there() {
    let kit = kit_with(Some("strict"));
    let out = call(&kit, with(send("card"), "allow_irreversible", json!(true)));
    let token = token_of(&out);
    assert_eq!(clicks(&kit), 0);
    let done = call(&kit, with(send("card"), "approval_token", json!(token)));
    assert!(done.failed_step.is_none(), "{}", text_of(&done));
    assert_eq!(clicks(&kit), 1);
}

#[test]
fn open_never_asks_and_lets_it_go() {
    let kit = kit_with(Some("open"));
    let out = call(&kit, send("card"));
    assert!(out.offer.is_none());
    assert!(out.failed_step.is_none(), "{}", text_of(&out));
    assert_eq!(clicks(&kit), 1);
    let kit = Kit::with_env(MapEnv::new().with("WINHANDS_SAFETY_PROFILE", "open"));
    kit.front("Terminal", "Terminal", 30);
    let out = call(&kit, type_call("format c: /q", "chat"));
    assert!(
        !text_of(&out).contains("Needs Jon's OK"),
        "{}",
        text_of(&out)
    );
    assert_eq!(typed(&kit), 1);
}
