use serde_json::{Map, Value, json};

use super::*;

fn args(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn simple() -> Simple {
    Simple::new(Profile::SimpleVision)
}

fn call(tool: &str, v: Value) -> Call {
    simple()
        .prepare(tool, &args(v))
        .unwrap_or_else(|r| panic!("{r:?}"))
}

fn text(r: &Reply) -> String {
    reply_text(r)
}

#[test]
fn profiles_list_the_documented_names_in_order() {
    assert_eq!(Profile::Full.tool_names(), ["computer"]);
    assert_eq!(
        Profile::Simple.tool_names(),
        [
            "look",
            "read_text",
            "press",
            "type",
            "key",
            "open",
            "scroll",
            "wait",
            "approve"
        ]
    );
    let vision = Profile::SimpleVision.tool_names();
    assert_eq!(vision.len(), 11);
    assert_eq!(vision[9..], ["screenshot", "click_at"]);
    assert_eq!(
        Profile::parse(" Simple-Vision "),
        Some(Profile::SimpleVision)
    );
    assert_eq!(Profile::parse("full"), Some(Profile::Full));
    assert_eq!(Profile::parse("huge"), None);
    assert!(Profile::Full.definitions().is_empty());
}

#[test]
fn every_definition_is_strict_small_and_free_of_em_dashes() {
    for d in Profile::SimpleVision.definitions() {
        let name = d["name"].as_str().unwrap();
        let desc = d["description"].as_str().unwrap();
        assert!(desc.chars().count() <= 200, "{name}: {desc}");
        let schema = &d["inputSchema"];
        assert_eq!(schema["type"], "object", "{name}");
        assert_eq!(schema["additionalProperties"], false, "{name}");
        let required = schema["required"].as_array().expect("a required list");
        let props = schema["properties"].as_object().unwrap();
        for r in required {
            assert!(props.contains_key(r.as_str().unwrap()), "{name}: {r}");
        }
        for (k, p) in props {
            let pd = p["description"].as_str().unwrap_or("");
            assert!(!pd.is_empty(), "{name}.{k} has no description");
            if k == "direction" || k == "button" {
                assert!(p["enum"].is_array(), "{name}.{k} needs an enum");
            }
        }
        assert!(!d.to_string().contains('\u{2014}'), "{name}");
    }
    let nine = serde_json::to_string(&Profile::Simple.definitions()).unwrap();
    assert!(
        nine.chars().count() < 3200,
        "{} chars",
        nine.chars().count()
    );
}

#[test]
fn look_and_read_text_translate() {
    let c = call("look", json!({"window": "Notes", "filter": "save"}));
    assert_eq!(
        Value::Object(c.args),
        json!({"action": "read", "wake": true, "shot": false, "limit": 60, "window": "Notes", "filter": "save"})
    );
    assert_eq!(c.kind, Kind::Look);
    let c = call("read_text", json!({"find": "total"}));
    assert_eq!(c.args["action"], "read_text");
    assert_eq!(c.args["text"], "total");
    assert_eq!(call("read_text", json!({})).args.get("text"), None);
}

#[test]
fn press_takes_numbers_digit_strings_and_labels() {
    for target in [json!(12), json!("12"), json!(" 12 ")] {
        let c = call("press", json!({"target": target}));
        assert_eq!(c.args["action"], "press");
        assert_eq!(c.args["text"], "12");
        assert_eq!(c.args["shot"], false);
    }
    let c = call("press", json!({"target": "Submit", "double": "true"}));
    assert_eq!(c.args["action"], "click_text");
    assert_eq!(c.args["text"], "Submit");
    assert_eq!(c.args["double"], true);
}

#[test]
fn type_builds_the_batch() {
    let c = call("type", json!({"text": "hi"}));
    assert_eq!(c.args["action"], "type");
    assert_eq!(c.args["text"], "hi");
    let c = call(
        "type",
        json!({"text": "hi", "into": 3, "clear": true, "submit": "true"}),
    );
    assert_eq!(
        c.args["actions"],
        json!([
            {"action": "press", "text": "3"},
            {"action": "key", "text": "cmd+a"},
            {"action": "type", "text": "hi"},
            {"action": "key", "text": "Return"}
        ])
    );
    let c = call("type", json!({"text": "hi", "into": "Search"}));
    assert_eq!(
        c.args["actions"][0],
        json!({"action": "click_text", "text": "Search"})
    );
}

#[test]
fn the_other_tools_translate() {
    let c = call("key", json!({"keys": "cmd+s", "repeat": "2"}));
    assert_eq!(c.args["action"], "key");
    assert_eq!(c.args["text"], "cmd+s");
    assert_eq!(c.args["repeat"], 2);
    let c = call("open", json!({"target": "https://example.com"}));
    assert_eq!(c.args["action"], "open_app");
    assert_eq!(c.args["text"], "https://example.com");
    let c = call("scroll", json!({"direction": "UP", "amount": "5"}));
    assert_eq!(c.args["action"], "scroll");
    assert_eq!(c.args["scroll_direction"], "up");
    assert_eq!(c.args["scroll_amount"], 5);
    assert_eq!(c.args["at"], json!([0.5, 0.5]));
    let c = call("wait", json!({"seconds": 2}));
    assert_eq!(c.args["action"], "wait");
    assert_eq!(c.args["duration"], 2);
    let c = call("wait", json!({"for_text": "Done", "seconds": "20"}));
    assert_eq!(c.args["action"], "wait_for_text");
    assert_eq!(c.args["text"], "Done");
    assert_eq!(c.args["new_only"], true);
    assert_eq!(c.args["timeout"], 20);
    assert_eq!(call("wait", json!({})).args["action"], "wait");
}

#[test]
fn vision_tools_translate_with_a_fixed_frame() {
    let c = call("screenshot", json!({}));
    assert_eq!(c.args["action"], "screenshot");
    assert_eq!(c.args["shot_long"], SHOT_LONG);
    for (v, action) in [
        (json!({"x": 5, "y": "6"}), "left_click"),
        (json!({"x": 5, "y": 6, "button": "right"}), "right_click"),
        (json!({"x": 5, "y": 6, "double": true}), "double_click"),
    ] {
        let c = call("click_at", v);
        assert_eq!(c.args["action"], action);
        assert_eq!(c.args["coordinate"], json!([5, 6]));
        assert_eq!(c.args["shot_long"], SHOT_LONG);
    }
}

#[test]
fn unknown_arguments_are_named_not_refused() {
    let c = call("look", json!({"filtre": "x", "zoom": 2}));
    assert_eq!(
        c.note.as_deref(),
        Some("(ignored unknown argument: filtre, zoom)")
    );
}

#[test]
fn errors_end_with_a_next_line() {
    let s = simple();
    let missing = s.prepare("press", &args(json!({}))).unwrap_err();
    let t = text(&missing);
    assert!(missing.is_error && t.contains("press needs target"), "{t}");
    assert!(
        t.lines().last().unwrap().starts_with("Next: call press {"),
        "{t}"
    );
    let unknown = s.prepare("nope", &args(json!({}))).unwrap_err();
    let t = text(&unknown);
    assert!(t.contains("look, read_text, press"), "{t}");
    assert!(t.lines().last().unwrap().starts_with("Next:"), "{t}");
    let bad = s
        .prepare("scroll", &args(json!({"direction": "sideways"})))
        .unwrap_err();
    assert!(text(&bad).lines().last().unwrap().starts_with("Next:"));
    // a stale number, as the daemon words it
    let c = call("press", json!({"target": 7}));
    let stale = Reply::error("no element 7 in the last read of that window; read it first");
    let shaped = s.finish(&c, stale).unwrap();
    let t = text(&shaped);
    assert!(
        shaped.is_error
            && t.lines()
                .last()
                .unwrap()
                .contains("call look, then press the new number"),
        "{t}"
    );
}

#[test]
fn a_simple_profile_does_not_serve_computer_or_the_vision_tools() {
    let plain = Simple::new(Profile::Simple);
    assert!(plain.prepare("computer", &args(json!({}))).is_err());
    assert!(plain.prepare("screenshot", &args(json!({}))).is_err());
}

#[test]
fn lists_are_capped_and_say_how_many_are_left() {
    let mut raw = String::from("window Notes (Notes)\n");
    for i in 1..=50 {
        raw.push_str(&format!("{i} Button \"b{i}\"\n"));
    }
    raw.push_str("(truncated, 7 more; pass filter= to narrow)\n");
    let out = shape_list(&raw, 40, 2500);
    assert!(out.starts_with("window Notes"));
    assert!(
        out.contains("40 Button") && !out.contains("41 Button"),
        "{out}"
    );
    assert!(
        out.contains("(17 more not shown; call look with filter"),
        "{out}"
    );
    let by_chars = shape_list(&raw, 60, 120);
    assert!(by_chars.chars().count() < 260, "{by_chars}");
    assert!(by_chars.contains("more not shown"));
}

#[test]
fn an_acting_reply_has_an_outcome_line_then_the_list() {
    let s = simple();
    let c = s.prepare("press", &args(json!({"target": 3}))).unwrap();
    let daemon = Reply {
        blocks: vec![
            Block::Text("pressed 3 Button 'Go' [image 1]\nscreen settled in 120 ms".into()),
            Block::Image {
                data: "x".into(),
                mime: "image/jpeg".into(),
            },
        ],
        is_error: false,
    };
    let outcome = s.finish(&c, daemon).unwrap_err();
    assert_eq!(outcome, "pressed 3 Button 'Go'");
    let list = Reply {
        blocks: vec![Block::Text("window A (B)\n1 Button \"Next\"".into())],
        is_error: false,
    };
    let r = s.with_list(&c, &outcome, &list);
    assert_eq!(
        text(&r),
        "pressed 3 Button 'Go'\nwindow A (B)\n1 Button \"Next\""
    );
    assert!(r.blocks.iter().all(|b| matches!(b, Block::Text(_))));
}

#[test]
fn approve_resends_the_refused_call_with_the_code() {
    let s = simple();
    let nothing = s
        .prepare("approve", &args(json!({"code": "K7QM"})))
        .unwrap_err();
    assert!(text(&nothing).contains("Nothing is waiting"));
    let c = s
        .prepare("press", &args(json!({"target": "Send"})))
        .unwrap();
    s.remember(
        &c,
        &Reply::error("Needs the owner's OK. Show them this in chat"),
    );
    let again = s
        .prepare("approve", &args(json!({"code": "K7QM"})))
        .unwrap();
    assert_eq!(again.args["action"], "click_text");
    assert_eq!(again.args["approval_code"], "K7QM");
    // taken: a second approve has nothing left
    assert!(
        s.prepare("approve", &args(json!({"code": "K7QM"})))
            .is_err()
    );
    // an ordinary failure is not stored
    s.remember(&c, &Reply::error("no element 1"));
    assert!(
        s.prepare("approve", &args(json!({"code": "K7QM"})))
            .is_err()
    );
}
