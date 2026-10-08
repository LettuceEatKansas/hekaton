//! Ported from `tests/test_wh_cli.py` and the `wh` half of `tests/test_20261008b_cli_shim.py`.
//! No daemon, no desktop: the call to the daemon is a closure.

use std::io::Cursor;

use serde_json::json;
use wh_core::env::MapEnv;
use wh_core::paths::Paths;

use super::*;

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn parse(items: &[&str]) -> (String, Value) {
    let (tool, payload) = parse_args(&args(items), &mut Cursor::new(Vec::new())).expect("parses");
    (tool, Value::Object(payload))
}

fn parse_err(items: &[&str]) -> String {
    parse_args(&args(items), &mut Cursor::new(Vec::new()))
        .expect_err("is refused")
        .0
}

fn parse_stdin(items: &[&str], stdin: &[u8]) -> Result<(String, Value), Exit> {
    parse_args(&args(items), &mut Cursor::new(stdin.to_vec())).map(|(t, p)| (t, Value::Object(p)))
}

fn cfg_with(env: MapEnv, file: Option<&str>) -> Config {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("wh-cli-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let paths = Paths::in_dir(&dir);
    if let Some(text) = file {
        std::fs::write(paths.config(), text).unwrap();
    }
    Config::new(paths, env)
}

fn cfg() -> Config {
    cfg_with(MapEnv::new(), None)
}

// ------------------------------------------------------------------ (a) no value coercion on text

#[test]
fn text_like_and_unknown_keys_stay_exactly_what_was_typed() {
    for key in [
        "text",
        "title",
        "name",
        "value",
        "query",
        "path",
        "keys",
        "app",
        "target",
        "window",
        "filter",
        "old",
        "new",
        "some_future_key",
    ] {
        for raw in [
            "123",
            "007",
            "1.50",
            "yes",
            "no",
            "on",
            "off",
            "true",
            "False",
            "null",
            "[1, 2]",
            "{\"a\": 1}",
        ] {
            let (_, p) = parse(&["computer", &format!("{key}={raw}")]);
            assert_eq!(p, json!({ key: raw }), "{key}={raw}");
            assert!(p[key].is_string());
        }
    }
}

#[test]
fn typing_digits_and_yes_no_works() {
    let (_, p) = parse(&["computer", "action=type", "text=123"]);
    assert_eq!(p, json!({"action": "type", "text": "123"}));
    let (_, p) = parse(&["computer", "action=type", "text=off"]);
    assert_eq!(p["text"], "off");
}

#[test]
fn known_number_keys_become_numbers() {
    let (_, p) = parse(&[
        "computer",
        "x=10",
        "y=20",
        "amount=3",
        "max_items=40",
        "shot_long=1024",
        "display=2",
        "timeout=7.5",
        "duration=0.5",
        "limit=5",
        "nth=2",
        "repeat=3",
    ]);
    assert_eq!(
        p,
        json!({"x": 10, "y": 20, "amount": 3, "max_items": 40, "shot_long": 1024, "display": 2,
               "timeout": 7.5, "duration": 0.5, "limit": 5, "nth": 2, "repeat": 3})
    );
    assert!(p["x"].is_i64() && p["timeout"].is_f64());
}

#[test]
fn known_flags_accept_yes_no_on_off_and_binary() {
    let (_, p) = parse(&[
        "computer",
        "shot=false",
        "wake=yes",
        "enter=on",
        "clear=0",
        "double=true",
        "exact=1",
        "allow_irreversible=off",
    ]);
    assert_eq!(
        p,
        json!({"shot": false, "wake": true, "enter": true, "clear": false, "double": true,
               "exact": true, "allow_irreversible": false})
    );
}

#[test]
fn a_number_key_with_a_non_number_is_left_for_the_daemon_to_refuse() {
    let (_, p) = parse(&["computer", "limit=many", "shot=maybe"]);
    assert_eq!(p, json!({"limit": "many", "shot": "maybe"}));
}

#[test]
fn coordinate_forms() {
    for raw in ["640,360", "640 360", "[640, 360]", "640, 360"] {
        let (_, p) = parse(&["computer", &format!("coordinate={raw}")]);
        assert_eq!(p["coordinate"], json!([640, 360]), "{raw}");
    }
    let (_, p) = parse(&["computer", "region=0,0,100,50", "start_coordinate=[1,2]"]);
    assert_eq!(
        p,
        json!({"region": [0, 0, 100, 50], "start_coordinate": [1, 2]})
    );
    let (_, p) = parse(&["computer", "coordinate=somewhere"]);
    assert_eq!(p["coordinate"], "somewhere");
}

#[test]
fn json_valued_keys_and_the_allowlist() {
    let (_, p) = parse(&[
        "computer",
        "actions=[{\"action\":\"key\",\"text\":\"ctrl+s\"}]",
        "expect={\"text\":\"Saved\"}",
    ]);
    assert_eq!(
        p,
        json!({"actions": [{"action": "key", "text": "ctrl+s"}], "expect": {"text": "Saved"}})
    );
    assert_eq!(
        parse(&["computer", "only_apps=notepad.exe, calc.exe"]).1["only_apps"],
        json!(["notepad.exe", "calc.exe"])
    );
    assert_eq!(parse(&["computer", "only_apps="]).1["only_apps"], json!([]));
    assert_eq!(
        parse(&["computer", "only_apps=[\"a.exe\"]"]).1["only_apps"],
        json!(["a.exe"])
    );
}

#[test]
fn the_same_key_is_typed_per_route() {
    // replace_word's `new` is text; open's `new` is a flag. snapshot's `coords` is a flag; the
    // computer tool's is "pixels" or "norm1000".
    assert_eq!(
        parse(&["computer", "action=replace_word", "old=a", "new=1"]).1["new"],
        "1"
    );
    assert_eq!(
        parse(&["open", "app=notepad", "new=no"]).1,
        json!({"app": "notepad", "new": false})
    );
    assert_eq!(
        parse(&["snapshot", "coords=true"]).1,
        json!({"coords": true})
    );
    assert_eq!(
        parse(&["computer", "coords=norm1000"]).1,
        json!({"coords": "norm1000"})
    );
}

#[test]
fn old_behaviour_that_other_callers_rely_on_is_unchanged() {
    let (tool, p) = parse(&[
        "click",
        "target=text:Save",
        "window=w3",
        "double=true",
        "settle_ms=100",
    ]);
    assert_eq!(tool, "click");
    assert_eq!(
        p,
        json!({"target": "text:Save", "window": "w3", "double": true, "settle_ms": 100})
    );
    assert_eq!(parse_value("{\"a\": 1}"), json!({"a": 1}));
    assert_eq!(parse_value("1.5"), json!(1.5));
    assert_eq!(parse_value("null"), Value::Null);
    assert_eq!(parse_value("none"), json!("none"));
    assert!(parse_err(&["run", "print(1)"]).contains("expected key=value"));
    assert!(
        parse_err(&[]).contains("wh computer"),
        "no arguments prints the usage"
    );
}

// ------------------------------------------------------------------ (b) --json

fn batch() -> Value {
    json!({"actions": [{"action": "open_app", "text": "notepad"},
                       {"action": "type", "text": "hello \"world\" 123", "expect": {"text": "hello"}}],
           "only_apps": ["notepad.exe"]})
}

#[test]
fn json_inline_defaults_to_the_computer_tool() {
    let (tool, payload) = parse(&["--json", &batch().to_string()]);
    assert_eq!(tool, "computer");
    assert_eq!(payload, batch());
}

#[test]
fn json_with_an_explicit_tool_and_the_equals_form() {
    assert_eq!(
        parse(&["computer", "--json", "{\"action\": \"list_windows\"}"]).1,
        json!({"action": "list_windows"})
    );
    assert_eq!(
        parse(&["--json", "{\"action\": \"screenshot\"}", "computer"]).0,
        "computer"
    );
    assert_eq!(
        parse(&["--json={\"action\": \"screenshot\"}"]).1,
        json!({"action": "screenshot"})
    );
    assert_eq!(
        parse(&["windows", "--json", "{}"]),
        ("windows".to_string(), json!({}))
    );
}

#[test]
fn json_dash_reads_stdin_including_a_powershell_bom() {
    let mut raw = vec![0xEF, 0xBB, 0xBF];
    raw.extend_from_slice(batch().to_string().as_bytes());
    let (tool, p) = parse_stdin(&["computer", "--json", "-"], &raw).unwrap();
    assert_eq!((tool.as_str(), p), ("computer", batch()));
}

#[test]
fn json_dash_reads_a_multiline_payload_and_keeps_text_exactly() {
    let body = "{\n  \"actions\": [\n    {\"action\": \"type\", \"text\": \"yes\"},\n    {\"action\": \"type\", \"text\": \"123\"}\n  ]\n}\n";
    let (_, p) = parse_stdin(&["--json", "-"], body.as_bytes()).unwrap();
    let texts: Vec<&str> = p["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["yes", "123"]);
}

#[test]
fn key_value_after_json_overrides_that_key() {
    let (_, p) = parse(&[
        "computer",
        "--json",
        "{\"action\": \"type\", \"text\": \"a\", \"shot\": true}",
        "shot=false",
        "text=1",
    ]);
    assert_eq!(p, json!({"action": "type", "text": "1", "shot": false}));
}

#[test]
fn bad_json_is_a_clear_exit_not_a_traceback() {
    for argv in [
        vec!["--json"],
        vec!["computer", "--json"],
        vec!["--json", "{not json"],
        vec!["--json", "[1, 2]"],
        vec!["--json", "\"text\""],
        vec!["--json", "   "],
    ] {
        let message = parse_err(&argv);
        assert!(message.contains("--json"), "{argv:?}: {message}");
    }
}

#[test]
fn json_dash_with_nothing_on_stdin_and_non_utf8() {
    let err = parse_stdin(&["--json", "-"], b"").unwrap_err().0;
    assert!(err.contains("stdin"), "{err}");
    let err = parse_stdin(&["--json", "-"], b"{\"text\": \"caf\xe9\"}")
        .unwrap_err()
        .0;
    assert!(err.contains("UTF-8"), "{err}");
}

#[test]
fn a_bare_key_value_without_a_tool_is_refused() {
    assert!(parse_err(&["text=hi"]).contains("tool"));
}

// ------------------------------------------------------------------ shortcuts

#[test]
fn recipe_list() {
    assert_eq!(
        parse(&["recipe", "list"]),
        ("computer".to_string(), json!({"action": "recipe_list"}))
    );
}

#[test]
fn recipe_run_turns_key_values_into_params_and_keeps_them_text() {
    let (tool, p) = parse(&[
        "recipe",
        "run",
        "explorer_newest",
        "path=C:/Users/me/Downloads",
        "n=3",
    ]);
    assert_eq!(tool, "computer");
    assert_eq!(
        p,
        json!({"action": "recipe", "name": "explorer_newest",
               "params": {"path": "C:/Users/me/Downloads", "n": "3"}})
    );
    assert!(p["params"]["n"].is_string());
}

#[test]
fn recipe_params_with_equals_signs_spaces_and_lookalike_flags_stay_exactly_as_typed() {
    let (_, p) = parse(&[
        "recipe",
        "run",
        "note",
        "text=a=b yes 007",
        "wait=no",
        "list=1",
    ]);
    assert_eq!(
        p["params"],
        json!({"text": "a=b yes 007", "wait": "no", "list": "1"})
    );
}

#[test]
fn call_options_are_not_recipe_params() {
    let (_, p) = parse(&[
        "recipe",
        "run",
        "capture_page",
        "url=https://x.org",
        "file=C:/t/p.png",
        "shot=false",
        "only_apps=notepad.exe,calc.exe",
        "allow_irreversible=yes",
        "shot_long=800",
    ]);
    assert_eq!(
        p["params"],
        json!({"url": "https://x.org", "file": "C:/t/p.png"})
    );
    assert_eq!(p["shot"], false);
    assert_eq!(p["only_apps"], json!(["notepad.exe", "calc.exe"]));
    assert_eq!(p["allow_irreversible"], true);
    assert_eq!(p["shot_long"], 800);
}

// Ported from test_20261008d_scope_shim.py::test_wh_passes_scope_to_a_computer_call_and_keeps_it_out_of_recipe_params.
#[test]
fn wh_passes_scope_to_a_computer_call_and_keeps_it_out_of_recipe_params() {
    let game = "Slay the Spire 2";
    let scope = format!("scope={game}");
    let (tool, p) = parse(&["computer", "action=screenshot", &scope]);
    assert_eq!(tool, "computer");
    assert_eq!(p["scope"], game);
    let (_, p) = parse(&["recipe", "run", "capture_page", "url=https://x.org", &scope]);
    assert_eq!(p["scope"], game);
    assert_eq!(p["params"], json!({"url": "https://x.org"}));
    // a window name that looks like a number stays text
    let (_, p) = parse(&["computer", "action=screenshot", "scope=2048"]);
    assert_eq!(p["scope"], "2048");
}

#[test]
fn bad_shortcut_commands_are_a_clear_exit() {
    for (argv, why) in [
        (vec!["recipe"], "wh recipe:"),
        (vec!["recipe", "walk"], "wh recipe:"),
        (vec!["recipe", "run"], "needs a recipe name"),
        (vec!["recipe", "run", "a", "b"], "unexpected argument 'b'"),
        (vec!["window"], "needs an operation"),
        (
            vec!["window", "close", "extra"],
            "unexpected argument 'extra'",
        ),
        (vec!["folder", "a", "b"], "unexpected argument 'b'"),
    ] {
        let message = parse_err(&argv);
        assert!(message.contains(why), "{argv:?}: {message}");
    }
}

#[test]
fn window_commands() {
    assert_eq!(
        parse(&["window", "close", "window=notepad.exe", "all=true"]),
        (
            "computer".to_string(),
            json!({"action": "window", "op": "close", "window": "notepad.exe", "all": true})
        )
    );
    assert_eq!(
        parse(&["window", "move", "window=w4", "rect=0,0,960,1080"]),
        (
            "computer".to_string(),
            json!({"action": "window", "op": "move", "window": "w4", "rect": [0, 0, 960, 1080]})
        )
    );
    assert_eq!(
        parse(&["window", "focus", "window=Untitled - Notepad"]).1["window"],
        "Untitled - Notepad"
    );
    let (_, p) = parse(&["window", "restore", "window=007"]);
    assert_eq!(p["window"], "007");
    assert!(p["window"].is_string());
}

#[test]
fn folder_commands() {
    assert_eq!(
        parse(&[
            "folder",
            "path=C:/x",
            "view=details",
            "sort=date_modified desc",
            "list=3"
        ]),
        (
            "computer".to_string(),
            json!({"action": "folder", "path": "C:/x", "view": "details", "sort": "date_modified desc", "list": 3})
        )
    );
    assert_eq!(
        parse(&["folder", "C:/x", "sort=name"]).1,
        json!({"action": "folder", "path": "C:/x", "sort": "name"})
    );
}

#[test]
fn at_and_save_to_parse_through_the_computer_tool_too() {
    let (_, p) = parse(&[
        "computer",
        "action=left_click",
        "window=Game",
        "at=0.5,0.25",
    ]);
    assert_eq!(p["at"], json!([0.5, 0.25]));
    assert_eq!(p["window"], "Game");
    let (_, p) = parse(&[
        "computer",
        "action=screenshot",
        "save_to=C:/t/a.png",
        "window=front",
    ]);
    assert_eq!(
        p,
        json!({"action": "screenshot", "save_to": "C:/t/a.png", "window": "front"})
    );
    let (_, p) = parse(&[
        "computer",
        "action=recipe_save",
        "name=x",
        "overwrite=yes",
        "steps=[{\"action\": \"wait\"}]",
        "params={}",
    ]);
    assert_eq!(p["overwrite"], true);
    assert_eq!(p["steps"], json!([{"action": "wait"}]));
    assert_eq!(p["params"], json!({}));
}

#[test]
fn existing_tools_are_untouched_by_the_new_command_names() {
    assert_eq!(parse(&["windows"]), ("windows".to_string(), json!({})));
    assert_eq!(
        parse(&["computer", "action=type", "text=recipe"]).1,
        json!({"action": "type", "text": "recipe"})
    );
}

// ------------------------------------------------------------------ (d) only_apps and coords

#[test]
fn configured_defaults_are_forwarded_on_computer_calls() {
    let c = cfg_with(
        MapEnv::new(),
        Some("{\"only_apps\": \"notepad.exe, calc.exe\", \"coords\": \"NORM1000\"}"),
    );
    let mut p = Map::new();
    p.insert("action".into(), Value::from("screenshot"));
    apply_computer_defaults(&mut p, &c);
    assert_eq!(
        Value::Object(p),
        json!({"action": "screenshot", "coords": "norm1000", "only_apps": ["notepad.exe", "calc.exe"]})
    );
}

#[test]
fn env_beats_config_and_a_list_in_config_works() {
    let c = cfg_with(
        MapEnv::new(),
        Some("{\"only_apps\": [\"a.exe\", \"b.exe\"]}"),
    );
    assert_eq!(default_only_apps(&c), ["a.exe", "b.exe"]);
    let c = cfg_with(
        MapEnv::new()
            .with("WINHANDS_ONLY_APPS", "calc.exe")
            .with("WINHANDS_COORDS", "norm1000"),
        Some("{\"only_apps\": [\"a.exe\", \"b.exe\"]}"),
    );
    assert_eq!(default_only_apps(&c), ["calc.exe"]);
    assert_eq!(default_coords(&c), "norm1000");
}

#[test]
fn no_config_means_no_allowlist_and_pixel_coords() {
    let mut p = Map::new();
    apply_computer_defaults(&mut p, &cfg());
    assert_eq!(
        Value::Object(p),
        json!({"coords": "pixels", "only_apps": []})
    );
}

#[test]
fn an_explicit_only_apps_wins_even_when_empty() {
    let c = cfg_with(MapEnv::new().with("WINHANDS_ONLY_APPS", "calc.exe"), None);
    let apply = |v: Value| {
        let mut p = v.as_object().unwrap().clone();
        apply_computer_defaults(&mut p, &c);
        Value::Object(p)
    };
    assert_eq!(
        apply(json!({"only_apps": ["notepad.exe"]}))["only_apps"],
        json!(["notepad.exe"])
    );
    assert_eq!(apply(json!({"only_apps": []}))["only_apps"], json!([]));
    assert_eq!(apply(json!({"coords": "norm1000"}))["coords"], "norm1000");
}

// ------------------------------------------------------------------ what is sent and the exit code

struct Run {
    code: i32,
    out: String,
    err: String,
    sent: Vec<(String, Value)>,
}

fn run_with(argv: &[&str], env: MapEnv, reply: Result<Reply, Exit>) -> Run {
    let c = cfg_with(env, None);
    let tmp = std::env::temp_dir().join(format!(
        "wh-run-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut sent = Vec::new();
    let mut call = |tool: &str, payload: &Map<String, Value>| {
        sent.push((tool.to_string(), Value::Object(payload.clone())));
        reply.clone()
    };
    let code = run(
        &args(argv),
        &c,
        &mut Cursor::new(Vec::new()),
        &mut out,
        &mut err,
        &tmp,
        &mut call,
    );
    Run {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
        sent,
    }
}

fn json_reply(status: u16, data: Value) -> Result<Reply, Exit> {
    Ok(Reply {
        status,
        content_type: "application/json".into(),
        body: data.to_string().into_bytes(),
    })
}

#[test]
fn main_sends_the_defaults_for_computer_but_not_for_other_routes() {
    let env = MapEnv::new().with("WINHANDS_ONLY_APPS", "notepad.exe");
    let ok = || json_reply(200, json!({"ok": true, "result": "fine"}));
    let a = run_with(&["computer", "action=screenshot"], env.clone(), ok());
    let b = run_with(&["windows"], env.clone(), ok());
    let c = run_with(&["snapshot", "coords=true"], env, ok());
    assert_eq!((a.code, b.code, c.code), (0, 0, 0));
    assert_eq!(
        a.sent[0],
        (
            "computer".to_string(),
            json!({"action": "screenshot", "coords": "pixels", "only_apps": ["notepad.exe"]})
        )
    );
    assert_eq!(b.sent[0], ("windows".to_string(), json!({})));
    assert_eq!(c.sent[0], ("snapshot".to_string(), json!({"coords": true})));
    assert!(a.out.contains("fine"));
}

#[test]
fn shortcuts_go_out_as_one_computer_call_with_the_usual_defaults() {
    let r = run_with(
        &["window", "close", "window=w3"],
        MapEnv::new(),
        json_reply(200, json!({"ok": true, "result": "closed w3"})),
    );
    assert_eq!(r.code, 0);
    let (tool, payload) = &r.sent[0];
    assert_eq!(tool, "computer");
    assert_eq!(
        (payload["action"].as_str(), payload["op"].as_str()),
        (Some("window"), Some("close"))
    );
    assert_eq!(payload["coords"], "pixels");
    assert_eq!(payload["only_apps"], json!([]));
    assert_eq!(r.out.trim(), "closed w3");
}

#[test]
fn a_failed_step_is_exit_code_one_for_the_shortcuts_only() {
    let failed = || {
        json_reply(
            200,
            json!({"ok": true, "result": "step 2 (key) failed: no window", "failed_step": 2, "error": "no window"}),
        )
    };
    let r = run_with(
        &["recipe", "run", "save_as", "path=C:/a.txt"],
        MapEnv::new(),
        failed(),
    );
    assert_eq!(r.code, 1);
    assert!(r.err.contains("error: no window"), "{}", r.err);
    // the older exit code for plain computer calls is unchanged
    let r = run_with(
        &["computer", "action=key", "text=a"],
        MapEnv::new(),
        failed(),
    );
    assert_eq!(r.code, 0);
}

#[test]
fn a_successful_recipe_is_exit_code_zero_and_the_list_prints() {
    let r = run_with(
        &["recipe", "run", "note", "text=hi"],
        MapEnv::new(),
        json_reply(
            200,
            json!({"ok": true, "result": "recipe note(text): 2 steps\ndid it"}),
        ),
    );
    assert_eq!(r.code, 0);
    let r = run_with(
        &["recipe", "list"],
        MapEnv::new(),
        json_reply(
            200,
            json!({"ok": true, "result": "close_app(app): Politely close"}),
        ),
    );
    assert!(r.out.contains("close_app(app)"));
}

#[test]
fn a_refusal_is_exit_code_one_with_the_error_on_stderr() {
    let r = run_with(
        &["computer", "action=left_click"],
        MapEnv::new(),
        json_reply(
            400,
            json!({"ok": false, "error": "winhands has not been acknowledged"}),
        ),
    );
    assert_eq!(r.code, 1);
    assert!(r.err.contains("error: winhands has not been acknowledged"));
}

#[test]
fn a_batch_prints_its_step_lines() {
    let r = run_with(
        &["batch", "stop_on_error=false"],
        MapEnv::new(),
        json_reply(
            200,
            json!({"ok": true, "results": ["step 1 windows: a", "step 2 windows: b"]}),
        ),
    );
    assert_eq!(r.out, "step 1 windows: a\nstep 2 windows: b\n");
}

#[test]
fn pictures_are_written_to_files_and_their_paths_printed() {
    use base64::Engine as _;
    let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG-fake");
    let r = run_with(
        &["computer", "action=screenshot"],
        MapEnv::new(),
        json_reply(
            200,
            json!({"ok": true, "result": "screenshot", "content": [
                {"type": "text", "text": "screenshot"},
                {"type": "image", "data": png, "mimeType": "image/jpeg"}]}),
        ),
    );
    assert_eq!(r.code, 0);
    let line = r.out.lines().last().unwrap();
    assert!(line.ends_with("winhands-shot1.jpg"), "{line}");
    assert_eq!(std::fs::read(line).unwrap(), b"\x89PNG-fake");
}

#[test]
fn an_image_answer_from_the_legacy_screenshot_route_is_saved_as_png() {
    let r = run_with(
        &["screenshot"],
        MapEnv::new(),
        Ok(Reply {
            status: 200,
            content_type: "image/png".into(),
            body: b"\x89PNG".to_vec(),
        }),
    );
    assert_eq!(r.code, 0);
    assert!(
        r.out.trim().ends_with("winhands-screenshot.png"),
        "{}",
        r.out
    );
}

#[test]
fn a_non_json_answer_goes_to_stderr_with_exit_one() {
    let r = run_with(
        &["windows"],
        MapEnv::new(),
        Ok(Reply {
            status: 500,
            content_type: "text/plain".into(),
            body: b"boom".to_vec(),
        }),
    );
    assert_eq!(r.code, 1);
    assert!(r.err.contains("boom"));
}

#[test]
fn the_daemon_not_answering_is_the_message_and_exit_one() {
    let r = run_with(
        &["computer", "action=screenshot"],
        MapEnv::new(),
        Err(Exit("wh: no winhands daemon answered".into())),
    );
    assert_eq!(r.code, 1);
    assert!(r.err.contains("no winhands daemon answered"));
}

#[test]
fn help_prints_the_usage_and_parse_errors_exit_one() {
    for argv in [vec![], vec!["help"], vec!["-h"], vec!["--help"]] {
        let r = run_with(&argv, MapEnv::new(), json_reply(200, json!({})));
        assert_eq!(r.code, 0);
        assert!(r.out.contains("wh recipe list"));
        assert!(r.sent.is_empty());
    }
    let r = run_with(&["text=hi"], MapEnv::new(), json_reply(200, json!({})));
    assert_eq!(r.code, 1);
    assert!(r.err.contains("expected a tool name"));
}

#[test]
fn the_usage_has_no_em_dashes() {
    assert!(!USAGE.contains('\u{2014}'));
}
