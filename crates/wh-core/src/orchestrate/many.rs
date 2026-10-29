//! One call, many actions, and everything around it. Port of `computer.run_many` (the batch loop, the
//! per-call options, the trailing settle and picture) and the computer branch of
//! `daemon._execute_tool` (pause, acknowledgement, rate cap, audit entry, failure fields).
//!
//! `{"actions": [{...}, {...}]}` or a single action. A batch stops at the first error and says which
//! step failed. It returns every zoom or screenshot taken inside the batch, plus ONE screenshot at the
//! end after the screen settles, so "hover spot A, zoom, hover spot B, zoom" compares two spots in one
//! model turn.

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::args::{Args, flag};
use super::error::{StepError, StepResult};
use super::guard::normalize_only_app;
use super::{CallOpts, Computer, Ctx};
use crate::actions::{POINTER_ACTIONS, is_action, is_excel_read, is_passive};
use crate::audit::log_action;
use crate::backend::Rect;
use crate::caller::Caller;
use crate::config::{Config, truthy};
use crate::destructive::Policy;
use crate::keys::{is_opener, normalize_key_text};
use crate::scale::{CallScale, Coords, pick_display};
use crate::textutil::py_str;

/// Maximum actions after expanding every recipe. Checked before any input.
pub const ACTION_LIMIT: usize = 50;
/// Maximum pictures retained by one call, including its trailing screenshot.
pub const IMAGE_LIMIT: usize = 8;

/// A failure recorded as data, never inferred from arbitrary application text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFailure {
    /// One-based action index after recipe expansion.
    pub step: u32,
    /// Typed cause.
    pub error: StepError,
}

/// What a call returns: the text, every picture taken, and their MIME type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManyResult {
    /// One line per step, then the settle line.
    pub text: String,
    /// Base64 pictures, in the order `[image N]` numbers them.
    pub images: Vec<String>,
    /// `image/jpeg` or `image/png`.
    pub mime: &'static str,
    /// First stopping error, if any.
    pub failure: Option<StepFailure>,
    /// Successfully completed actions, excluding skipped or failed steps.
    pub completed_steps: u32,
    /// Input counter changed during the call.
    pub input_sent: bool,
    /// Total time, including waits, on the tool's monotonic clock.
    pub elapsed_ms: u64,
    /// A step returned cells of the owner's spreadsheet, so the audit log must not keep the text.
    pub private_output: bool,
}

/// What the daemon sends back for a `computer` call (Python `_execute_tool`'s dict).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    /// The call ran (a failed step inside a batch is still `ok`; see `failed_step`).
    pub ok: bool,
    /// The result text (`ok` only).
    pub result: Option<String>,
    /// Why the call was refused or failed (not `ok`), or the first line of a failed step's error.
    pub error: Option<String>,
    /// Pictures, base64.
    pub images: Vec<String>,
    /// Their MIME type.
    pub mime: &'static str,
    /// The 1-based step that failed, when the text names one.
    pub failed_step: Option<u32>,
    /// Optional session identifier, never an authorization grant.
    pub session_id: Option<String>,
    /// Fresh observation identifier for this session.
    pub snapshot_id: Option<String>,
    /// Additional counters only when requested with profile=true.
    pub metrics: Option<Value>,
}

impl ToolOutcome {
    /// The JSON the HTTP daemon returns: `{"ok", "result", "content": [...], "failed_step", "error"}`
    /// or `{"ok": false, "error"}`.
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("ok".into(), Value::from(self.ok));
        if let Some(text) = &self.result {
            out.insert("result".into(), Value::from(text.clone()));
            let mut content = vec![json!({"type": "text", "text": text})];
            for im in &self.images {
                content.push(json!({"type": "image", "data": im, "mimeType": self.mime}));
            }
            out.insert("content".into(), Value::Array(content));
            if let Some(n) = self.failed_step {
                out.insert("failed_step".into(), Value::from(n));
            }
        }
        if let Some(e) = &self.error {
            out.insert("error".into(), Value::from(e.clone()));
        }
        if let Some(id) = &self.session_id {
            out.insert("session_id".into(), json!(id));
        }
        if let Some(id) = &self.snapshot_id {
            out.insert("snapshot_id".into(), json!(id));
        }
        if let Some(metrics) = &self.metrics {
            out.insert("metrics".into(), metrics.clone());
        }
        Value::Object(out)
    }
}

#[cfg(test)]
static STEP_FAILED: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"step (\d+) \([^)]*\) failed: (.+)").expect("STEP_FAILED")
});

/// `{"failed_step": n, "error": "..."}` if `text` names a failed step (Python
/// `_batch_failure_fields`). A script can read it without string-matching the result.
#[cfg(test)]
pub fn batch_failure_fields(text: &str) -> Option<(u32, String)> {
    let m = STEP_FAILED.captures(text)?;
    let n = m[1].parse().ok()?;
    let first = m[2].lines().next().unwrap_or("").to_string();
    Some((n, first))
}

impl CallOpts {
    /// The configured defaults with this call's own arguments layered on top. Every call starts from
    /// the defaults: present (even a falsy value like `"only_apps": []`) is that call's exact choice
    /// and absent is always the default, never a leftover from someone else's earlier call
    /// (D1.1, 2026-09-21: a caller that asked for norm1000 coordinates, a smaller screenshot or a
    /// second monitor leaked that choice into a later call from a different caller).
    pub(crate) fn from_args(c: &Computer, args: &Args, policy: Policy) -> StepResult<Self> {
        let budget_ms = match args.get("budget_ms") {
            None => 60_000,
            Some(v) => v
                .as_u64()
                .filter(|n| (1..=120_000).contains(n))
                .ok_or_else(|| StepError::value("budget_ms must be an integer from 1 to 120000"))?,
        };
        let budget = crate::request::remaining().map_or(Duration::from_millis(budget_ms), |left| {
            left.min(Duration::from_millis(budget_ms))
        });
        let scale = CallScale::from_args(&c.cfg, &Value::Object(args.clone()))?;
        let display: Option<Rect> = match scale.display_index {
            Some(n) => {
                let mut mons = c.backend.displays()?;
                crate::scale::sort_displays(&mut mons);
                Some(pick_display(&mons, n)?)
            }
            None => None,
        };
        let only_apps: BTreeSet<String> = match args.get("only_apps") {
            Some(Value::Array(list)) => list
                .iter()
                .map(|a| normalize_only_app(&py_str(a)))
                .collect(),
            Some(Value::String(s)) if !s.is_empty() => BTreeSet::from([normalize_only_app(s)]),
            _ => BTreeSet::new(),
        };
        Ok(Self {
            cancel_generation: c.pause.generation(),
            saved_paths: Default::default(),
            private_output: Default::default(),
            input_follows: Default::default(),
            deadline: Some(c.clock.now().saturating_add(budget)),
            read_only: args.get("read_only").is_some_and(truthy) || truthy(&c.cfg.get("read_only")),
            helper_images: args.get("max_images").is_none_or(|v| v.as_u64() != Some(0))
                && !args.get("compact").is_some_and(truthy),
            coords: scale.coords,
            long_edge: scale.long_edge,
            display,
            only_apps,
            snap_call: args.get("snap").is_some_and(truthy),
            policy,
            scope: super::scope::ScopeCell::default(),
        })
    }

    /// The configured defaults and nothing else (a read outside a `computer` call).
    pub(crate) fn defaults(cfg: &Config, policy: Policy) -> Self {
        let long = cfg
            .shot_long()
            .clamp(crate::scale::SHOT_LONG_MIN, crate::scale::SHOT_LONG_MAX);
        Self {
            cancel_generation: 0,
            saved_paths: Default::default(),
            private_output: Default::default(),
            input_follows: Default::default(),
            deadline: None,
            read_only: false,
            helper_images: true,
            coords: Coords::from_name(&cfg.coords()),
            long_edge: long as u32,
            display: None,
            only_apps: BTreeSet::new(),
            snap_call: false,
            policy,
            scope: super::scope::ScopeCell::default(),
        }
    }
}

/// `on_fail: "skip"`: carry on after this step fails instead of stopping the batch.
fn on_fail_skips(step: &Args) -> bool {
    match step.get("on_fail") {
        Some(v) if truthy(v) => py_str(v).to_lowercase() == "skip",
        _ => false,
    }
}

fn step_action_text(step: &Args) -> String {
    match step.get("action") {
        None => "None".to_string(),
        Some(v) => py_str(v),
    }
}

fn steps_observe(args: &Value) -> bool {
    let observes = |v: &Value| {
        matches!(
            v.get("action").and_then(Value::as_str),
            Some("read" | "read_field" | "read_text" | "find_text" | "screenshot" | "zoom")
        )
    };
    match args.get("actions").and_then(Value::as_array) {
        Some(steps) => steps.iter().any(observes),
        None => observes(args),
    }
}

/// `excel` with `op: read` in this step: a look at a workbook, not a change.
fn is_excel_read_step(step: &Value, action: &str) -> bool {
    is_excel_read(
        action,
        step.get("op").and_then(Value::as_str).unwrap_or_default(),
    )
}

fn only_observes(args: &Value) -> bool {
    if args.get("scope").is_some_and(truthy) {
        return false;
    }
    let passive = |v: &Value| {
        v.get("action")
            .and_then(Value::as_str)
            .is_some_and(|a| (is_passive(a) && a != "clipboard_set") || is_excel_read_step(v, a))
            && !v.get("wake").is_some_and(truthy)
            && !v.get("save_to").is_some_and(truthy)
    };
    match args.get("actions").and_then(Value::as_array) {
        Some(steps) => !steps.is_empty() && steps.iter().all(passive),
        None => passive(args),
    }
}

impl Ctx<'_> {
    /// After F2: wait for the rename box's caret. 2026-09-21: twice, right after Explorer opened, F2's
    /// box came late, the typed name was lost and Return kept the old name.
    ///
    /// F2 means "rename" only in File Explorer's file list. Elsewhere (Excel edits a cell in place with
    /// no separate caret-bearing box, a game's own F2 hotkey) no caret ever appears and this aborted
    /// every batch that used F2 for anything else. Only abort when File Explorer is actually in front;
    /// elsewhere just give the app a moment and move on. Python `_wait_rename_box`.
    fn wait_rename_box(&self) -> StepResult<()> {
        let max_s = 1.5;
        self.sleep(0.1);
        let (caret, _) = self.poll(
            || self.be().has_caret(None).unwrap_or(false).then_some(()),
            max_s - 0.1,
            0.05,
        );
        if caret.is_some() {
            self.sleep(0.05);
            return Ok(());
        }
        let in_explorer = self
            .fg_root()
            .and_then(|w| self.info(w))
            .is_some_and(|i| super::ctx::is_file_manager(&i));
        if !in_explorer {
            return Ok(());
        }
        // No box: stop the batch here. 2026-09-21: nothing was selected, F2 did nothing, and the
        // batch's Return then OPENED the file (in the owner's Notepad) instead of confirming a rename.
        Err(StepError::target(
            "F2 opened no rename box (is the file selected? click it once first); stopped before \
             the rest of the batch",
        ))
    }

    /// After an opener shortcut: wait until the foreground window or focused control changes and the
    /// screen calms, capped at `max_s`. Returns at once if nothing opened (settle's own cap). Win11's
    /// XAML apps (Notepad, new Explorer) keep one focus handle, so a missing change is not proof
    /// nothing opened: stop polling after 0.4 s and let settle watch the pixels. Python
    /// `_wait_focus_change`.
    ///
    /// `next_finds_its_own_target`: the next step (`file_dialog`) waits for the dialog's name box
    /// itself, so once focus has moved the screen settle is skipped (2026-10-28: saved ~250 ms per
    /// Save As).
    fn wait_focus_change(&self, next_finds_its_own_target: bool) -> StepResult<()> {
        let max_s = 1.5;
        let (fg0, focus0) = self.c.mem().pre_key;
        let t = self.now();
        let mut moved = false;
        self.sleep(0.03);
        while self.now() - t < 0.4 {
            self.check_cancelled()?;
            let fg = self.be().foreground_window().ok().flatten();
            let focus = self.be().focus_token().unwrap_or(0);
            if fg != fg0 || focus != focus0 {
                moved = true;
                break;
            }
            self.sleep(0.03);
        }
        if moved && next_finds_its_own_target {
            return Ok(());
        }
        self.settle(Some((max_s - (self.now() - t)).max(0.2)), 0.15, 2)?;
        Ok(())
    }

    /// The batch loop. Python `run_many` after the per-call options are set.
    fn run_steps(&self, args: &Args) -> StepResult<ManyResult> {
        let start = self.clock().now();
        let compact = args.get("compact").is_some_and(truthy);
        if args
            .get("max_chars")
            .is_some_and(|v| !v.as_u64().is_some_and(|n| (128..=64_000).contains(&n)))
        {
            return Err(StepError::value(
                "max_chars must be an integer from 128 to 64000",
            ));
        }
        let max_images = match args.get("max_images") {
            None => IMAGE_LIMIT,
            Some(v) => v
                .as_u64()
                .filter(|n| *n <= IMAGE_LIMIT as u64)
                .ok_or_else(|| StepError::value("max_images must be an integer from 0 to 8"))?
                as usize,
        };
        let want_shot = flag(args.get("shot"), !compact) && max_images > 0;
        let (steps, own_args): (Vec<Value>, bool) = match args.get("actions") {
            Some(Value::Array(list)) if !list.is_empty() => (list.clone(), false),
            Some(v) if truthy(v) && !v.is_array() => (vec![v.clone()], false),
            _ => (vec![Value::Object(args.clone())], true),
        };
        // The call's own arguments are the one step when it is a lone action (not a recipe, whose
        // steps are its own): its `scope` was applied for the whole call already.
        let own_args = own_args && !crate::recipes::is_recipe_step(&steps[0]);
        // 2026-10-08: a `recipe` step becomes that recipe's own steps, so everything below (expect, the
        // stop at the first failure, settling, the safety checks in `run`) treats them like any batch.
        let items = crate::recipes::expand(&self.c.recipes, &steps, &self.o.only_apps)
            .map_err(StepError::value)?;
        let steps: Vec<Value> = items.iter().map(|item| item.step.clone()).collect();
        for (index, step) in steps.iter().enumerate() {
            if !step.is_object() {
                return Err(StepError::value(format!(
                    "step {} must be an object with an action",
                    index + 1
                )));
            }
        }
        let requested_images = steps
            .iter()
            .filter(|s| {
                matches!(
                    s.get("action").and_then(Value::as_str),
                    Some("screenshot" | "zoom")
                ) && !s.get("save_to").is_some_and(truthy)
            })
            .count();
        let trailing = want_shot
            && steps
                .last()
                .and_then(|s| s.get("action"))
                .and_then(Value::as_str)
                .is_some_and(|a| !Self::no_trailing_shot(Some(a)));
        if requested_images + usize::from(trailing) > max_images {
            return Err(StepError::value(
                "image budget exceeded; use shot=false, fewer image actions, or max_images<=8; no actions sent",
            ));
        }
        if self.o.read_only {
            for step in &steps {
                let action = step.get("action").and_then(Value::as_str).unwrap_or("");
                if !matches!(
                    action,
                    "screenshot"
                        | "status"
                        | "zoom"
                        | "read"
                        | "find_text"
                        | "list_windows"
                        | "cursor_position"
                        | "list_files"
                        | "recipe_list"
                        | "clipboard_get"
                ) && !is_excel_read_step(step, action)
                    || step.get("wake").is_some_and(truthy)
                    || step.get("scope").is_some_and(truthy)
                    || step.get("save_to").is_some_and(truthy)
                {
                    return Err(StepError::permission(
                        "read_only refuses this action or a focus-changing observation; no actions sent",
                    ));
                }
            }
        }
        // All request-wide budgets and expanded action policies are checked before focus can change.
        self.apply_call_scope(args)?;
        let mut notes: Vec<String> = Vec::new();
        let mut shots: Vec<String> = Vec::new();
        let mut failure = None;
        let mut completed_steps = 0;
        let mut last_executed = None;
        let input_before = self.be().input_events();
        let mut acted_in_call = false; // an acting step has already finished in this call
        let mut pre_input_failure = false; // a step failed before anything was sent
        // profile=true: wall time per step, its settle included (2026-10-28).
        let mut step_mark: Option<(String, std::time::Instant)> = None;
        for (i, item) in items.iter().enumerate() {
            let Value::Object(step) = &item.step else {
                return Err(StepError::value(format!(
                    "step {} must be an object with an action",
                    i + 1
                )));
            };
            if let Some((action, t)) = step_mark.take() {
                crate::prof::step_done(action, t.elapsed());
            }
            step_mark = Some((step_action_text(step), std::time::Instant::now()));
            let input_follows = items.get(i + 1).is_some_and(|next| {
                next.step
                    .get("action")
                    .and_then(Value::as_str)
                    .is_some_and(|a| is_action(a) && !is_passive(a) && !a.starts_with("wait"))
            });
            self.o
                .input_follows
                .store(input_follows, std::sync::atomic::Ordering::Relaxed);
            if !item.header.is_empty() {
                notes.push(item.header.clone()); // a recipe step: say which recipe is running
            }
            let release = step.get("action").and_then(Value::as_str) == Some("left_mouse_up");
            let outcome = (if release {
                Ok(())
            } else {
                self.check_cancelled()
            })
            .and_then(|()| self.skip_reason(step))
            .and_then(|skip| {
                if skip.is_empty() {
                    // a recipe's own only_apps narrows the call's for its steps
                    self.run_scoped(step, item.scope.as_ref(), own_args)
                        .map(Some)
                } else {
                    notes.push(format!(
                        "step {} ({}) skipped: {skip}",
                        i + 1,
                        step_action_text(step)
                    ));
                    Ok(None)
                }
            });
            let (note, img) = match outcome {
                Ok(Some(done)) => done,
                Ok(None) => continue, // a `when` that does not hold right now: leave this step out
                Err(exc) => {
                    if on_fail_skips(step) && self.check_cancelled().is_ok() {
                        // on_fail "skip": say so, then carry on (not worded "failed:")
                        notes.push(format!(
                            "step {} ({}) errored and was skipped: {exc}",
                            i + 1,
                            step_action_text(step)
                        ));
                        continue;
                    }
                    notes.push(format!(
                        "step {} ({}) failed: {}",
                        i + 1,
                        step_action_text(step),
                        exc
                    ));
                    // 2026-10-08 (W3): a validation error, unknown action, bad or missing target or a
                    // refusal (ValueError incl. TargetError, PermissionError incl. destructive and
                    // protected) raised before any input went out leaves the screen as the caller last
                    // saw it, so no screenshot is attached (a bogus action used to cost a 126 KB
                    // image). Anything else, or any failure after earlier steps acted or SendInput
                    // ran, still gets the picture.
                    pre_input_failure = !acted_in_call
                        && self.be().input_events() == input_before
                        && exc.is_validation();
                    failure = Some(StepFailure {
                        step: (i + 1) as u32,
                        error: exc,
                    });
                    break;
                }
            };
            let step_action = step
                .get("action")
                .map(|v| py_str(v).trim().to_string())
                .unwrap_or_default();
            completed_steps += 1;
            last_executed = Some(step_action.clone());
            if is_action(&step_action) && !is_passive(&step_action) {
                acted_in_call = true;
            }
            let has_img = img.is_some();
            if let Some(img) = img {
                if shots.len() >= max_images {
                    failure = Some(StepFailure {
                        step: (i + 1) as u32,
                        error: StepError::value("image limit reached; remaining actions stopped"),
                    });
                    notes.push(format!("step {} failed: image limit reached", i + 1));
                    break;
                }
                shots.push(img);
                notes.push(format!("{note} [image {}]", shots.len()));
            } else {
                notes.push(note.clone());
            }
            if !release && let Err(error) = self.check_cancelled() {
                failure = Some(StepFailure {
                    step: (i + 1) as u32,
                    error,
                });
                break;
            }
            if !has_img && i + 1 < steps.len() {
                let raw_action = step.get("action").and_then(Value::as_str);
                let pointer_or_input = raw_action.is_some_and(|a| {
                    POINTER_ACTIONS.contains(&a) || matches!(a, "key" | "type" | "window")
                });
                if pointer_or_input {
                    let text = super::args::text(step, "text");
                    let next_is_pointer = steps[i + 1]
                        .get("action")
                        .and_then(Value::as_str)
                        .is_some_and(|a| POINTER_ACTIONS.contains(&a));
                    if raw_action == Some("key") && next_is_pointer {
                        // a key that selects something, then a click: no redraw to wait for
                        self.sleep(0.08);
                    } else if raw_action == Some("key") && normalize_key_text(&text) == "f2" {
                        if let Err(exc) = self.wait_rename_box() {
                            notes.push(format!("step {} and later skipped: {exc}", i + 2));
                            failure = Some(StepFailure {
                                step: (i + 1) as u32,
                                error: exc,
                            });
                            break;
                        }
                    } else if raw_action == Some("key") && is_opener(&normalize_key_text(&text)) {
                        // 2026-09-21 eval: ctrl+n then type in one batch saved an EMPTY file: the
                        // paste landed before Notepad's new tab (or the Save As dialog) had focus.
                        let next_is_dialog = items.get(i + 1).is_some_and(|n| {
                            n.step.get("action").and_then(Value::as_str) == Some("file_dialog")
                        });
                        if let Err(error) = self.wait_focus_change(next_is_dialog) {
                            failure = Some(StepFailure {
                                step: (i + 1) as u32,
                                error,
                            });
                            break;
                        }
                    } else {
                        // One calm read is enough here: the next step acts anyway (W4, 2026-10-08).
                        if let Err(error) = self.settle(Some(0.35), 0.0, 1) {
                            failure = Some(StepFailure {
                                step: (i + 1) as u32,
                                error,
                            });
                            break;
                        }
                    }
                }
            }
        }
        if let Some((action, t)) = step_mark.take() {
            crate::prof::step_done(action, t.elapsed());
        }
        let final_mark = std::time::Instant::now();
        let last = last_executed.as_deref();
        if self.cfg().auto_shot()
            && !Self::no_trailing_shot(last)
            && !pre_input_failure
            && self.check_cancelled().is_ok()
        {
            let acted = last.is_some_and(|a| {
                !matches!(a, "wait" | "wait_for_text" | "wait_for_file") && is_action(a)
            });
            if want_shot || acted {
                // Acting calls always settle, even with shot=false: the next call must find the UI at
                // rest.
                let min_s = if acted { self.settle_floor_s() } else { 0.0 };
                let background = !want_shot
                    && self.be().settle_in_background()
                    && self.settle_in_background(min_s);
                let observation = (|| -> StepResult<()> {
                    if background {
                        notes.push("screen still settling; the next call waits for it".into());
                        return Ok(());
                    }
                    let settled = self.settle_looks(None, min_s, 2)?;
                    let waited = settled.elapsed.as_secs_f64();
                    self.remember_target();
                    if want_shot && shots.len() < max_images {
                        let final_frame = match settled.full() {
                            Some(image) => image,
                            None => self.grab(self.display_rect())?,
                        };
                        shots.push(self.encode_full_frame(&final_frame)?);
                        notes.push(format!(
                            "screen settled in {:.0} ms [image {}: {}]",
                            waited * 1000.0,
                            shots.len(),
                            self.image_tag()
                        ));
                    } else {
                        notes.push(format!("screen settled in {:.0} ms", waited * 1000.0));
                    }
                    Ok(())
                })();
                if let Err(error) = observation {
                    notes.push(format!("trailing observation failed: {error}"));
                    if failure.is_none() {
                        failure = Some(StepFailure {
                            step: steps.len().max(1) as u32,
                            error,
                        });
                    }
                }
            }
        }
        if final_mark.elapsed() >= std::time::Duration::from_millis(1) {
            crate::prof::step_done("(final settle)".into(), final_mark.elapsed());
        }
        Ok(ManyResult {
            text: notes.join("\n"),
            images: shots,
            mime: self.mime(),
            failure,
            completed_steps,
            input_sent: self.be().input_events() != input_before,
            private_output: self
                .o
                .private_output
                .load(std::sync::atomic::Ordering::Relaxed),
            elapsed_ms: self
                .clock()
                .now()
                .saturating_sub(start)
                .as_millis()
                .min(u64::MAX as u128) as u64,
        })
    }
}

impl Computer {
    /// A context for work outside a `computer` call: the configured defaults, no allowlist.
    pub(crate) fn read_ctx<'a>(&'a self, who: &'a Caller) -> Ctx<'a> {
        let mut options = CallOpts::defaults(&self.cfg, Policy::REFUSE);
        options.cancel_generation = self.pause.generation();
        options.read_only = truthy(&self.cfg.get("read_only"));
        options.deadline =
            crate::request::remaining().map(|left| self.clock.now().saturating_add(left));
        Ctx {
            c: self,
            o: options,
            who,
        }
    }

    /// The `computer` tool: one action or `{"actions": [...]}`. Stops at the first failing step and
    /// names it. Returns the text and every picture the batch took, plus one at the end after the
    /// screen settles. `policy` is this call's destructive opt-in (see
    /// [`Policy::take_from_args`]); `who` is the process driving this connection. Python `run_many`.
    pub fn run_many(
        &self,
        args: &Args,
        policy: Policy,
        who: &Caller,
    ) -> Result<ManyResult, StepError> {
        let o = CallOpts::from_args(self, args, policy)?;
        let ctx = Ctx { c: self, o, who };
        let joined = std::time::Instant::now();
        if self.join_background_settle() {
            crate::prof::step_done("(previous settle)".into(), joined.elapsed());
            ctx.remember_target();
        }
        if ctx.o.read_only && args.get("scope").is_some_and(truthy) {
            return Err(StepError::permission(
                "read_only refuses scope because it may raise a window",
            ));
        }
        // after `only_apps`, because the window goes through the same allowlist check
        ctx.run_steps(args)
    }

    /// The whole `computer` route of the daemon: the per-call opt-in, the pause flag, the
    /// acknowledgement gate, the rate cap, one action at a time, the audit entry, and the failure
    /// fields. Never fails: a refusal or an error comes back as `ok: false`. Python
    /// `_execute_tool(path="computer", ...)`.
    pub fn execute(&self, args: &Value, who: &Caller) -> ToolOutcome {
        crate::prof::reset();
        self.idle.touch(self.clock.as_ref());
        if args.get("action").and_then(Value::as_str) == Some("status")
            && args.get("actions").is_none()
        {
            return ToolOutcome {
                ok: true,
                result: Some(self.status().to_string()),
                error: None,
                images: Vec::new(),
                mime: "image/jpeg",
                failed_step: None,
                session_id: None,
                snapshot_id: None,
                metrics: None,
            };
        }
        let _one_at_a_time = match self.lock_tool(only_observes(args)) {
            Some(guard) => guard,
            None => {
                return self.fail(
                    args,
                    &StepError::runtime(
                        "desktop busy; no input sent; wait for current request before retrying",
                    ),
                );
            }
        };
        if crate::request::remaining().is_some_and(|d| d.is_zero()) {
            return self.fail(
                args,
                &StepError::permission("request expired before execution; no input sent"),
            );
        }
        let outcome = self.execute_locked(args, who);
        self.idle.touch(self.clock.as_ref());
        outcome
    }

    fn fail(&self, args: &Value, err: &StepError) -> ToolOutcome {
        let mut error = err.message().to_string();
        log_action(self.cfg.paths(), "computer", args, &error, false);
        error = self.fails.note_failure("computer", args, &error);
        ToolOutcome {
            ok: false,
            result: None,
            error: Some(error),
            images: Vec::new(),
            mime: "image/jpeg",
            failed_step: None,
            session_id: None,
            snapshot_id: None,
            metrics: None,
        }
    }

    fn execute_locked(&self, raw: &Value, who: &Caller) -> ToolOutcome {
        let Value::Object(map) = raw else {
            return self.fail(raw, &StepError::value("tool args must be an object"));
        };
        let mut map = map.clone();
        // The owner's hard line. Per call, never sticky: the refusals are on unless THIS call asked
        // for them off, and the next call starts from on again.
        let profile = self.cfg.profile();
        let mut policy = Policy::take_from_args(&mut map);
        if !profile.honours_call_opt_in() {
            // strict: a flag the model wrote is not the owner's approval
            policy = Policy::REFUSE;
        }
        let args = Value::Object(map.clone());
        let release = map.get("action").and_then(Value::as_str) == Some("left_mouse_up")
            && !map.contains_key("actions");
        if self.pause.is_paused() && !release {
            return self.fail(&args, &StepError::permission("paused by user hotkey"));
        }
        if !release && !only_observes(&args) {
            if let Some(why) = self.game_lock() {
                return self.fail(
                    &args,
                    &StepError::permission(format!("gaming lock: {why}. No input was sent.")),
                );
            }
            if profile.needs_allowlist()
                && !args.get("only_apps").is_some_and(truthy)
                && self.cfg.only_apps_default().is_empty()
            {
                return self.fail(
                    &args,
                    &StepError::permission(
                        "the strict profile needs only_apps on every call that acts; no input was sent",
                    ),
                );
            }
        }
        if let Err(refused) = crate::ack::check_acknowledged(&self.cfg, "computer", &args) {
            return self.fail(&args, &refused.into());
        }
        if !self.rate.check(self.clock.as_ref()) {
            return self.fail(&args, &StepError::runtime("rate limit exceeded"));
        }
        let mut session = match super::sessions::SessionGuard::enter(self, &args, who) {
            Ok(s) => s,
            Err(e) => return self.fail(&args, &e),
        };
        if session.is_none() && map.get("if_changed").is_some_and(truthy) {
            return self.fail(&args, &StepError::value("if_changed requires session_id"));
        }
        if !release
            && let Some(s) = session.as_mut()
            && let Err(e) = s.validate(&args)
        {
            return self.fail(&args, &e);
        }
        if !only_observes(&args) {
            self.observation_epoch
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        match self.run_many(&map, policy, who) {
            Err(e) => self.fail(&args, &e),
            Ok(done) => {
                let steps = match map.get("actions") {
                    Some(Value::Array(a)) if !a.is_empty() => a.len(),
                    _ => 1,
                };
                let ok = done.failure.is_none();
                let mut entry = Map::new();
                entry.insert(
                    "action".into(),
                    map.get("action").cloned().unwrap_or(Value::Null),
                );
                entry.insert("steps".into(), Value::from(steps));
                // 2026-10-08: which recipes ran or were saved (param NAMES only, never values)
                if let Some(Value::Object(extra)) = crate::recipes::audit_fields(&args) {
                    entry.extend(extra);
                }
                // excel steps log what they touched (op, sheet, range, counts, path),
                // never the cells; a call that read cells logs a fixed line, not their text.
                if let Some(extra) = super::excel::audit_fields(&args) {
                    entry.extend(extra);
                }
                let logged = match (&done.failure, done.private_output) {
                    (Some(f), true) => f.error.message().to_string(),
                    (None, true) => "ok; excel cells are not logged".to_string(),
                    _ => done.text.clone(),
                };
                log_action(
                    self.cfg.paths(),
                    "computer",
                    &Value::Object(entry),
                    &logged,
                    ok,
                );
                let failure = done.failure.as_ref();
                let compact = map.get("compact").is_some_and(truthy);
                let limit = map
                    .get("max_chars")
                    .and_then(Value::as_u64)
                    .unwrap_or(if compact { 2_000 } else { 16_000 })
                    .clamp(128, 64_000) as usize;
                let mut text = done.text;
                let mut images = done.images;
                let mut snapshot_id = None;
                if let Some(s) = session.as_mut() {
                    if ok && (!images.is_empty() || steps_observe(&args)) {
                        snapshot_id = s.observe(&args);
                    }
                    if ok && map.get("if_changed").is_some_and(truthy) && only_observes(&args) {
                        use sha2::{Digest, Sha256};
                        let mut hash = Sha256::new();
                        // Include request and output boundaries. Distinct queries cannot suppress one another.
                        hash.update(args.to_string().as_bytes());
                        hash.update([0]);
                        hash.update(text.as_bytes());
                        for im in &images {
                            hash.update([0]);
                            hash.update(im.as_bytes());
                        }
                        if s.unchanged(hash.finalize().into()) {
                            text = "unchanged".into();
                            images.clear();
                        }
                    }
                }
                if text.chars().count() > limit {
                    text = text.chars().take(limit).collect();
                    text.push_str("\n[truncated; request larger max_chars or narrower filter]");
                }
                if let Some(f) = failure
                    && !text.contains(f.error.message())
                {
                    text.push_str(&format!("\nstep {} failed: {}", f.step, f.error));
                }
                if let Some(id) = &snapshot_id {
                    text.push_str(&format!("\n[snapshot {id}]"));
                }
                let profile = map.get("profile").is_some_and(truthy);
                let metrics = profile.then(|| json!({"elapsed_ms": done.elapsed_ms, "completed_steps": done.completed_steps, "input_sent": done.input_sent, "images": images.len(), "image_bytes_base64": images.iter().map(String::len).sum::<usize>(), "phases": crate::prof::take(), "steps": crate::prof::take_steps()}));
                if profile {
                    text.push_str(&format!(
                        "\n[{} ms; {} completed; input_sent={}]",
                        done.elapsed_ms, done.completed_steps, done.input_sent
                    ));
                }
                ToolOutcome {
                    ok,
                    result: Some(text),
                    error: failure.map(|f| f.error.message().to_string()),
                    images,
                    mime: done.mime,
                    failed_step: failure.map(|f| f.step),
                    session_id: session.as_ref().map(|s| s.id.clone()),
                    snapshot_id,
                    metrics,
                }
            }
        }
    }
}
