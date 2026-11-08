//! The OCR warm-up gate (Python `daemon._WARM_DONE`, `_warm_ocr`, `_wait_for_warm`).
//!
//! 2026-10-08 (W11b): the first `find_text` after a daemon start was cold (1,140 ms against 174 ms
//! warm). The warm-up starts first thing in the daemon's start-up, covers the whole first-call path
//! (monitors, a few pixels of capture, the OCR engine and one recognition), and OCR actions that arrive
//! while it still runs wait for it, up to a cap, so they find the engine warm instead of racing the
//! warm-up through the same cold start.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::backend::{Backend, CaptureTarget, Rect};

/// How long an OCR action waits for a running warm-up (`WARM_WAIT_S`). A stuck warm-up must not stall
/// every OCR call for good.
pub const WARM_WAIT: Duration = Duration::from_secs(8);

/// Actions that read the screen through OCR: what the gate holds back. Plain clicks, keys, windows and
/// files never wait (`_OCR_ACTIONS`).
pub const OCR_ACTIONS: [&str; 7] = [
    "find_text",
    "click_text",
    "read_text",
    "zoom",
    "wait_for_text",
    "read_field",
    "replace_word",
];

/// "Is the warm-up still running?", with a way to wait for the answer to become no. The default state
/// is not warming, so a process that never calls [`WarmGate::begin`] never waits.
pub struct WarmGate {
    warming: Mutex<bool>,
    done: Condvar,
    cap: Duration,
}

impl Default for WarmGate {
    fn default() -> Self {
        Self::new()
    }
}

impl WarmGate {
    /// A gate that is not warming, with the standard cap.
    pub fn new() -> Self {
        Self::with_cap(WARM_WAIT)
    }

    /// A gate with its own cap (tests).
    pub fn with_cap(cap: Duration) -> Self {
        Self {
            warming: Mutex::new(false),
            done: Condvar::new(),
            cap,
        }
    }

    /// The warm-up is about to start: OCR actions wait from now until [`WarmGate::finish`].
    pub fn begin(&self) {
        *self.warming.lock().unwrap_or_else(|e| e.into_inner()) = true;
    }

    /// The warm-up is over (whatever happened): release everyone waiting.
    pub fn finish(&self) {
        *self.warming.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.done.notify_all();
    }

    /// Is the warm-up running?
    pub fn is_warming(&self) -> bool {
        *self.warming.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hold an OCR action that arrives while the warm-up runs. `args` is the whole `computer` request
    /// (a single action or `{"actions": [...]}`); any step in [`OCR_ACTIONS`] makes it wait. Returns
    /// true when it had to wait. Python `_wait_for_warm`.
    pub fn wait_for(&self, args: &Value) -> bool {
        if !self.is_warming() || !reads_screen_text(args) {
            return false;
        }
        let start = Instant::now();
        let mut warming = self.warming.lock().unwrap_or_else(|e| e.into_inner());
        while *warming {
            let left = self.cap.saturating_sub(start.elapsed());
            if left.is_zero() {
                break;
            }
            let (guard, _) = self
                .done
                .wait_timeout(warming, left)
                .unwrap_or_else(|e| e.into_inner());
            warming = guard;
        }
        true
    }
}

/// Does any step of this `computer` request read the screen through OCR?
fn reads_screen_text(args: &Value) -> bool {
    let is_ocr = |step: &Value| {
        step.get("action")
            .and_then(Value::as_str)
            .is_some_and(|a| OCR_ACTIONS.contains(&a))
    };
    match args.get("actions") {
        Some(Value::Array(steps)) if !steps.is_empty() => steps.iter().any(is_ocr),
        _ => is_ocr(args),
    }
}

/// Run the first-call path once so the first real call does none of the one-time work: list the
/// monitors, grab a few pixels, and build the OCR engine AND run a recognition through it. One failing
/// step (no desktop session, a locked screen) must not skip the rest. Always releases the gate.
/// Python `_warm_ocr` and `_warm_in_background`.
pub fn run_warmup(gate: &WarmGate, backend: &dyn Backend) {
    let _ = backend.displays();
    let _ = backend.capture(CaptureTarget::Region(Rect::from_xywh(0, 0, 8, 8)));
    let _ = backend.warm_ocr();
    gate.finish();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use serde_json::json;

    use super::*;
    use crate::backend::fake::FakeBackend;

    // Ported from tests/test_daemon_warmup.py.

    #[test]
    fn the_default_state_is_not_warming_so_nothing_ever_waits() {
        let gate = WarmGate::with_cap(Duration::from_secs(5));
        let t = Instant::now();
        assert!(!gate.wait_for(&json!({"action": "find_text", "text": "x"})));
        assert!(t.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn an_ocr_action_waits_for_a_running_warmup_up_to_the_cap() {
        let gate = WarmGate::with_cap(Duration::from_millis(200));
        gate.begin();
        let t = Instant::now();
        assert!(gate.wait_for(&json!({"action": "find_text", "text": "x"})));
        assert!(
            t.elapsed() >= Duration::from_millis(150),
            "waited, then gave up at the cap"
        );
        assert!(t.elapsed() < Duration::from_secs(5), "did not hang");
    }

    #[test]
    fn an_ocr_action_is_released_the_moment_the_warmup_finishes() {
        let gate = Arc::new(WarmGate::with_cap(Duration::from_secs(10)));
        gate.begin();
        let g = gate.clone();
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            g.finish();
        });
        let t = Instant::now();
        let args =
            json!({"actions": [{"action": "left_click"}, {"action": "click_text", "text": "OK"}]});
        assert!(gate.wait_for(&args));
        assert!(t.elapsed() < Duration::from_secs(5));
        releaser.join().unwrap();
    }

    #[test]
    fn actions_that_do_not_read_the_screen_never_wait() {
        let gate = WarmGate::with_cap(Duration::from_secs(10));
        gate.begin();
        for args in [
            json!({"action": "left_click", "coordinate": [1, 2]}),
            json!({"action": "list_windows"}),
            json!({"actions": [{"action": "key", "text": "ctrl+s"}, {"action": "type"}]}),
        ] {
            let t = Instant::now();
            assert!(!gate.wait_for(&args), "{args}");
            assert!(t.elapsed() < Duration::from_millis(500));
        }
    }

    #[test]
    fn the_warmup_runs_the_whole_path_and_always_releases_waiters() {
        let fake = FakeBackend::new();
        let gate = WarmGate::new();
        gate.begin();
        run_warmup(&gate, &fake);
        assert!(!gate.is_warming());
        assert!(fake.capture_count() >= 1, "a few pixels were grabbed");
    }

    #[test]
    fn a_failing_screen_step_does_not_skip_the_release() {
        let fake = FakeBackend::new();
        fake.clear_screen(); // capture fails: no desktop session
        let gate = WarmGate::new();
        gate.begin();
        run_warmup(&gate, &fake);
        assert!(!gate.is_warming());
    }
}
