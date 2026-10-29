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
pub const TOOL_DESCRIPTION: &str = r##"Use the Windows/macOS desktop. Acting calls settle, then return a screenshot; shot=false or compact=true returns short text only.
Pointer: left_click right_click double_click triple_click middle_click mouse_move left_click_drag(start_coordinate) scroll(scroll_direction,scroll_amount) left_mouse_down left_mouse_up. coordinate=[x,y] in the last screenshot, or at=[fx, fy] (0..1 of window); keep shot_long the same. text on a click = held modifier.
Keys: type(text) key(text "ctrl+s",repeat) hold_key(text,duration).
Look: status screenshot(save_to) zoom(region) find_text read(text=window|ref,filter,limit; wake=true wakes an empty browser tree) read_field(text=label) read_text list_windows cursor_position.
Do: click_text(text,nth,button,double) replace_word(old,new) open_app file_dialog(text=path) clipboard_get clipboard_set list_files(text=folder,filter,limit) window(op,window; move rect [x,y,w,h]) folder(path,view,sort,list=n).
Wait: wait(duration) wait_for_text(text,timeout) wait_for_file.
Batch: actions=[{action,...}] max 50, one screenshot at the end, stops at the first FAIL; a step may carry expect, on_fail "skip", when {text|gone_text|title}.
Excel (owner opt-in): excel(op=new|open|write|read|add_sheet|calc|save_as|close|format_conditional|chart|pivot, workbook, sheet, range, values=[[..]] "=f" is a formula, formulas, path, name, overwrite; rule,value,value2,color,kind,title,anchor,rows,sum_field,columns) edits a hidden workbook in bulk; prefer it to clicking cells.
recipe_list recipe(name,params) recipe_save(name,text,params,steps using {k}). Prefer recipe/window/folder to clicking; recipe_save it with params, not literals.
scope = a window: screenshots and coordinates cover only that window (sharper, fewer tokens). display=monitor n. only_apps = processes input may reach; allow_irreversible presses Send/Buy/Delete-like buttons there. max_chars, max_images, budget_ms bound a call; profile=true adds timing. session_id isolates state; snapshot_id pins a screenshot; if_changed=true skips an unchanged read; read_only=true blocks input."##;

/// The JSON input schema, compact (Python `list_tools()[0].inputSchema` through `json.dumps`).
pub const INPUT_SCHEMA_JSON: &str = r##"{"properties":{"action":{"type":"string"},"coordinate":{"items":{"type":"integer"},"type":"array"},"start_coordinate":{"items":{"type":"integer"},"type":"array"},"text":{"type":"string"},"scroll_direction":{"type":"string"},"scroll_amount":{"type":"integer"},"duration":{"type":"number"},"repeat":{"type":"integer"},"region":{"items":{"type":"integer"},"type":"array"},"actions":{"items":{"type":"object"},"type":"array","maxItems":50},"display":{"type":"integer"},"only_apps":{"items":{"type":"string"},"type":"array"},"allow_irreversible":{"type":"boolean"},"button":{"type":"string"},"double":{"type":"boolean"},"nth":{"type":"integer"},"old":{"type":"string"},"new":{"type":"string"},"timeout":{"type":"number"},"expect":{"type":"object"},"filter":{"type":"string"},"limit":{"type":"integer"},"allow_destructive":{"type":"boolean"},"shot":{"type":"boolean"},"shot_long":{"type":"integer"},"wake":{"type":"boolean"},"window":{"type":"string"},"op":{"type":"string"},"rect":{"items":{"type":"integer"},"type":"array"},"at":{"items":{"type":"number"},"type":"array"},"path":{"type":"string"},"view":{"type":"string"},"sort":{"type":"string"},"list":{"type":"integer"},"name":{"type":"string"},"params":{"type":"object"},"steps":{"items":{"type":"object"},"type":"array"},"save_to":{"type":"string"},"overwrite":{"type":"boolean"},"workbook":{"type":"string"},"sheet":{"type":"string"},"range":{"type":"string"},"values":{"items":{"type":"array"},"type":"array"},"formulas":{"type":"boolean"},"kind":{"type":"string"},"rule":{"type":"string"},"value":{"type":"number"},"value2":{"type":"number"},"color":{"type":"string"},"title":{"type":"string"},"rows":{"type":"string"},"columns":{"type":"string"},"sum_field":{"type":"string"},"anchor":{"type":"string"},"scope":{"type":"string"},"compact":{"type":"boolean"},"profile":{"type":"boolean"},"read_only":{"type":"boolean"},"if_changed":{"type":"boolean"},"session_id":{"type":"string"},"snapshot_id":{"type":"string"},"budget_ms":{"type":"integer"},"max_chars":{"type":"integer"},"max_images":{"type":"integer"}},"type":"object"}"##;

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
        assert!(size <= 4175, "{size}"); // 2026-10-28: room for the nine session/budget params and the excel format/chart/pivot args
        assert!(size < 4176, "{size}"); // the size before the 2026-10-08 slimming
        assert!(size > 2732, "{size}"); // the slim size, before the shortcuts and recipes
    }

    // ---- ported from test_schema_has_no_titles_and_every_old_param_plus_the_new_ones
    #[test]
    fn the_schema_keeps_every_old_param_and_adds_the_new_ones_without_titles() {
        let schema = input_schema();
        // no JSON-schema `title` annotations; `title` itself is the excel chart's parameter
        assert!(schema.get("title").is_none());
        let p = props();
        assert!(p.values().all(|v| v.get("title").is_none()));
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
        assert_eq!(names.last().map(String::as_str), Some("max_images"));
        assert_eq!(names.len(), 64);
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
