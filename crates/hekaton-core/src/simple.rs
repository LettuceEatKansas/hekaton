//! Simple mode: a handful of small, strict tools for small models, served instead of the one big
//! `computer` tool. Every call is translated into an ordinary `computer` payload (so the approval
//! gate, `only_apps`, the audit log and the owner card apply unchanged) and every reply is shaped
//! into plain text: one line saying what happened, then a fresh numbered list. The design is in
//! `docs/SIMPLE-MODE.md`. Platform neutral: no OS code here.

use std::sync::Mutex;

use serde_json::{Map, Value, json};

use crate::mcp::{Block, Reply};

/// The usage line for an unknown profile name.
pub const PROFILE_USAGE: &str = "usage: hekaton mcp [--profile full|simple|simple-vision] [--daemon-cmd CMD] [--daemon-task NAME]";

/// The screenshot long edge `screenshot` and `click_at` always use, so the click frame never moves.
pub const SHOT_LONG: i64 = 1024;

/// Items an acting reply shows, and its character cap.
pub const ACT_ITEMS: usize = 40;
/// See [`ACT_ITEMS`].
pub const ACT_CHARS: usize = 2500;
/// Items `look` shows, and its character cap.
pub const LOOK_ITEMS: usize = 60;
/// See [`LOOK_ITEMS`].
pub const LOOK_CHARS: usize = 4000;

const REFUSED: &str = "Needs the owner's OK";

/// Which tools `hekaton mcp` serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// The one `computer` tool, exactly as always.
    #[default]
    Full,
    /// Nine text tools.
    Simple,
    /// The nine, plus `screenshot` and `click_at`.
    SimpleVision,
}

impl Profile {
    /// `full`, `simple` or `simple-vision` (any case); anything else is `None`.
    pub fn parse(name: &str) -> Option<Profile> {
        match name.trim().to_ascii_lowercase().as_str() {
            "full" => Some(Profile::Full),
            "simple" => Some(Profile::Simple),
            "simple-vision" => Some(Profile::SimpleVision),
            _ => None,
        }
    }

    /// The tool names, in the order they are listed.
    pub fn tool_names(self) -> Vec<&'static str> {
        match self {
            Profile::Full => vec![crate::tool_schema::TOOL_NAME],
            Profile::Simple => NINE.to_vec(),
            Profile::SimpleVision => NINE
                .iter()
                .copied()
                .chain(["screenshot", "click_at"])
                .collect(),
        }
    }

    /// Is this one of the simple profiles?
    pub fn is_simple(self) -> bool {
        self != Profile::Full
    }

    /// The simple tools as `{name, description, inputSchema}` objects (empty for `full`).
    pub fn definitions(self) -> Vec<Value> {
        if !self.is_simple() {
            return Vec::new();
        }
        self.tool_names().into_iter().map(definition).collect()
    }
}

const NINE: [&str; 9] = [
    "look",
    "read_text",
    "press",
    "type",
    "key",
    "open",
    "scroll",
    "wait",
    "approve",
];

fn s(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}

fn n(desc: &str) -> Value {
    json!({"type": "integer", "description": desc})
}

fn b(desc: &str) -> Value {
    json!({"type": "boolean", "description": desc})
}

/// (name, one-line description, parameters, required names, example call).
type Spec = (
    &'static str,
    &'static str,
    Vec<(&'static str, Value)>,
    &'static [&'static str],
    &'static str,
);

fn spec(name: &str) -> Spec {
    match name {
        "look" => (
            "look",
            "Show the front window and a numbered list of things you can press or type into. Call it first, and whenever you are lost.",
            vec![
                ("window", s("Window title; default the front window")),
                ("filter", s("Only list items with these words")),
            ],
            &[],
            "look {}",
        ),
        "read_text" => (
            "read_text",
            "Read the text of the window or page.",
            vec![("find", s("Only show text with these words"))],
            &[],
            "read_text {}",
        ),
        "press" => (
            "press",
            "Press item N from the newest list, or the control with that visible label.",
            vec![
                ("target", s("Item number, or visible label")),
                ("double", b("Press twice quickly")),
            ],
            &["target"],
            "press {\"target\": \"12\"}",
        ),
        "type" => (
            "type",
            "Type text. Optionally press a field first, clear it first, and press Return after.",
            vec![
                ("text", s("The text to type")),
                ("into", s("Item number or label to press first")),
                ("clear", b("Replace what is there")),
                ("submit", b("Press Return after")),
            ],
            &["text"],
            "type {\"text\": \"hello\", \"into\": \"3\", \"submit\": true}",
        ),
        "key" => (
            "key",
            "Press a key or key combination.",
            vec![
                ("keys", s("For example Return, Tab or cmd+s")),
                ("repeat", n("Times to press; default 1")),
            ],
            &["keys"],
            "key {\"keys\": \"Return\"}",
        ),
        "open" => (
            "open",
            "Open an app by name, or a web address in the browser.",
            vec![("target", s("App name or web address"))],
            &["target"],
            "open {\"target\": \"Notes\"}",
        ),
        "scroll" => (
            "scroll",
            "Scroll the front window.",
            vec![
                (
                    "direction",
                    json!({"type": "string", "enum": ["up", "down", "left", "right"], "description": "Which way"}),
                ),
                ("amount", n("Steps; default 3")),
            ],
            &["direction"],
            "scroll {\"direction\": \"down\"}",
        ),
        "wait" => (
            "wait",
            "Wait some seconds, or until new text appears on screen.",
            vec![
                ("seconds", n("Seconds to wait")),
                ("for_text", s("Wait until this text appears")),
            ],
            &[],
            "wait {\"seconds\": 2}",
        ),
        "approve" => (
            "approve",
            "Repeat the last refused step, using the code the owner gave you.",
            vec![("code", s("The code from the owner"))],
            &["code"],
            "approve {\"code\": \"K7QM\"}",
        ),
        "screenshot" => (
            "screenshot",
            "Take one picture of the screen. Its size is stated in the reply.",
            Vec::new(),
            &[],
            "screenshot {}",
        ),
        _ => (
            "click_at",
            "Click a point in the newest screenshot.",
            vec![
                ("x", n("Across, in screenshot pixels")),
                ("y", n("Down, in screenshot pixels")),
                (
                    "button",
                    json!({"type": "string", "enum": ["left", "right"], "description": "Default left"}),
                ),
                ("double", b("Double-click")),
            ],
            &["x", "y"],
            "click_at {\"x\": 100, \"y\": 200}",
        ),
    }
}

fn definition(name: &str) -> Value {
    let (name, desc, props, required, _) = spec(name);
    let properties: Map<String, Value> =
        props.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    json!({
        "name": name,
        "description": desc,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
    })
}

// ------------------------------------------------------------------------------ one call

/// What a translated call needs from its reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `look`: the list is the answer.
    Look,
    /// `read_text` and `screenshot`: the answer is the text (and the picture).
    Plain,
    /// Anything that changes the screen: an outcome line, then a fresh list.
    Act,
}

/// A simple call, translated.
#[derive(Debug, Clone)]
pub struct Call {
    /// The simple tool's name.
    pub tool: String,
    /// The `computer` arguments (a batch has `actions`).
    pub args: Map<String, Value>,
    /// How to shape the reply.
    pub kind: Kind,
    /// A line about arguments that were ignored.
    pub note: Option<String>,
}

/// The state one server session keeps: the last refused call, for `approve`.
#[derive(Debug)]
pub struct Simple {
    profile: Profile,
    refused: Mutex<Option<Call>>,
}

impl Simple {
    /// For a simple profile.
    pub fn new(profile: Profile) -> Self {
        Self {
            profile,
            refused: Mutex::new(None),
        }
    }

    /// The profile served.
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// Turn a tool call into a [`Call`], or the error reply to send instead.
    pub fn prepare(&self, name: &str, args: &Map<String, Value>) -> Result<Call, Reply> {
        if !self.profile.tool_names().contains(&name) {
            return Err(fail(format!(
                "Unknown tool: {name}. The tools are: {}.\nNext: call one of those tools, for example look {{}}.",
                self.profile.tool_names().join(", ")
            )));
        }
        let (_, _, props, required, example) = spec(name);
        for key in required {
            if is_blank(args.get(*key)) {
                return Err(fail(format!("{name} needs {key}.\nNext: call {example}")));
            }
        }
        let known: Vec<&str> = props.iter().map(|(k, _)| *k).collect();
        let mut ignored: Vec<&str> = args
            .keys()
            .map(String::as_str)
            .filter(|k| !known.contains(k) && *k != "session_id")
            .collect();
        ignored.sort_unstable();
        let note = (!ignored.is_empty())
            .then(|| format!("(ignored unknown argument: {})", ignored.join(", ")));
        let mut call =
            translate(name, args).map_err(|why| fail(format!("{why}\nNext: call {example}")))?;
        call.note = note;
        if name == "approve" {
            let code = text_of(args.get("code")).unwrap_or_default();
            let stored = self
                .refused
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            let Some(mut stored) = stored else {
                return Err(fail(
                    "Nothing is waiting for approval.\nNext: call look to see the screen, then carry on.".into(),
                ));
            };
            stored
                .args
                .insert("approval_code".into(), Value::from(code));
            stored.note = call.note;
            return Ok(stored);
        }
        Ok(call)
    }

    /// Remember a refused call so `approve` can send it again. Call after the main step's reply.
    pub fn remember(&self, call: &Call, reply: &Reply) {
        let text = reply_text(reply);
        if reply.is_error && text.contains(REFUSED) {
            let mut kept = call.clone();
            kept.args.remove("approval_code");
            *self.refused.lock().unwrap_or_else(|e| e.into_inner()) = Some(kept);
        }
    }

    /// The reply for a call that needs no fresh list, or `Err(outcome)` when the step worked and a
    /// list should follow (send [`list_call`], then [`Simple::with_list`]).
    pub fn finish(&self, call: &Call, reply: Reply) -> Result<Reply, String> {
        if reply.is_error {
            return Ok(error_reply(&reply, call.note.as_deref(), self.profile));
        }
        match call.kind {
            Kind::Look => Ok(Reply {
                blocks: vec![Block::Text(with_note(
                    shape_list(&reply_text(&reply), LOOK_ITEMS, LOOK_CHARS),
                    call.note.as_deref(),
                ))],
                is_error: false,
            }),
            Kind::Plain => {
                let blocks = reply
                    .blocks
                    .into_iter()
                    .filter(|b| {
                        self.profile == Profile::SimpleVision || matches!(b, Block::Text(_))
                    })
                    .collect();
                Ok(Reply {
                    blocks,
                    is_error: false,
                })
            }
            Kind::Act => Err(outcome_line(&reply_text(&reply))),
        }
    }

    /// An acting step's reply: its outcome line, then the fresh list. A failed list is said in a
    /// line, not turned into an error: the step itself worked.
    pub fn with_list(&self, call: &Call, outcome: &str, list: &Reply) -> Reply {
        let tail = if list.is_error {
            "(could not list the screen)\nNext: call look.".to_string()
        } else {
            shape_list(&reply_text(list), ACT_ITEMS, ACT_CHARS)
        };
        let head = with_note(outcome.to_string(), call.note.as_deref());
        Reply {
            blocks: vec![Block::Text(format!("{head}\n{tail}"))],
            is_error: false,
        }
    }
}

/// The `computer` arguments for the list that follows an acting step.
pub fn list_call() -> Map<String, Value> {
    look_args(None, None, ACT_ITEMS)
}

fn look_args(window: Option<String>, filter: Option<String>, limit: usize) -> Map<String, Value> {
    let mut a = Map::new();
    a.insert("action".into(), "read".into());
    a.insert("wake".into(), true.into());
    a.insert("shot".into(), false.into());
    a.insert("limit".into(), Value::from(limit));
    if let Some(w) = window {
        a.insert("window".into(), w.into());
    }
    if let Some(f) = filter {
        a.insert("filter".into(), f.into());
    }
    a
}

// ------------------------------------------------------------------------------ translation

fn fail(message: String) -> Reply {
    Reply::error(message)
}

fn is_blank(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        _ => false,
    }
}

/// A string argument; a number arrives as its digits. Empty is `None`.
fn text_of(v: Option<&Value>) -> Option<String> {
    let t = match v? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(num) => num.to_string(),
        _ => return None,
    };
    (!t.is_empty()).then_some(t)
}

fn int_of(args: &Map<String, Value>, key: &str) -> Result<Option<i64>, String> {
    let Some(v) = args.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let parsed = match v {
        Value::Number(num) => num
            .as_i64()
            .or_else(|| num.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    parsed
        .map(Some)
        .ok_or_else(|| format!("{key} must be a whole number, not {v}."))
}

fn bool_of(args: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    let Some(v) = args.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let parsed = match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" => Some(true),
            "false" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    };
    parsed
        .map(Some)
        .ok_or_else(|| format!("{key} must be true or false, not {v}."))
}

/// A press step: an item number, or a visible label.
fn press_step(target: &str, double: bool) -> Map<String, Value> {
    let mut step = Map::new();
    if target.chars().all(|c| c.is_ascii_digit()) {
        step.insert("action".into(), "press".into());
    } else {
        step.insert("action".into(), "click_text".into());
        if double {
            step.insert("double".into(), true.into());
        }
    }
    step.insert("text".into(), target.into());
    step
}

fn simple_step(action: &str, text: &str) -> Map<String, Value> {
    let mut step = Map::new();
    step.insert("action".into(), action.into());
    step.insert("text".into(), text.into());
    step
}

fn acting(args: Map<String, Value>) -> Call {
    let mut args = args;
    args.insert("shot".into(), false.into());
    Call {
        tool: String::new(),
        args,
        kind: Kind::Act,
        note: None,
    }
}

fn translate(name: &str, a: &Map<String, Value>) -> Result<Call, String> {
    let mut call = match name {
        "look" => {
            let args = look_args(
                text_of(a.get("window")),
                text_of(a.get("filter")),
                LOOK_ITEMS,
            );
            Call {
                tool: String::new(),
                args,
                kind: Kind::Look,
                note: None,
            }
        }
        "read_text" => {
            let mut args = Map::new();
            args.insert("action".into(), "read_text".into());
            args.insert("shot".into(), false.into());
            if let Some(find) = text_of(a.get("find")) {
                args.insert("text".into(), find.into());
            }
            Call {
                tool: String::new(),
                args,
                kind: Kind::Plain,
                note: None,
            }
        }
        "press" => {
            let target = text_of(a.get("target")).unwrap_or_default();
            let double = bool_of(a, "double")?.unwrap_or(false);
            let mut args = press_step(&target, double);
            args.insert("shot".into(), false.into());
            acting(args)
        }
        "type" => {
            let text = a.get("text").and_then(Value::as_str).unwrap_or_default();
            let text = if text.is_empty() {
                text_of(a.get("text")).unwrap_or_default()
            } else {
                text.to_string()
            };
            let mut steps = Vec::new();
            if let Some(into) = text_of(a.get("into")) {
                steps.push(press_step(&into, false));
            }
            if bool_of(a, "clear")?.unwrap_or(false) {
                steps.push(simple_step("key", "cmd+a"));
            }
            steps.push(simple_step("type", &text));
            if bool_of(a, "submit")?.unwrap_or(false) {
                steps.push(simple_step("key", "Return"));
            }
            let mut args = Map::new();
            if steps.len() == 1 {
                args = steps.remove(0);
            } else {
                args.insert(
                    "actions".into(),
                    Value::Array(steps.into_iter().map(Value::Object).collect()),
                );
            }
            acting(args)
        }
        "key" => {
            let mut args = simple_step("key", &text_of(a.get("keys")).unwrap_or_default());
            if let Some(times) = int_of(a, "repeat")?.filter(|t| *t > 1) {
                args.insert("repeat".into(), times.into());
            }
            acting(args)
        }
        "open" => acting(simple_step(
            "open_app",
            &text_of(a.get("target")).unwrap_or_default(),
        )),
        "scroll" => {
            let dir = text_of(a.get("direction"))
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !["up", "down", "left", "right"].contains(&dir.as_str()) {
                return Err(format!(
                    "direction must be up, down, left or right, not {dir:?}."
                ));
            }
            let mut args = Map::new();
            args.insert("action".into(), "scroll".into());
            args.insert("scroll_direction".into(), dir.into());
            if let Some(amount) = int_of(a, "amount")? {
                args.insert("scroll_amount".into(), amount.into());
            }
            args.insert("at".into(), json!([0.5, 0.5]));
            acting(args)
        }
        "wait" => {
            let seconds = int_of(a, "seconds")?;
            let mut args = Map::new();
            if let Some(words) = text_of(a.get("for_text")) {
                args.insert("action".into(), "wait_for_text".into());
                args.insert("text".into(), words.into());
                args.insert("new_only".into(), true.into());
                if let Some(s) = seconds {
                    args.insert("timeout".into(), s.into());
                }
            } else {
                args.insert("action".into(), "wait".into());
                if let Some(s) = seconds {
                    args.insert("duration".into(), s.into());
                }
            }
            acting(args)
        }
        "approve" => acting(Map::new()),
        "screenshot" => {
            let mut args = Map::new();
            args.insert("action".into(), "screenshot".into());
            args.insert("shot_long".into(), SHOT_LONG.into());
            Call {
                tool: String::new(),
                args,
                kind: Kind::Plain,
                note: None,
            }
        }
        _ => {
            let (Some(x), Some(y)) = (int_of(a, "x")?, int_of(a, "y")?) else {
                return Err("x and y must be whole numbers.".into());
            };
            let button = text_of(a.get("button"))
                .unwrap_or_else(|| "left".into())
                .to_ascii_lowercase();
            let double = bool_of(a, "double")?.unwrap_or(false);
            let action = match (button.as_str(), double) {
                ("left", false) => "left_click",
                ("left", true) => "double_click",
                ("right", false) => "right_click",
                _ => return Err("button must be left or right (double works with left).".into()),
            };
            let mut args = Map::new();
            args.insert("action".into(), action.into());
            args.insert("coordinate".into(), json!([x, y]));
            args.insert("shot_long".into(), SHOT_LONG.into());
            acting(args)
        }
    };
    call.tool = name.to_string();
    Ok(call)
}

// ------------------------------------------------------------------------------ replies

fn reply_text(reply: &Reply) -> String {
    reply
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(t.as_str()),
            Block::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn with_note(text: String, note: Option<&str>) -> String {
    match note {
        Some(n) => format!("{text}\n{n}"),
        None => text,
    }
}

/// What the step said it did, on one line: the daemon's own lines minus the settling chatter.
fn outcome_line(text: &str) -> String {
    let mut parts = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with("screen settled")
            || line.starts_with("screen still settling")
            || line.starts_with("[snapshot")
        {
            continue;
        }
        let line = match line.rfind(" [image ") {
            Some(at) if line.ends_with(']') => &line[..at],
            _ => line,
        };
        parts.push(line.to_string());
    }
    let joined = parts.join("; ");
    if joined.is_empty() {
        return "done".into();
    }
    let mut out: String = joined.chars().take(300).collect();
    if joined.chars().count() > 300 {
        out.push_str("...");
    }
    out
}

/// What to do next, for an error the daemon worded.
fn next_for(text: &str) -> &'static str {
    let lower = text.to_lowercase();
    if lower.contains("no element")
        || lower.contains("page changed since read")
        || lower.contains("stale")
    {
        "Next: call look, then press the new number."
    } else if lower.contains("ambiguous") {
        "Next: call look, then press the number of the one you mean, or press a longer label."
    } else if text.contains(REFUSED) {
        "Next: ask the owner for the code, then call approve with code set to it."
    } else if lower.contains("no text on screen matches") {
        "Next: call look, then press the number of the item you want."
    } else {
        "Next: call look to see the screen, then choose the next step."
    }
}

fn error_reply(reply: &Reply, note: Option<&str>, _profile: Profile) -> Reply {
    let text = reply_text(reply);
    let text = text.trim_end();
    let has_next = text.lines().any(|l| l.trim_start().starts_with("Next:"));
    let mut out = text.to_string();
    if let Some(n) = note {
        out.push('\n');
        out.push_str(n);
    }
    if !has_next {
        out.push('\n');
        out.push_str(next_for(text));
    }
    Reply::error(out)
}

fn truncated_count(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("(truncated, ")?;
    rest.split_whitespace().next()?.parse().ok()
}

/// The `read` text cut to `max_items` numbered items and `max_chars` characters, with a line saying
/// how many more exist.
pub fn shape_list(text: &str, max_items: usize, max_chars: usize) -> String {
    let (mut head, mut items, mut tail) = (Vec::new(), Vec::new(), Vec::new());
    let mut elsewhere = 0usize;
    for line in text.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
        if line.starts_with("window ") {
            head.push(line);
        } else if line.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            items.push(line);
        } else if let Some(more) = truncated_count(line) {
            elsewhere += more;
        } else {
            tail.push(line);
        }
    }
    let mut out: Vec<String> = head.iter().map(|l| (*l).to_string()).collect();
    let mut used: usize = out.iter().map(|l| l.chars().count() + 1).sum();
    let mut shown = 0;
    for line in &items {
        let cost = line.chars().count() + 1;
        if shown >= max_items || used + cost > max_chars {
            break;
        }
        out.push((*line).to_string());
        used += cost;
        shown += 1;
    }
    let hidden = items.len() - shown + elsewhere;
    if hidden > 0 {
        out.push(format!(
            "({hidden} more not shown; call look with filter set to the words you want)"
        ));
    }
    out.extend(
        tail.iter()
            .take(3)
            .map(|l| l.chars().take(200).collect::<String>()),
    );
    if out.is_empty() {
        out.push("(nothing listed)".into());
    }
    out.join("\n")
}

#[cfg(test)]
mod tests;
