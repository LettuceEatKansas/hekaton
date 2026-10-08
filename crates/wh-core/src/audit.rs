//! The audit log. Port of `daemon._log_action`, `daemon._redact_args` and `daemon._rotate`.
//!
//! One JSON object per line in `actions.jsonl`, appended after each handled call, written exactly
//! as Python's `json.dumps` does (`", "` and `": "` separators, non-ASCII escaped as `\uXXXX`) so a
//! reader of either build's log sees one format:
//!
//! ```text
//! {"ts": 1791460800.123, "action": "type", "args": {"text": "<7 chars 9f86d081ee>", "window": "w1"}, "ok": true, "result": "ok"}
//! ```
//!
//! Redaction: the audit log used to hold everything the agent typed (passwords included) and
//! every code snippet in full. A string under `text` or `code` is replaced by its length in
//! characters and the first ten hex characters of its SHA-256, which says WHAT happened without
//! keeping the content. The log rotates to `actions.1.jsonl` once it passes five megabytes, and a
//! logging failure never fails the call.

use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::ser::{CharEscape, Formatter, Serializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::paths::Paths;

/// The log is moved aside once it is larger than this many bytes (Python `LOG_ROTATE_BYTES`).
pub const LOG_ROTATE_BYTES: u64 = 5_000_000;

/// The `result` field keeps at most this many characters.
pub const RESULT_MAX_CHARS: usize = 300;

/// Keys whose string values are replaced by a length and a short hash.
pub const REDACTED_KEYS: [&str; 2] = ["text", "code"];

/// Keys whose value is a list of steps, each redacted in turn.
pub const NESTED_KEYS: [&str; 2] = ["actions", "steps"];

/// `<7 chars 9f86d081ee>`: the length in characters and the first ten hex characters of the SHA-256
/// of `v` (a string as it is, anything else as `json.dumps` writes it).
fn mask(v: &Value) -> String {
    let text = match v {
        Value::String(s) => s.clone(),
        other => python_json(other),
    };
    let digest = Sha256::digest(text.as_bytes());
    let hex: String = digest.iter().take(5).map(|b| format!("{b:02x}")).collect();
    format!("<{} chars {hex}>", text.chars().count())
}

/// Keep what is needed to see WHAT happened, not the content: length and a short hash of typed
/// text and snippets. 2026-10-08: a recipe's `params` and the `actions` of a batch or the `steps` of
/// a `recipe_save` can hold typed text too, so those are masked as well, steps recursively. Python
/// `_redact_args`. Anything that is not an object comes back unchanged.
pub fn redact_args(args: &Value) -> Value {
    let Value::Object(map) = args else {
        return args.clone();
    };
    let mut out = map.clone();
    for key in REDACTED_KEYS {
        if let Some(v @ Value::String(_)) = out.get(key) {
            let redacted = mask(v);
            out.insert(key.to_string(), Value::String(redacted));
        }
    }
    if let Some(Value::Object(params)) = out.get("params") {
        let masked: Map<String, Value> = params
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(mask(v))))
            .collect();
        out.insert("params".to_string(), Value::Object(masked));
    }
    for key in NESTED_KEYS {
        if let Some(Value::Array(steps)) = out.get(key) {
            let redacted: Vec<Value> = steps
                .iter()
                .map(|s| {
                    if s.is_object() {
                        redact_args(s)
                    } else {
                        s.clone()
                    }
                })
                .collect();
            out.insert(key.to_string(), Value::Array(redacted));
        }
    }
    Value::Object(out)
}

/// Move `path` aside to `<stem>.1<ext>` when it is bigger than `limit` bytes. Python `_rotate`.
pub fn rotate(path: &Path, limit: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= limit {
        return;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let _ = std::fs::rename(path, path.with_file_name(format!("{stem}.1{ext}")));
}

/// A serde_json formatter that reproduces Python's `json.dumps` defaults.
struct PythonFormatter;

impl Formatter for PythonFormatter {
    fn begin_array_value<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }

    fn begin_object_key<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if first { Ok(()) } else { w.write_all(b", ") }
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b": ")
    }

    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        // ensure_ascii: everything outside space..~ becomes \uXXXX (UTF-16 units, like Python).
        let mut buf = String::with_capacity(fragment.len());
        for ch in fragment.chars() {
            if (' '..='~').contains(&ch) {
                buf.push(ch);
            } else {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    buf.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
        w.write_all(buf.as_bytes())
    }

    fn write_char_escape<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        ch: CharEscape,
    ) -> io::Result<()> {
        match ch {
            CharEscape::AsciiControl(b) => w.write_all(format!("\\u{b:04x}").as_bytes()),
            CharEscape::Quote => w.write_all(b"\\\""),
            CharEscape::ReverseSolidus => w.write_all(b"\\\\"),
            CharEscape::Solidus => w.write_all(b"/"),
            CharEscape::Backspace => w.write_all(b"\\b"),
            CharEscape::FormFeed => w.write_all(b"\\f"),
            CharEscape::LineFeed => w.write_all(b"\\n"),
            CharEscape::CarriageReturn => w.write_all(b"\\r"),
            CharEscape::Tab => w.write_all(b"\\t"),
        }
    }
}

/// `json.dumps(value)` as Python prints it.
pub fn python_json(value: &Value) -> String {
    let mut buf = Vec::new();
    let mut ser = Serializer::with_formatter(&mut buf, PythonFormatter);
    if value.serialize(&mut ser).is_err() {
        return String::new();
    }
    String::from_utf8(buf).unwrap_or_default()
}

/// One audit line, without the trailing newline. `args` is redacted here.
pub fn format_entry(ts: f64, action: &str, args: &Value, ok: bool, result: &str) -> String {
    let mut entry = Map::new();
    entry.insert("ts".into(), Value::from(ts));
    entry.insert("action".into(), Value::from(action));
    entry.insert("args".into(), redact_args(args));
    entry.insert("ok".into(), Value::from(ok));
    entry.insert(
        "result".into(),
        Value::from(result.chars().take(RESULT_MAX_CHARS).collect::<String>()),
    );
    python_json(&Value::Object(entry))
}

/// Append one entry to `paths.audit_log()`, rotating first. Never fails the caller: any I/O
/// problem is swallowed, as in Python. Python `_log_action(action, args, result, ok)`.
pub fn log_action(paths: &Paths, action: &str, args: &Value, result: &str, ok: bool) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    log_action_at(paths, ts, action, args, result, ok);
}

/// [`log_action`] with an explicit timestamp, for tests and replays.
pub fn log_action_at(paths: &Paths, ts: f64, action: &str, args: &Value, result: &str, ok: bool) {
    let _ = write_entry(paths, &format_entry(ts, action, args, ok, result));
}

fn write_entry(paths: &Paths, line: &str) -> io::Result<()> {
    std::fs::create_dir_all(paths.dir())?;
    let log = paths.audit_log();
    rotate(&log, LOG_ROTATE_BYTES);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use serde_json::json;

    // Ported from tests/test_20260919_fixes.py::test_audit_log_redacts_typed_text_and_rotates.
    #[test]
    fn audit_log_redacts_typed_text_and_rotates() {
        let tmp = TempDir::new("audit");
        let paths = Paths::in_dir(tmp.path());
        log_action(
            &paths,
            "type",
            &json!({"text": "hunter2", "window": "w1"}),
            "ok",
            true,
        );
        let log = paths.audit_log();
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            !body.contains("hunter2") && body.contains("<7 chars"),
            "{body}"
        );
        assert!(body.contains("\"window\": \"w1\""));

        std::fs::write(&log, "x".repeat(LOG_ROTATE_BYTES as usize + 1)).unwrap();
        log_action(&paths, "windows", &json!({}), "ok", true);
        assert!(tmp.path().join("actions.1.jsonl").exists());
        assert!(std::fs::metadata(&log).unwrap().len() < 1000);
    }

    #[test]
    fn the_line_format_is_what_python_json_dumps_writes() {
        let line = format_entry(
            1791460800.5,
            "type",
            &json!({"text": "hunter2", "window": "w1"}),
            true,
            "ok",
        );
        assert_eq!(
            line,
            "{\"ts\": 1791460800.5, \"action\": \"type\", \"args\": {\"text\": \"<7 chars \
             9f86d081ee>\", \"window\": \"w1\"}, \"ok\": true, \"result\": \"ok\"}"
                .replace("9f86d081ee", &sha_prefix("hunter2"))
        );
    }

    fn sha_prefix(s: &str) -> String {
        Sha256::digest(s.as_bytes())
            .iter()
            .take(5)
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn redaction_hashes_text_and_code_and_leaves_the_rest() {
        let args = json!({"text": "abc", "code": "print(1)", "limit": 5, "window": "w2"});
        let out = redact_args(&args);
        // sha256("abc") = ba7816bf8f01...
        assert_eq!(out["text"], "<3 chars ba7816bf8f>");
        assert_eq!(out["code"], format!("<8 chars {}>", sha_prefix("print(1)")));
        assert_eq!(out["limit"], 5);
        assert_eq!(out["window"], "w2");
        assert_eq!(args["text"], "abc", "the input is not modified");
    }

    #[test]
    fn redaction_counts_characters_not_bytes_and_skips_non_strings() {
        let out = redact_args(&json!({"text": "h\u{e9}llo \u{1F642}", "code": 7, "other": null}));
        assert!(
            out["text"].as_str().unwrap().starts_with("<7 chars "),
            "{out}"
        );
        assert_eq!(out["code"], 7);
        assert_eq!(redact_args(&json!([1, 2])), json!([1, 2]));
    }

    #[test]
    fn non_ascii_is_escaped_like_ensure_ascii() {
        let line = format_entry(
            1.0,
            "x",
            &json!({"q": "caf\u{e9} \u{1F642}\n\"\\\u{7f}"}),
            false,
            "\u{2713}",
        );
        assert!(
            line.contains("\"q\": \"caf\\u00e9 \\ud83d\\ude42\\n\\\"\\\\\\u007f\""),
            "{line}"
        );
        assert!(line.contains("\"result\": \"\\u2713\""), "{line}");
        assert!(line.contains("\"ok\": false"));
        assert!(line.is_ascii());
    }

    #[test]
    fn arrays_and_nesting_use_python_separators() {
        let s = python_json(&json!({"a": [1, 2, {"b": null}], "c": 1.5}));
        assert_eq!(s, "{\"a\": [1, 2, {\"b\": null}], \"c\": 1.5}");
    }

    #[test]
    fn the_result_is_cut_at_300_characters() {
        let line = format_entry(1.0, "x", &json!({}), true, &"y".repeat(500));
        let doc: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(doc["result"].as_str().unwrap().chars().count(), 300);
    }

    #[test]
    fn entries_append_one_per_line() {
        let tmp = TempDir::new("audit-append");
        let paths = Paths::in_dir(tmp.path());
        for i in 0..3 {
            log_action_at(&paths, i as f64, "windows", &json!({}), "ok", true);
        }
        let body = std::fs::read_to_string(paths.audit_log()).unwrap();
        assert_eq!(body.lines().count(), 3);
        assert!(
            body.lines()
                .all(|l| serde_json::from_str::<Value>(l).is_ok())
        );
    }

    #[test]
    fn rotation_replaces_an_older_rotated_file_and_ignores_small_logs() {
        let tmp = TempDir::new("audit-rotate");
        let log = tmp.path().join("actions.jsonl");
        std::fs::write(&log, "small").unwrap();
        rotate(&log, 100);
        assert!(log.exists() && !tmp.path().join("actions.1.jsonl").exists());
        std::fs::write(tmp.path().join("actions.1.jsonl"), "old").unwrap();
        std::fs::write(&log, "z".repeat(101)).unwrap();
        rotate(&log, 100);
        assert!(!log.exists());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("actions.1.jsonl"))
                .unwrap()
                .len(),
            101
        );
    }

    #[test]
    fn a_log_that_cannot_be_written_does_not_fail_the_call() {
        let tmp = TempDir::new("audit-fail");
        let blocker = tmp.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        // The "directory" is a file, so nothing can be created under it.
        log_action(
            &Paths::in_dir(blocker.join("sub")),
            "x",
            &json!({}),
            "ok",
            true,
        );
    }

    // Ported from tests/test_20261008b_recipes.py::test_redaction_reaches_into_batches_and_recipe_steps.
    #[test]
    fn redaction_reaches_into_batches_and_recipe_steps() {
        let out = redact_args(&json!({
            "actions": [{"action": "type", "text": "hunter2"}, {"action": "key", "text": "Return"}],
            "steps": [{"action": "type", "text": "s3cret"}],
            "params": {"a": 1, "b": "x"},
            "name": "n"
        }));
        let flat = out.to_string();
        assert!(
            !flat.contains("hunter2") && !flat.contains("s3cret"),
            "{flat}"
        );
        assert_eq!(out["name"], "n");
        assert!(
            out["params"]["a"]
                .as_str()
                .unwrap()
                .starts_with("<1 chars ")
        );
        assert!(
            out["params"]["b"]
                .as_str()
                .unwrap()
                .starts_with("<1 chars ")
        );
        assert_eq!(out["actions"][1]["action"], "key");
        // a non-text param is masked by what json.dumps writes for it
        let out = redact_args(&json!({"params": {"list": [1, 2], "none": null}}));
        assert!(
            out["params"]["list"]
                .as_str()
                .unwrap()
                .starts_with("<6 chars ")
        );
        assert!(
            out["params"]["none"]
                .as_str()
                .unwrap()
                .starts_with("<4 chars ")
        );
        // steps that are not objects are left as they were
        assert_eq!(
            redact_args(&json!({"actions": ["junk", 3]}))["actions"],
            json!(["junk", 3])
        );
    }
}
