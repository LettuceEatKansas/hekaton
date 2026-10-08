//! `wh`: the command-line front end for the daemon (Python `winhands/wh.py`).
//!
//! `winhands wh ...` (or this executable under the name `wh`) is for an agent that has a shell: no tool
//! schema in the prompt, no stdio shim, plain text back. The syntax is the Python tool's, rule for rule.

use std::io::{Read, Write};
use std::path::Path;

use serde_json::{Map, Value};
use wh_core::config::Config;
use wh_core::textutil::{py_repr, py_str};

use crate::client::Reply;

/// What `wh help` prints.
pub const USAGE: &str = r#"    wh windows
    wh snapshot window=Calculator filter=Seven
    wh computer action=type text=123
    wh computer action=left_click coordinate=640,360
    wh computer --json '{"actions": [{"action": "open_app", "text": "notepad"}, {"action": "type", "text": "hi"}]}'
    echo {"action": "list_windows"} | wh computer --json -

Shortcuts and recipes (each is one `computer` call, so Hermes cron or a script runs them with no model):

    wh recipe list
    wh recipe run explorer_newest path=C:/Users/me/Downloads n=3
    wh window close window=notepad.exe all=true
    wh window move window=w4 rect=0,0,960,1080
    wh folder path=C:/Users/me/Downloads sort="date_modified desc" list=3

`recipe run NAME` turns every other key=value into a recipe param, except the call options shot,
shot_long, only_apps, display, scope, coords, allow_irreversible and allow_destructive. Exit code 1 when a step failed.
`scope=<window>` (a wN ref, title part or app.exe, also on any `computer` call) limits screenshots and
coordinates to that window.

Values: text-like keys (text, title, name, value, query, path, keys, app, target, window, filter,
and any key wh does not know) are ALWAYS kept as text, so text=123 and text=yes type those words.
Only known number keys (x, y, amount, repeat, limit, nth, display, timeout, shot_long, ...) become
numbers, known flags (shot, wake, enter, clear, double, exact, ...) accept true/false/yes/no/on/off,
and coordinate/start_coordinate/region take 640,360 or [640,360]. actions=, expect= and only_apps=
take JSON (only_apps also takes a comma list). For a batch, --json '<payload>' sends a raw payload
with no key=value parsing (the tool defaults to `computer`), and --json - reads it from stdin, so
nothing needs shell escaping. key=value after --json overrides that key.

The `computer` tool gets the configured defaults the MCP shim sends: only_apps (WINHANDS_ONLY_APPS or
config.json) and coords. If no daemon answers, wh starts it (daemon_cmd, daemon_task, on macOS the
winhands.app LaunchAgent, or this executable's `daemon`, see start.rs) and retries once. Exit code 1 and the error on stderr when the daemon
refuses. Screenshots are written to a file and the path is printed."#;

/// Keys whose value is a number when it parses as one.
pub const NUM_KEYS: [&str; 19] = [
    "x",
    "y",
    "amount",
    "scroll_amount",
    "repeat",
    "max_items",
    "shot_long",
    "display",
    "timeout",
    "duration",
    "limit",
    "nth",
    "wait",
    "scale",
    "settle_ms",
    "capture_scale",
    "width",
    "height",
    "list",
];

/// Keys that accept true/false/yes/no/on/off/1/0.
pub const BOOL_KEYS: [&str; 15] = [
    "shot",
    "wake",
    "double",
    "enter",
    "clear",
    "exact",
    "snap",
    "all",
    "desc",
    "no_focus",
    "capture_after",
    "stop_on_error",
    "allow_irreversible",
    "allow_destructive",
    "overwrite",
];

/// Same key, different type per route: `new` is open's flag but replace_word's replacement text, and
/// `coords` is snapshot's flag but the computer tool's "pixels"/"norm1000" choice.
fn tool_bool_keys(tool: &str) -> &'static [&'static str] {
    match tool {
        "open" => &["new", "reuse"],
        "snapshot" => &["coords"],
        _ => &[],
    }
}

/// Keys that are lists of numbers.
pub const POINT_KEYS: [&str; 7] = [
    "coordinate",
    "start_coordinate",
    "region",
    "capture_region",
    "rect",
    "at",
    "start_at",
];

/// Keys that take JSON.
pub const JSON_KEYS: [&str; 5] = ["actions", "expect", "args", "params", "steps"];

/// Keys that are a JSON list or a comma list.
pub const LIST_KEYS: [&str; 1] = ["only_apps"];

/// The first words that are sugar for one `computer` call.
pub const SUGAR: [&str; 3] = ["recipe", "window", "folder"];

/// The options of a call, which `recipe run` does not turn into recipe params.
pub const CALL_KEYS: [&str; 8] = [
    "shot",
    "shot_long",
    "only_apps",
    "display",
    "scope",
    "coords",
    "allow_irreversible",
    "allow_destructive",
];

fn sugar_usage(cmd: &str) -> &'static str {
    match cmd {
        "recipe" => "wh recipe list | wh recipe run NAME key=value ...",
        "window" => {
            "wh window close|minimize|maximize|restore|focus|move window=<w3 | title part | app.exe> \
             [rect=x,y,w,h] [all=true]"
        }
        _ => "wh folder path=<folder> [view=details] [sort=\"date_modified desc\"] [list=3]",
    }
}

/// A message for stderr and exit code 1 (Python `SystemExit(message)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit(pub String);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Exit {}

fn exit<T>(message: impl Into<String>) -> Result<T, Exit> {
    Err(Exit(message.into()))
}

// ---------------------------------------------------------------- value parsing
// 2026-10-08: `text=123` used to become the int 123 and `text=yes` the bool True, so typing digits or
// yes/no/on/off failed. Only keys wh KNOWS are numbers, flags, lists or JSON get coerced; every other
// key (text, title, name, value, query, path, keys, app, target, window, ... and anything new) stays
// exactly the text that was typed.

const TRUE_WORDS: [&str; 4] = ["true", "yes", "on", "1"];
const FALSE_WORDS: [&str; 4] = ["false", "no", "off", "0"];

/// `int(raw.strip())`, else `float(raw.strip())`, else nothing.
fn number(raw: &str) -> Option<Value> {
    let t = raw.trim();
    if let Ok(i) = t.parse::<i64>() {
        return Some(Value::from(i));
    }
    let f: f64 = t.parse().ok()?;
    serde_json::Number::from_f64(f).map(Value::Number)
}

fn json_or_raw(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::from(raw))
}

/// Key-blind guess for older callers: true/false/null, numbers and JSON objects or lists become
/// values; the rest stays text. `wh` itself uses [`parse_field`], which looks at the key first.
pub fn parse_value(raw: &str) -> Value {
    match raw.to_lowercase().as_str() {
        "true" | "yes" | "on" => return Value::Bool(true),
        "false" | "no" | "off" => return Value::Bool(false),
        "null" => return Value::Null, // "none" stays a string: report=none is a real winhands value
        _ => {}
    }
    if raw.starts_with(['[', '{']) {
        return json_or_raw(raw);
    }
    number(raw).unwrap_or_else(|| Value::from(raw))
}

/// One `key=value` from the command line, as the value to send. Text stays text.
pub fn parse_field(tool: &str, key: &str, raw: &str) -> Value {
    if POINT_KEYS.contains(&key) {
        if raw.starts_with('[') {
            return json_or_raw(raw);
        }
        let nums: Vec<Option<Value>> = raw
            .replace(',', " ")
            .split_whitespace()
            .map(number)
            .collect();
        if !nums.is_empty() && nums.iter().all(Option::is_some) {
            return Value::Array(nums.into_iter().flatten().collect());
        }
        return Value::from(raw);
    }
    if JSON_KEYS.contains(&key) {
        if raw.starts_with(['[', '{']) {
            return json_or_raw(raw);
        }
        return Value::from(raw);
    }
    if LIST_KEYS.contains(&key) {
        if raw.starts_with('[') {
            return json_or_raw(raw);
        }
        return Value::Array(
            raw.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(Value::from)
                .collect(),
        );
    }
    if BOOL_KEYS.contains(&key) || tool_bool_keys(tool).contains(&key) {
        let low = raw.trim().to_lowercase();
        if TRUE_WORDS.contains(&low.as_str()) {
            return Value::Bool(true);
        }
        if FALSE_WORDS.contains(&low.as_str()) {
            return Value::Bool(false);
        }
        return Value::from(raw);
    }
    if NUM_KEYS.contains(&key) {
        return number(raw).unwrap_or_else(|| Value::from(raw));
    }
    Value::from(raw)
}

fn read_stdin(stdin: &mut dyn Read) -> Result<String, Exit> {
    let mut raw = Vec::new();
    let _ = stdin.read_to_end(&mut raw);
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&raw);
    match std::str::from_utf8(raw) {
        Ok(text) => Ok(text.to_string()),
        Err(_) => exit(
            "wh: --json - needs UTF-8 on stdin (a PowerShell pipe may be using the OEM code page). \
             Pass the payload as --json '<payload>' instead.",
        ),
    }
}

fn load_json_payload(src: &str, stdin: &mut dyn Read) -> Result<Map<String, Value>, Exit> {
    let text = if src == "-" {
        read_stdin(stdin)?
    } else {
        src.to_string()
    };
    if text.trim().is_empty() {
        return exit(format!(
            "wh: --json got an empty payload{}",
            if src == "-" {
                " (nothing on stdin)"
            } else {
                ""
            }
        ));
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => exit(
            "wh: --json payload must be a JSON object, e.g. {\"action\": \"screenshot\"} or \
             {\"actions\": [...]}",
        ),
        Err(e) => exit(format!("wh: --json payload is not valid JSON: {e}")),
    }
}

// ---------------------------------------------------------------- shortcuts: recipe, window, folder
// Not daemon routes: each becomes one `computer` call (action=recipe_list / recipe / window / folder).

fn parse_sugar(argv: &[String]) -> Result<(String, Map<String, Value>), Exit> {
    let cmd = argv[0].as_str();
    let positional: Vec<&str> = argv[1..]
        .iter()
        .filter(|a| !a.contains('='))
        .map(String::as_str)
        .collect();
    let kv: Vec<(&str, &str)> = argv[1..].iter().filter_map(|a| a.split_once('=')).collect();
    let mut payload = Map::new();
    let allowed = match cmd {
        "recipe" => 2,
        _ => 1,
    };
    if positional.len() > allowed {
        return exit(format!(
            "wh {cmd}: unexpected argument {} (extra values go as key=value). {}",
            py_repr(positional[allowed]),
            sugar_usage(cmd)
        ));
    }
    match cmd {
        "recipe" => {
            if positional.is_empty() || !["list", "run"].contains(&positional[0]) {
                return exit(format!("wh recipe: {}", sugar_usage("recipe")));
            }
            let listing = positional[0] == "list";
            if listing {
                payload.insert("action".into(), Value::from("recipe_list"));
            } else {
                if positional.len() < 2 {
                    return exit(format!(
                        "wh recipe run: needs a recipe name. {}",
                        sugar_usage("recipe")
                    ));
                }
                payload.insert("action".into(), Value::from("recipe"));
                payload.insert("name".into(), Value::from(positional[1]));
            }
            let mut params = Map::new();
            for (key, value) in kv {
                if CALL_KEYS.contains(&key) || listing {
                    payload.insert(key.to_string(), parse_field("computer", key, value));
                } else {
                    params.insert(key.to_string(), Value::from(value)); // params stay text; the daemon converts
                }
            }
            if !params.is_empty() {
                payload.insert("params".into(), Value::Object(params));
            }
            return Ok(("computer".into(), payload));
        }
        "window" => {
            if positional.is_empty() {
                return exit(format!(
                    "wh window: needs an operation. {}",
                    sugar_usage("window")
                ));
            }
            payload.insert("action".into(), Value::from("window"));
            payload.insert("op".into(), Value::from(positional[0]));
        }
        _ => {
            payload.insert("action".into(), Value::from("folder"));
            if let Some(path) = positional.first() {
                payload.insert("path".into(), Value::from(*path));
            }
        }
    }
    for (key, value) in kv {
        payload.insert(key.to_string(), parse_field("computer", key, value));
    }
    Ok(("computer".into(), payload))
}

/// `tool key=value ...` or `[tool] --json <payload|-> [key=value ...]` into the tool and its payload.
pub fn parse_args(
    argv: &[String],
    stdin: &mut dyn Read,
) -> Result<(String, Map<String, Value>), Exit> {
    if argv.is_empty() {
        return exit(USAGE);
    }
    if SUGAR.contains(&argv[0].as_str()) {
        return parse_sugar(argv);
    }
    let mut tool: Option<String> = None;
    let mut json_src: Option<String> = None;
    let mut pairs: Vec<&str> = Vec::new();
    let mut items = argv.iter();
    while let Some(item) = items.next() {
        if item == "--json" {
            match items.next() {
                Some(src) => json_src = Some(src.clone()),
                None => return exit("wh: --json needs a payload, or - to read it from stdin"),
            }
        } else if let Some(src) = item.strip_prefix("--json=") {
            json_src = Some(src.to_string());
        } else if tool.is_none() && pairs.is_empty() && !item.contains('=') {
            tool = Some(item.clone());
        } else {
            pairs.push(item);
        }
    }
    let tool = match tool {
        Some(t) => t,
        None => {
            if json_src.is_none() {
                return exit(format!(
                    "wh: expected a tool name first (e.g. computer), got {}",
                    py_repr(pairs.first().copied().unwrap_or(""))
                ));
            }
            "computer".to_string()
        }
    };
    let mut payload = match &json_src {
        Some(src) => load_json_payload(src, stdin)?,
        None => Map::new(),
    };
    for item in pairs {
        let Some((key, value)) = item.split_once('=') else {
            return exit(format!(
                "wh {tool}: expected key=value, got {}",
                py_repr(item)
            ));
        };
        payload.insert(key.to_string(), parse_field(&tool, key, value));
    }
    Ok((tool, payload))
}

/// The configured default allowlist, as the shim sends it. A list in `config.json` works here (the
/// shim reads only comma strings).
pub fn default_only_apps(cfg: &Config) -> Vec<String> {
    match cfg.get("only_apps") {
        Value::Array(items) => items
            .iter()
            .map(|a| py_str(a).trim().to_string())
            .filter(|a| !a.is_empty())
            .collect(),
        other => wh_core::config::to_text(&other)
            .split(',')
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect(),
    }
}

/// The configured coordinate convention.
pub fn default_coords(cfg: &Config) -> String {
    let coords = cfg.coords();
    if coords.trim().is_empty() {
        "pixels".to_string()
    } else {
        coords
    }
}

/// Send what the MCP shim sends on every `computer` call (`coords` and `only_apps`), so the configured
/// default allowlist is not bypassed from this command line. `only_apps` is per-call on the daemon
/// (absent and `[]` both mean no allowlist), so an explicit `only_apps` in the payload, even an empty
/// list, wins.
pub fn apply_computer_defaults(payload: &mut Map<String, Value>, cfg: &Config) {
    payload
        .entry("coords")
        .or_insert_with(|| Value::from(default_coords(cfg)));
    if !payload.contains_key("only_apps") {
        payload.insert(
            "only_apps".into(),
            Value::Array(
                default_only_apps(cfg)
                    .into_iter()
                    .map(Value::from)
                    .collect(),
            ),
        );
    }
}

/// How `wh` reaches the daemon: tool name and payload in, the daemon's answer or the message to exit
/// with out.
pub type DaemonCall<'a> = dyn FnMut(&str, &Map<String, Value>) -> Result<Reply, Exit> + 'a;

/// Run `wh`: parse, call the daemon through `call`, print. `call` returns the daemon's answer or the
/// message to exit with. Returns the process exit code.
pub fn run(
    argv: &[String],
    cfg: &Config,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
    temp_dir: &Path,
    call: &mut DaemonCall<'_>,
) -> i32 {
    if argv.is_empty() || ["-h", "--help", "help"].contains(&argv[0].as_str()) {
        let _ = writeln!(out, "{USAGE}");
        return 0;
    }
    let sugar = SUGAR.contains(&argv[0].as_str());
    let (tool, mut payload) = match parse_args(argv, stdin) {
        Ok(parsed) => parsed,
        Err(Exit(message)) => {
            let _ = writeln!(err, "{message}");
            return 1;
        }
    };
    if tool == "computer" {
        apply_computer_defaults(&mut payload, cfg);
    }
    let reply = match call(&tool, &payload) {
        Ok(r) => r,
        Err(Exit(message)) => {
            let _ = writeln!(err, "{message}");
            return 1;
        }
    };
    if reply.content_type.starts_with("image/") {
        let path = temp_dir.join("winhands-screenshot.png");
        if std::fs::write(&path, &reply.body).is_ok() {
            let _ = writeln!(out, "{}", path.display());
        }
        return 0;
    }
    let data = match serde_json::from_slice::<Value>(&reply.body) {
        Ok(Value::Object(map)) => map,
        _ => {
            let text = String::from_utf8_lossy(&reply.body);
            let head: String = text.chars().take(2000).collect();
            let _ = writeln!(err, "{head}");
            return 1;
        }
    };
    let text = match data.get("results") {
        Some(Value::Array(lines)) => lines.iter().map(py_str).collect::<Vec<_>>().join("\n"),
        _ => data.get("result").map_or_else(String::new, py_str),
    };
    if !text.is_empty() {
        let _ = writeln!(out, "{text}");
    }
    if let Some(Value::Array(content)) = data.get("content") {
        for (i, item) in content.iter().enumerate() {
            // computer / capture_after results carry their picture here
            if item.get("type").and_then(Value::as_str) == Some("image") {
                let mime = item.get("mimeType").and_then(Value::as_str).unwrap_or("");
                let ext = if mime.contains("jpeg") {
                    ".jpg"
                } else {
                    ".png"
                };
                let data64 = item.get("data").and_then(Value::as_str).unwrap_or("");
                if let Some(bytes) = decode_base64(data64) {
                    let path = temp_dir.join(format!("winhands-shot{i}{ext}"));
                    if std::fs::write(&path, bytes).is_ok() {
                        let _ = writeln!(out, "{}", path.display());
                    }
                }
            }
        }
    }
    if let Some(error) = data.get("error").filter(|e| wh_core::config::truthy(e)) {
        let _ = writeln!(err, "error: {}", py_str(error));
    }
    if sugar && data.get("failed_step").is_some_and(wh_core::config::truthy) {
        return 1; // a recipe or shortcut whose step failed must not look like success to a script
    }
    let ok = data.get("ok").is_none_or(wh_core::config::truthy);
    if reply.status == 200 && ok { 0 } else { 1 }
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

#[cfg(test)]
mod tests;
