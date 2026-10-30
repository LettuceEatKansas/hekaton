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
use crate::approval::{self, CallApproval, Entry, Kind, Mode, Need, Offer, Pending};
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
    /// A step stopped for a refusal the person could approve, and the call can ask (see
    /// [`crate::approval`]).
    pub approval: Option<Pending>,
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
    /// The failed step could go ahead if the person agrees on a card: a single-use token and the
    /// card sentence. Only for a call marked `approvable: "card"`.
    pub offer: Option<Offer>,
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
        if let Some(offer) = &self.offer {
            out.insert("approval".into(), offer.to_json());
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
            copied_text: Default::default(),
            deadline: Some(c.clock.now().saturating_add(budget)),
            max_chars: args
                .get("max_chars")
                .and_then(Value::as_u64)
                .map(|n| n.clamp(128, 64_000) as usize),
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
            approval: crate::approval::CallApproval::default(),
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
            copied_text: Default::default(),
            deadline: None,
            max_chars: None,
            read_only: false,
            helper_images: true,
            coords: Coords::from_name(&cfg.coords()),
            long_edge: long as u32,
            display: None,
            only_apps: BTreeSet::new(),
            snap_call: false,
            policy,
            scope: super::scope::ScopeCell::default(),
            approval: crate::approval::CallApproval::default(),
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

/// A click of any kind, the steps that land behind a dialog nobody saw open.
fn is_click(action: Option<&Value>) -> bool {
    action.and_then(Value::as_str).is_some_and(|a| {
        POINTER_ACTIONS.contains(&a) && !matches!(a, "mouse_move" | "scroll" | "left_mouse_up")
    })
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
    /// A new, much smaller window of the same app that took the front since `before` was noted: a
    /// page alert or an app's own dialog. Named with its text, unless the next step clicks words
    /// the dialog itself shows (its OK button, say).
    fn dialog_in_front(&self, before: crate::backend::WindowRef, next: &Args) -> Option<String> {
        let now = self.be().foreground_window().ok().flatten()?;
        if now == before {
            return None;
        }
        let windows = self.be().list_windows(false).ok()?;
        let old = windows.iter().find(|w| w.window == before)?;
        let new = windows.iter().find(|w| w.window == now)?;
        let area = |r: &Rect| i64::from(r.width().max(0)) * i64::from(r.height().max(0));
        if new.pid != old.pid || area(&new.rect) * 5 > area(&old.rect) * 2 {
            return None;
        }
        let title = new.title.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut text = self.be().window_text(now).unwrap_or_default();
        text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text == title || text.is_empty() {
            // Brave's alert names its message only as an element name, which window_text misses.
            let limits = crate::backend::TreeLimits {
                max_items: 20,
                ..crate::backend::TreeLimits::named(Duration::from_millis(300))
            };
            if let Ok(tree) = self.be().element_tree(now, &limits) {
                let words: Vec<String> = tree
                    .root
                    .children
                    .iter()
                    .map(|e| e.name.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|n| !n.is_empty() && *n != title)
                    .collect();
                if !words.is_empty() {
                    text = words.join(" | ");
                }
            }
        }
        let aimed_inside = next
            .get("text")
            .and_then(Value::as_str)
            .map(|t| t.trim().to_lowercase())
            .is_some_and(|t| !t.is_empty() && text.to_lowercase().contains(&t));
        if aimed_inside {
            return None;
        }
        let mut said: String = text.chars().take(200).collect();
        if text.chars().count() > 200 {
            said.push_str("...");
        }
        Some(if said.is_empty() || said == new.title {
            format!("\"{}\"", new.title)
        } else {
            format!("\"{}\" saying \"{said}\"", new.title)
        })
    }

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
        // 2026-10-29: resuming on an approval, the steps before the waiting one already ran in the
        // first call. Their lines and pictures come from the approval; the loop starts at the step
        // that was waiting, and only if it is the very step the approval was issued for.
        let resume = self.o.approval.resume.as_deref();
        let resume_at = match resume {
            None => 1,
            Some(r) => {
                let same = steps
                    .get(r.index.wrapping_sub(1))
                    .and_then(Value::as_object)
                    .is_some_and(|s| approval::step_id(s) == r.step_id);
                if !same {
                    return Err(StepError::permission(APPROVAL_MISMATCH));
                }
                r.index
            }
        };
        // All request-wide budgets and expanded action policies are checked before focus can change.
        self.apply_call_scope(args)?;
        let mut notes: Vec<String> = resume.map(|r| r.notes.clone()).unwrap_or_default();
        let mut shots: Vec<String> = resume.map(|r| r.shots.clone()).unwrap_or_default();
        let mut failure = None;
        let mut completed_steps = (resume_at - 1) as u32;
        let mut last_executed = None;
        let input_before = self.be().input_events();
        let mut acted_in_call = resume_at > 1; // an acting step has already finished in this call
        let asking = self.o.approval.mode != Mode::Off;
        let mut pending: Option<Pending> = None; // a step stopped for a yes from the person
        let mut pre_input_failure = false; // a step failed before anything was sent
        // profile=true: wall time per step, its settle included (2026-10-28).
        let mut step_mark: Option<(String, std::time::Instant)> = None;
        let mut dialog_watch: Option<crate::backend::WindowRef> = None;
        for (i, item) in items.iter().enumerate() {
            let Value::Object(step) = &item.step else {
                return Err(StepError::value(format!(
                    "step {} must be an object with an action",
                    i + 1
                )));
            };
            if i + 1 < resume_at {
                continue; // ran in the call that asked
            }
            if let Some((action, t)) = step_mark.take() {
                crate::prof::step_done(action, t.elapsed());
            }
            step_mark = Some((step_action_text(step), std::time::Instant::now()));
            if let Some(before) = dialog_watch.take()
                && let Some(dialog) = self.dialog_in_front(before, step)
            {
                let msg = format!(
                    "a dialog opened in front after step {i}: {dialog}; later clicks would land behind it, so step {} ({}) and the rest were not run",
                    i + 1,
                    step_action_text(step)
                );
                notes.push(format!("step {} not run: {msg}", i + 1));
                failure = Some(StepFailure {
                    step: (i + 1) as u32,
                    error: StepError::runtime(msg),
                });
                break;
            }
            // 2026-10-29 (form job): a click that opens a page alert, then a click in the same batch,
            // landed behind Brave's "This page says" box without a word. Note the front window first.
            if is_click(step.get("action"))
                && items
                    .get(i + 1)
                    .is_some_and(|n| is_click(n.step.get("action")))
            {
                dialog_watch = self.be().foreground_window().ok().flatten();
            }
            // read_text acts (select-all + copy) but only to look: a wait before it may end once
            // the page settles. 2026-10-29 leads run 3: machands' wait 4 s returned after 1.7 s on
            // each YouTube channel page, Hekaton's wait 3 s ran out every time (8 channels, ~10 s).
            let input_follows = items.get(i + 1).is_some_and(|next| {
                next.step
                    .get("action")
                    .and_then(Value::as_str)
                    .is_some_and(|a| {
                        is_action(a) && !is_passive(a) && !a.starts_with("wait") && a != "read_text"
                    })
            });
            if step.get("action").and_then(Value::as_str) != Some("read_text") {
                self.o
                    .copied_text
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
            }
            self.o
                .input_follows
                .store(input_follows, std::sync::atomic::Ordering::Relaxed);
            let notes_before_step = notes.len();
            let events_before_step = if asking { self.be().input_events() } else { 0 };
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
                    self.run_approved(step, item.scope.as_ref(), own_args, i + 1)
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
                    // a refusal the person could approve, raised before this step sent anything
                    if asking
                        && self.be().input_events() == events_before_step
                        && let Some(need) = self.need_for(&exc)
                    {
                        pending = Some(Pending {
                            need: self.complete_need(step, need),
                            step_id: approval::step_id(step),
                            index: i + 1,
                            notes: notes[..notes_before_step].to_vec(),
                            shots: shots.clone(),
                        });
                    }
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
            // 2026-10-11: shot=false means text only, but find_text still attached a crop (~300
            // tokens a hit). A picture stays only where the step itself asks for one.
            let img =
                img.filter(|_| want_shot || matches!(step_action.as_str(), "zoom" | "screenshot"));
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
                    let next_action = steps[i + 1].get("action").and_then(Value::as_str);
                    let next_text = steps[i + 1]
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    // Typing into the same box, then more typing or a plain key: the keys queue in
                    // order and nothing on screen has to move first, so no settle. Enter, Tab and
                    // shortcuts may submit or open something; those still wait. 2026-10-29: the
                    // settle cost 170-300 ms per step of a 5-line batch on the Mac.
                    let plain_key = |k: &str| {
                        let k = normalize_key_text(k);
                        !k.contains('+')
                            && !matches!(
                                k.as_str(),
                                "enter" | "return" | "tab" | "escape" | "esc" | "f2"
                            )
                    };
                    let keys_into_same_box = raw_action == Some("type")
                        && !text.ends_with(['\n', '\r', '\t'])
                        && match next_action {
                            Some("type") => true,
                            Some("key") => plain_key(next_text),
                            _ => false,
                        };
                    if keys_into_same_box {
                        // no settle: see above
                    } else if raw_action == Some("key") && next_is_pointer {
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
            approval: pending,
        })
    }

    /// Run one step. On the step an approval was issued for, a refusal for exactly the approved
    /// target runs the step once more with only that refusal off; a different target is refused
    /// (and can be asked about again).
    fn run_approved(
        &self,
        step: &Args,
        scope: Option<&BTreeSet<String>>,
        call_args: bool,
        index: usize,
    ) -> StepResult<super::actions::StepOut> {
        let Some(granted) = self
            .o
            .approval
            .resume
            .as_deref()
            .filter(|r| r.index == index)
        else {
            return self.run_scoped(step, scope, call_args);
        };
        let before = self.be().input_events();
        let exc = match self.run_scoped(step, scope, call_args) {
            Err(exc) => exc,
            done => return done,
        };
        let Some(need) = self.need_for(&exc) else {
            return Err(exc);
        };
        if need.key() != granted.key || self.be().input_events() != before {
            self.o.approval.note(need); // the loop asks about this one afresh
            return Err(exc);
        }
        let mut o = CallOpts {
            approval: CallApproval {
                resume: None,
                granted: None,
                ..self.o.approval.clone()
            },
            ..self.o.clone()
        };
        match need.kind {
            // only the rule that wrote this refusal, seen in this place, and only for this step
            Kind::Destructive(_) => o.policy = Policy::only(&need.reason),
            Kind::IrreversibleButton => o.approval.granted = Some(need.key()),
        }
        let again = Ctx {
            c: self.c,
            o,
            who: self.who,
        };
        again.run_scoped(step, scope, call_args)
    }

    /// The need behind a refusal: one the guard noted, or a destructive-action refusal read back
    /// from its text. `None` for anything the person cannot approve.
    fn need_for(&self, exc: &StepError) -> Option<Need> {
        if let Some(need) = self.o.approval.take_noted(exc.message()) {
            return Some(need);
        }
        if !matches!(exc, StepError::Permission(_)) {
            return None;
        }
        let (category, _, _) = crate::destructive::parse_refusal(exc.message())?;
        Some(Need::new(
            Kind::Destructive(category),
            exc.message(),
            format!("could {}", category.consequence()),
            exc.message(),
        ))
    }

    /// The need as the card will say it: what the step does and where, filled in from the step and
    /// the front window when the guard did not know.
    fn complete_need(&self, step: &Args, need: Need) -> Need {
        let mut n = need;
        if n.action.is_empty() {
            n.action = approval::describe_step(step, false);
            n.action_log = approval::describe_step(step, true);
        }
        if n.place.is_empty()
            && let Some(app) = self
                .fg_root()
                .and_then(|w| self.info(w))
                .map(|i| i.app)
                .filter(|a| !a.is_empty())
        {
            n.place = format!("in {app}");
        }
        n
    }
}

/// Said when an approval is used on a call or a step it was not issued for.
const APPROVAL_MISMATCH: &str = "that approval was issued for a different step than the one now \
    waiting, so nothing ran. Ask again by repeating the original call";

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
        self.run_many_with(args, policy, who, CallApproval::default())
    }

    /// [`Computer::run_many`] for a call that may stop to ask the person (`approval.mode`) or
    /// resumes on an approval the person gave (`approval.resume`).
    pub(crate) fn run_many_with(
        &self,
        args: &Args,
        policy: Policy,
        who: &Caller,
        approval: CallApproval,
    ) -> Result<ManyResult, StepError> {
        let mut o = CallOpts::from_args(self, args, policy)?;
        o.approval = approval;
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
                offer: None,
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
        let mut outcome = self.execute_locked(args, who);
        self.idle.touch(self.clock.as_ref());
        // Last stop before the text goes to the model's provider (see `redact`). A local model has
        // no provider, so its replies keep their text.
        let cloud = !self.cfg.local_model();
        for text in [&mut outcome.result, &mut outcome.error]
            .into_iter()
            .flatten()
            .filter(|_| cloud)
        {
            if let std::borrow::Cow::Owned(clean) = crate::redact::redact(text) {
                *text = clean;
            }
        }
        // Errors quote the screen too (an alert's text, a field's value), so both get the check.
        let mut seen = false;
        for text in [&mut outcome.result, &mut outcome.error]
            .into_iter()
            .flatten()
        {
            if crate::redact::looks_like_injection(text) {
                text.push('\n');
                text.push_str(crate::redact::INJECTION_NOTE);
                seen = true;
            }
        }
        if seen {
            *self.tainted_until.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::time::Instant::now() + Self::TAINT);
        }
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
            offer: None,
        }
    }

    /// The approval a call brings back (a card token or a chat code), checked and spent. `Ok(None)`
    /// when it brings none; an error when it is unknown, used, expired, not approved yet, denied, or
    /// issued for a different call.
    fn take_approval(
        &self,
        token: Option<String>,
        code: Option<String>,
        call: &Args,
    ) -> Result<Option<Entry>, StepError> {
        if token.is_none() && code.is_none() {
            return Ok(None);
        }
        let now = self.clock.now();
        let entry = {
            let mut store = self.approvals.lock().unwrap_or_else(|e| e.into_inner());
            match (token, code) {
                (Some(t), _) => store
                    .take(&t, now)
                    .map_err(|why| StepError::permission(why.message()))?,
                (None, Some(c)) => store
                    .take_code(&c, now)
                    .map_err(|why| StepError::permission(why.message()))?,
                (None, None) => return Ok(None),
            }
        };
        if entry.call_id != approval::call_id(call) {
            return Err(StepError::permission(APPROVAL_MISMATCH));
        }
        Ok(Some(entry))
    }

    fn entry_for(&self, p: &Pending, call: &Args) -> Entry {
        Entry {
            call_id: approval::call_id(call),
            step_id: p.step_id.clone(),
            index: p.index,
            key: p.need.key(),
            expires: Duration::ZERO,
            notes: p.notes.clone(),
            shots: p.shots.clone(),
        }
    }

    /// Turn a step that stopped for approval into a card token and sentence. `None` when no
    /// randomness is available; the caller then just gets the refusal.
    fn offer_for(&self, p: &Pending, call: &Args) -> Option<Offer> {
        let token = self
            .approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mint(self.entry_for(p, call), self.clock.now())?;
        Some(Offer {
            token,
            summary: p.need.summary(),
            log_summary: p.need.log_summary(),
            kind: p.need.kind.tag(),
            step: p.index,
        })
    }

    /// Turn a step that stopped for approval into a chat code and the text that tells the model
    /// what to show the person. `None` when no randomness is available; the caller then just gets
    /// the refusal.
    fn code_for(&self, p: &Pending, call: &Args) -> Option<String> {
        let code = self
            .approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mint_code(
                self.entry_for(p, call),
                &p.need.log_summary(),
                self.clock.now(),
            )?;
        Some(approval::code_refusal(&p.need.summary(), &code))
    }

    /// The person's answer to a chat code (`hekaton approve|deny CODE`, through the daemon's
    /// `/approve` and `/deny` routes). The text is the one line the command prints; the error says
    /// why the code did not take.
    pub fn answer_approval_code(&self, code: &str, allow: bool) -> Result<String, String> {
        let code = approval::normalize_code(code);
        let summary = self
            .approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .answer_code(&code, allow, self.clock.now())
            .map_err(|e| match e {
                approval::CodeError::Unknown => {
                    format!("no waiting approval {code} (already answered, used, or never issued)")
                }
                approval::CodeError::Expired => format!("approval {code} expired"),
                other => other.message(),
            })?;
        let word = if allow { "allow" } else { "deny" };
        log_action(
            self.cfg.paths(),
            "approval",
            &json!({"answer": word, "via": "chat code", "code": code, "card": summary}),
            if allow { "allowed" } else { "denied" },
            allow,
        );
        Ok(format!(
            "{} {code}: {summary}",
            if allow { "approved" } else { "denied" }
        ))
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
        if !self.honours_opt_in() {
            // strict: a flag the model wrote is not the owner's approval
            policy = Policy::REFUSE;
        }
        if profile.unrestricted() {
            // open: the owner chose to let the model judge destructive text and keys itself
            policy = Policy::allow();
        }
        // 2026-10-29: approvals (see crate::approval). `approvable` says the MCP server can ask the
        // person; `approval_token` (a card's Allow) or `approval_code` (a chat code the person
        // approved) is the yes it brings back. None of them is forwarded or logged as an argument.
        let mode = Mode::from_arg(map.remove("approvable").as_ref());
        if mode != Mode::Off && !profile.unrestricted() {
            // a call that can ask the person: the model's own allow_destructive is a request to
            // ask, not a yes (the irreversible guard does the same with allow_irreversible)
            policy = Policy::REFUSE;
        }
        let token = map
            .remove("approval_token")
            .and_then(|v| v.as_str().map(str::to_string))
            .filter(|t| !t.is_empty());
        let code = map
            .remove("approval_code")
            .and_then(|v| v.as_str().map(str::to_string))
            .filter(|t| !t.trim().is_empty());
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
        if !profile.unrestricted() && !self.rate.check(self.clock.as_ref()) {
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
        let resume = match self.take_approval(token, code, &map) {
            Ok(resume) => resume.map(std::sync::Arc::new),
            Err(e) => return self.fail(&args, &e),
        };
        let resumed = resume.is_some();
        let approval = CallApproval {
            mode,
            resume,
            ..CallApproval::default()
        };
        match self.run_many_with(&map, policy, who, approval) {
            Err(e) => self.fail(&args, &e),
            Ok(mut done) => {
                // a step stopped for a yes: a card for a client that can show one, else a chat code
                let mut offer = None;
                let mut code_card = None;
                if let Some(p) = done.approval.take() {
                    match mode {
                        Mode::Card => offer = self.offer_for(&p, &map),
                        Mode::Chat => {
                            if let Some(text) = self.code_for(&p, &map) {
                                done.text = match done.text.rfind(&p.need.reason) {
                                    Some(at) if !p.need.reason.is_empty() => format!(
                                        "{}{text}{}",
                                        &done.text[..at],
                                        &done.text[at + p.need.reason.len()..]
                                    ),
                                    _ => format!("{}\n{text}", done.text),
                                };
                                if let Some(f) = done.failure.as_mut() {
                                    f.error = f.error.with_message(text);
                                }
                                code_card = Some(p.need.log_summary());
                            }
                        }
                        Mode::Off => {}
                    }
                }
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
                // every card offered, every chat code issued and every approval honoured
                if let Some(o) = &offer {
                    entry.insert("approval_card".into(), Value::from(o.log_summary.clone()));
                }
                if let Some(summary) = &code_card {
                    entry.insert("approval_code_card".into(), Value::from(summary.clone()));
                }
                if resumed {
                    entry.insert("approval_used".into(), Value::Bool(true));
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
                    offer,
                }
            }
        }
    }
}
