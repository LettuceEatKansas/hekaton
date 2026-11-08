//! Two small memories the daemon keeps per process: a read whose answer is identical to the last one
//! says so ("same result as 4s ago"), and three identical failures get a hint to change approach.
//! Port of `daemon._nudge_if_unchanged`, `daemon._count_failure`, `daemon._call_key`,
//! `daemon.ALTERNATIVES`.
//!
//! 2026-09-19: no call is ever answered from a cache. A repeated snapshot used to return the pre-click
//! tree, and a repeated open must really focus the app again. What is left is a note on a read whose
//! answer did not change, and a hint after three identical failures. A measured run on 2026-09-18
//! burned 17 identical open attempts on one task.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::audit::python_json;
use crate::clock::Clock;

/// How long ago an identical read still counts as a repeat (`REPEAT_WINDOW_S`).
pub const REPEAT_WINDOW: Duration = Duration::from_secs(15);

/// Identical failures before the error carries a hint (`REPEAT_FAIL_LIMIT`).
pub const REPEAT_FAIL_LIMIT: u32 = 3;

/// Routes whose repeated answers are noted (`READS_NUDGE`).
pub const READS_NUDGE: [&str; 3] = ["windows", "snapshot", "files"];

/// What to try instead of repeating a call (`ALTERNATIVES`).
pub fn alternative(path: &str) -> Option<&'static str> {
    Some(match path {
        "open" => "if you are listing a folder, call files(path) instead: no window needed",
        "snapshot" => "add filter= to narrow it, or call files() if you only want content",
        "windows" => {
            "you already have the window list; act on it, or send several actions in one computer call"
        }
        "click" => {
            "if the target is not appearing, try a keyboard shortcut via key(), or snapshot the window first"
        }
        _ => return None,
    })
}

fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), sorted(&m[k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// `path|` plus the arguments as sorted-key JSON: what makes two calls "the same" (Python
/// `_call_key`).
pub fn call_key(path: &str, args: &Value) -> String {
    format!("{path}|{}", python_json(&sorted(args)))
}

/// Remembers the last answer of each read call.
#[derive(Default)]
pub struct RepeatNote {
    last: Mutex<HashMap<String, (Duration, String)>>,
}

impl RepeatNote {
    /// `text` as it should be returned: unchanged, or with a note when the same read gave the same
    /// answer within [`REPEAT_WINDOW`]. Only the read routes are tracked; anything else passes
    /// through. Python `_nudge_if_unchanged`.
    pub fn nudge(&self, clock: &dyn Clock, path: &str, args: &Value, text: &str) -> String {
        if !READS_NUDGE.contains(&path) {
            return text.to_string();
        }
        let key = call_key(path, args);
        let now = clock.now();
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = last.insert(key, (now, text.to_string()));
        if last.len() > 50 {
            last.retain(|_, (t, _)| now.saturating_sub(*t) <= Duration::from_secs(60));
        }
        if let Some((at, old)) = prev
            && now.saturating_sub(at) <= REPEAT_WINDOW
            && old == text
        {
            let hint = alternative(path).unwrap_or("act on it or change the arguments");
            return format!(
                "{text}\n(same result as {}s ago, nothing has changed. {hint})",
                now.saturating_sub(at).as_secs()
            );
        }
        text.to_string()
    }
}

/// Counts identical failures.
#[derive(Default)]
pub struct FailureCounter {
    counts: Mutex<HashMap<String, u32>>,
}

impl FailureCounter {
    /// Record a failure of `path` with `args` and return the error text, with a hint to change
    /// approach once the same call has failed [`REPEAT_FAIL_LIMIT`] times. Python `_count_failure`.
    pub fn note_failure(&self, path: &str, args: &Value, error: &str) -> String {
        let key = call_key(path, args);
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = counts.entry(key).or_insert(0);
        *n += 1;
        if *n >= REPEAT_FAIL_LIMIT {
            let hint = alternative(path).unwrap_or("try a different approach");
            return format!("{error} (failed {n} times with these arguments. {hint})")
                .trim()
                .to_string();
        }
        error.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use serde_json::json;

    // Ported from tests/test_20260919_fixes.py::test_repeated_snapshot_is_never_served_stale: the
    // text is always the fresh one; an identical answer only gains a note.
    #[test]
    fn an_identical_read_gains_a_note_and_a_changed_one_does_not() {
        let clock = FakeClock::new();
        let memory = RepeatNote::default();
        let args = json!({"window": "w1"});
        assert_eq!(
            memory.nudge(&clock, "snapshot", &args, "tree v1"),
            "tree v1"
        );
        assert_eq!(
            memory.nudge(&clock, "snapshot", &args, "tree v2"),
            "tree v2"
        );
        clock.advance(Duration::from_secs(4));
        let third = memory.nudge(&clock, "snapshot", &args, "tree v2");
        assert!(third.starts_with("tree v2"), "{third}");
        assert!(third.contains("nothing has changed"), "{third}");
        assert!(third.contains("(same result as 4s ago"), "{third}");
        assert!(third.contains(alternative("snapshot").unwrap()), "{third}");
    }

    #[test]
    fn a_repeat_after_the_window_or_of_another_route_is_not_noted() {
        let clock = FakeClock::new();
        let memory = RepeatNote::default();
        let args = json!({});
        memory.nudge(&clock, "windows", &args, "same");
        clock.advance(Duration::from_secs(16));
        assert_eq!(memory.nudge(&clock, "windows", &args, "same"), "same");
        // other routes never track
        assert_eq!(memory.nudge(&clock, "click", &args, "x"), "x");
        assert_eq!(memory.nudge(&clock, "click", &args, "x"), "x");
        // different arguments are a different call
        memory.nudge(&clock, "files", &json!({"path": "a"}), "list");
        assert_eq!(
            memory.nudge(&clock, "files", &json!({"path": "b"}), "list"),
            "list"
        );
    }

    #[test]
    fn key_order_in_the_arguments_does_not_matter() {
        assert_eq!(
            call_key("x", &json!({"b": 1, "a": {"d": 1, "c": 2}})),
            call_key("x", &json!({"a": {"c": 2, "d": 1}, "b": 1}))
        );
    }

    #[test]
    fn three_identical_failures_get_a_hint() {
        let fails = FailureCounter::default();
        let args = json!({"app": "x"});
        assert_eq!(fails.note_failure("open", &args, "no window"), "no window");
        assert_eq!(fails.note_failure("open", &args, "no window"), "no window");
        let third = fails.note_failure("open", &args, "no window");
        assert!(
            third.starts_with(
                "no window (failed 3 times with these arguments. if you are listing a folder"
            ),
            "{third}"
        );
        // a different call has its own count; a route with no alternative says to change approach
        assert_eq!(
            fails.note_failure("open", &json!({"app": "y"}), "no window"),
            "no window"
        );
        for _ in 0..2 {
            fails.note_failure("computer", &args, "boom");
        }
        assert!(
            fails
                .note_failure("computer", &args, "boom")
                .ends_with("try a different approach)")
        );
    }
}
