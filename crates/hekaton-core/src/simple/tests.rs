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

// ---- 2026-11-09: error and hint texts name only things simple mode has.

/// Texts the daemon sends for the full tool, as they reach the shaping (placeholders filled in).
const DAEMON_HINTS: &[&str] = &[
    "'Brave Browser' was started but showed no window within 8s. It is probably still loading: call wait_for('window:<expected title>', timeout=30) or windows() in a moment. Do not open it again, that makes a second copy",
    "no text on screen matches 'Save' (tried UIA then OCR); zoom or find_text first to see what is actually there",
    "sent https://example.com to the browser in front, but its window title is still 'New Tab' after 8s, so it did not navigate. If that page was already showing, nothing is wrong; otherwise screenshot to see what the browser did (a dialog, a blocked pop-up, no network) and do not assume the page loaded",
    "started https://example.com but no browser window came to the front within 8s; list_windows or screenshot shows where it went. Do not open it again",
    "opened /tmp but no Finder window came up for it within 8s; list_windows shows where it went",
    "no window matches 'Foo'; list_windows shows what is open",
    "ambiguous window 'Foo'; use a wN ref from windows",
    "2 windows match 'Foo', ambiguous: w1, w2. Use a wN ref, or all=true",
    "Cmd+W needs a window: none is in front, so pass window=...",
    "no element 99 in the last read of that window; read it first",
    "page changed since read: element 3 'Paris' is gone (element gone); read again",
    "page changed since read: element 3 was AXButton 'A' and is now AXButton 'B'; read again",
    "press needs text: an element number from read, e.g. 12",
    "wait_for_text: 'Done' did not appear within 10s",
    "wait_for_text: no new 'Done' appeared within 10s (1 already on screen when the wait began)",
    "wait_for_file: C:/x did not appear within 10s",
    "2 matches for 'Save', ambiguous: 1: 'Save'@[30, 20], 2: 'Save As'@[80, 50]. Pass nth to pick one",
    "nth=3 out of range; found 2 match(es) for 'save'",
    "coordinate [5000, 5000] is outside the 1024x768 screenshot",
    "file dialog: entered /tmp/x and pressed Enter, but a dialog is still open (maybe 'already exists, replace?' or an invalid name): look at the screenshot",
    "window \"Notes\"\n1 Button \"Save\"\n(this app exposes no clickable elements to Windows: it is drawn by a game engine or an embedded browser. Use read_text for its words, look for a picture with numbered boxes, or click by 'x,y'. For a browser page use read_page.)",
    "window \"Notes\"\n(accessibility tree looks empty: pass wake=true to wake this window's accessibility tree; it is brought to the front briefly)",
    "(no element matches 'zzz'; try a shorter word (a|b matches either), or read_page/read_text for content)",
    "zoomed view (coordinates stay in screenshot space)",
    "1 Edit \"Body\" =\"abc...\" (200 chars; read_text shows all)",
    "scrolled down 30 (capped: 100 was asked, one scroll is at most 30; scroll again for more)",
];

/// Names that exist only in the full tool.
const FULL_ONLY: &[&str] = &[
    "wait_for",
    "windows()",
    "list_windows",
    "read_page",
    "find_text",
    "click_text",
    "open_app",
    "file_dialog",
    "snapshot",
    "zoom",
    "nth",
    "shot",
    "coordinate",
    "hwnd",
    "left_click",
    "replace_word",
    "wait=",
    "wake=",
    "filter=",
];

#[test]
fn no_daemon_hint_keeps_a_name_simple_mode_does_not_have() {
    for profile in [Profile::Simple, Profile::SimpleVision] {
        let allowed = profile.tool_names();
        for hint in DAEMON_HINTS {
            // as an error, as an outcome line and inside a look
            let err = error_reply(&Reply::error(*hint), None, profile);
            let t = text(&err);
            let shaped = [
                t.clone(),
                simplify(&outcome_line(hint), profile),
                simplify(&shape_list(hint, 60, 4000), profile),
            ];
            for out in &shaped {
                let low = out.to_lowercase();
                let words: Vec<&str> = low
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .collect();
                for bad in FULL_ONLY {
                    let plain = bad.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                    let found = if plain {
                        words.contains(bad)
                    } else {
                        low.contains(bad)
                    };
                    assert!(
                        !found,
                        "{profile:?}: {bad:?} survives in {out:?} (from {hint:?})"
                    );
                }
                if profile == Profile::Simple {
                    assert!(!low.contains("screenshot"), "{out:?}");
                }
                // any snake_case name left must be one of this profile's tools
                for word in out.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
                    if word.contains('_')
                        && word.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                    {
                        assert!(allowed.contains(&word), "{profile:?}: {word:?} in {out:?}");
                    }
                }
            }
            // an error always ends with a Next: line that names a simple tool
            let last = t.lines().last().unwrap();
            assert!(last.starts_with("Next: "), "{t}");
            assert!(
                allowed.iter().any(|name| last.contains(name)),
                "{profile:?}: no tool named in {last:?}"
            );
        }
    }
}

#[test]
fn an_error_ends_with_the_next_line_that_fits_it() {
    let next = |msg: &str| {
        let t = text(&error_reply(&Reply::error(msg), None, Profile::Simple));
        t.lines().last().unwrap().to_string()
    };
    assert!(next("'x' was started but showed no window within 8s").contains("call wait"));
    assert!(next("wait_for_text: 'x' did not appear within 5s").contains("call wait"));
    assert!(
        next("refused: 'Brave' is not an app this task may open (allowed: x)")
            .contains("do not retry")
    );
    assert!(next("page changed since read: element 3 is gone").contains("press the new number"));
    // an error that already has a Next: line keeps it
    let t = text(&error_reply(
        &Reply::error("bad\nNext: call key."),
        None,
        Profile::Simple,
    ));
    assert_eq!(t.matches("Next:").count(), 1);
}

#[test]
fn an_ambiguous_label_lists_the_numbers_press_takes() {
    let simple = Simple::new(Profile::Simple);
    let c = simple
        .prepare("press", &args(json!({"target": "Save"})))
        .unwrap();
    let ambiguous = Reply::error(
        "2 matches for 'Save', ambiguous: 1: 'Save'@[30, 20], 2: 'Save As'@[80, 50]. Pass nth to pick one",
    );
    assert_eq!(
        simple.ambiguous_label(&c, &ambiguous).as_deref(),
        Some("Save")
    );
    let list = Reply {
        blocks: vec![Block::Text(
            "window \"Doc\"\n4 Button \"Save\"\n9 Link \"Save a copy\"\n12 Button \"Cancel\""
                .into(),
        )],
        is_error: false,
    };
    let r = simple.with_candidates(&c, "Save", &list).unwrap();
    let t = text(&r);
    assert!(r.is_error);
    assert!(
        t.contains("4 Button \"Save\"") && t.contains("9 Link \"Save a copy\""),
        "{t}"
    );
    assert!(
        !t.contains("Cancel") && !t.contains("nth") && !t.contains("@["),
        "{t}"
    );
    assert!(
        t.lines()
            .last()
            .unwrap()
            .starts_with("Next: call press with target set to one of those numbers"),
        "{t}"
    );
    assert!(t.contains("press 4"), "{t}");
    // type says into
    let c = simple
        .prepare("type", &args(json!({"text": "x", "into": "Save"})))
        .unwrap();
    let t = text(&simple.with_candidates(&c, "Save", &list).unwrap());
    assert!(t.contains("into 4"), "{t}");
    // nothing numbered matches (drawn text): the plain error stands
    let none = Reply {
        blocks: vec![Block::Text("window \"Doc\"".into())],
        is_error: false,
    };
    assert!(simple.with_candidates(&c, "Save", &none).is_none());
    // an ordinary error is not an ambiguity
    assert!(
        simple
            .ambiguous_label(&c, &Reply::error("no text on screen matches 'x'"))
            .is_none()
    );
}

#[test]
fn dialogs_keep_their_heading_and_their_numbers_in_a_list() {
    let text = "window \"Doc\"\nDialog: 'Save as'\n2 Button \"Save\"\n3 Button \"Cancel\"\nWindow:\n0 Button \"New\"\n1 Button \"Open\"";
    let out = shape_list(text, 60, 4000);
    assert_eq!(out, text);
    // the cap cuts the window's items, never the dialog's
    let out = shape_list(text, 2, 4000);
    assert!(
        out.contains("Dialog: 'Save as'") && out.contains("2 Button") && out.contains("3 Button"),
        "{out}"
    );
    assert!(
        !out.contains("Window:") && !out.contains("0 Button"),
        "{out}"
    );
    assert!(out.contains("(2 more not shown"), "{out}");
}
