//! Ported from `test_20261008_speed_fixes.py` (W7), `test_20261008b_cli_shim.py` (the shim half) and
//! `test_shim_results.py`, plus the byte comparison with the Python server's own `list_tools`.

use serde_json::{Value, json};

use super::*;
use crate::actions::ACTIONS_SORTED;

/// `list_tools` of the Python shim, dumped on 2026-10-08 (see `tests/parity/`).
const PYTHON_LIST_TOOLS: &str = include_str!("../../../../tests/parity/python_list_tools.json");

fn python_tool() -> Value {
    let all: Value = serde_json::from_str(PYTHON_LIST_TOOLS).expect("fixture parses");
    all.as_array().expect("an array")[0].clone()
}

fn defaults() -> ShimDefaults {
    ShimDefaults {
        coords: "pixels".into(),
        only_apps: Vec::new(),
    }
}

fn payload(v: Value) -> Map<String, Value> {
    let args = v.as_object().cloned().expect("an object");
    match build_payload(&args, &defaults()).expect("builds") {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

// ---------------------------------------------------------------------------------- definition

#[test]
fn the_description_is_cross_platform_and_smaller_than_the_legacy_description() {
    let tool = python_tool();
    assert!(TOOL_DESCRIPTION.contains("Windows/macOS"));
    assert!(
        TOOL_DESCRIPTION.chars().count() < tool["description"].as_str().unwrap().chars().count()
    );
}

#[test]
fn the_schema_preserves_legacy_property_types_and_adds_bounded_options() {
    let tool = python_tool();
    let ours = Value::Object(input_schema());
    let old = tool["inputSchema"]["properties"].as_object().unwrap();
    for (key, property) in old {
        assert_eq!(ours["properties"][key]["type"], property["type"], "{key}");
        if let Some(items) = property.get("items") {
            assert_eq!(&ours["properties"][key]["items"], items);
        }
    }
    assert_eq!(ours["properties"]["actions"]["maxItems"], 50);
    assert_eq!(ours["properties"]["compact"]["type"], "boolean");
}

#[test]
fn the_python_dump_has_exactly_one_tool_with_the_expected_name() {
    let all: Value = serde_json::from_str(PYTHON_LIST_TOOLS).unwrap();
    assert_eq!(all.as_array().unwrap().len(), 1);
    assert_eq!(all[0]["name"], TOOL_NAME);
}

#[test]
fn the_definition_stays_within_its_budget() {
    let size = TOOL_DESCRIPTION.chars().count()
        + serde_json::to_string(&Value::Object(input_schema()))
            .unwrap()
            .chars()
            .count();
    assert!((2732..=3900).contains(&size), "{size}");
    assert!(size < 4176, "smaller than the original baseline");
}

#[test]
fn the_schema_has_no_titles_and_every_old_parameter_plus_the_new_ones() {
    let schema = Value::Object(input_schema());
    assert!(!schema.to_string().contains("\"title\""));
    let props = schema["properties"].as_object().unwrap();
    for name in [
        "action",
        "coordinate",
        "start_coordinate",
        "text",
        "scroll_direction",
        "scroll_amount",
        "duration",
        "repeat",
        "region",
        "actions",
        "display",
        "only_apps",
        "allow_irreversible",
        "button",
        "double",
        "nth",
        "old",
        "new",
        "timeout",
        "expect",
        "filter",
        "limit",
        "allow_destructive",
        "shot",
        "shot_long",
        "wake",
    ] {
        assert!(props.contains_key(name), "{name}");
    }
    assert_eq!(props["shot"], json!({"type": "boolean"}));
    assert_eq!(props["shot_long"], json!({"type": "integer"}));
}

#[test]
fn the_shortcut_and_recipe_parameters_have_the_right_types() {
    let schema = Value::Object(input_schema());
    let props = &schema["properties"];
    assert_eq!(props["window"], json!({"type": "string"}));
    assert_eq!(props["op"], json!({"type": "string"}));
    assert_eq!(
        props["rect"],
        json!({"items": {"type": "integer"}, "type": "array"})
    );
    assert_eq!(
        props["at"],
        json!({"items": {"type": "number"}, "type": "array"})
    );
    for name in ["path", "view", "sort", "name", "save_to"] {
        assert_eq!(props[name], json!({"type": "string"}), "{name}");
    }
    assert_eq!(props["list"], json!({"type": "integer"}));
    assert_eq!(props["overwrite"], json!({"type": "boolean"}));
    assert_eq!(props["params"], json!({"type": "object"}));
    assert_eq!(
        props["steps"],
        json!({"items": {"type": "object"}, "type": "array"})
    );
}

#[test]
fn the_description_names_every_action_and_the_advice() {
    for action in ACTIONS_SORTED {
        assert!(TOOL_DESCRIPTION.contains(action), "{action}");
    }
    for action in ["window", "folder", "recipe_list", "recipe_save"] {
        assert!(TOOL_DESCRIPTION.contains(action), "{action}");
    }
    for needle in [
        "shot=false",
        "shot_long",
        "wake=true",
        "actions=[",
        "expect",
        "Prefer recipe/window/folder to clicking",
        "recipe_save it with params, not literals",
        "on_fail",
        "when {text|gone_text|title}",
        "at=[fx, fy]",
        "save_to",
        "list=n",
        "rect [x,y,w,h]",
    ] {
        assert!(TOOL_DESCRIPTION.contains(needle), "{needle}");
    }
    assert!(!TOOL_DESCRIPTION.contains("Always refused"));
}

#[test]
fn no_em_dashes_in_the_tool_definition() {
    assert!(!TOOL_DESCRIPTION.contains('\u{2014}'));
    assert!(!INPUT_SCHEMA_JSON.contains('\u{2014}'));
}

// ---------------------------------------------------------------------------------- the payload

#[test]
fn a_bare_call_is_a_screenshot_with_the_fixed_fields_only() {
    let p = payload(json!({}));
    assert_eq!(
        Value::Object(p),
        json!({
            "action": "screenshot", "coordinate": null, "start_coordinate": null, "text": "",
            "scroll_direction": "down", "scroll_amount": 3, "duration": 1.0, "repeat": 1,
            "region": null, "allow_irreversible": false, "coords": "pixels", "only_apps": []
        })
    );
}

#[test]
fn unset_extras_are_not_sent_so_old_payloads_are_unchanged() {
    let p = payload(json!({"action": "left_click", "coordinate": [5, 6]}));
    for key in [
        "button",
        "double",
        "nth",
        "old",
        "new",
        "expect",
        "filter",
        "limit",
        "wake",
        "timeout",
        "window",
        "op",
        "rect",
        "at",
        "path",
        "view",
        "sort",
        "list",
        "name",
        "params",
        "steps",
        "save_to",
        "overwrite",
        "shot",
        "shot_long",
        "display",
        "scope",
        "allow_destructive",
    ] {
        assert!(!p.contains_key(key), "{key}");
    }
    assert_eq!(p["coordinate"], json!([5, 6]));
}

#[test]
fn set_extras_are_sent() {
    let p = payload(json!({
        "action": "read", "text": "Notepad", "filter": "save", "limit": 5, "button": "right",
        "double": true, "nth": 2, "old": "a", "new": "", "expect": {"text": "x"}, "wake": true,
        "display": 2, "shot": false, "shot_long": 800, "allow_destructive": true
    }));
    assert_eq!(p["filter"], "save");
    assert_eq!(p["limit"], 5);
    assert_eq!(p["text"], "Notepad");
    assert_eq!(p["button"], "right");
    assert_eq!(p["double"], true);
    assert_eq!(p["nth"], 2);
    assert_eq!(p["old"], "a");
    assert_eq!(
        p["new"], "",
        "new is sent whenever it was given, even empty"
    );
    assert_eq!(p["expect"], json!({"text": "x"}));
    assert_eq!(p["wake"], true);
    assert_eq!(p["display"], 2);
    assert_eq!(p["shot"], false);
    assert_eq!(p["shot_long"], 800);
    assert_eq!(p["allow_destructive"], true);
}

#[test]
fn the_timeout_only_rides_with_the_two_wait_actions() {
    let p = payload(json!({"action": "wait_for_text", "text": "Done"}));
    assert_eq!(p["timeout"], json!(10.0));
    let p = payload(json!({"action": "wait_for_file", "text": "C:/a", "timeout": 3}));
    assert_eq!(p["timeout"], json!(3.0));
    let p = payload(json!({"action": "left_click", "timeout": 3}));
    assert!(!p.contains_key("timeout"));
}

#[test]
fn the_shortcut_parameters_reach_the_daemon() {
    let p = payload(
        json!({"action": "window", "op": "move", "window": "w4", "rect": [0, 0, 800, 600]}),
    );
    assert_eq!(
        (p["op"].as_str(), p["window"].as_str()),
        (Some("move"), Some("w4"))
    );
    assert_eq!(p["rect"], json!([0, 0, 800, 600]));
    let p = payload(json!({
        "action": "folder", "path": "C:\\x", "view": "details", "sort": "date_modified desc", "list": 3
    }));
    assert_eq!(p["path"], "C:\\x");
    assert_eq!(p["view"], "details");
    assert_eq!(p["sort"], "date_modified desc");
    assert_eq!(p["list"], 3);
    let p = payload(json!({"action": "left_click", "at": [0.5, 0.25], "window": "Game"}));
    assert_eq!(p["at"], json!([0.5, 0.25]));
    assert_eq!(p["window"], "Game");
    let p = payload(
        json!({"action": "screenshot", "save_to": "C:\\t\\a.png", "window": "front", "shot": false}),
    );
    assert_eq!(p["save_to"], "C:\\t\\a.png");
    assert_eq!(p["shot"], false);
}

#[test]
fn the_recipe_parameters_reach_the_daemon() {
    let p =
        payload(json!({"action": "recipe", "name": "save_as", "params": {"path": "C:\\a.txt"}}));
    assert_eq!(p["name"], "save_as");
    assert_eq!(p["params"], json!({"path": "C:\\a.txt"}));
    let p = payload(json!({
        "action": "recipe_save", "name": "x", "text": "d", "params": {"p": {"required": true}},
        "steps": [{"action": "type", "text": "{p}"}], "overwrite": true
    }));
    assert_eq!(p["steps"], json!([{"action": "type", "text": "{p}"}]));
    assert_eq!(p["overwrite"], true);
    assert_eq!(p["text"], "d");
    // an empty params object is still "set" (Python sends anything that is not None)
    let p = payload(json!({"action": "recipe_save", "params": {}}));
    assert_eq!(p["params"], json!({}));
}

#[test]
fn the_excel_parameters_reach_the_daemon() {
    let p = payload(json!({
        "action": "excel", "op": "write", "workbook": "x1", "sheet": "Data", "range": "B2",
        "values": [[1, "=A1", null, true]], "formulas": true, "path": "C:/t/a.xlsx",
        "overwrite": true, "name": "Extra"
    }));
    assert_eq!(p["op"], "write");
    assert_eq!(p["workbook"], "x1");
    assert_eq!(p["sheet"], "Data");
    assert_eq!(p["range"], "B2");
    assert_eq!(p["values"], json!([[1, "=A1", null, true]]));
    assert_eq!(p["formulas"], true);
    assert_eq!(p["overwrite"], true);
    let bare = payload(json!({"action": "excel", "op": "new"}));
    for key in ["workbook", "sheet", "range", "values", "formulas"] {
        assert!(!bare.contains_key(key), "{key}");
    }
    let bad = build_payload(
        json!({"action": "excel", "values": "x"})
            .as_object()
            .unwrap(),
        &defaults(),
    );
    assert!(bad.is_err());
}

#[test]
fn a_batch_replaces_the_payload_and_keeps_the_top_level_options() {
    let p = payload(json!({
        "action": "ignored", "text": "ignored too",
        "actions": [{"action": "open_app", "text": "notepad"}],
        "shot": false, "shot_long": 640, "display": 2, "only_apps": ["notepad.exe"],
        "allow_destructive": true
    }));
    assert_eq!(
        p["actions"],
        json!([{"action": "open_app", "text": "notepad"}])
    );
    assert!(!p.contains_key("action") && !p.contains_key("text"));
    assert_eq!(p["shot"], false);
    assert_eq!(p["shot_long"], 640);
    assert_eq!(p["display"], 2);
    assert_eq!(p["only_apps"], json!(["notepad.exe"]));
    assert_eq!(p["allow_destructive"], true);
    assert_eq!(p["coords"], "pixels");
}

// Ported from test_20261008d_scope_shim.py::test_scope_rides_at_the_top_level_so_it_covers_a_batch
// and test_scope_is_not_sent_when_not_set.
#[test]
fn scope_rides_at_the_top_level_so_it_covers_a_batch() {
    let game = "Slay the Spire 2";
    let p = payload(json!({"action": "screenshot", "scope": game}));
    assert_eq!(p["scope"], game);
    let p = payload(json!({"scope": game, "actions": [
        {"action": "left_click", "coordinate": [1, 2]}, {"action": "screenshot"}]}));
    assert_eq!(p["scope"], game);
    assert_eq!(p["actions"].as_array().map(Vec::len), Some(2));
    assert!(!p["actions"][0].as_object().unwrap().contains_key("scope"));
}

#[test]
fn scope_is_not_sent_when_not_set() {
    assert!(!payload(json!({"action": "screenshot"})).contains_key("scope"));
    assert!(!payload(json!({"action": "screenshot", "scope": ""})).contains_key("scope"));
}

#[test]
fn an_empty_actions_list_is_not_a_batch() {
    let p = payload(json!({"action": "list_windows", "actions": []}));
    assert_eq!(p["action"], "list_windows");
    assert!(!p.contains_key("actions"));
}

#[test]
fn the_default_allowlist_fills_in_only_when_the_caller_sent_none() {
    let d = ShimDefaults {
        coords: "norm1000".into(),
        only_apps: vec!["calc.exe".into()],
    };
    let build = |v: Value| match build_payload(v.as_object().unwrap(), &d).unwrap() {
        Value::Object(m) => m,
        _ => unreachable!(),
    };
    let p = build(json!({"action": "screenshot"}));
    assert_eq!(p["only_apps"], json!(["calc.exe"]));
    assert_eq!(p["coords"], "norm1000");
    let p = build(json!({"only_apps": []}));
    assert_eq!(
        p["only_apps"],
        json!([]),
        "an explicit empty list means no allowlist"
    );
    let p = build(json!({"only_apps": ["notepad.exe"]}));
    assert_eq!(p["only_apps"], json!(["notepad.exe"]));
    let p = build(json!({"only_apps": null}));
    assert_eq!(p["only_apps"], json!(["calc.exe"]));
}

#[test]
fn null_for_an_optional_parameter_is_accepted() {
    let p = payload(
        json!({"action": "screenshot", "coordinate": null, "only_apps": null, "shot_long": null}),
    );
    assert_eq!(p["action"], "screenshot");
    assert!(!p.contains_key("shot_long"));
}

#[test]
fn the_wrong_type_is_a_clear_error() {
    let bad = |v: Value| {
        build_payload(v.as_object().unwrap(), &defaults())
            .unwrap_err()
            .0
    };
    assert!(bad(json!({"action": 5})).contains("action"));
    assert!(bad(json!({"coordinate": "1,2"})).contains("coordinate"));
    assert!(bad(json!({"coordinate": [1, "x"]})).contains("integers"));
    assert!(bad(json!({"actions": [1]})).contains("objects"));
    assert!(bad(json!({"shot": "maybe"})).contains("boolean"));
    assert!(bad(json!({"limit": 1.5})).contains("integer"));
    assert!(bad(json!({"expect": []})).contains("object"));
}

#[test]
fn numbers_and_flags_are_accepted_loosely_like_pydantic_does() {
    let p = payload(
        json!({"coordinate": [640.0, "360"], "limit": "7", "wake": "yes", "scroll_amount": 2.0}),
    );
    assert_eq!(p["coordinate"], json!([640, 360]));
    assert_eq!(p["limit"], 7);
    assert_eq!(p["wake"], true);
    assert_eq!(p["scroll_amount"], 2);
}

// ---------------------------------------------------------------------------------- the reply

fn reply(code: u16, data: Value) -> Reply {
    reply_from_daemon("computer", code, data.to_string().as_bytes())
}

const PNG: &str = "ZmFrZS1wbmctcHJlc2VydmVk";

#[test]
fn error_flags_follow_the_python_rules() {
    for (data, code) in [
        (json!({"ok": false, "error": "button missing"}), 200),
        (json!({"result": "error: button missing"}), 200),
        (json!({"error": "unauthorized"}), 401),
    ] {
        let r = reply(code, data);
        assert!(r.is_error);
        assert!(r.blocks.iter().any(|b| matches!(b, Block::Text(_))));
    }
}

#[test]
fn a_partial_failure_keeps_text_and_images_then_the_error() {
    let r = reply(
        200,
        json!({"ok": false, "error": "third action failed", "content": [
            {"type": "text", "text": "first action sent"},
            {"type": "image", "data": PNG, "mimeType": "image/png"}]}),
    );
    assert!(r.is_error);
    assert_eq!(r.blocks[0], Block::Text("first action sent".into()));
    assert_eq!(
        r.blocks[1],
        Block::Image {
            data: PNG.into(),
            mime: "image/png".into()
        }
    );
    assert_eq!(
        r.blocks[2],
        Block::Text("error: third action failed".into())
    );
}

#[test]
fn native_content_passes_through() {
    let r = reply(
        200,
        json!({"ok": true, "content": [
            {"type": "text", "text": "screenshot"},
            {"type": "image", "data": PNG, "mimeType": "image/jpeg"}]}),
    );
    assert!(!r.is_error);
    assert_eq!(
        r.blocks[1],
        Block::Image {
            data: PNG.into(),
            mime: "image/jpeg".into()
        }
    );
}

#[test]
fn a_result_without_content_becomes_one_text_block() {
    let r = reply(200, json!({"ok": true, "result": "clicked"}));
    assert_eq!(r.blocks, vec![Block::Text("clicked".into())]);
    assert!(!r.is_error);
}

#[test]
fn a_failed_step_is_an_error_even_though_the_call_was_ok() {
    // the daemon answers ok:true with failed_step/error beside the text; the shim flags it
    let text = "step 2 (key) failed: no window";
    let r = reply(
        200,
        json!({"ok": true, "result": text, "failed_step": 2, "error": "no window",
               "content": [{"type": "text", "text": text}]}),
    );
    assert!(r.is_error);
    assert_eq!(
        r.blocks,
        vec![Block::Text(text.into())],
        "the error text is already there"
    );
}

#[test]
fn an_empty_answer_is_ok_or_an_http_error() {
    assert_eq!(
        reply_from_daemon("computer", 200, b""),
        Reply {
            blocks: vec![Block::Text("ok".into())],
            is_error: false
        }
    );
    let r = reply_from_daemon("computer", 500, b"{}");
    assert!(r.is_error);
    assert_eq!(r.blocks, vec![Block::Text("error: HTTP 500".into())]);
}

#[test]
fn a_malformed_block_is_reported_after_the_good_ones() {
    let r = reply(
        200,
        json!({"ok": true, "content": [{"type": "text", "text": "fine"}, {"type": "audio"}]}),
    );
    assert_eq!(r.blocks[0], Block::Text("fine".into()));
    assert_eq!(
        r.blocks[1],
        Block::Text("error: malformed daemon content".into())
    );
    assert!(r.is_error, "a block that starts with error: flags the call");
}

#[test]
fn invalid_json_is_an_error_with_the_status() {
    let r = reply_from_daemon("computer", 500, b"not-json");
    assert!(r.is_error);
    let Block::Text(t) = &r.blocks[0] else {
        panic!("text expected")
    };
    assert!(t.contains("HTTP 500") && t.contains("not-json"), "{t}");
    let r = reply_from_daemon("computer", 200, b"[1, 2]");
    assert!(r.is_error, "an array is not an object");
}

#[test]
fn the_timeout_message_says_the_outcome_is_unknown() {
    let m = timeout_message("computer");
    assert!(m.contains("Outcome unknown") && m.contains("computer timed out"));
}
