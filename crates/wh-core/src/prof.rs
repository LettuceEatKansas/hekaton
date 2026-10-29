//! Per-call phase timing for `profile=true`.
//!
//! A [`span`] adds its wall time to a named phase (capture, encode, uia, ocr, input, settle, ...) in
//! a thread-local table that [`reset`] clears at the start of a call and [`take`] reads at the end.
//! Times are inclusive: `settle` contains the captures it takes, so phases can sum to more than the
//! call. A span costs two clock reads and a map update, so it stays on whether or not a call asked
//! for a profile.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

thread_local! {
    static PHASES: RefCell<BTreeMap<&'static str, (u32, Duration)>> = const { RefCell::new(BTreeMap::new()) };
}

/// Times one phase until it is dropped.
#[must_use = "a span measures until it is dropped"]
pub struct Span {
    name: &'static str,
    start: Instant,
}

impl Drop for Span {
    fn drop(&mut self) {
        let took = self.start.elapsed();
        PHASES.with(|p| {
            let mut p = p.borrow_mut();
            let entry = p.entry(self.name).or_insert((0, Duration::ZERO));
            entry.0 += 1;
            entry.1 += took;
        });
    }
}

/// Start timing `name`.
pub fn span(name: &'static str) -> Span {
    Span {
        name,
        start: Instant::now(),
    }
}

/// Forget everything recorded on this thread.
pub fn reset() {
    PHASES.with(|p| p.borrow_mut().clear());
}

/// `{"phase": {"n": calls, "ms": total}}` for this thread, then clear it.
pub fn take() -> Value {
    let phases = PHASES.with(|p| std::mem::take(&mut *p.borrow_mut()));
    let mut out = Map::new();
    for (name, (n, took)) in phases {
        let ms = (took.as_secs_f64() * 10_000.0).round() / 10.0;
        out.insert(name.to_string(), json!({"n": n, "ms": ms}));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_add_up_per_phase_and_take_clears() {
        reset();
        {
            let _a = span("capture");
        }
        {
            let _b = span("capture");
            let _c = span("encode");
        }
        let v = take();
        assert_eq!(v["capture"]["n"], json!(2));
        assert_eq!(v["encode"]["n"], json!(1));
        assert_eq!(take(), json!({}));
    }
}
