//! Session regressions use only the in-memory backend. No desktop access.

use serde_json::{Value, json};

use super::sessions::SessionGuard;
use super::testkit::Kit;
use crate::backend::{Backend, Rect};
use crate::caller::Caller;
use crate::clock::Clock;
use std::time::Duration;

fn observe(kit: &Kit, args: &Value) -> String {
    let mut session = SessionGuard::enter(&kit.computer, args, &kit.who)
        .unwrap()
        .unwrap();
    kit.computer.mem().target = kit.fake.foreground_window().unwrap();
    session.observe(args).expect("simple screenshot frame")
}

#[test]
fn named_unfocused_read_does_not_mint_foreground_snapshot() {
    let kit = Kit::new();
    let front = kit.front("Front", "front.exe", 7);
    kit.window("Background", "other.exe", 8, Rect::new(40, 40, 150, 120));
    let args = json!({"session_id":"a", "action":"read", "window":"Background"});
    let mut session = SessionGuard::enter(&kit.computer, &args, &kit.who)
        .unwrap()
        .unwrap();
    kit.computer.mem().target = Some(front);
    assert_eq!(session.observe(&args), None);
}

#[test]
fn moved_or_resized_window_invalidates_snapshot_before_input() {
    let kit = Kit::new();
    let window = kit.front("App", "app.exe", 7);
    let shot = json!({"session_id":"a", "action":"screenshot"});
    for rect in [Rect::new(20, 20, 320, 180), Rect::new(20, 20, 200, 120)] {
        let snapshot = observe(&kit, &shot);
        kit.fake.update_window(window, |info| info.rect = rect);
        let args = json!({"session_id":"a", "snapshot_id":snapshot, "action":"left_click", "coordinate":[20,20]});
        let out = kit.computer.execute(&args, &kit.who);
        assert!(!out.ok, "changed window frame must fail");
        assert!(
            kit.fake.events().is_empty(),
            "stale frame must inject nothing"
        );
    }
}

#[test]
fn snapshot_geometry_is_per_request_and_never_inherits_old_settings() {
    let kit = Kit::new();
    kit.front("App", "app.exe", 7);
    let shot =
        json!({"session_id":"a", "action":"screenshot", "coords":"norm1000", "shot_long":128});
    let snapshot = observe(&kit, &shot);
    let args = json!({"session_id":"a", "snapshot_id":snapshot, "action":"left_click", "coordinate":[10,10]});
    let mut session = SessionGuard::enter(&kit.computer, &args, &kit.who)
        .unwrap()
        .unwrap();
    assert!(
        session.validate(&args).is_err(),
        "omitted settings mean defaults, not old screenshot geometry"
    );
}

#[test]
fn snapshot_is_consumed_even_when_attempted_request_fails() {
    let kit = Kit::new();
    kit.front("App", "app.exe", 7);
    let shot = json!({"session_id":"a", "action":"screenshot"});
    let snapshot = observe(&kit, &shot);
    let invalid = json!({"session_id":"a", "snapshot_id":snapshot, "action":"key", "text":"not-a-key", "shot":false});
    let out = kit.computer.execute(&invalid, &kit.who);
    assert!(!out.ok);
    let mut session = SessionGuard::enter(&kit.computer, &invalid, &kit.who)
        .unwrap()
        .unwrap();
    assert!(session.validate(&invalid).is_err());
}

#[test]
fn identical_session_names_are_isolated_between_known_callers() {
    let kit = Kit::new();
    let window = kit.front("App", "app.exe", 7);
    let first = Caller::new(vec![(100, "first.exe".into())], false);
    let second = Caller::new(vec![(200, "second.exe".into())], false);
    let args = json!({"session_id":"shared"});
    {
        let _session = SessionGuard::enter(&kit.computer, &args, &first)
            .unwrap()
            .unwrap();
        kit.computer.mem().target = Some(window);
        kit.computer.window_refs().list(true).unwrap();
        assert!(kit.computer.window_refs().ref_of(window).is_some());
    }
    {
        let _session = SessionGuard::enter(&kit.computer, &args, &second)
            .unwrap()
            .unwrap();
        assert_eq!(kit.computer.mem().target, None);
        assert_eq!(kit.computer.window_refs().ref_of(window), None);
    }
    {
        let _session = SessionGuard::enter(&kit.computer, &args, &first)
            .unwrap()
            .unwrap();
        assert_eq!(kit.computer.mem().target, Some(window));
        assert!(kit.computer.window_refs().ref_of(window).is_some());
    }
    assert_eq!(
        kit.computer.mem().target,
        None,
        "legacy session memory restored"
    );
}

#[test]
fn evicted_session_never_reuses_snapshot_identifiers() {
    let kit = Kit::new();
    kit.front("App", "app.exe", 7);
    let shot = json!({"session_id":"original", "action":"screenshot"});
    let old = observe(&kit, &shot);
    // Exceed bounded session capacity without invoking the daemon's independent rate limit.
    for index in 0..20 {
        kit.clock.sleep(Duration::from_millis(1));
        let args = json!({"session_id":format!("other{index}")});
        drop(SessionGuard::enter(&kit.computer, &args, &kit.who).unwrap());
    }
    let new = observe(&kit, &shot);
    assert_ne!(old, new);
    let args = json!({"session_id":"original", "snapshot_id":old});
    let mut session = SessionGuard::enter(&kit.computer, &args, &kit.who)
        .unwrap()
        .unwrap();
    assert!(session.validate(&args).is_err());
}

#[test]
fn successful_action_also_consumes_snapshot() {
    let kit = Kit::new();
    kit.front("App", "app.exe", 7);
    let shot = json!({"session_id":"a", "action":"screenshot"});
    let snapshot = observe(&kit, &shot);
    let args = json!({"session_id":"a", "snapshot_id":snapshot, "action":"clipboard_set", "text":"new", "shot":false});
    let out = kit.computer.execute(&args, &kit.who);
    assert!(out.ok, "{:?}", out.error);
    let mut session = SessionGuard::enter(&kit.computer, &args, &kit.who)
        .unwrap()
        .unwrap();
    assert!(session.validate(&args).is_err());
}

#[test]
fn snapshot_accepts_matching_geometry_once_and_refuses_scope_change() {
    let kit = Kit::new();
    kit.front("App", "app.exe", 7);
    let shot =
        json!({"session_id":"a", "action":"screenshot", "coords":"norm1000", "shot_long":128});
    let snapshot = observe(&kit, &shot);
    let args =
        json!({"session_id":"a", "snapshot_id":snapshot, "coords":"norm1000", "shot_long":128});
    {
        let mut session = SessionGuard::enter(&kit.computer, &args, &kit.who)
            .unwrap()
            .unwrap();
        assert!(session.validate(&args).is_ok());
        assert!(session.validate(&args).is_err());
    }
    let snapshot = observe(&kit, &shot);
    let args = json!({"session_id":"a", "snapshot_id":snapshot, "coords":"norm1000", "shot_long":128, "scope":"App"});
    let mut session = SessionGuard::enter(&kit.computer, &args, &kit.who)
        .unwrap()
        .unwrap();
    assert!(session.validate(&args).is_err());
}

#[test]
fn another_session_or_unnamed_mutation_invalidates_old_window_observation() {
    let kit = Kit::new();
    kit.front("App", "app.exe", 7);
    let shot = json!({"session_id":"observer", "action":"screenshot"});
    for session in [Some("other"), None] {
        let snapshot = observe(&kit, &shot);
        let mut mutation = json!({"action":"clipboard_set", "text":"new", "shot":false});
        if let Some(session) = session {
            mutation["session_id"] = json!(session);
        }
        let out = kit.computer.execute(&mutation, &kit.who);
        assert!(out.ok, "{:?}", out.error);
        let args = json!({"session_id":"observer", "snapshot_id":snapshot});
        let mut observer = SessionGuard::enter(&kit.computer, &args, &kit.who)
            .unwrap()
            .unwrap();
        assert!(
            observer.validate(&args).is_err(),
            "mutations must invalidate other saved frames"
        );
    }
}
