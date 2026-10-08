//! The `computer` tool as the MCP server advertises it: name, description and JSON input schema.
//!
//! This is the text a model reads on every turn, so it is mirrored from the Python build's
//! `winhands/shim.py` (`TOOL_DESCRIPTION` and the slimmed schema FastMCP builds from the `computer`
//! function signature) exactly, as of the 2026-10-08 additions: `shot`, `shot_long`, `wake`, the
//! shortcuts (`window`, `folder`, `at`, `save_to`), the recipe actions and `scope`. Parameter names,
//! their order and their types match `asyncio.run(shim.mcp.list_tools())` character for character; there are
//! no `title` or `default` fields and no `required` list, and optional parameters are not wrapped in
//! `anyOf` with null.
//!
//! This is the ONE copy of the definition. `wh_core::mcp` re-exports the constants and `winhands mcp`
//! serves them; `mcp/tests.rs` compares them with the Python dump in `tests/parity/python_list_tools.json`
//! (regenerate that file whenever the constants are refreshed, see below), and the tests here pin the
//! text and the size budget (`test_20261008_speed_fixes.py`, `test_20261008b_cli_shim.py`).
//!
//! To refresh after the Python shim changes: `python -c "import asyncio, json; from winhands import
//! shim; t = asyncio.run(shim.mcp.list_tools())[0]; print(t.description); print(json.dumps(
//! t.inputSchema, separators=(',', ':')))"` and paste the two outputs into the constants below.

use std::sync::LazyLock;

use serde_json::{Value, json};

/// The tool's name.
pub const TOOL_NAME: &str = "computer";

/// The tool description (Python `shim.TOOL_DESCRIPTION`).
pub const TOOL_DESCRIPTION: &str = r##"Use the Windows desktop like a person. Acting calls settle, then return a screenshot; shot=false returns text only.
Pointer: left_click | right_click (lists menu items) | double_click | triple_click | middle_click | mouse_move (hover) | left_click_drag (start_coordinate -> coordinate) | scroll (scroll_direction, scroll_amount) | left_mouse_down | left_mouse_up. coordinate = [x, y] in the screenshot; text on a click = modifier held ("shift"). If you pass shot_long (screenshot long edge, px), pass the same value on every call that uses coordinates from that screenshot.
Keys: type (text) | key (text: "ctrl+s", "Return"; repeat) | hold_key (text, duration).
Look: screenshot | zoom (region [x0,y0,x1,y1]; adds UIA/OCR text) | find_text (text; lines with click points) | read (text = window title or ref, blank = front window; UI elements; filter, limit; wake=true raises an empty browser window briefly to build its tree) | read_field (text = label) | read_text (document text, numbered; text = word keeps matching lines) | list_windows | cursor_position.
Do: click_text (text; button, double, nth) | replace_word (old, new) | open_app (text = app name, path or folder) | file_dialog (text = full path) | clipboard_get | clipboard_set (text).
Wait: wait (duration) | wait_for_text (text, timeout) | wait_for_file (text = path, timeout). Files: list_files (text = folder; filter glob; limit).
Batch known sequences: actions=[{"action": ...}, ...] runs many steps in one call, one screenshot at the end. A step may carry expect {"text"|"gone_text"|"title": ...}; a batch stops at the first FAIL.
Shortcuts, no clicks: window (op close|minimize|maximize|restore|focus|move; window = wN, title part or app.exe; move rect [x,y,w,h]). folder (path, view details|list|tiles|icons, sort "date_modified desc", list=n: opens Explorer set up, returns the top n entries as text).
Recipes: recipe_list | recipe (name, params {k: v}) | recipe_save (name, text = description, params, steps using {k}, overwrite). Prefer recipe/window/folder to clicking; after a task likely to repeat, recipe_save it with params, not literals. A step may carry on_fail "skip", when {text|gone_text|title}.
at=[fx, fy] (0..1 of window) replaces coordinate. screenshot save_to=file.png.
display = monitor 1, 2, ... scope = a window: screenshots and coordinates cover only that window (sharper, fewer tokens). only_apps = process names input may reach, e.g. ["notepad.exe"]. allow_irreversible = true presses a Send/Buy/Delete-looking button under only_apps."##;

/// The JSON input schema, compact (Python `list_tools()[0].inputSchema` through `json.dumps`).
pub const INPUT_SCHEMA_JSON: &str = r##"{"properties":{"action":{"type":"string"},"coordinate":{"items":{"type":"integer"},"type":"array"},"start_coordinate":{"items":{"type":"integer"},"type":"array"},"text":{"type":"string"},"scroll_direction":{"type":"string"},"scroll_amount":{"type":"integer"},"duration":{"type":"number"},"repeat":{"type":"integer"},"region":{"items":{"type":"integer"},"type":"array"},"actions":{"items":{"type":"object"},"type":"array"},"display":{"type":"integer"},"only_apps":{"items":{"type":"string"},"type":"array"},"allow_irreversible":{"type":"boolean"},"button":{"type":"string"},"double":{"type":"boolean"},"nth":{"type":"integer"},"old":{"type":"string"},"new":{"type":"string"},"timeout":{"type":"number"},"expect":{"type":"object"},"filter":{"type":"string"},"limit":{"type":"integer"},"allow_destructive":{"type":"boolean"},"shot":{"type":"boolean"},"shot_long":{"type":"integer"},"wake":{"type":"boolean"},"window":{"type":"string"},"op":{"type":"string"},"rect":{"items":{"type":"integer"},"type":"array"},"at":{"items":{"type":"number"},"type":"array"},"path":{"type":"string"},"view":{"type":"string"},"sort":{"type":"string"},"list":{"type":"integer"},"name":{"type":"string"},"params":{"type":"object"},"steps":{"items":{"type":"object"},"type":"array"},"save_to":{"type":"string"},"overwrite":{"type":"boolean"},"scope":{"type":"string"}},"type":"object"}"##;

static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(INPUT_SCHEMA_JSON).expect("the tool schema is valid JSON")
});

/// The input schema as a JSON value, properties in the order the Python build lists them.
pub fn input_schema() -> Value {
    SCHEMA.clone()
}

/// `{"name", "description", "inputSchema"}`, the object an MCP `tools/list` answer carries.
pub fn tool_definition() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": TOOL_DESCRIPTION,
        "inputSchema": input_schema(),
    })
}

/// The size the speed-fix tests measure: description plus the schema as compact JSON.
pub fn definition_chars() -> usize {
    TOOL_DESCRIPTION.chars().count() + INPUT_SCHEMA_JSON.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ACTIONS_SORTED;

    const ORIGINAL_PARAMS: [&str; 23] = [
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
    ];

    fn props() -> serde_json::Map<String, Value> {
        input_schema()["properties"].as_object().cloned().unwrap()
    }

    // ---- ported from test_20261008_speed_fixes.py::test_tool_definition_stays_within_its_budget
    #[test]
    fn the_definition_stays_within_its_budget() {
        let size = definition_chars();
        assert!(size <= 3900, "{size}");
        assert!(size < 4176, "{size}"); // the size before the 2026-10-08 slimming
        assert!(size > 2732, "{size}"); // the slim size, before the shortcuts and recipes
    }

    // ---- ported from test_schema_has_no_titles_and_every_old_param_plus_the_new_ones
    #[test]
    fn the_schema_keeps_every_old_param_and_adds_the_new_ones_without_titles() {
        let schema = input_schema();
        assert!(!schema.to_string().contains("\"title\""));
        let p = props();
        for name in ORIGINAL_PARAMS {
            assert!(p.contains_key(name), "{name}");
        }
        assert_eq!(p["shot"], json!({"type": "boolean"}));
        assert_eq!(p["shot_long"], json!({"type": "integer"}));
        assert!(p.contains_key("wake"));
        assert!(schema.get("required").is_none());
    }

    // ---- ported from test_the_new_parameters_are_in_the_schema_with_the_right_types
    #[test]
    fn the_new_parameters_have_the_right_types() {
        let p = props();
        assert_eq!(p["window"], json!({"type": "string"}));
        assert_eq!(p["op"], json!({"type": "string"}));
        assert_eq!(
            p["rect"],
            json!({"items": {"type": "integer"}, "type": "array"})
        );
        assert_eq!(
            p["at"],
            json!({"items": {"type": "number"}, "type": "array"})
        );
        for name in ["path", "view", "sort", "name", "save_to"] {
            assert_eq!(p[name], json!({"type": "string"}), "{name}");
        }
        assert_eq!(p["list"], json!({"type": "integer"}));
        assert_eq!(p["overwrite"], json!({"type": "boolean"}));
        assert_eq!(p["params"], json!({"type": "object"}));
        assert_eq!(
            p["steps"],
            json!({"items": {"type": "object"}, "type": "array"})
        );
    }

    #[test]
    fn the_parameters_come_in_the_pythons_order() {
        let names: Vec<String> = props().keys().cloned().collect();
        assert_eq!(names.first().map(String::as_str), Some("action"));
        assert_eq!(names.last().map(String::as_str), Some("scope"));
        assert_eq!(names.len(), 40);
        let pos = |n: &str| names.iter().position(|x| x == n).unwrap();
        assert!(pos("allow_destructive") < pos("shot") && pos("wake") < pos("window"));
        assert_eq!(
            input_schema()["type"],
            json!("object"),
            "an object schema, nothing else at the top"
        );
    }

    // ---- ported from test_the_description_names_every_action_and_gives_the_one_or_two_sentences_of_advice
    #[test]
    fn the_description_names_every_action_and_gives_its_advice() {
        for action in ACTIONS_SORTED {
            assert!(TOOL_DESCRIPTION.contains(action), "{action}");
        }
        assert!(TOOL_DESCRIPTION.contains("Prefer recipe/window/folder to clicking"));
        assert!(TOOL_DESCRIPTION.contains("recipe_save it with params, not literals"));
        for needle in [
            "on_fail",
            "when {text|gone_text|title}",
            "at=[fx, fy]",
            "save_to",
            "list=n",
            "rect [x,y,w,h]",
            "shot=false",
            "shot_long",
            "wake=true",
            "actions=[",
            "expect",
        ] {
            assert!(TOOL_DESCRIPTION.contains(needle), "{needle}");
        }
        assert!(!TOOL_DESCRIPTION.contains('\u{2014}'));
    }

    // ---- ported from test_20261008d_scope_shim.py::test_the_schema_has_scope_and_one_short_sentence
    #[test]
    fn the_schema_has_scope_and_one_short_sentence() {
        assert_eq!(props()["scope"], json!({"type": "string"}));
        assert!(TOOL_DESCRIPTION.contains(
            "scope = a window: screenshots and coordinates cover only that window (sharper, fewer tokens)"
        ));
        assert!(!TOOL_DESCRIPTION.contains('\u{2014}'));
    }

    #[test]
    fn the_tool_definition_is_the_three_fields_an_mcp_client_reads() {
        let d = tool_definition();
        assert_eq!(d["name"], "computer");
        assert_eq!(d["description"], TOOL_DESCRIPTION);
        assert_eq!(d["inputSchema"], input_schema());
        assert_eq!(d.as_object().unwrap().len(), 3);
    }
}
