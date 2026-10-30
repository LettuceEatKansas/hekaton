//! One action. Port of `computer.run`: every branch of the `computer` tool's vocabulary, in the same
//! order, with the same checks before the same input and the same words in the result.
//!
//! The shape of a step: validate the action; for an acting action wait for a person to stop using the
//! mouse and bring the remembered target back; do the action; then (for an acting action) mark the
//! input as ours, prefix the note, and run the step's `expect` check if it has one.

use std::path::Path;

use serde_json::Value;

use super::args::{Args, flag, float_or, get, int_or, is_truthy, text as text_arg, to_int};
use super::error::{StepError, StepResult};
use super::snapshot::WAKE_HINT;
use super::{Ctx, secs};
use crate::actions::{ACTIONS_SORTED, NO_TRAILING_SHOT, is_action, is_excel_read, is_passive};
use crate::backend::{ClipboardContent, ElementAction, MouseButton, Point, Role, ScrollDirection};
use crate::keys::{
    Key, Platform, is_calc_like, key_from_name, normalize_key_text, parse_key_combo,
};
use crate::textutil::{fmt_g, py_repr, py_value_repr};

/// Shortcuts that put something on the clipboard (`_COPY_COMBOS`).
const COPY_COMBOS: [&str; 3] = ["ctrl+c", "ctrl+x", "ctrl+insert"];

/// A step's result: the note and, when the step produced one, a base64 picture.
pub(crate) type StepOut = (String, Option<String>);

/// Do two element labels read the same? Whitespace runs and the ends do not count.
fn same_label(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

/// `_wait_timeout`: `max(0.1, min(float(timeout or 10), 120.0))`.
fn wait_timeout(args: &Args) -> StepResult<f64> {
    Ok(float_or(args, "timeout", 10.0)?.clamp(0.1, 120.0))
}

fn click_button(name: &str) -> StepResult<MouseButton> {
    match name {
        "left" => Ok(MouseButton::Left),
        "right" => Ok(MouseButton::Right),
        "middle" => Ok(MouseButton::Middle),
        // Python indexed a dict with it: the KeyError's text is the quoted key
        other => Err(StepError::runtime(py_repr(other))),
    }
}

/// Can this action put input events into the OS? Reads and waits cannot; `excel` and the recipe
/// calls reach other things (a workbook; the steps a recipe expands to, each run through `run`).
fn sends_input(action: &str) -> bool {
    is_action(action)
        && !is_passive(action)
        && !matches!(action, "excel" | "recipe" | "recipe_save")
}

impl Ctx<'_> {
    /// Do one action. Returns the result text and, for `screenshot`, `zoom` and the find actions, a
    /// picture. Python `run(args)`.
    pub(crate) fn run(&self, step: &Args) -> StepResult<StepOut> {
        let result = self.run_one(step);
        // 2026-10-30: an unattended Mac run was refused with "a person is using the mouse" and nobody
        // there. The mark that tells the gate "that input was ours" used to run only on the success
        // path of a step, so a step that sent keys or clicks and then failed (and left_mouse_up, and
        // the window restores before a step) left its own input looking like a person's to the next
        // step. Mark on every way out of a step that can send input.
        let action = step
            .get("action")
            .map(|v| crate::textutil::py_str(v).trim().to_string())
            .unwrap_or_default();
        // A refusal or a bad argument (Value, Permission, Target) sent nothing, so it must not hide
        // a real person's input behind a fresh mark; a failed OS call or a missed postcondition did.
        let sent = match &result {
            Ok(_) => true,
            Err(e) => !e.is_validation(),
        };
        if sent && sends_input(&action) {
            self.mark_ours();
        }
        result
    }

    fn run_one(&self, step: &Args) -> StepResult<StepOut> {
        let action = match step.get("action") {
            None => String::new(),
            Some(v) => crate::textutil::py_str(v).trim().to_string(),
        };
        if !is_action(&action) {
            return Err(StepError::value(format!(
                "unknown action {}; one of: {}",
                py_repr(&action),
                ACTIONS_SORTED.join(", ")
            )));
        }
        // Releasing input is cleanup: never wait for a person or raise another window first.
        if action == "left_mouse_up" {
            let mut owned = self
                .c
                .owned_mouse
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            self.be().mouse_button(MouseButton::Left, false)?;
            *owned = false;
            return Ok(("left mouse up".into(), None));
        }
        if action != "mouse_move"
            && !is_passive(&action)
            && *self
                .c
                .owned_mouse
                .lock()
                .unwrap_or_else(|error| error.into_inner())
        {
            return Err(StepError::permission(
                "left mouse button is held by this tool; send left_mouse_up before another acting action",
            ));
        }
        self.check_cancelled()?;
        if self.o.read_only
            && (!matches!(
                action.as_str(),
                "status"
                    | "screenshot"
                    | "zoom"
                    | "read"
                    | "find_text"
                    | "list_windows"
                    | "cursor_position"
                    | "list_files"
                    | "recipe_list"
                    | "clipboard_get"
            ) && !is_excel_read(&action, &text_arg(step, "op"))
                || is_truthy(step, "wake")
                || is_truthy(step, "save_to")
                || is_truthy(step, "scope"))
        {
            return Err(StepError::permission(
                "read_only refuses this action or a focus-changing observation; no actions sent",
            ));
        }
        // 2026-10-08: the shortcuts (window, folder) wait for a person themselves, and the recipe
        // actions that are not expanded by the batch loop need no input wait at all.
        match action.as_str() {
            "window" => return self.run_window(step).map(|note| (note, None)),
            "folder" => return self.run_folder(step).map(|note| (note, None)),
            "recipe_list" => return self.recipe_list(),
            "recipe_save" => return self.recipe_save(step),
            "recipe" => return self.recipe_direct(),
            // Excel is reached through its own interface, not the screen: no person to wait for,
            // no window to bring back, no picture afterwards.
            "excel" => return self.run_excel(step).map(|note| (note, None)),
            _ => {}
        }
        let mut coord: Option<Value> = get(step, "coordinate").cloned();
        let mut text = text_arg(step, "text");
        let mut scroll_amount: Option<i64> = None;
        if action == "scroll" {
            // 2026-09-21 eval: Qwen sent scroll with start_coordinate for the spot and text "10" for
            // the amount; the spot was ignored and "10" failed as a modifier key. Read what it meant.
            if coord.is_none()
                && let Some(sc) = get(step, "start_coordinate")
            {
                coord = Some(sc.clone());
            }
            let t = text.trim();
            if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) {
                scroll_amount = Some(if is_truthy(step, "scroll_amount") {
                    int_or(step, "scroll_amount", 3)?
                } else {
                    t.parse::<i64>().unwrap_or(3)
                });
                text.clear();
            }
        }
        let acts = !is_passive(&action);
        let mut prefix = String::new();
        if acts {
            self.wait_for_human()?;
            self.check_cancelled()?;
            // press acts on the element itself, so its window may stay behind others.
            if action != "press" {
                prefix = self.restore_target(matches!(
                    action.as_str(),
                    "key" | "type" | "hold_key" | "read_text"
                ));
            }
        }

        let note: String = match action.as_str() {
            "status" => return Ok((self.c.status().to_string(), None)),
            "screenshot" => {
                if is_truthy(step, "save_to") {
                    // 2026-10-08: write the picture to a file as well
                    return self.save_screenshot(step);
                }
                let (sw, sh) = self.scale().shot_size();
                self.remember_target();
                if self.scoped() {
                    // say what the picture is, so the model knows its coordinate space is the window
                    return Ok((
                        format!(
                            "screenshot of {} ({sw}x{sh}); give coordinates in {}",
                            self.scope_name(),
                            self.this_image_space()
                        ),
                        Some(self.screenshot_b64(None)?),
                    ));
                }
                let space = if self.o.coords == crate::scale::Coords::Norm1000 {
                    "0-1000 on both axes".to_string()
                } else {
                    format!("pixels of this {sw}x{sh} image")
                };
                return Ok((
                    format!("screenshot; give coordinates in {space}"),
                    Some(self.screenshot_b64(None)?),
                ));
            }
            "zoom" => {
                // 2026-09-21 h-clockdate: a model sent the box as "coordinate" and got a bare
                // "'NoneType' object is not subscriptable"; the four numbers say what it meant.
                let region_v = get(step, "region").cloned().or_else(|| {
                    coord
                        .as_ref()
                        .filter(|c| matches!(c, Value::Array(a) if a.len() == 4))
                        .cloned()
                });
                let boxed = region_v.as_ref().and_then(super::args::four_numbers);
                let Some(boxed) = boxed else {
                    return Err(StepError::value(
                        "zoom needs region: [x0, y0, x1, y1], the box to magnify",
                    ));
                };
                let region = self.scale().min_zoom(boxed?);
                let region_f = [
                    f64::from(region[0]),
                    f64::from(region[1]),
                    f64::from(region[2]),
                    f64::from(region[3]),
                ];
                let img = self.screenshot_b64(Some(region_f))?;
                let whole = if self.scoped() {
                    format!("the {} screenshot's", self.scope_name())
                } else {
                    "the full screenshot's".to_string()
                };
                return Ok((
                    format!(
                        "zoomed view (coordinates stay in {whole} space){}",
                        self.zoom_text(region)?
                    ),
                    Some(img),
                ));
            }
            "find_text" => {
                if text.is_empty() {
                    return Err(StepError::value(
                        "find_text needs text: the words to look for, e.g. Developer",
                    ));
                }
                return self.find_text(&text);
            }
            "read_field" => {
                let label = if text.is_empty() {
                    text_arg(step, "label")
                } else {
                    text.clone()
                };
                if label.is_empty() {
                    return Err(StepError::value(
                        "read_field needs text: the field's label, e.g. Email",
                    ));
                }
                if let Some(val) = self.read_field_uia(&label) {
                    return Ok((format!("{label}: {} (uia)", py_repr(&val)), None));
                }
                let (row_note, img) = self.find_text(&label)?;
                return Ok((
                    format!(
                        "no labelled field found via UIA for {}; nearest OCR row:\n{row_note}",
                        py_repr(&label)
                    ),
                    img,
                ));
            }
            "cursor_position" => {
                let p = self.be().cursor_position()?;
                return Ok((format!("cursor at {}", self.pt(p.x, p.y)), None));
            }
            "list_windows" => {
                let mut wins = self.c.window_refs().list(is_truthy(step, "all"))?;
                // 2026-10-29 leads run: under only_apps the list still named every other app's
                // window, the owner's mail title included. Those windows cannot be used anyway, so
                // they are counted, not named.
                let before = wins.len();
                if !self.o.only_apps.is_empty() {
                    wins.retain(|w| self.check_allowed(w.window).is_ok());
                }
                let mut lines = super::refs::format_windows(&wins);
                if wins.len() < before {
                    lines.push(format!(
                        "({} windows of apps outside only_apps not shown)",
                        before - wins.len()
                    ));
                }
                return Ok((lines.join("\n"), None));
            }
            "read" => {
                // UIA first: the window's elements as `N Type "name" [state]`, no picture needed.
                let refs = self.c.window_refs();
                // 2026-10-28: a model wrote read(window="Safari") and silently got the front window.
                let target = if text.is_empty() {
                    text_arg(step, "window")
                } else {
                    text.clone()
                };
                let hwnd = refs.resolve((!target.is_empty()).then_some(target.as_str()))?;
                self.check_allowed(self.root_of(hwnd))?;
                // 2026-10-08 (W10): read is passive, so it no longer raises an occluded Chromium
                // window to wake its accessibility tree unless the caller asked (wake=true);
                // otherwise it says how.
                let wake = flag(step.get("wake"), false);
                let limit = int_or(step, "limit", 40)?;
                let filter = text_arg(step, "filter");
                // a browser's tab strip and toolbar are left out unless asked for (chrome=true)
                let chrome = flag(step.get("chrome"), false);
                let (mut out, hint) =
                    self.do_snapshot(Some(&hwnd.spec()), limit, &filter, false, wake, chrome)?;
                if hint {
                    out.push('\n');
                    out.push_str(WAKE_HINT);
                }
                return Ok((out, None));
            }
            "list_files" => {
                let path = if text.is_empty() {
                    text_arg(step, "path")
                } else {
                    text.clone()
                };
                if path.is_empty() {
                    return Err(StepError::value("list_files needs text: the folder path"));
                }
                let env = self.cfg().env();
                self.check_wait_path_allowed(
                    &super::files::expand_vars(env, &path),
                    "list_files may only list",
                )?;
                let limit = int_or(step, "limit", 20)?;
                let filter = text_arg(step, "filter");
                let out = super::files::do_files(
                    env,
                    &|secs| self.utc_offset(secs),
                    &path,
                    "modified",
                    true,
                    limit,
                    &filter,
                )?;
                return Ok((out, None));
            }
            "wait" => {
                // 2026-10-30: wait(duration=60) returned after 12 s because the screen settled. A
                // duration the caller names is waited in full (up to 115 s, inside the 120 s call maximum); the early return on a
                // calm screen is for a wait with no duration, which is "give it a moment".
                let named = is_truthy(step, "duration");
                if named {
                    let d = float_or(step, "duration", 1.0)?.clamp(0.0, 115.0);
                    if let Some(deadline) = self.o.deadline {
                        let left = deadline.saturating_sub(self.clock().now()).as_secs_f64();
                        if d > left {
                            return Err(StepError::value(format!(
                                "wait of {}s is longer than the {left:.0}s left in this call; pass \
                                 budget_ms (up to 120000) to wait that long",
                                fmt_g(d)
                            )));
                        }
                    }
                    self.sleep_checked(d)?;
                    format!("waited {d:.1}s (asked {}s)", fmt_g(d))
                } else {
                    let d = 1.0;
                    // Wait for the animation instead of the clock: return once the screen settles,
                    // after at least 40% of the time. In a first Gemini run, 56% of all acting time
                    // was waits it added itself (66 of 117 s).
                    // ... unless the next step acts blind: then a calm screen may only mean the app
                    // has not started drawing what comes next, so the full time is waited.
                    let blind_next = self
                        .o
                        .input_follows
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let took = if !blind_next {
                        self.settle(Some(d), 0.4 * d, 2)?
                    } else {
                        self.sleep(d);
                        d
                    };
                    format!("waited {took:.1}s (asked {}s, screen settled)", fmt_g(d))
                }
            }
            "read_text" => {
                // Sends select-all / copy only when the app exposes no text to UIA, so it is an
                // acting step.
                let limit = int_or(step, "limit", 30)?.clamp(1, 400) as usize;
                // 2026-10-30: `filter` is the same words as read's, so it is taken here when text is empty
                let query = if text.is_empty() {
                    text_arg(step, "filter")
                } else {
                    text.clone()
                };
                self.read_text_lines(&query, &text_arg(step, "window"), limit)?
            }
            "file_dialog" => {
                if text.is_empty() {
                    return Err(StepError::value(
                        "file_dialog needs text: the full file path",
                    ));
                }
                self.file_dialog(&text)?
            }
            "open_app" => {
                if text.is_empty() {
                    return Err(StepError::value(
                        "open_app needs text: the app name, e.g. Notepad",
                    ));
                }
                let text = super::open::repair_escapes(&text, &|p| Path::new(p).is_file());
                let is_url = super::shortcuts::is_url(&text);
                let browser = text_arg(step, "browser");
                // a URL sent to a named browser is checked as that browser; otherwise as the URL
                self.check_launch_allowed(if is_url && !browser.trim().is_empty() {
                    &browser
                } else {
                    &text
                })?;
                let mut note = if is_url {
                    // 2026-10-08: a web address starts the default browser and waits for its window;
                    // the old route waited for a NEW window and failed when the browser just added a
                    // tab. 2026-10-30: `browser` names another one (the one holding the session).
                    self.open_url_in(&text, 8.0, Some(browser.as_str()))?
                } else {
                    // A plain name that matches an open window's title focuses that window
                    // (2026-09-21: Search Console open in a browser as "Overview - Brave" sat behind
                    // the terminal and open_app "Overview" tried to launch a program called
                    // Overview; the model had no way to reach it).
                    let is_program = !super::guard::real_launch_target(text.to_lowercase().trim())
                        .0
                        .is_empty();
                    let win = if is_program {
                        None
                    } else {
                        self.window_titled(&text)
                    };
                    if let Some(w) = win {
                        self.be().bring_to_foreground(w.window)?;
                        format!("focused existing window {}", py_repr(&w.title))
                    } else {
                        // `new_window: true` (or `reuse: false`) launches another instance instead of
                        // focusing a running one
                        let reuse =
                            flag(step.get("reuse"), true) && !flag(step.get("new_window"), false);
                        self.do_open(&text, 8.0, true, reuse)?
                    }
                };
                if flag(step.get("blank"), false) {
                    // the front document must end up blank, or the step fails before anything is typed
                    let verdict = self.ensure_blank_document()?;
                    note = format!("{note}; {verdict}");
                }
                self.remember_target();
                note
            }
            "clipboard_get" => {
                let val = self.be().clipboard_text()?;
                format!(
                    "clipboard ({} chars): {}",
                    val.chars().count(),
                    py_repr(&val)
                )
            }
            "clipboard_set" => {
                if text.is_empty() {
                    return Err(StepError::value("clipboard_set needs text"));
                }
                self.check_outgoing(step, &text)?;
                let n = text.chars().count();
                let cap = self.cfg().clip_max();
                if n as i64 > cap {
                    return Err(StepError::value(format!(
                        "clipboard_set text is {n} chars, over the {cap}-char cap"
                    )));
                }
                self.be().set_clipboard_text(&text)?;
                format!("clipboard set to {n} chars")
            }
            "wait_for_text" => {
                if text.is_empty() {
                    return Err(StepError::value(
                        "wait_for_text needs text: the words to wait for",
                    ));
                }
                let timeout = wait_timeout(step)?;
                // new_only: what matches now was there before the wait began (the page's own
                // instructions); only a match that was not, or one in another window, counts
                let new_only = flag(step.get("new_only"), false);
                let (start_fg, before) = if new_only {
                    (self.fg_root(), self.locate_text(&text)?)
                } else {
                    (None, Vec::new())
                };
                let mut failure: Option<StepError> = None;
                let (found, took) = self.poll(
                    || match self.locate_text(&text) {
                        Ok(c) if new_only => {
                            let c = if self.fg_root() == start_fg {
                                super::find::new_since(&before, c)
                            } else {
                                c
                            };
                            (!c.is_empty()).then_some(c)
                        }
                        Ok(c) if !c.is_empty() => Some(c),
                        Ok(_) => None,
                        Err(e) => {
                            failure = Some(e);
                            Some(Vec::new())
                        }
                    },
                    timeout,
                    0.3,
                );
                if let Some(e) = failure {
                    return Err(e);
                }
                match found {
                    Some(c) => format!(
                        "wait_for_text: {} appeared after {took:.1}s ({}{})",
                        py_repr(&text),
                        c[0].source,
                        if new_only { ", new" } else { "" }
                    ),
                    None if new_only => format!(
                        "wait_for_text: no new {} appeared within {}s ({} already on screen when the wait began)",
                        py_repr(&text),
                        fmt_g(timeout),
                        before.len()
                    ),
                    None => format!(
                        "wait_for_text: {} did not appear within {}s",
                        py_repr(&text),
                        fmt_g(timeout)
                    ),
                }
            }
            "wait_for_file" => {
                let path = if text.is_empty() {
                    text_arg(step, "path")
                } else {
                    text.clone()
                };
                if path.is_empty() {
                    return Err(StepError::value(
                        "wait_for_file needs text: the file path to watch for",
                    ));
                }
                self.check_wait_path_allowed(&path, "wait_for_file may only watch")?;
                let timeout = wait_timeout(step)?;
                let (found, took) =
                    self.poll(|| Path::new(&path).exists().then_some(()), timeout, 0.05);
                if found.is_some() {
                    format!("wait_for_file: {path} appeared after {took:.1}s")
                } else {
                    format!(
                        "wait_for_file: {path} did not appear within {}s",
                        fmt_g(timeout)
                    )
                }
            }
            "click_text" => self.click_text(step, &text)?,
            "press" => self.press_ref(step, &text)?,
            "replace_word" => self.replace_word(step, &text)?,
            "type" | "key" | "hold_key" => self.keyboard(step, &action, &text)?,
            _ => self.pointer(
                step,
                &action,
                coord.as_ref(),
                &text,
                scroll_amount,
                &mut prefix,
            )?,
        };
        if acts {
            self.mark_ours();
        }
        let mut note = format!("{prefix}{note}");
        if let Some(expect) = step.get("expect").filter(|v| crate::config::truthy(v)) {
            self.settle(Some(0.6), 0.1, 2)?;
            if let Some(reason) = self.check_expect_result(expect)? {
                return Err(StepError::expectation(format!(
                    "{note}; expect: FAIL ({reason})"
                )));
            }
            note.push_str("; expect: PASS");
        }
        Ok((note, None))
    }

    /// The local zone's offset from UTC at a Unix time, for the `list_files` stamps. A backend that
    /// cannot say reads as UTC.
    pub(crate) fn utc_offset(&self, unix_secs: i64) -> i32 {
        self.be().local_utc_offset(unix_secs).unwrap_or(0)
    }

    // ------------------------------------------------------------------ text-targeted clicks

    fn click_text(&self, step: &Args, text: &str) -> StepResult<String> {
        if text.is_empty() {
            return Err(StepError::value(
                "click_text needs text: the words to click",
            ));
        }
        let cands = self.locate_click_text(text)?;
        if cands.is_empty() {
            return Err(StepError::target(format!(
                "no text on screen matches {} (tried UIA then OCR); zoom or find_text first to see \
                 what is actually there",
                py_repr(text)
            )));
        }
        let nth_arg = get(step, "nth");
        // no nth: controls beat static text, and only several controls stay ambiguous
        let choices = super::find::click_choices(&cands, text);
        if nth_arg.is_none() && choices.len() > 1 {
            let labels = choices
                .iter()
                .take(8)
                .map(|&i| {
                    let c = &cands[i];
                    format!("{}: {}@{}", i + 1, py_repr(&c.label), self.pt(c.x, c.y))
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(StepError::target(format!(
                "{} matches for {}, ambiguous: {labels}. Pass nth to pick one",
                choices.len(),
                py_repr(text)
            )));
        }
        let nth = match nth_arg {
            Some(v) if crate::config::truthy(v) => to_int(v)?,
            _ => choices.first().map_or(1, |&i| i as i64 + 1),
        };
        if !(1..=cands.len() as i64).contains(&nth) {
            return Err(StepError::target(format!(
                "nth={nth} out of range; found {} match(es) for {}",
                cands.len(),
                py_repr(text)
            )));
        }
        let c = &cands[(nth - 1) as usize];
        let p = Point { x: c.x, y: c.y };
        self.set_last_point(p);
        self.host_at(p)?;
        let button_name = if is_truthy(step, "button") {
            text_arg(step, "button")
        } else {
            "left".to_string()
        };
        let double = is_truthy(step, "double");
        let button = click_button(&button_name)?;
        if button == MouseButton::Left {
            self.check_click(p, double, step)?;
        }
        self.with_modifiers("", || {
            self.be()
                .mouse_click(p, button, if double { 2 } else { 1 }, &|| {
                    self.pointer_guard(p)
                })
                .map_err(StepError::from)
        })?;
        Ok(format!(
            "clicked {} ({}) at {}",
            py_repr(&c.label),
            c.source,
            self.pt(c.x, c.y)
        ))
    }

    /// `press`: do an element's own action (AXPress, UIA Invoke) by its number in the last `read`,
    /// without moving the mouse or raising the window. An element with no action is clicked at its
    /// centre instead. 2026-10-29: computer-use presses by element at ~67 ms; a located click_text
    /// cost ~270 ms before the click.
    fn press_ref(&self, step: &Args, text: &str) -> StepResult<String> {
        let raw = if text.is_empty() {
            text_arg(step, "ref")
        } else {
            text.to_string()
        };
        let Ok(n) = raw.trim().parse::<usize>() else {
            return Err(StepError::value(
                "press needs text: an element number from read, e.g. 12",
            ));
        };
        let window = text_arg(step, "window");
        let scoped = if window.is_empty() {
            None
        } else {
            Some(self.c.window_refs().resolve(Some(window.as_str()))?)
        };
        let Some(el) = self.c.element_refs().get(n, scoped) else {
            return Err(StepError::target(format!(
                "no element {n} in the last read of that window; read it first"
            )));
        };
        // 2026-10-30 (quiz run): the page moved to the next question and `press 3` acted on option c
        // of the PREVIOUS page. A number from `read` is a promise about what was listed, so look at
        // the element now: it must still exist and still be the same kind of thing with the same
        // label. Anything else means the page changed, and pressing "whatever has that number now"
        // could do the wrong thing, so refuse and say to read again.
        let live = self.be().element(&el.id).map_err(|e| {
            StepError::target(format!(
                "page changed since read: element {n} {} is gone ({e}); read again",
                py_repr(&el.name)
            ))
        })?;
        if live.native_role != el.native_role || !same_label(&live.name, &el.name) {
            return Err(StepError::target(format!(
                "page changed since read: element {n} was {} {} and is now {} {}; read again",
                el.native_role,
                py_repr(&el.name),
                live.native_role,
                py_repr(&live.name)
            )));
        }
        // 2026-10-30 (quiz run): press refused a Submit button as "disabled" after the page had
        // enabled it (a radio was chosen). The state in the list is as old as the read; ask now.
        if !live.enabled {
            return Err(StepError::target(format!(
                "element {n} {} is disabled",
                py_repr(&el.name)
            )));
        }
        let host = self.be().element_window(&el.id).map_err(|e| {
            StepError::target(format!(
                "page changed since read: element {n} has no window now ({e}); read again"
            ))
        })?;
        self.input_guard()?;
        self.check_allowed(self.root_of(host))?;
        // where it is now (the page may have scrolled since the read), else where the read saw it
        let (cx, cy) = live
            .rect
            .filter(|r| !r.is_empty())
            .map_or(el.center, |r| (r.center().x, r.center().y));
        let p = Point { x: cx, y: cy };
        self.check_irreversible(p, step)?;
        let why = match self.be().perform(&el.id, ElementAction::Press) {
            Ok(true) => {
                return Ok(format!(
                    "pressed {n} {} {}",
                    el.native_role,
                    py_repr(&el.name)
                ));
            }
            Ok(false) => "it has no press action".to_string(),
            // a browser can refuse AXPress on a control a click still reaches
            Err(e) => format!("its press action was refused: {e}"),
        };
        self.set_last_point(p);
        self.host_at(p)?;
        self.be()
            .mouse_click(p, MouseButton::Left, 1, &|| self.pointer_guard(p))
            .map_err(StepError::from)?;
        Ok(format!(
            "clicked {n} {} {} at {} ({why})",
            el.native_role,
            py_repr(&el.name),
            self.pt(p.x, p.y)
        ))
    }

    fn replace_word(&self, step: &Args, text: &str) -> StepResult<String> {
        let old = if is_truthy(step, "old") {
            get(step, "old").cloned().unwrap_or(Value::Null)
        } else {
            Value::String(text.to_string())
        };
        let new = get(step, "new");
        if !crate::config::truthy(&old) || new.is_none() {
            return Err(StepError::value("replace_word needs old and new"));
        }
        let old_text = crate::textutil::py_str(&old);
        let cands = self.locate_text(&old_text)?;
        let Some(c) = cands.first() else {
            return Err(StepError::target(format!(
                "no text on screen matches {} (tried UIA then OCR); zoom or find_text first to see \
                 what is actually there",
                py_value_repr(&old)
            )));
        };
        let p = Point { x: c.x, y: c.y };
        self.set_last_point(p);
        self.host_at(p)?;
        let hit = self.be().window_at_point(p).ok().flatten();
        self.refuse_explorer_open(hit.map(|h| self.root_of(h)))?;
        // 2026-10-11 (TextEdit): a double click selects one word, so "line here" became "line row".
        // A phrase with an exact on-screen box is selected from its first to its last character.
        let span = if old_text.trim().contains(' ') {
            self.doc_text_hits(old_text.trim(), 1)
                .into_iter()
                .find_map(|h| h.match_rect.filter(|_| self.visible_box(h.match_rect)))
        } else {
            None
        };
        if let Some(r) = span {
            let y = r.center().y;
            let (first, last) = (Point { x: r.left + 1, y }, Point { x: r.right - 1, y });
            self.check_click(first, false, step)?;
            self.check_click(last, false, step)?;
            self.be()
                .mouse_click(first, MouseButton::Left, 1, &|| self.pointer_guard(first))?;
            self.sleep(0.05);
            self.be().key_down(Key::Shift)?;
            let clicked = self
                .be()
                .mouse_click(last, MouseButton::Left, 1, &|| self.pointer_guard(last));
            self.be().key_up(Key::Shift)?;
            clicked?;
        } else {
            let (x, y, _) = self.snap_to_word(p.x, p.y);
            self.check_click(Point { x, y }, true, step)?;
            self.be()
                .mouse_click(Point { x, y }, MouseButton::Left, 2, &|| {
                    self.pointer_guard(Point { x, y })
                })?;
        }
        self.sleep(0.05);
        let new_text = crate::textutil::py_str(new.unwrap_or(&Value::Null));
        let suffix = self.type_text(&new_text)?;
        Ok(format!(
            "replaced {} ({}) with {}{suffix}",
            py_repr(&c.label),
            c.source,
            py_repr(&new_text)
        ))
    }

    // ------------------------------------------------------------------ keyboard

    fn keyboard(&self, step: &Args, action: &str, text: &str) -> StepResult<String> {
        let _p = crate::prof::span("keyboard");
        let fg = self.be().foreground_window().ok().flatten();
        let is_key = action == "key" || action == "hold_key";
        let combo_now = if is_key {
            normalize_key_text(text)
        } else {
            String::new()
        };
        let system_combo = is_key
            && matches!(
                combo_now.split('+').next().unwrap_or(""),
                "win" | "windows" | "meta"
            );
        // 2026-09-21: a Windows-key combo used to skip every check below because it "goes to the
        // shell, not the focused window" - true, but under an allowlist the shell IS the escape:
        // win+r is Run, win+s is Search, both can launch anything. Refuse them outright there.
        if system_combo && !self.o.only_apps.is_empty() {
            return Err(StepError::target(
                "refused: Windows-key shortcuts (win+...) are blocked while a task allowlist is \
                 active; they reach the Start menu, Run or Search, all outside it",
            ));
        }
        if let Some(fg) = fg
            && !system_combo
        {
            // Windows-key combos go to the shell, not to the focused window, so they are safe even
            // when Claude or Hermes is in front (a local model's Start-menu batch was refused,
            // 2026-09-20).
            let root = self.root_of(fg);
            self.check_not_protected(root)?;
            self.refuse_host(root)?;
            self.check_allowed(root)?;
            if is_key && matches!(combo_now.as_str(), "enter" | "alt+enter") {
                // alt+enter on a selected file shows Properties, not the file itself, but it is still
                // File Explorer acting on the selection outside the allowlist's editor.
                self.refuse_explorer_open(Some(root))?;
            }
        }
        match action {
            "type" => {
                if text.is_empty() {
                    return Err(StepError::value("type needs text"));
                }
                self.check_outgoing(step, text)?;
                Ok(format!(
                    "typed {} chars{}",
                    text.chars().count(),
                    self.type_text(text)?
                ))
            }
            "key" => {
                let pieces: Vec<String> = text.split_whitespace().map(normalize_key_text).collect();
                let is_mod = |p: &String| {
                    parse_key_combo(p, Platform::current())
                        .is_ok_and(|k| k.len() == 1 && k[0].is_modifier())
                };
                // "ctrl shift s" is one chord (normalize_key_text drops the spaces)
                let combo = if pieces.len() > 1 && pieces[..pieces.len() - 1].iter().all(is_mod) {
                    pieces.join("+")
                } else {
                    combo_now
                };
                let reps = int_or(step, "repeat", 1)?.clamp(1, 100) as u32;
                let trimmed = text.trim();
                let lone = trimmed.chars().count() == 1;
                if lone && key_from_name(&combo).is_none() {
                    // 2026-09-21 eval: Qwen pressed key "*" in Calculator, got "unknown key" and lost
                    // the rest of its batch. A lone symbol (* / % = ( etc.) means that character:
                    // type it.
                    for _ in 0..reps {
                        self.be().type_text(trimmed, &|| self.keyboard_guard())?;
                        self.sleep(0.012);
                    }
                    return Ok(format!("typed {}", py_repr(trimmed)));
                }
                if !text.trim_matches('+').contains('+')
                    && trimmed.chars().count() > 1
                    && parse_key_combo(&combo, Platform::current()).is_err()
                    && is_calc_like(trimmed)
                {
                    // 2026-09-21 eval: Qwen sent key "4309*82=" to Calculator and the batch died on
                    // "unknown key". Text that is not a key name is what the model wants typed.
                    // Narrowed to calculator-like text after this fallback silently typed an
                    // unrecognised key NAME (a typo, a made-up shortcut) instead of raising the normal
                    // "unknown key" error a model needs to see and fix.
                    return Ok(format!(
                        "typed {} (not a key name, so typed as text){}",
                        py_repr(text),
                        self.type_text(text)?
                    ));
                }
                if pieces.len() > 1
                    && parse_key_combo(&combo, Platform::current()).is_err()
                    && pieces.iter().all(|p| {
                        p.chars().count() == 1 || parse_key_combo(p, Platform::current()).is_ok()
                    })
                {
                    // 2026-10-29 Mac demo: a worker sent key "1 2 3 4 Return" (the xdotool style
                    // computer-use takes) and got "unknown key", then spent a step per digit.
                    // Space-separated keys are pressed one after another; "ctrl shift s" is still
                    // one chord.
                    for _ in 0..reps {
                        for p in &pieces {
                            if p.chars().count() == 1 && key_from_name(p).is_none() {
                                // a symbol such as * or = is typed, as for a lone key
                                self.be().type_text(p, &|| self.keyboard_guard())?;
                            } else {
                                self.send_combo(p, 1)?;
                            }
                            self.sleep(0.012);
                        }
                    }
                    return Ok(format!("pressed {} in turn", pieces.join(", ")));
                }
                let focus = self.be().focus_token().unwrap_or(0);
                self.c.mem().pre_key = (self.be().foreground_window().ok().flatten(), focus);
                let seq0 = if COPY_COMBOS.contains(&combo.as_str()) {
                    self.be().clipboard_sequence().ok().flatten()
                } else {
                    None
                };
                {
                    let _p = crate::prof::span("send_combo");
                    self.send_combo(&combo, reps)?;
                }
                let mut note = format!("pressed {combo}");
                if COPY_COMBOS.contains(&combo.as_str()) {
                    note.push_str(&self.copy_result(seq0));
                }
                if combo == "shift+f10" {
                    // the keyboard's right-click: list the menu it opens
                    let items = self.menu_items(1.0, 0.4);
                    if !items.is_empty() {
                        note.push_str("; menu: ");
                        note.push_str(&self.menu_note(&items));
                    }
                }
                Ok(note)
            }
            _ => {
                let combo = combo_now;
                let keys = parse_key_combo(&combo, Platform::current())?;
                let d = float_or(step, "duration", 0.5)?.clamp(0.0, 30.0);
                self.holding(&keys, || self.sleep_checked(d))?;
                Ok(format!("held {combo} {}s", fmt_g(d)))
            }
        }
    }

    /// After ctrl+c / ctrl+x: did anything reach the clipboard? h-copyfile: a copy with nothing
    /// selected changed nothing, and the model only found out after pasting into an empty folder,
    /// twice. Python `_copy_result`.
    fn copy_result(&self, seq0: Option<u64>) -> String {
        let Some(seq0) = seq0 else {
            return String::new();
        };
        let (changed, _) = self.poll(
            || {
                (self
                    .be()
                    .clipboard_sequence()
                    .ok()
                    .flatten()
                    .unwrap_or(seq0)
                    != seq0)
                    .then_some(())
            },
            0.6,
            0.05,
        );
        if changed.is_none() {
            return "; clipboard unchanged: nothing was copied (is the item or text really selected? \
                    click it once and check it is highlighted)"
                .to_string();
        }
        format!("; clipboard now holds {}", self.clipboard_summary())
    }

    /// What the clipboard holds now, in a few words: files (by name), text, or something else.
    /// Python `_clipboard_summary`.
    fn clipboard_summary(&self) -> String {
        match self.be().clipboard() {
            Ok(ClipboardContent::Files(files)) => {
                let n = files.len();
                let names: Vec<String> = files
                    .iter()
                    .take(5)
                    .map(|f| super::guard::basename(f))
                    .collect();
                let more = if n > 5 {
                    format!(" and {} more", n - 5)
                } else {
                    String::new()
                };
                format!("{n} file(s): {}{more}", names.join(", "))
            }
            Ok(ClipboardContent::Text(val)) => format!(
                "text ({} chars): {}",
                val.chars().count(),
                py_repr(&val.chars().take(60).collect::<String>())
            ),
            Ok(_) => "new content (not text or files)".to_string(),
            Err(_) => "new content".to_string(),
        }
    }

    // ------------------------------------------------------------------ pointer

    fn describe_point(&self, p: Point) -> String {
        let Ok(Some(hit)) = self.be().element_at(p, 3, secs(0.25)) else {
            return String::new();
        };
        // a list row's name cell is a child of the row: report the row. Python walks the element and
        // two parents looking for one.
        let chain = [
            Some(&hit.element),
            hit.ancestors.first(),
            hit.ancestors.get(1),
        ];
        let pick = chain
            .iter()
            .flatten()
            .find(|e| e.role.is_item())
            .copied()
            .unwrap_or(&hit.element);
        let name = pick.name.trim();
        if name.is_empty() || matches!(pick.role, Role::Pane | Role::Window) {
            return String::new();
        }
        let tail = if pick.role.is_container() {
            ", no item there"
        } else {
            ""
        };
        format!(
            " (on {} {}{tail})",
            pick.role.words(),
            py_repr(&name.chars().take(60).collect::<String>())
        )
    }

    fn pointer(
        &self,
        step: &Args,
        action: &str,
        coord: Option<&Value>,
        text: &str,
        scroll_amount: Option<i64>,
        prefix: &mut String,
    ) -> StepResult<String> {
        // 2026-10-08: at=[fx, fy] (fractions of window=...) replaces coordinate, whatever the resolution
        let at_point = self.at_point(step, "at")?;
        let (mut x, mut y) = match (at_point, coord) {
            (Some(p), _) => (p.x, p.y),
            (None, Some(c)) => self.scale().to_screen_value(c)?,
            (None, None) => {
                let p = self.be().cursor_position()?;
                (p.x, p.y)
            }
        };
        self.set_last_point(Point { x, y });
        let coord_repr = match coord {
            Some(c) => py_value_repr(c),
            // a window-relative point has no screenshot coordinate to echo: say where it landed
            None if at_point.is_some() => self.pt(x, y),
            None => "None".to_string(),
        };
        match action {
            "left_click_drag" => {
                let start = get(step, "start_coordinate")
                    .cloned()
                    .unwrap_or(Value::Null);
                let (sx, sy, start_repr) = match self.at_point(step, "start_at")? {
                    Some(p) => (p.x, p.y, self.pt(p.x, p.y)),
                    None => {
                        let (sx, sy) = self.scale().to_screen_value(&start)?;
                        (sx, sy, py_value_repr(&start))
                    }
                };
                // 2026-09-21: host_at already refuses either end that is a host or a not-allowed
                // window on its own, same as every other pointer action; a drag whose two ends land in
                // two DIFFERENT windows is allowed as long as both individually pass that check (both
                // unrestricted, with no only_apps active, or both named on the allowlist).
                self.host_at(Point { x: sx, y: sy })?;
                self.host_at(Point { x, y })?;
                let h1 = self
                    .be()
                    .window_at_point(Point { x: sx, y: sy })
                    .ok()
                    .flatten();
                let h2 = self.be().window_at_point(Point { x, y }).ok().flatten();
                let cross =
                    matches!((h1, h2), (Some(a), Some(b)) if self.root_of(a) != self.root_of(b));
                self.with_modifiers(text, || {
                    self.be()
                        .mouse_drag(Point { x: sx, y: sy }, Point { x, y }, &|| {
                            self.pointer_guard(Point { x: sx, y: sy })?;
                            self.pointer_guard(Point { x, y })
                        })
                        .map_err(StepError::from)
                })?;
                Ok(format!(
                    "dragged {start_repr} -> {coord_repr}{}",
                    if cross { " (cross-window)" } else { "" }
                ))
            }
            "mouse_move" => {
                self.pointer_guard(Point { x, y })?;
                self.be().mouse_move(Point { x, y })?;
                Ok(format!("moved to {coord_repr}"))
            }
            "left_mouse_down" | "left_mouse_up" => {
                let point = Point { x, y };
                self.check_click(point, false, step)?;
                // Pause cleanup shares this short lock. It cannot race a successful press
                // between native injection and the ownership record.
                let mut owned = self
                    .c
                    .owned_mouse
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                self.input_guard()?;
                self.be().mouse_move(point)?;
                self.pointer_guard(self.be().cursor_position()?)?;
                self.be().mouse_button(MouseButton::Left, true)?;
                *owned = true;
                Ok(action.replace('_', " "))
            }
            "scroll" => {
                self.host_at(Point { x, y })?;
                let direction = if is_truthy(step, "scroll_direction") {
                    text_arg(step, "scroll_direction")
                } else {
                    "down".to_string()
                };
                let amount = match scroll_amount {
                    Some(n) if n != 0 => n,
                    _ => int_or(step, "scroll_amount", 3)?,
                }
                .clamp(1, 30) as u32;
                self.with_modifiers(text, || {
                    let Some(dir) = ScrollDirection::parse(&direction) else {
                        // Python moved the pointer first, then refused the direction
                        self.be().mouse_move(Point { x, y })?;
                        return Err(StepError::value(format!(
                            "bad direction: {}",
                            py_repr(&direction)
                        )));
                    };
                    self.be()
                        .mouse_scroll(Point { x, y }, dir, amount, &|| {
                            self.pointer_guard(Point { x, y })
                        })
                        .map_err(StepError::from)
                })?;
                Ok(format!("scrolled {direction} {amount}"))
            }
            _ => {
                self.host_at(Point { x, y })?;
                if action == "double_click" {
                    let hit = self.be().window_at_point(Point { x, y }).ok().flatten();
                    self.refuse_explorer_open(hit.map(|h| self.root_of(h)))?;
                    let (nx, ny, snapped) = self.snap_to_word(x, y);
                    x = nx;
                    y = ny;
                    if !snapped.is_empty() {
                        prefix.push_str(&format!(
                            "(aim: moved onto the word {}) ",
                            py_repr(&snapped)
                        ));
                    }
                }
                let p = Point { x, y };
                if matches!(action, "left_click" | "double_click" | "triple_click") {
                    self.check_click(p, action != "left_click", step)?;
                }
                let button = match action {
                    "right_click" => MouseButton::Right,
                    "middle_click" => MouseButton::Middle,
                    _ => MouseButton::Left,
                };
                let landed = if matches!(action, "left_click" | "right_click" | "double_click") {
                    self.describe_point(p)
                } else {
                    String::new()
                };
                let in_menu = action == "left_click" && !self.popup_menus().is_empty();
                if action == "triple_click" {
                    self.with_modifiers(text, || {
                        for _ in 0..3 {
                            self.be()
                                .mouse_click(p, MouseButton::Left, 1, &|| self.pointer_guard(p))?;
                        }
                        Ok(())
                    })?;
                } else {
                    let count = if action == "double_click" { 2 } else { 1 };
                    self.with_modifiers(text, || {
                        self.be()
                            .mouse_click(p, button, count, &|| self.pointer_guard(p))
                            .map_err(StepError::from)
                    })?;
                }
                let at = if coord.is_some() {
                    coord_repr
                } else {
                    self.pt(x, y)
                };
                let mut note = format!("{} at {at}{landed}", action.replace('_', " "));
                if action == "right_click" {
                    let items = self.right_click_menu_items();
                    if !items.is_empty() {
                        note.push_str("; menu: ");
                        note.push_str(&self.menu_note(&items));
                    }
                } else if in_menu {
                    // A click inside an open menu often opens a submenu: say what is showing now.
                    self.sleep(0.15);
                    let items = self.menu_items(0.3, 0.4);
                    if !items.is_empty() {
                        note.push_str("; menu now: ");
                        note.push_str(&self.menu_note(&items));
                    }
                }
                Ok(note)
            }
        }
    }

    /// Does the last step of a call end in a text-only answer?
    pub(crate) fn no_trailing_shot(action: Option<&str>) -> bool {
        action.is_some_and(|a| NO_TRAILING_SHOT.contains(&a))
    }
}
