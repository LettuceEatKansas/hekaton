//! One call, many actions, and everything around it. Port of `computer.run_many` (the batch loop, the
//! per-call options, the trailing settle and picture) and the computer branch of
//! `daemon._execute_tool` (pause, acknowledgement, rate cap, audit entry, failure fields).
//!
//! `{"actions": [{...}, {...}]}` or a single action. A batch stops at the first error and says which
//! step failed. It returns every zoom or screenshot taken inside the batch, plus ONE screenshot at the
//! end after the screen settles, so "hover spot A, zoom, hover spot B, zoom" compares two spots in one
//! model turn.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use super::args::{Args, flag};
use super::error::{StepError, StepResult};
use super::guard::normalize_only_app;
use super::{CallOpts, Computer, Ctx};
use crate::actions::{POINTER_ACTIONS, is_action, is_passive};
use crate::audit::log_action;
use crate::backend::Rect;
use crate::caller::Caller;
use crate::config::{Config, truthy};
use crate::destructive::Policy;
use crate::keys::{is_opener, normalize_key_text};
use crate::scale::{CallScale, Coords, pick_display};
use crate::textutil::py_str;

/// What a call returns: the text, every picture taken, and their MIME type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManyResult {
    /// One line per step, then the settle line.
    pub text: String,
    /// Base64 pictures, in the order `[image N]` numbers them.
    pub images: Vec<String>,
    /// `image/jpeg` or `image/png`.
    pub mime: &'static str,
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
        Value::Object(out)
    }
}

static STEP_FAILED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"step (\d+) \([^)]*\) failed: (.+)").expect("STEP_FAILED"));

/// `{"failed_step": n, "error": "..."}` if `text` names a failed step (Python
/// `_batch_failure_fields`). A script can read it without string-matching the result.
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
            coords: scale.coords,
            long_edge: scale.long_edge,
            display,
            only_apps,
            snap_call: args.get("snap").is_some_and(truthy),
            policy,
        })
    }

    /// The configured defaults and nothing else (a read outside a `computer` call).
    pub(crate) fn defaults(cfg: &Config, policy: Policy) -> Self {
        let long = cfg
            .shot_long()
            .clamp(crate::scale::SHOT_LONG_MIN, crate::scale::SHOT_LONG_MAX);
        Self {
            coords: Coords::from_name(&cfg.coords()),
            long_edge: long as u32,
            display: None,
            only_apps: BTreeSet::new(),
            snap_call: false,
            policy,
        }
    }
}

/// The call as it is logged when it fails: what each step typed is redacted too. (The Python audit
/// log redacted only the top level, so a refused or malformed batch wrote the text of its `type`
/// steps to disk.)
fn redact_steps(args: &Value) -> Value {
    let mut out = args.clone();
    if let Some(Value::Array(steps)) = out.get_mut("actions") {
        for step in steps.iter_mut() {
            if step.is_object() {
                *step = crate::audit::redact_args(step);
            }
        }
    }
    out
}

fn step_action_text(step: &Args) -> String {
    match step.get("action") {
        None => "None".to_string(),
        Some(v) => py_str(v),
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
    fn wait_focus_change(&self) -> StepResult<()> {
        let max_s = 1.5;
        let (fg0, focus0) = self.c.mem().pre_key;
        let t = self.now();
        self.sleep(0.12);
        while self.now() - t < 0.4 {
            let fg = self.be().foreground_window().ok().flatten();
            let focus = self.be().focus_token().unwrap_or(0);
            if fg != fg0 || focus != focus0 {
                break;
            }
            self.sleep(0.05);
        }
        self.settle(Some((max_s - (self.now() - t)).max(0.2)), 0.15, 2)?;
        Ok(())
    }

    /// The batch loop. Python `run_many` after the per-call options are set.
    fn run_steps(&self, args: &Args) -> StepResult<ManyResult> {
        let want_shot = flag(args.get("shot"), true);
        let steps: Vec<Value> = match args.get("actions") {
            Some(Value::Array(list)) if !list.is_empty() => list.clone(),
            Some(v) if truthy(v) && !v.is_array() => vec![v.clone()],
            _ => vec![Value::Object(args.clone())],
        };
        let mut notes: Vec<String> = Vec::new();
        let mut shots: Vec<String> = Vec::new();
        let input_before = self.be().input_events();
        let mut acted_in_call = false; // an acting step has already finished in this call
        let mut pre_input_failure = false; // a step failed before anything was sent
        for (i, raw) in steps.iter().enumerate() {
            let Value::Object(step) = raw else {
                return Err(StepError::value(format!(
                    "step {} must be an object with an action",
                    i + 1
                )));
            };
            let (note, img) = match self.run(step) {
                Ok(done) => done,
                Err(exc) => {
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
                    break;
                }
            };
            let step_action = step
                .get("action")
                .map(|v| py_str(v).trim().to_string())
                .unwrap_or_default();
            if is_action(&step_action) && !is_passive(&step_action) {
                acted_in_call = true;
            }
            let has_img = img.is_some();
            if let Some(img) = img {
                shots.push(img);
                notes.push(format!("{note} [image {}]", shots.len()));
            } else {
                notes.push(note.clone());
            }
            if note.contains("expect: FAIL") {
                break;
            }
            if !has_img && i + 1 < steps.len() {
                let raw_action = step.get("action").and_then(Value::as_str);
                let pointer_or_input = raw_action
                    .is_some_and(|a| POINTER_ACTIONS.contains(&a) || a == "key" || a == "type");
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
                            break;
                        }
                    } else if raw_action == Some("key") && is_opener(&normalize_key_text(&text)) {
                        // 2026-09-21 eval: ctrl+n then type in one batch saved an EMPTY file: the
                        // paste landed before Notepad's new tab (or the Save As dialog) had focus.
                        self.wait_focus_change()?;
                    } else {
                        // One calm read is enough here: the next step acts anyway (W4, 2026-10-08).
                        self.settle(Some(0.35), 0.0, 1)?; // let each step land before the next one
                    }
                }
            }
        }
        let last_raw = steps
            .last()
            .and_then(|s| s.as_object())
            .and_then(|o| o.get("action"));
        let last: Option<&str> = last_raw.and_then(Value::as_str);
        if self.cfg().auto_shot() && !Self::no_trailing_shot(last) && !pre_input_failure {
            let acted = last.is_some_and(|a| {
                !matches!(a, "wait" | "wait_for_text" | "wait_for_file") && is_action(a)
            });
            if want_shot || acted {
                // Acting calls always settle, even with shot=false: the next call must find the UI at
                // rest.
                let min_s = if acted { self.settle_floor_s() } else { 0.0 };
                let waited = self.settle(None, min_s, 2)?;
                self.remember_target();
                if want_shot {
                    shots.push(self.screenshot_b64(None)?);
                    notes.push(format!(
                        "screen settled in {:.0} ms [image {}: full screen]",
                        waited * 1000.0,
                        shots.len()
                    ));
                } else {
                    notes.push(format!("screen settled in {:.0} ms", waited * 1000.0));
                }
            }
        }
        Ok(ManyResult {
            text: notes.join("\n"),
            images: shots,
            mime: self.mime(),
        })
    }
}

impl Computer {
    /// A context for work outside a `computer` call: the configured defaults, no allowlist.
    pub(crate) fn read_ctx<'a>(&'a self, who: &'a Caller) -> Ctx<'a> {
        Ctx {
            c: self,
            o: CallOpts::defaults(&self.cfg, Policy::REFUSE),
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
        ctx.run_steps(args)
    }

    /// The whole `computer` route of the daemon: the per-call opt-in, the pause flag, the
    /// acknowledgement gate, the rate cap, one action at a time, the audit entry, and the failure
    /// fields. Never fails: a refusal or an error comes back as `ok: false`. Python
    /// `_execute_tool(path="computer", ...)`.
    pub fn execute(&self, args: &Value, who: &Caller) -> ToolOutcome {
        self.idle.touch(self.clock.as_ref());
        let _one_at_a_time = self.tool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = self.execute_locked(args, who);
        self.idle.touch(self.clock.as_ref());
        outcome
    }

    fn fail(&self, args: &Value, err: &StepError) -> ToolOutcome {
        let mut error = err.message().to_string();
        log_action(
            self.cfg.paths(),
            "computer",
            &redact_steps(args),
            &error,
            false,
        );
        error = self.fails.note_failure("computer", args, &error);
        ToolOutcome {
            ok: false,
            result: None,
            error: Some(error),
            images: Vec::new(),
            mime: "image/jpeg",
            failed_step: None,
        }
    }

    fn execute_locked(&self, raw: &Value, who: &Caller) -> ToolOutcome {
        let Value::Object(map) = raw else {
            return self.fail(raw, &StepError::value("tool args must be an object"));
        };
        let mut map = map.clone();
        // The owner's hard line. Per call, never sticky: the refusals are on unless THIS call asked
        // for them off, and the next call starts from on again.
        let policy = Policy::take_from_args(&mut map);
        let args = Value::Object(map.clone());
        if self.pause.is_paused() {
            return self.fail(&args, &StepError::permission("paused by user hotkey"));
        }
        if let Err(refused) = crate::ack::check_acknowledged(&self.cfg, "computer", &args) {
            return self.fail(&args, &refused.into());
        }
        if !self.rate.check(self.clock.as_ref()) {
            return self.fail(&args, &StepError::runtime("rate limit exceeded"));
        }
        match self.run_many(&map, policy, who) {
            Err(e) => self.fail(&args, &e),
            Ok(done) => {
                let steps = match map.get("actions") {
                    Some(Value::Array(a)) if !a.is_empty() => a.len(),
                    _ => 1,
                };
                let ok = !done.text.contains("failed:");
                log_action(
                    self.cfg.paths(),
                    "computer",
                    &json!({"action": map.get("action"), "steps": steps}),
                    &done.text,
                    ok,
                );
                let failure = if ok {
                    None
                } else {
                    batch_failure_fields(&done.text)
                };
                ToolOutcome {
                    ok: true,
                    result: Some(done.text),
                    error: failure.as_ref().map(|(_, e)| e.clone()),
                    images: done.images,
                    mime: done.mime,
                    failed_step: failure.map(|(n, _)| n),
                }
            }
        }
    }
}
