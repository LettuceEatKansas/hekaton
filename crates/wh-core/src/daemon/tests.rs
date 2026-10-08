//! Routing, auth, the acknowledgement gate, batches, the legacy routes, idle exit and the pause flag,
//! over a fake desktop. Ported from `test_daemon_auth.py`, `test_daemon_batch_status.py`,
//! `test_shutdown_hygiene.py` (the idle half) and `test_daemon_warmup.py` (the route half).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::backend::fake::FakeBackend;
use crate::backend::{Display, Image, Rect, WindowInfo};
use crate::clock::FakeClock;
use crate::config::Config;
use crate::env::MapEnv;
use crate::paths::Paths;
use crate::testutil::TempDir;

const TOKEN: &str = "test-token-12345";

struct Rig {
    fake: Arc<FakeBackend>,
    clock: Arc<FakeClock>,
    daemon: Daemon,
    paths: Paths,
    tmp: TempDir,
}

fn textured(w: u32, h: u32) -> Image {
    let mut bytes = Vec::new();
    for i in 0..(w * h) {
        let v = ((i / 2) * 7 % 256) as u8;
        bytes.extend_from_slice(&[v, v, v, 255]);
    }
    Image::new(w, h, bytes).expect("textured image")
}

fn rig_with(env: MapEnv) -> Rig {
    let tmp = TempDir::new("daemon");
    let paths = Paths::in_dir(tmp.path().join("data"));
    let fake = Arc::new(FakeBackend::new());
    fake.set_displays(vec![Display {
        id: 1,
        name: "kit".into(),
        rect: Rect::from_xywh(0, 0, 320, 180),
        scale: 1.0,
        primary: true,
    }]);
    fake.set_screen(textured(320, 180));
    let clock = Arc::new(FakeClock::new());
    let cfg = Arc::new(Config::new(paths.clone(), env));
    let computer = Arc::new(Computer::new(fake.clone(), clock.clone(), cfg));
    Rig {
        fake,
        clock,
        daemon: Daemon::new(computer, TOKEN, 47811),
        paths,
        tmp,
    }
}

/// A machine whose owner has acknowledged winhands.
fn rig() -> Rig {
    rig_with(MapEnv::new().with("WINHANDS_ACKNOWLEDGED", "1"))
}

/// A machine nobody has acknowledged yet.
fn fresh_rig() -> Rig {
    rig_with(MapEnv::new())
}

impl Rig {
    fn send(&self, method: &str, path: &str, token: Option<&str>, body: &Value) -> Response {
        let bytes = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(body).unwrap()
        };
        self.daemon.handle(&Request {
            method,
            path,
            token,
            body: &bytes,
            peer: None,
        })
    }

    fn post(&self, path: &str, body: Value) -> Response {
        self.send("POST", path, Some(TOKEN), &body)
    }

    fn get(&self, path: &str, token: Option<&str>) -> Response {
        self.send("GET", path, token, &Value::Null)
    }

    fn window(&self, title: &str, app: &str) {
        let w = self.fake.add_window(WindowInfo::basic(
            title,
            app,
            4242,
            Rect::from_xywh(10, 10, 300, 160),
        ));
        self.fake.set_foreground(Some(w));
    }
}

fn body(r: &Response) -> Value {
    r.json_body().expect("a JSON body")
}

// ---------------------------------------------------------------------------------- auth

#[test]
fn health_with_the_wrong_token_is_401() {
    let r = rig();
    let res = r.get("/health", Some("wrong-token"));
    assert_eq!(res.status, 401);
    assert!(body(&res).get("error").is_some());
}

#[test]
fn health_with_the_right_token_is_200_and_has_the_python_fields() {
    let r = rig();
    let res = r.get("/health", Some(TOKEN));
    assert_eq!(res.status, 200);
    assert_eq!(res.content_type, "application/json");
    let data = body(&res);
    for key in [
        "elevated",
        "paused",
        "version",
        "hotkey",
        "idle_remaining_seconds",
        "idle_timeout_seconds",
        "remote_caller",
        "remote_bind",
        "caller_guard",
        "ocr",
    ] {
        assert!(data.get(key).is_some(), "{key} missing in {data}");
    }
    assert_eq!(data["paused"], false);
    assert_eq!(data["hotkey"], Value::Null);
    assert_eq!(data["caller_guard"], "caller-relative");
    assert_eq!(data["idle_timeout_seconds"], 3600);
    assert!(data["idle_remaining_seconds"].as_f64().unwrap() > 3599.0);
    assert_eq!(data["impl"], "winhands-rs");
}

#[test]
fn health_with_no_token_is_401() {
    let r = rig();
    assert_eq!(r.get("/health", None).status, 401);
    assert_eq!(r.get("/health", Some("")).status, 401);
}

#[test]
fn health_says_when_the_caller_guard_is_off() {
    let r = rig_with(
        MapEnv::new()
            .with("WINHANDS_ACKNOWLEDGED", "1")
            .with("WINHANDS_REMOTE_CALLER", "1"),
    );
    let data = body(&r.get("/health", Some(TOKEN)));
    assert_eq!(data["remote_caller"], true);
    assert_eq!(data["caller_guard"], "protect_processes only");
}

#[test]
fn health_reports_the_registered_hotkey() {
    let r = rig();
    assert!(r.daemon.install_kill_hotkey().is_some());
    let data = body(&r.get("/health", Some(TOKEN)));
    assert_eq!(data["hotkey"], "ctrl+alt+shift+f12");
}

#[test]
fn the_hotkey_falls_back_when_the_first_chord_is_taken_and_toggles_pause() {
    let r = rig();
    r.fake.block_hotkeys(&["ctrl+alt+shift+f12"]);
    assert_eq!(
        r.daemon.install_kill_hotkey().as_deref(),
        Some("ctrl+alt+shift+pause")
    );
    assert!(r.fake.fire_kill_hotkey());
    assert!(r.daemon.computer().pause_flag().is_paused());
    assert_eq!(body(&r.get("/health", Some(TOKEN)))["paused"], true);
    assert!(r.fake.fire_kill_hotkey());
    assert!(!r.daemon.computer().pause_flag().is_paused());
}

#[test]
fn no_hotkey_at_all_is_not_an_error() {
    let r = rig();
    r.fake.block_hotkeys(&[
        "ctrl+alt+shift+f12",
        "ctrl+alt+shift+pause",
        "ctrl+shift+win+k",
    ]);
    assert_eq!(r.daemon.install_kill_hotkey(), None);
    assert_eq!(body(&r.get("/health", Some(TOKEN)))["hotkey"], Value::Null);
}

#[test]
fn every_post_needs_the_token_even_unknown_routes() {
    let r = rig();
    for path in ["/computer", "/nope", "/shutdown", "/batch", "/pause"] {
        let res = r.send("POST", path, Some("wrong"), &json!({}));
        assert_eq!(res.status, 401, "{path}");
        assert_eq!(body(&res), json!({"error": "unauthorized"}));
        assert!(!res.shutdown);
        assert_eq!(r.send("POST", path, None, &json!({})).status, 401);
    }
    assert!(!r.daemon.computer().pause_flag().is_paused());
}

#[test]
fn any_other_get_is_404_and_other_methods_are_refused() {
    let r = rig();
    let res = r.get("/computer", Some(TOKEN));
    assert_eq!(res.status, 404);
    assert_eq!(body(&res), json!({"error": "not found"}));
    assert_eq!(r.get("/health?x=1", Some(TOKEN)).status, 404);
    assert_eq!(
        r.send("DELETE", "/health", Some(TOKEN), &Value::Null)
            .status,
        501
    );
}

#[test]
fn a_body_that_is_not_json_is_400() {
    let r = rig();
    let res = r.daemon.handle(&Request {
        method: "POST",
        path: "/computer",
        token: Some(TOKEN),
        body: b"{not json",
        peer: None,
    });
    assert_eq!(res.status, 400);
    assert!(
        body(&res)["error"]
            .as_str()
            .unwrap()
            .starts_with("bad json: ")
    );
}

#[test]
fn an_empty_body_is_an_empty_object_and_a_non_object_is_refused() {
    let r = rig();
    // empty body: a computer call with no action, which the core answers (unknown action)
    let res = r.daemon.handle(&Request {
        method: "POST",
        path: "/computer",
        token: Some(TOKEN),
        body: b"",
        peer: None,
    });
    assert_eq!(res.status, 200, "{:?}", body(&res));
    let res = r.post("/computer", json!([1, 2]));
    assert_eq!(res.status, 400);
    assert_eq!(body(&res)["error"], "tool args must be an object");
}

// ---------------------------------------------------------------------------------- /computer

#[test]
fn a_screenshot_comes_back_with_text_and_an_image() {
    let r = rig();
    let res = r.post("/computer", json!({"action": "screenshot"}));
    assert_eq!(res.status, 200);
    let data = body(&res);
    assert_eq!(data["ok"], true);
    let content = data["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image");
    assert!(
        content[1]["mimeType"]
            .as_str()
            .unwrap()
            .starts_with("image/")
    );
    assert_eq!(data["result"], content[0]["text"]);
}

#[test]
fn a_failed_step_is_ok_200_with_the_failure_fields_beside_the_text() {
    let r = rig();
    r.window("Notepad", "notepad.exe");
    let res = r.post(
        "/computer",
        json!({"actions": [{"action": "key", "text": "definitely not a key name"}]}),
    );
    assert_eq!(
        res.status, 200,
        "unchanged: still 200/ok at the transport level"
    );
    let data = body(&res);
    assert_eq!(data["ok"], true);
    assert_eq!(data["failed_step"], 1);
    assert!(
        data["result"]
            .as_str()
            .unwrap()
            .contains("step 1 (key) failed:")
    );
    assert!(!data["error"].as_str().unwrap().is_empty());
}

#[test]
fn a_successful_call_has_no_failure_fields() {
    let r = rig();
    let data = body(&r.post("/computer", json!({"action": "list_windows"})));
    assert_eq!(data["ok"], true);
    assert!(data.get("failed_step").is_none() && data.get("error").is_none());
}

#[test]
fn until_the_owner_acknowledges_only_reads_run() {
    let r = fresh_rig();
    assert_eq!(
        r.post("/computer", json!({"action": "list_windows"}))
            .status,
        200
    );
    let res = r.post(
        "/computer",
        json!({"action": "left_click", "coordinate": [1, 2]}),
    );
    assert_eq!(res.status, 400);
    let data = body(&res);
    assert_eq!(data["ok"], false);
    let error = data["error"].as_str().unwrap();
    assert!(
        error.contains("winhands acknowledge") && error.contains("left_click"),
        "{error}"
    );
    assert!(r.fake.events().is_empty(), "nothing was sent");
}

#[test]
fn a_paused_daemon_refuses_the_computer_route() {
    let r = rig();
    assert_eq!(
        body(&r.post("/pause", json!({}))),
        json!({"ok": true, "result": "paused"})
    );
    let res = r.post("/computer", json!({"action": "list_windows"}));
    assert_eq!(res.status, 400);
    assert_eq!(body(&res)["error"], "paused by user hotkey");
    assert_eq!(
        body(&r.post("/resume", json!({}))),
        json!({"ok": true, "result": "resumed"})
    );
    assert_eq!(
        r.post("/computer", json!({"action": "list_windows"}))
            .status,
        200
    );
}

#[test]
fn the_audit_log_gets_an_entry_for_a_computer_call() {
    let r = rig();
    r.post("/computer", json!({"action": "list_windows"}));
    let log = std::fs::read_to_string(r.paths.audit_log()).unwrap();
    assert!(
        log.contains("\"action\": \"computer\"") && log.contains("list_windows"),
        "{log}"
    );
}

// The R1c actions (shortcuts and recipes) used to answer "unknown action" through the daemon while the
// core already knew them: they are routed like every other computer action.
#[test]
fn the_shortcut_and_recipe_actions_reach_the_core_through_the_computer_route() {
    let r = rig();
    r.window("Notepad", "notepad.exe");
    let list = body(&r.post("/computer", json!({"action": "recipe_list"})));
    assert_eq!(list["ok"], true, "{list}");
    let text = list["result"].as_str().unwrap();
    for bundled in ["open_url", "save_as", "note"] {
        assert!(text.contains(bundled), "{bundled} missing from {text}");
    }
    let win = body(&r.post(
        "/computer",
        json!({"action": "window", "op": "minimize", "window": "notepad.exe", "shot": false}),
    ));
    assert_eq!(win["ok"], true, "{win}");
    assert!(!win.to_string().contains("unknown action"), "{win}");
    assert_eq!(r.fake.window_ops().len(), 1);
    // folder without a path is the core's own complaint, not "unknown action"
    let folder = body(&r.post("/computer", json!({"action": "folder"})));
    assert!(!folder.to_string().contains("unknown action"), "{folder}");
    // recipe_save writes into the owner's recipes folder and recipe_list then shows it
    let save = body(&r.post(
        "/computer",
        json!({"action": "recipe_save", "name": "demo_recipe", "text": "a demo",
               "steps": [{"action": "key", "text": "ctrl+s"}], "shot": false}),
    ));
    assert_eq!(save["ok"], true, "{save}");
    assert!(r.paths.recipes_dir().join("demo_recipe.json").exists());
    let list = body(&r.post("/computer", json!({"action": "recipe_list"})));
    assert!(list["result"].as_str().unwrap().contains("demo_recipe"));
}

// ---------------------------------------------------------------------------------- /batch

#[test]
fn batch_must_carry_a_list_and_at_most_fifty_members() {
    let r = rig();
    let res = r.post("/batch", json!({"actions": "x"}));
    assert_eq!(res.status, 400);
    assert_eq!(
        body(&res),
        json!({"ok": false, "error": "actions must be a list"})
    );
    assert_eq!(r.post("/batch", json!([])).status, 400);
    let many: Vec<Value> = (0..51).map(|_| json!({"tool": "windows"})).collect();
    let res = r.post("/batch", json!({"actions": many}));
    assert_eq!(res.status, 400);
    assert_eq!(
        body(&res)["error"],
        "batch limit is 50 actions; no actions sent"
    );
}

#[test]
fn batch_runs_members_and_numbers_the_lines() {
    let r = rig();
    r.window("Notepad", "notepad.exe");
    let res = r.post(
        "/batch",
        json!({"actions": [
            {"tool": "windows"},
            {"tool": "computer", "args": {"action": "cursor_position"}},
        ]}),
    );
    assert_eq!(res.status, 200);
    let data = body(&res);
    assert_eq!(data["ok"], true);
    let lines = data["results"].as_array().unwrap();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].as_str().unwrap().starts_with("step 1 windows: "));
    assert!(lines[1].as_str().unwrap().starts_with("step 2 computer: "));
    let content = data["content"].as_array().unwrap();
    assert_eq!(content[0]["text"], lines[0]);
    assert!(data.get("error").is_none());
}

#[test]
fn batch_stops_at_the_first_failure_unless_told_not_to() {
    let r = rig();
    let members = json!([{"tool": "nope"}, {"tool": "windows"}]);
    let res = r.post("/batch", json!({"actions": members}));
    assert_eq!(res.status, 400);
    let data = body(&res);
    assert_eq!(data["ok"], false);
    assert_eq!(data["results"].as_array().unwrap().len(), 1);
    assert_eq!(
        data["results"][0],
        "step 1 nope: error: unknown action: nope"
    );
    assert_eq!(
        data["error"],
        "batch partially executed or failed; inspect step results before retrying"
    );
    let res = r.post(
        "/batch",
        json!({"actions": members, "stop_on_error": false}),
    );
    let data = body(&res);
    assert_eq!(data["results"].as_array().unwrap().len(), 2);
    assert_eq!(res.status, 400, "still a failed batch");
}

#[test]
fn batch_refuses_nesting_and_non_objects() {
    let r = rig();
    let res = r.post(
        "/batch",
        json!({"actions": [{"tool": "batch", "args": {}}]}),
    );
    assert_eq!(
        body(&res)["results"][0],
        "step 1 invalid: error: batch members must be tools; nested batch not allowed"
    );
    let res = r.post("/batch", json!({"actions": ["windows"]}));
    assert_eq!(res.status, 400);
    assert!(
        body(&res)["results"][0]
            .as_str()
            .unwrap()
            .starts_with("step 1 invalid:")
    );
}

#[test]
fn batch_keeps_the_images_of_earlier_steps() {
    let r = rig();
    let res = r.post(
        "/batch",
        json!({"actions": [
            {"tool": "computer", "args": {"action": "screenshot"}},
            {"tool": "nope"},
        ]}),
    );
    let data = body(&res);
    let kinds: Vec<&str> = data["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["text", "text", "image", "text"]);
}

// ---------------------------------------------------------------------------------- panics

fn click(r: &Rig) -> Response {
    r.post(
        "/computer",
        json!({"action": "left_click", "coordinate": [50, 50], "shot": false}),
    )
}

#[test]
fn a_panic_in_the_core_is_an_error_answer_and_the_daemon_keeps_serving() {
    let r = rig();
    r.window("Notepad", "notepad.exe");
    r.fake.set_click_hook(|_, _| panic!("the backend blew up"));
    let res = click(&r);
    assert_eq!(res.status, 500);
    let data = body(&res);
    assert_eq!(data["ok"], false);
    let error = data["error"].as_str().unwrap();
    assert!(
        error.starts_with("internal error: ") && error.contains("the backend blew up"),
        "{error}"
    );
    // the panic was logged like any failed call
    let log = std::fs::read_to_string(r.paths.audit_log()).unwrap();
    assert!(log.contains("the backend blew up"), "{log}");
    // still serving: the same route, other routes, auth, and the tool lock the panic poisoned
    assert_eq!(r.get("/health", Some(TOKEN)).status, 200);
    assert_eq!(r.get("/health", Some("wrong")).status, 401);
    assert_eq!(
        r.post("/computer", json!({"action": "list_windows"}))
            .status,
        200
    );
    assert_eq!(click(&r).status, 500, "a second panic is caught too");
    r.fake.set_click_hook(|_, _| {});
    let res = click(&r);
    assert_eq!(res.status, 200, "{:?}", body(&res));
    assert_eq!(body(&res)["ok"], true);
}

#[test]
fn a_panic_inside_a_batch_is_an_error_answer_for_that_request_only() {
    let r = rig();
    r.window("Notepad", "notepad.exe");
    r.fake
        .set_click_hook(|_, _| panic!("batch member exploded"));
    let res = r.post(
        "/batch",
        json!({"actions": [
            {"tool": "computer", "args": {"action": "list_windows"}},
            {"tool": "computer", "args": {"action": "left_click", "coordinate": [50, 50], "shot": false}},
        ]}),
    );
    assert_eq!(res.status, 500);
    assert!(
        body(&res)["error"]
            .as_str()
            .unwrap()
            .contains("batch member exploded")
    );
    assert_eq!(r.get("/health", Some(TOKEN)).status, 200);
}

// ---------------------------------------------------------------------------------- shutdown, unknown

#[test]
fn shutdown_answers_then_asks_the_server_to_stop() {
    let r = rig();
    let res = r.post("/shutdown", json!({}));
    assert_eq!(res.status, 200);
    assert_eq!(body(&res), json!({"ok": true, "result": "shutting down"}));
    assert!(res.shutdown);
    assert!(
        !r.post("/computer", json!({"action": "list_windows"}))
            .shutdown
    );
}

#[test]
fn an_unknown_route_is_a_400_that_names_it_and_is_logged() {
    let r = rig();
    let res = r.post("/frobnicate", json!({}));
    assert_eq!(res.status, 400);
    assert_eq!(
        body(&res),
        json!({"ok": false, "error": "unknown action: frobnicate"})
    );
    let log = std::fs::read_to_string(r.paths.audit_log()).unwrap();
    assert!(
        log.contains("frobnicate") && log.contains("\"ok\": false"),
        "{log}"
    );
}

#[test]
fn three_identical_failures_get_a_hint() {
    let r = rig();
    let mut last = Value::Null;
    for _ in 0..3 {
        last = body(&r.post("/frobnicate", json!({})));
    }
    let error = last["error"].as_str().unwrap();
    assert!(
        error.contains("failed 3 times with these arguments"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------------- idle

#[test]
fn the_daemon_goes_idle_only_after_the_timeout_and_a_request_restarts_the_count() {
    let r = rig_with(
        MapEnv::new()
            .with("WINHANDS_ACKNOWLEDGED", "1")
            .with("WINHANDS_IDLE_MIN", "15"),
    );
    assert!(!r.daemon.idle_expired());
    r.clock.advance(Duration::from_secs(899));
    assert!(!r.daemon.idle_expired());
    r.clock.advance(Duration::from_secs(2));
    assert!(r.daemon.idle_expired());
    r.post("/computer", json!({"action": "list_windows"}));
    assert!(
        !r.daemon.idle_expired(),
        "a request arrived: the countdown starts over"
    );
}

#[test]
fn idle_minutes_zero_disables_the_exit() {
    let r = rig_with(
        MapEnv::new()
            .with("WINHANDS_ACKNOWLEDGED", "1")
            .with("WINHANDS_IDLE_MIN", "0"),
    );
    r.clock.advance(Duration::from_secs(1_000_000));
    assert!(!r.daemon.idle_expired());
    let data = body(&r.get("/health", Some(TOKEN)));
    assert_eq!(data["idle_remaining_seconds"], -1.0);
}

// ---------------------------------------------------------------------------------- warm-up gate

#[test]
fn an_ocr_call_waits_for_the_warmup_but_a_plain_call_does_not() {
    let tmp_gate = Arc::new(WarmGate::with_cap(Duration::from_millis(300)));
    let r = rig();
    let daemon =
        Daemon::new(r.daemon.computer().clone(), TOKEN, 1).with_warm_gate(tmp_gate.clone());
    tmp_gate.begin();
    let send = |v: Value| {
        let bytes = serde_json::to_vec(&v).unwrap();
        daemon.handle(&Request {
            method: "POST",
            path: "/computer",
            token: Some(TOKEN),
            body: &bytes,
            peer: None,
        })
    };
    let t = std::time::Instant::now();
    send(json!({"action": "list_windows"}));
    assert!(
        t.elapsed() < Duration::from_millis(250),
        "list_windows never waits"
    );
    let t = std::time::Instant::now();
    send(json!({"action": "find_text", "text": "x"}));
    assert!(
        t.elapsed() >= Duration::from_millis(250),
        "find_text waited for the cap"
    );
    tmp_gate.finish();
    let t = std::time::Instant::now();
    send(json!({"action": "find_text", "text": "x"}));
    assert!(t.elapsed() < Duration::from_millis(250));
}

// ---------------------------------------------------------------------------------- legacy routes

#[test]
fn the_windows_route_lists_windows_and_notes_an_unchanged_repeat() {
    let r = rig();
    r.window("Notepad", "notepad.exe");
    let first = body(&r.post("/windows", json!({})));
    assert_eq!(first["ok"], true);
    assert!(first["result"].as_str().unwrap().contains("Notepad"));
    r.clock.advance(Duration::from_secs(4));
    let again = body(&r.post("/windows", json!({})));
    let text = again["result"].as_str().unwrap();
    assert!(
        text.contains("(same result as 4s ago, nothing has changed."),
        "{text}"
    );
}

#[test]
fn the_read_routes_work_before_acknowledgement_and_while_paused_except_files() {
    let r = fresh_rig();
    r.window("Notepad", "notepad.exe");
    assert_eq!(r.post("/windows", json!({})).status, 200);
    let res = r.post("/snapshot", json!({}));
    assert_eq!(res.status, 200, "{:?}", body(&res));
    let shot = r.post("/screenshot", json!({}));
    assert_eq!((shot.status, shot.content_type), (200, "image/png"));
    let dir = r.tmp.path().to_string_lossy().to_string();
    assert_eq!(r.post("/files", json!({"path": dir})).status, 200);
    r.post("/pause", json!({}));
    assert_eq!(r.post("/windows", json!({})).status, 200);
    assert_eq!(r.post("/screenshot", json!({})).status, 200);
    let res = r.post("/files", json!({"path": dir}));
    assert_eq!(
        res.status, 400,
        "files is not a read-only route for the pause flag"
    );
    assert_eq!(body(&res)["error"], "paused by user hotkey");
}

#[test]
fn the_screenshot_route_answers_with_png_bytes() {
    let r = rig();
    let res = r.post("/screenshot", json!({}));
    assert_eq!(res.status, 200);
    assert_eq!(res.content_type, "image/png");
    assert_eq!(&res.body[..8], b"\x89PNG\r\n\x1a\n");
}

#[test]
fn the_files_route_lists_a_folder() {
    let r = rig();
    let dir = r.tmp.path().join("listing");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("alpha.txt"), "a").unwrap();
    let res = r.post(
        "/files",
        json!({"path": dir.to_string_lossy(), "sort": "name"}),
    );
    assert_eq!(res.status, 200, "{:?}", body(&res));
    assert!(body(&res)["result"].as_str().unwrap().contains("alpha.txt"));
    let res = r.post("/files", json!({}));
    assert_eq!(body(&res)["error"], "missing required argument 'path'");
}

#[test]
fn input_routes_are_refused_until_acknowledged_by_their_own_name() {
    let r = fresh_rig();
    for route in ["click", "type", "key", "scroll", "drag", "hover", "open"] {
        let res = r.post(&format!("/{route}"), json!({"target": "1,2"}));
        assert_eq!(res.status, 400, "{route}");
        let error = body(&res)["error"].as_str().unwrap().to_string();
        assert!(error.contains(&format!("the {route} action")), "{error}");
    }
    assert!(r.fake.events().is_empty());
}

#[test]
fn the_key_route_presses_the_combo() {
    let r = rig();
    r.window("Notepad", "notepad.exe");
    let res = r.post("/key", json!({"combo": "ctrl+s"}));
    assert_eq!(res.status, 200, "{:?}", body(&res));
    let data = body(&res);
    assert_eq!(data["ok"], true);
    assert!(
        data["result"].as_str().unwrap().contains("pressed ctrl+s"),
        "{data}"
    );
    assert!(!r.fake.events().is_empty());
}

#[test]
fn the_type_route_types_and_can_press_enter() {
    let r = rig();
    r.window("Notepad", "notepad.exe");
    let res = r.post("/type", json!({"text": "hi", "enter": true}));
    assert_eq!(res.status, 200, "{:?}", body(&res));
    assert!(
        body(&res)["result"]
            .as_str()
            .unwrap()
            .contains("typed 2 chars")
    );
    assert_eq!(r.fake.typed_text(), "hi");
}

#[test]
fn the_click_route_hits_the_screen_point_it_was_given() {
    let r = rig();
    r.window("Notepad", "notepad.exe");
    let res = r.post("/click", json!({"target": "100,50"}));
    assert_eq!(res.status, 200, "{:?}", body(&res));
    let clicked = r
        .fake
        .events()
        .iter()
        .any(|e| format!("{e:?}").contains("x: 100, y: 50"));
    assert!(clicked, "{:?}", r.fake.events());
}

#[test]
fn what_the_legacy_routes_cannot_do_is_refused_with_a_reason() {
    let r = rig();
    let res = r.post("/click", json!({"target": "12"}));
    assert_eq!(res.status, 400);
    assert!(
        body(&res)["error"]
            .as_str()
            .unwrap()
            .contains("computer tool")
    );
    let res = r.post("/key", json!({"combo": "a", "window": "w1"}));
    assert_eq!(res.status, 400);
    assert!(body(&res)["error"].as_str().unwrap().contains("window="));
}

#[test]
fn the_legacy_routes_share_the_rate_cap() {
    let r = rig();
    let mut refused = None;
    for i in 0..=crate::state::RATE_CAP {
        let res = r.post("/windows", json!({}));
        if res.status != 200 {
            refused = Some((i, body(&res)));
            break;
        }
    }
    let (i, data) = refused.expect("the 51st call in a second is refused");
    assert_eq!(i, crate::state::RATE_CAP);
    assert_eq!(data["error"], "rate limit exceeded");
}

#[test]
fn the_hotkey_candidates_are_the_python_chain() {
    let names: Vec<String> = hotkey_candidates().into_iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        [
            "ctrl+alt+shift+f12",
            "ctrl+alt+shift+pause",
            "ctrl+shift+win+k"
        ]
    );
    assert!(
        !names.iter().any(|n| n.ends_with("shift+k")),
        "the contested combo stays out"
    );
}
