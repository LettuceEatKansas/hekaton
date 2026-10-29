//! The MCP `computer` tool, the platform-agnostic half (Python `winhands/shim.py`).
//!
//! `winhands mcp` is a thin stdio server: it advertises one tool, turns each call into the payload the
//! daemon's `/computer` route expects, and turns the daemon's answer back into MCP content blocks.
//! Everything that needs no MCP library and no socket lives here so it is unit-tested on every OS:
//!
//! * [`TOOL_DESCRIPTION`] and [`input_schema`]: the definition, byte for byte what the Python shim
//!   serves from `list_tools` (a test compares them with `tests/parity/python_list_tools.json`);
//! * [`build_payload`]: the body of the Python `computer()` function, defaults and "extras only when
//!   set" included, so a daemon sees the same request from either shim;
//! * [`reply_from_daemon`]: the Python `_result`, including which answers count as an error.

use serde_json::{Map, Value};

use crate::config::Config;

// The tool definition has one home, `crate::tool_schema`; the MCP server and the tests read it here.
pub use crate::tool_schema::{INPUT_SCHEMA_JSON, TOOL_DESCRIPTION, TOOL_NAME};

/// The server name the Python shim announces (`FastMCP("winhands")`).
pub const SERVER_NAME: &str = "winhands";

/// The tool's JSON schema as an object, keys in the order the Python server prints them.
pub fn input_schema() -> Map<String, Value> {
    match crate::tool_schema::input_schema() {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// What the shim adds to every call when the caller did not say (Python `COORDS` and
/// `DEFAULT_ONLY_APPS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimDefaults {
    /// `pixels` or `norm1000`.
    pub coords: String,
    /// The default `only_apps` allowlist; an explicit list on a call, even an empty one, wins.
    pub only_apps: Vec<String>,
}

impl ShimDefaults {
    /// The configured defaults (`coords`, `only_apps`).
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            coords: cfg.coords(),
            only_apps: cfg.only_apps_default(),
        }
    }
}

/// A call argument had the wrong type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgError(pub String);

impl std::fmt::Display for ArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ArgError {}

// ------------------------------------------------------------------------------ argument types

fn wrong(key: &str, want: &str) -> ArgError {
    ArgError(format!("argument {key:?} must be {want}"))
}

fn present<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    args.get(key).filter(|v| !v.is_null())
}

fn opt_str(args: &Map<String, Value>, key: &str) -> Result<Option<String>, ArgError> {
    match present(args, key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(wrong(key, "a string")),
    }
}

fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(_) => None,
        _ => None,
    }
}

fn as_float(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => match n.as_i64() {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        },
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "yes" | "on" | "1" | "t" | "y" => Some(true),
            "false" | "no" | "off" | "0" | "f" | "n" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn opt_int(args: &Map<String, Value>, key: &str) -> Result<Option<i64>, ArgError> {
    match present(args, key) {
        None => Ok(None),
        Some(v) => as_int(v).map(Some).ok_or_else(|| wrong(key, "an integer")),
    }
}

fn opt_float(args: &Map<String, Value>, key: &str) -> Result<Option<f64>, ArgError> {
    match present(args, key) {
        None => Ok(None),
        Some(v) => as_float(v).map(Some).ok_or_else(|| wrong(key, "a number")),
    }
}

fn opt_bool(args: &Map<String, Value>, key: &str) -> Result<Option<bool>, ArgError> {
    match present(args, key) {
        None => Ok(None),
        Some(v) => as_bool(v).map(Some).ok_or_else(|| wrong(key, "a boolean")),
    }
}

fn opt_list<'a>(
    args: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a Vec<Value>>, ArgError> {
    match present(args, key) {
        None => Ok(None),
        Some(Value::Array(a)) => Ok(Some(a)),
        Some(_) => Err(wrong(key, "a list")),
    }
}

fn opt_int_list(args: &Map<String, Value>, key: &str) -> Result<Option<Value>, ArgError> {
    let Some(items) = opt_list(args, key)? else {
        return Ok(None);
    };
    let ints: Option<Vec<Value>> = items.iter().map(|v| as_int(v).map(Value::from)).collect();
    ints.map(|v| Some(Value::Array(v)))
        .ok_or_else(|| wrong(key, "a list of integers"))
}

fn opt_float_list(args: &Map<String, Value>, key: &str) -> Result<Option<Value>, ArgError> {
    let Some(items) = opt_list(args, key)? else {
        return Ok(None);
    };
    let nums: Option<Vec<Value>> = items.iter().map(|v| as_float(v).map(Value::from)).collect();
    nums.map(|v| Some(Value::Array(v)))
        .ok_or_else(|| wrong(key, "a list of numbers"))
}

fn opt_str_list(args: &Map<String, Value>, key: &str) -> Result<Option<Vec<String>>, ArgError> {
    let Some(items) = opt_list(args, key)? else {
        return Ok(None);
    };
    items
        .iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .map(Some)
        .ok_or_else(|| wrong(key, "a list of strings"))
}

fn opt_obj_list(args: &Map<String, Value>, key: &str) -> Result<Option<Value>, ArgError> {
    let Some(items) = opt_list(args, key)? else {
        return Ok(None);
    };
    if items.iter().all(Value::is_object) {
        Ok(Some(Value::Array(items.clone())))
    } else {
        Err(wrong(key, "a list of objects"))
    }
}

fn opt_obj(args: &Map<String, Value>, key: &str) -> Result<Option<Value>, ArgError> {
    match present(args, key) {
        None => Ok(None),
        Some(v @ Value::Object(_)) => Ok(Some(v.clone())),
        Some(_) => Err(wrong(key, "an object")),
    }
}

// ------------------------------------------------------------------------------ the payload

/// The body of the Python `computer()` tool function: the request the daemon's `/computer` route
/// gets for one MCP call.
///
/// The fixed fields go out on every call (`action` defaults to `screenshot`). Everything else is an
/// "extra" and is only sent when set, so the payload an existing action sees never changes. `actions`
/// replaces the whole payload with the batch, and `coords`, `display`, `shot`, `shot_long`,
/// `allow_destructive` and `only_apps` ride top level so they apply to a batch too. `only_apps` is
/// per-call state on the daemon, so it is sent on every call: an explicit `[]` means "no allowlist for
/// this call", which differs from leaving the key out, so the configured default only fills in when the
/// caller passed nothing at all.
pub fn build_payload(
    args: &Map<String, Value>,
    defaults: &ShimDefaults,
) -> Result<Value, ArgError> {
    let action = opt_str(args, "action")?.unwrap_or_else(|| "screenshot".to_string());
    let text = opt_str(args, "text")?.unwrap_or_default();
    let mut payload = Map::new();
    payload.insert("action".into(), Value::from(action.clone()));
    payload.insert(
        "coordinate".into(),
        opt_int_list(args, "coordinate")?.unwrap_or(Value::Null),
    );
    payload.insert(
        "start_coordinate".into(),
        opt_int_list(args, "start_coordinate")?.unwrap_or(Value::Null),
    );
    payload.insert("text".into(), Value::from(text));
    payload.insert(
        "scroll_direction".into(),
        Value::from(opt_str(args, "scroll_direction")?.unwrap_or_else(|| "down".to_string())),
    );
    payload.insert(
        "scroll_amount".into(),
        Value::from(opt_int(args, "scroll_amount")?.unwrap_or(3)),
    );
    payload.insert(
        "duration".into(),
        Value::from(opt_float(args, "duration")?.unwrap_or(1.0)),
    );
    payload.insert(
        "repeat".into(),
        Value::from(opt_int(args, "repeat")?.unwrap_or(1)),
    );
    payload.insert(
        "region".into(),
        opt_int_list(args, "region")?.unwrap_or(Value::Null),
    );
    payload.insert(
        "allow_irreversible".into(),
        Value::from(opt_bool(args, "allow_irreversible")?.unwrap_or(false)),
    );

    // Extras are only sent when set, so the payload an existing action sees is unchanged.
    let mut put = |key: &str, value: Option<Value>| {
        if let Some(v) = value {
            payload.insert(key.to_string(), v);
        }
    };
    let truthy_str = |key: &str| -> Result<Option<Value>, ArgError> {
        Ok(opt_str(args, key)?
            .filter(|s| !s.is_empty())
            .map(Value::from))
    };
    let button = opt_str(args, "button")?.unwrap_or_else(|| "left".to_string());
    put("button", (button != "left").then(|| Value::from(button)));
    put(
        "double",
        opt_bool(args, "double")?.filter(|b| *b).map(Value::from),
    );
    put("nth", opt_int(args, "nth")?.map(Value::from));
    put("old", truthy_str("old")?);
    put("new", opt_str(args, "new")?.map(Value::from));
    put(
        "expect",
        opt_obj(args, "expect")?.filter(|v| v.as_object().is_some_and(|m| !m.is_empty())),
    );
    put("filter", truthy_str("filter")?);
    put(
        "limit",
        opt_int(args, "limit")?.filter(|n| *n != 0).map(Value::from),
    );
    put(
        "wake",
        opt_bool(args, "wake")?.filter(|b| *b).map(Value::from),
    );
    let timeout = opt_float(args, "timeout")?.unwrap_or(10.0);
    put(
        "timeout",
        matches!(action.as_str(), "wait_for_text" | "wait_for_file").then(|| Value::from(timeout)),
    );
    // shortcuts and recipes
    put("window", truthy_str("window")?);
    put("op", truthy_str("op")?);
    put("rect", opt_int_list(args, "rect")?);
    put("at", opt_float_list(args, "at")?);
    put("path", truthy_str("path")?);
    put("view", truthy_str("view")?);
    put("sort", truthy_str("sort")?);
    put(
        "list",
        opt_int(args, "list")?.filter(|n| *n != 0).map(Value::from),
    );
    put("name", truthy_str("name")?);
    put("params", opt_obj(args, "params")?);
    put("steps", opt_obj_list(args, "steps")?);
    put("save_to", truthy_str("save_to")?);
    put(
        "overwrite",
        opt_bool(args, "overwrite")?.filter(|b| *b).map(Value::from),
    );

    let batch =
        opt_obj_list(args, "actions")?.filter(|v| v.as_array().is_some_and(|a| !a.is_empty()));
    let mut payload = match batch {
        Some(actions) => {
            let mut only_actions = Map::new();
            only_actions.insert("actions".into(), actions);
            only_actions
        }
        None => payload,
    };
    if opt_bool(args, "allow_destructive")?.unwrap_or(false) {
        // Per call, never sticky: the daemon reads it off this one request.
        payload.insert("allow_destructive".into(), Value::Bool(true));
    }
    payload.insert("coords".into(), Value::from(defaults.coords.clone()));
    let display = opt_int(args, "display")?.unwrap_or(0);
    if display != 0 {
        payload.insert("display".into(), Value::from(display));
    }
    // 2026-10-08: scope rides top-level, so it covers every step of a batch.
    if let Some(scope) = opt_str(args, "scope")?.filter(|s| !s.is_empty()) {
        payload.insert("scope".into(), Value::from(scope));
    }
    // shot=false drops the trailing screenshot (the call still settles); shot_long is this call's
    // screenshot long edge and the daemon reads this call's coordinates in that same scale.
    if !opt_bool(args, "shot")?.unwrap_or(true) {
        payload.insert("shot".into(), Value::Bool(false));
    }
    if let Some(long) = opt_int(args, "shot_long")?.filter(|n| *n != 0) {
        payload.insert("shot_long".into(), Value::from(long));
    }
    let only_apps = opt_str_list(args, "only_apps")?.unwrap_or_else(|| defaults.only_apps.clone());
    payload.insert(
        "only_apps".into(),
        Value::Array(only_apps.into_iter().map(Value::from).collect()),
    );
    for key in ["compact", "profile", "read_only", "if_changed"] {
        if let Some(value) = opt_bool(args, key)? {
            payload.insert(key.into(), Value::Bool(value));
        }
    }
    for key in ["session_id", "snapshot_id"] {
        if let Some(value) = opt_str(args, key)? {
            payload.insert(key.into(), Value::String(value));
        }
    }
    for key in ["budget_ms", "max_chars", "max_images"] {
        if let Some(value) = opt_int(args, key)? {
            payload.insert(key.into(), Value::from(value));
        }
    }
    // Preserve an explicit image opt-in even when compact mode defaults to text only.
    if opt_bool(args, "compact")?.unwrap_or(false)
        && let Some(shot) = opt_bool(args, "shot")?
    {
        payload.insert("shot".into(), Value::Bool(shot));
    }
    Ok(Value::Object(payload))
}

// ------------------------------------------------------------------------------ the reply

/// One block of an MCP tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Text.
    Text(String),
    /// A picture: base64 data and its MIME type.
    Image {
        /// Base64 bytes.
        data: String,
        /// `image/jpeg` or `image/png`.
        mime: String,
    },
}

impl Block {
    fn text_contains(&self, needle: &str) -> bool {
        matches!(self, Block::Text(t) if t.contains(needle))
    }
}

/// What the MCP client gets back: blocks plus the error flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The content blocks, in order.
    pub blocks: Vec<Block>,
    /// `isError`.
    pub is_error: bool,
}

impl Reply {
    /// A one-block error reply (Python `_error`).
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            blocks: vec![Block::Text(message.into())],
            is_error: true,
        }
    }
}

fn python_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => crate::textutil::py_str(other),
    }
}

/// Python truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    crate::config::truthy(v)
}

fn parse_blocks(items: &Value) -> Option<Vec<Block>> {
    let list = items.as_array()?;
    let mut blocks = Vec::new();
    for item in list {
        let obj = item.as_object()?;
        match obj.get("type").and_then(Value::as_str) {
            Some("text") => blocks.push(Block::Text(obj.get("text")?.as_str()?.to_string())),
            Some("image") => blocks.push(Block::Image {
                data: obj.get("data")?.as_str()?.to_string(),
                mime: obj.get("mimeType")?.as_str()?.to_string(),
            }),
            _ => return None,
        }
    }
    Some(blocks)
}

/// The Python `_result`: the daemon's HTTP status and body as MCP content.
///
/// `content` from the daemon passes through (a malformed block adds `error: malformed daemon
/// content`); without it a non-empty `result` becomes one text block. An `error` the content does not
/// already mention is appended as `error: ...`. The call is an error when the status is not 200, the
/// daemon said `ok: false`, an `error` is present, or a text block has a line starting with `error:`.
pub fn reply_from_daemon(path: &str, code: u16, body: &[u8]) -> Reply {
    let invalid = || {
        let text = String::from_utf8_lossy(body);
        let head: String = text.chars().take(1000).collect();
        Reply::error(format!(
            "winhands {path} returned invalid response (HTTP {code}): {head}"
        ))
    };
    let data = if body.is_empty() {
        Map::new()
    } else {
        match serde_json::from_slice::<Value>(body) {
            Ok(Value::Object(map)) => map,
            _ => return invalid(),
        }
    };
    let mut blocks: Vec<Block> = Vec::new();
    if let Some(content) = data.get("content") {
        match parse_blocks(content) {
            Some(parsed) => blocks = parsed,
            None => {
                // keep what parsed before the bad block, like the Python loop did
                if let Some(list) = content.as_array() {
                    for item in list {
                        match parse_blocks(&Value::Array(vec![item.clone()])) {
                            Some(mut one) => blocks.append(&mut one),
                            None => break,
                        }
                    }
                }
                blocks.push(Block::Text("error: malformed daemon content".to_string()));
            }
        }
    } else if let Some(result) = data.get("result").filter(|r| truthy(r)) {
        blocks = vec![Block::Text(python_str(result))];
    }
    let error = data.get("error").filter(|e| truthy(e));
    if let Some(error) = error {
        let message = python_str(error);
        if !blocks.iter().any(|b| b.text_contains(&message)) {
            blocks.push(Block::Text(format!("error: {message}")));
        }
    }
    let line_says_error = blocks.iter().any(|b| match b {
        Block::Text(t) => t
            .lines()
            .any(|line| line.trim_start().to_lowercase().starts_with("error:")),
        Block::Image { .. } => false,
    });
    let failed = code != 200
        || data.get("ok").is_some_and(|ok| ok == &Value::Bool(false))
        || error.is_some()
        || line_says_error;
    if blocks.is_empty() {
        blocks = vec![Block::Text(if failed {
            format!("error: HTTP {code}")
        } else {
            "ok".to_string()
        })];
    }
    Reply {
        blocks,
        is_error: failed,
    }
}

/// The message when the daemon does not answer in time. The call may still be running, so nothing
/// is retried.
pub fn timeout_message(path: &str) -> String {
    format!(
        "winhands {path} timed out. Outcome unknown: work may still be running. \
         Inspect current state before retrying; do not repeat actions blindly."
    )
}

#[cfg(test)]
mod tests;
