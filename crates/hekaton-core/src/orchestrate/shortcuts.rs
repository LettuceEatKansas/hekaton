//! Task shortcuts: calls that replace a long run of clicks with one native call (2026-10-08). Port of
//! `shortcuts.py` (everything but `folder`, which is [`super::folder`]).
//!
//! Why: a local 27B model spends about 13 s per turn, and PLAN-fewer-turns.md items 2 and 6 say the
//! only lever is fewer turns. Preferred order for any job: a native API, then the keyboard, then
//! clicking.
//!
//! * `window` closes, minimises, maximises, restores, focuses or moves a window by ref, title part or
//!   exe name, with the OS's own window messages. Zero clicks, no screenshot needed. A close is the
//!   polite kind (the app may ask to save); a process is never killed.
//! * `at` / `start_at` are window-relative pointer points: fractions 0..1 of a window instead of
//!   screenshot pixels, so a click survives a different resolution or window position.
//! * `save_to` makes `screenshot` write the picture to a file (png or jpg).
//! * `open_app` on an http(s) URL starts the default browser and waits for its window, and accepts a
//!   browser that adds a tab instead of a window.
//!
//! Every one of these goes through the same safety layer as a click or a keystroke: the first-run
//! acknowledgement and the pause hotkey (the daemon), the human-input wait, the `only_apps`
//! allowlist, the refusal to touch the windows of the process calling hekaton, protected processes,
//! and the owner's hard line in `destructive`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::Ctx;
use super::args::{Args, flag, get, text as text_arg};
use super::ctx::is_desktop_or_taskbar;
use super::error::{StepError, StepResult};
use super::files::{expand_user, expand_vars};
use super::refs::{WindowLine, format_windows};
use crate::backend::{AppTarget, LaunchSpec, Point, Rect, WindowOp, WindowRef, WindowState};
use crate::imaging;
use crate::textutil::{fmt_g, py_repr, py_value_repr};

const NEEDS_WINDOW: &str = "window needs window=...: a ref like w3 (list_windows), part of a title, \
                            or an app exe name like notepad.exe";

/// The operations of the `window` action, in the order the error message lists them.
pub(crate) const WINDOW_OPS: [&str; 6] =
    ["close", "minimize", "maximize", "restore", "focus", "move"];

fn op_alias(op: &str) -> &str {
    match op {
        "min" => "minimize",
        "max" => "maximize",
        "activate" | "front" | "raise" => "focus",
        "shut" => "close",
        other => other,
    }
}

/// Window classes and bundle ids of the browsers `open_app` on a URL waits for.
const BROWSER_CLASSES: [&str; 2] = ["chrome_widgetwin_1", "mozillawindowclass"];
const BROWSER_APPS: [&str; 14] = [
    "chrome.exe",
    "msedge.exe",
    "brave.exe",
    "firefox.exe",
    "opera.exe",
    "vivaldi.exe",
    "arc.exe",
    "com.apple.safari",
    "com.google.chrome",
    "com.microsoft.edgemac",
    "com.brave.browser",
    "org.mozilla.firefox",
    "com.operasoftware.opera",
    "company.thebrowser.browser",
];

/// "doc - App", "page | Site", with an en or em dash too. Splits a title into its segments.
static TITLE_SEP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("\\s[-|\u{2013}\u{2014}]\\s").expect("TITLE_SEP"));
static W_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^w\d+$").expect("W_REF"));

/// The segments of a lowercased title, trimmed.
fn title_segments(title_lower: &str) -> Vec<String> {
    TITLE_SEP
        .split(title_lower)
        .map(|s| s.trim().to_string())
        .collect()
}

/// A window a `window` spec named: its handle, and the `wN` ref and title for result lines.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub window: WindowRef,
    /// The stable `wN` number; `None` for a window the list does not show (untitled).
    pub reference: Option<u32>,
    pub title: String,
    pub process: String,
}

impl Target {
    /// `w3 "Downloads"`, or `hwnd:123 "..."` for a window with no ref.
    pub(crate) fn label(&self) -> String {
        match self.reference {
            Some(r) => format!("w{r} \"{}\"", self.title),
            None => format!("hwnd:{} \"{}\"", self.window.raw(), self.title),
        }
    }
}

/// Is `text` a plain web address (`http://` or `https://` and no spaces or control characters)?
/// Anything else keeps going through `open_app`'s old route.
pub(crate) fn is_url(text: &str) -> bool {
    let t = text.trim();
    let starts = |n: usize, scheme: &str| {
        t.get(..n)
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    };
    let rest = if starts(8, "https://") {
        &t[8..]
    } else if starts(7, "http://") {
        &t[7..]
    } else {
        return false;
    };
    let n = rest.chars().count();
    (1..=2040).contains(&n) && !rest.chars().any(|c| c.is_whitespace() || (c as u32) < 0x20)
}

/// `os.path.isabs` for either kind of path.
fn is_abs(path: &str) -> bool {
    let b = path.as_bytes();
    path.starts_with(['/', '\\'])
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'/' | b'\\'))
}

impl Ctx<'_> {
    // ------------------------------------------------------------------ finding windows

    /// The exe a person means by a window: a Store app's real exe, not ApplicationFrameHost.
    fn proc_name(&self, w: &WindowLine) -> String {
        let name = w.process.to_lowercase();
        if name == "applicationframehost.exe"
            && let Ok(Some(app)) = self.be().hosted_app(w.window)
        {
            let app = app.to_lowercase();
            if !app.is_empty() {
                return app;
            }
        }
        name
    }

    /// The `Target` for `window` from the given window list; a stand-in (no `wN` ref) when the window
    /// is untitled and so not in the list.
    pub(crate) fn target_of(&self, window: WindowRef, listed: &[WindowLine]) -> Target {
        if let Some(l) = listed.iter().find(|l| l.window == window) {
            return Target {
                window,
                reference: Some(l.reference),
                title: l.title.clone(),
                process: l.process.clone(),
            };
        }
        let info = self.info(window);
        Target {
            window,
            reference: None,
            title: info.as_ref().map(|i| i.title.clone()).unwrap_or_default(),
            process: info
                .map(|i| i.app)
                .filter(|a| !a.is_empty())
                .unwrap_or_else(|| "?".to_string()),
        }
    }

    /// Windows a `window` spec names, and a note for the result line.
    ///
    /// `spec` is one of: a ref (`w3`, `hwnd:123`, `pid:456`), `front` (the window in front), an exe
    /// name (`notepad.exe`: every window of it, the first is used unless `all`), an exact title, an
    /// app name without `.exe` (`notepad`), a whole segment of a title (`c` in `c - File Explorer`),
    /// or part of a title. A part of a title that fits several windows is an error that lists them; an
    /// exe or app name with several windows takes the first (top of the stack) and says how many
    /// there were. Python `shortcuts.resolve`.
    pub(crate) fn resolve_window(
        &self,
        spec: &str,
        all: bool,
    ) -> StepResult<(Vec<Target>, String)> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(StepError::value(NEEDS_WINDOW));
        }
        let low = spec.to_lowercase();
        if low == "front" || low == "foreground" {
            let Some(fg) = self.fg_root() else {
                return Err(StepError::target("no window is in front"));
            };
            return Ok((
                vec![self.target_of(fg, &[])],
                "the window in front".to_string(),
            ));
        }
        let wins = self.c.window_refs().list(false)?;
        if low.starts_with("hwnd:") || low.starts_with("pid:") || W_REF.is_match(&low) {
            let w = self.c.window_refs().resolve_in(Some(spec), Some(&wins))?;
            return Ok((vec![self.target_of(w, &wins)], String::new()));
        }
        // the desktop and the taskbar are never matched by name
        let live: Vec<&WindowLine> = wins
            .iter()
            .filter(|w| {
                !w.cloaked
                    && !self
                        .info(w.window)
                        .is_some_and(|i| is_desktop_or_taskbar(&i))
            })
            .collect();
        let mut app_kind = true;
        let hits: Vec<&WindowLine> = if low.ends_with(".exe") {
            live.iter()
                .copied()
                .filter(|w| self.proc_name(w) == low)
                .collect()
        } else {
            app_kind = false;
            let mut hits: Vec<&WindowLine> = live
                .iter()
                .copied()
                .filter(|w| w.title.to_lowercase() == low)
                .collect();
            if hits.is_empty() {
                app_kind = true;
                hits = live
                    .iter()
                    .copied()
                    .filter(|w| {
                        let p = self.proc_name(w);
                        let p = p.strip_suffix(".exe").unwrap_or(&p);
                        // macOS reports a bundle id (`com.apple.textedit`): its last part is the app
                        // name people say (`TextEdit`).
                        p == low
                            || (p.matches('.').count() >= 2 && p.rsplit('.').next() == Some(&low))
                    })
                    .collect();
            }
            if hits.is_empty() {
                // 2026-10-11: Brave is `com.brave.Browser`, so "Brave" names the middle part.
                hits = live
                    .iter()
                    .copied()
                    .filter(|w| {
                        let p = self.proc_name(w);
                        p.matches('.').count() >= 2 && p.split('.').skip(1).any(|part| part == low)
                    })
                    .collect();
            }
            if hits.is_empty() {
                // A whole title segment ("c" in "c - File Explorer") beats a bare substring, so a
                // short folder or document name is not ambiguous with every title that contains the
                // letter.
                app_kind = false;
                hits = live
                    .iter()
                    .copied()
                    .filter(|w| title_segments(&w.title.to_lowercase()).contains(&low))
                    .collect();
            }
            if hits.is_empty() {
                hits = live
                    .iter()
                    .copied()
                    .filter(|w| w.title.to_lowercase().contains(&low))
                    .collect();
            }
            hits
        };
        if hits.is_empty() {
            return Err(StepError::target(format!(
                "no window matches {}; list_windows shows what is open",
                py_repr(spec)
            )));
        }
        let to_targets = |list: &[&WindowLine]| -> Vec<Target> {
            list.iter()
                .map(|l| self.target_of(l.window, &wins))
                .collect()
        };
        if all || hits.len() == 1 {
            let note = if all && hits.len() > 1 {
                format!("all {} windows of {spec}", hits.len())
            } else {
                String::new()
            };
            return Ok((to_targets(&hits), note));
        }
        if app_kind {
            return Ok((
                to_targets(&hits[..1]),
                format!(
                    "first of {} windows of {spec}; all=true takes every one",
                    hits.len()
                ),
            ));
        }
        let shown: Vec<WindowLine> = hits.iter().take(8).map(|l| (*l).clone()).collect();
        Err(StepError::target(format!(
            "{} windows match {}, ambiguous: {}. Use a wN ref, or all=true",
            hits.len(),
            py_repr(spec),
            format_windows(&shown).join("; ")
        )))
    }

    /// The refusals every input action has, applied to one window. Errors if input may not go there.
    /// Python `shortcuts.guard`.
    pub(crate) fn guard_window(&self, w: &Target) -> StepResult<()> {
        let root = self.root_of(w.window);
        if self.info(root).is_some_and(|i| is_desktop_or_taskbar(&i)) {
            return Err(StepError::target(
                "refused: that is the desktop or the taskbar, not an app window",
            ));
        }
        self.check_not_protected(root)?;
        self.refuse_host(root)?;
        let app = self.info(root).map(|i| i.app).unwrap_or_default();
        // the owner's hard line
        self.o.policy.check_consent_window(&app, "the window")?;
        self.check_allowed(root)
    }

    pub(crate) fn state_of(&self, w: WindowRef) -> StepResult<WindowState> {
        Ok(self.be().window_geometry(w)?.state)
    }

    /// Has the window gone from the screen: destroyed, or no longer listed (hidden to the tray)?
    fn window_closed(&self, w: WindowRef) -> bool {
        if self.be().window_info(w).is_err() {
            return true;
        }
        match self.be().list_windows(false) {
            Ok(list) => !list.iter().any(|i| i.window == w),
            Err(_) => false,
        }
    }

    // ------------------------------------------------------------------ window action

    /// `move`'s `rect`: `[x, y, width, height]` in screen pixels, checked.
    fn rect_arg(&self, value: Option<&Value>) -> StepResult<Rect> {
        let bad = || {
            StepError::value(
                "move needs rect: [x, y, width, height] in screen pixels, as list_windows shows",
            )
        };
        let Some(Value::Array(items)) = value else {
            return Err(bad());
        };
        if items.len() != 4 || items.iter().any(|v| !v.is_number()) {
            return Err(bad());
        }
        let n: Vec<i64> = items
            .iter()
            .map(|v| {
                v.as_i64()
                    .unwrap_or_else(|| v.as_f64().unwrap_or(0.0) as i64)
            })
            .collect();
        let shown = value.map_or_else(String::new, py_value_repr);
        let (x, y, w, h) = (n[0], n[1], n[2], n[3]);
        if !((20..=20000).contains(&w) && (20..=20000).contains(&h)) {
            return Err(StepError::value(format!(
                "move rect {shown}: width and height must be between 20 and 20000"
            )));
        }
        let vs = self.be().virtual_screen()?;
        let (vl, vt, vw, vh) = (
            i64::from(vs.left),
            i64::from(vs.top),
            i64::from(vs.width()),
            i64::from(vs.height()),
        );
        if x + w <= vl || y + h <= vt || x >= vl + vw || y >= vt + vh {
            return Err(StepError::value(format!(
                "move rect {shown} would put the window entirely off screen (screen is \
                 {vl},{vt},{vw},{vh})"
            )));
        }
        let clamp = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        Ok(Rect::from_xywh(clamp(x), clamp(y), clamp(w), clamp(h)))
    }

    fn do_window_op(&self, op: &str, w: &Target, rect: Option<Rect>) -> StepResult<String> {
        let window = w.window;
        let label = w.label();
        match op {
            "close" => {
                self.be().window_op(window, WindowOp::Close)?;
                let (gone, _) = self.poll(|| self.window_closed(window).then_some(()), 1.5, 0.05);
                if gone.is_some() {
                    return Ok(format!("closed {label}"));
                }
                Ok(format!(
                    "asked {label} to close, but it is still open (probably asking to save or \
                     confirm: look at the screen and answer it yourself)"
                ))
            }
            "focus" => {
                self.be()
                    .restore_window(window, std::time::Duration::from_secs(2))?;
                self.be().bring_to_foreground(window)?;
                Ok(format!("focused {label}"))
            }
            "move" => {
                let want = rect.unwrap_or_default();
                if self.state_of(window)? != WindowState::Normal {
                    // a maximised or minimised window ignores a move
                    self.be().window_op(window, WindowOp::Restore)?;
                    self.poll(
                        || (self.state_of(window).ok() == Some(WindowState::Normal)).then_some(()),
                        1.0,
                        0.05,
                    );
                }
                self.be().window_op(window, WindowOp::Move(want))?;
                let asked = [want.left, want.top, want.width(), want.height()];
                // 2026-10-11 (macOS): the window animates to its new place, so a read 0.1s in
                // reported the old position. Wait up to 1s for it to land.
                let read = || {
                    self.be()
                        .window_geometry(window)
                        .map(|g| [g.frame.left, g.frame.top, g.frame.width(), g.frame.height()])
                };
                let close = |r: [i32; 4]| r.iter().zip(asked).all(|(g, a)| (g - a).abs() <= 1);
                let mut got = read()?;
                for _ in 0..20 {
                    if close(got) {
                        break;
                    }
                    self.sleep(0.05);
                    got = read()?;
                }
                if close(got) {
                    got = asked;
                }
                let fmt = |r: [i32; 4]| format!("[{}, {}, {}, {}]", r[0], r[1], r[2], r[3]);
                let mut line = format!("moved {label} to {}", fmt(got));
                if got != asked {
                    line.push_str(&format!(" (asked {}; the app limits its size)", fmt(asked)));
                }
                Ok(line)
            }
            _ => {
                let (want, wop) = match op {
                    "minimize" => (WindowState::Minimized, WindowOp::Minimize),
                    "maximize" => (WindowState::Maximized, WindowOp::Maximize),
                    _ => (WindowState::Normal, WindowOp::Restore),
                };
                self.be().window_op(window, wop)?;
                let (reached, _) = self.poll(
                    || (self.state_of(window).ok() == Some(want)).then_some(()),
                    1.5,
                    0.05,
                );
                if reached.is_some() {
                    return Ok(format!(
                        "{} {label}",
                        if op == "restore" {
                            "restored"
                        } else {
                            want.word()
                        }
                    ));
                }
                let now = self.state_of(window).map_or("gone", WindowState::word);
                Ok(format!(
                    "asked {label} to {op}, but it is {now} (the app may not allow it)"
                ))
            }
        }
    }

    /// The `window` action. Python `shortcuts.run_window`.
    pub(crate) fn run_window(&self, step: &Args) -> StepResult<String> {
        let mut op = text_arg(step, "op");
        if op.is_empty() {
            op = text_arg(step, "text");
        }
        let op = op.trim().to_lowercase();
        let op = op_alias(&op).to_string();
        if !WINDOW_OPS.contains(&op.as_str()) {
            return Err(StepError::value(format!(
                "window needs op: one of {}",
                WINDOW_OPS.join(", ")
            )));
        }
        let rect = if op == "move" {
            Some(self.rect_arg(step.get("rect"))?)
        } else {
            None
        };
        let all = flag(step.get("all"), false);
        let (targets, note) = self.resolve_window(&text_arg(step, "window"), all)?;
        let mut ok: Vec<&Target> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        for w in &targets {
            match self.guard_window(w) {
                Ok(()) => ok.push(w),
                Err(e @ (StepError::Target(_) | StepError::Permission(_))) => {
                    if targets.len() == 1 {
                        return Err(e);
                    }
                    refused.push(format!("skipped {}: {e}", w.label()));
                }
                Err(e) => return Err(e),
            }
        }
        if ok.is_empty() {
            return Err(StepError::target(
                refused
                    .first()
                    .cloned()
                    .unwrap_or_else(|| NEEDS_WINDOW.to_string()),
            ));
        }
        self.wait_for_human()?;
        let mut lines = Vec::new();
        for w in &ok {
            lines.push(self.do_window_op(&op, w, rect)?);
        }
        self.mark_ours();
        if matches!(op.as_str(), "minimize" | "close") {
            let mut mem = self.c.mem();
            if mem.target.is_some_and(|t| ok.iter().any(|w| w.window == t)) {
                // `run` brings the "app the model was using" back before the next acting step; a
                // window the caller just minimised or closed on purpose must not be pulled up again
                mem.target = None;
            }
        }
        let mut parts = Vec::new();
        if !note.is_empty() {
            parts.push(note);
        }
        parts.extend(lines);
        parts.extend(refused);
        Ok(parts.join("; "))
    }

    // ------------------------------------------------------------------ window-relative points

    fn fractions(value: &Value, key: &str) -> StepResult<(f64, f64)> {
        let Value::Array(items) = value else {
            return Err(StepError::value(format!(
                "{key} must be [fx, fy]: two fractions from 0 to 1 of the window's width and height"
            )));
        };
        let nums: Vec<f64> = items.iter().filter_map(Value::as_f64).collect();
        if items.len() != 2 || nums.len() != 2 {
            return Err(StepError::value(format!(
                "{key} must be [fx, fy]: two fractions from 0 to 1 of the window's width and height"
            )));
        }
        let (fx, fy) = (nums[0], nums[1]);
        if !((0.0..=1.0).contains(&fx) && (0.0..=1.0).contains(&fy)) {
            return Err(StepError::value(format!(
                "{key} {} is outside 0..1; 0,0 is the top left and 1,1 the bottom right",
                py_value_repr(value)
            )));
        }
        Ok((fx, fy))
    }

    /// The screen pixel for `args[key]` = `[fx, fy]`, or `None` when the call has no such key.
    ///
    /// Fractions are of the target window's client area (the part inside the frame and title bar), or
    /// of the whole window when `at_rect` is `window` or the client area is unknown. The target is
    /// `window` (same forms as the window action) or the window in front. 1,1 is the last pixel.
    /// Python `shortcuts.at_point`.
    pub(crate) fn at_point(&self, step: &Args, key: &str) -> StepResult<Option<Point>> {
        let Some(value) = get(step, key) else {
            return Ok(None);
        };
        let (fx, fy) = Self::fractions(value, key)?;
        let spec = text_arg(step, "window");
        let target = if spec.is_empty() {
            let Some(fg) = self.fg_root() else {
                return Err(StepError::target(format!(
                    "{key} needs a window: none is in front, so pass window=..."
                )));
            };
            self.target_of(fg, &[])
        } else {
            self.resolve_window(&spec, false)?
                .0
                .into_iter()
                .next()
                .ok_or_else(|| StepError::target(NEEDS_WINDOW))?
        };
        self.guard_window(&target)?;
        let geo = self.be().window_geometry(target.window)?;
        if geo.state == WindowState::Minimized {
            return Err(StepError::target(format!(
                "{} is minimized; window op=restore first",
                target.label()
            )));
        }
        let mut area = geo.frame;
        if text_arg(step, "at_rect").to_lowercase() != "window"
            && let Some(client) = geo.client.filter(|c| !c.is_empty())
        {
            area = client;
        }
        let (width, height) = (area.width(), area.height());
        if width <= 0 || height <= 0 {
            return Err(StepError::target(format!(
                "{} has no visible area",
                target.label()
            )));
        }
        Ok(Some(Point {
            x: area.left + (width - 1).min((fx * f64::from(width)) as i32),
            y: area.top + (height - 1).min((fy * f64::from(height)) as i32),
        }))
    }

    // ------------------------------------------------------------------ screenshot save_to

    /// The checked absolute path `save_to` names. Python `shortcuts._save_path`.
    pub(crate) fn save_path(&self, raw: &str) -> StepResult<String> {
        let text = raw.trim().trim_matches('"');
        if text.is_empty() {
            return Err(StepError::value(
                "save_to needs a file path ending in .png or .jpg",
            ));
        }
        let env = self.cfg().env();
        let repaired = super::open::repair_escapes(text, &|p| Path::new(p).is_file());
        let path = expand_vars(env, &expand_user(env, &repaired));
        if !is_abs(&path) {
            return Err(StepError::value(format!(
                "save_to needs a full path (got {})",
                py_repr(raw)
            )));
        }
        let path = crate::folders::normpath(&path);
        let ext = Path::new(&path)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
            return Err(StepError::value("save_to must end in .png, .jpg or .jpeg"));
        }
        // the owner's hard line: system paths, credentials
        self.o.policy.check_path(&path, "the screenshot path")?;
        // under an allowlist: %TEMP% only
        self.check_wait_path_allowed(&path, "save_to may only write")?;
        Ok(path)
    }

    /// `screenshot` with `save_to`: write the picture (the whole display, the call's scope, or one
    /// window with `window=`) at full resolution, and still return the usual picture unless `shot=false`.
    /// Python `shortcuts.save_screenshot`.
    pub(crate) fn save_screenshot(&self, step: &Args) -> StepResult<(String, Option<String>)> {
        let path = self.save_path(&text_arg(step, "save_to"))?;
        let spec = text_arg(step, "window");
        let (area, what) = if spec.is_empty() {
            // under a scope the display is the window (Python `_scope_name`)
            let what = if self.scoped() {
                self.scope_name()
            } else {
                "the display".to_string()
            };
            (self.display_rect(), what)
        } else {
            let target = self
                .resolve_window(&spec, false)?
                .0
                .into_iter()
                .next()
                .ok_or_else(|| StepError::target(NEEDS_WINDOW))?;
            self.check_not_protected(self.root_of(target.window))?;
            let geo = self.be().window_geometry(target.window)?;
            if geo.state == WindowState::Minimized {
                return Err(StepError::target(format!(
                    "{} is minimized; window op=restore first",
                    target.label()
                )));
            }
            (geo.visible, target.label())
        };
        if area.width() <= 0 || area.height() <= 0 {
            return Err(StepError::target(
                "nothing to capture: the window has no visible area",
            ));
        }
        let img = self.grab(area)?;
        let existed = Path::new(&path).exists();
        if let Some(parent) = Path::new(&path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| StepError::runtime(e.to_string()))?;
        }
        let png = Path::new(&path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("png"));
        let bytes = if png {
            imaging::encode_png(&img)?
        } else {
            imaging::encode_jpeg_quality(&img, 90)?
        };
        let part = format!("{path}.part");
        let written = std::fs::write(&part, &bytes).and_then(|()| {
            // a rename onto an existing file fails on some systems: replace it, as os.replace does
            std::fs::rename(&part, &path).or_else(|_| {
                let _ = std::fs::remove_file(&path);
                std::fs::rename(&part, &path)
            })
        });
        if let Err(e) = written {
            let _ = std::fs::remove_file(&part);
            return Err(StepError::runtime(e.to_string()));
        }
        let kb = bytes.len() / 1024;
        let note = format!(
            "saved {what} to {path} ({}x{}px, {kb} KB{})",
            img.width(),
            img.height(),
            if existed {
                ", replaced the old file"
            } else {
                ""
            }
        );
        let shot = if flag(step.get("shot"), true) {
            Some(self.screenshot_b64(None)?)
        } else {
            None
        };
        // the picture that comes back uses the call's frame, so say which
        let note = if shot.is_some() {
            format!("{note}\n{}", self.frame_line())
        } else {
            note
        };
        Ok((note, shot))
    }

    // ------------------------------------------------------------------ open_app on a URL

    pub(crate) fn is_browser(&self, w: WindowRef) -> bool {
        let Some(i) = self.info(w) else {
            return false;
        };
        BROWSER_CLASSES.contains(&i.class.to_lowercase().as_str())
            || BROWSER_APPS.contains(&i.app.to_lowercase().as_str())
    }

    /// A browser window that now shows something new, compared with the windows listed before the
    /// launch: the window in front when it is another one or its title changed, a browser window that
    /// did not exist, or any browser window whose title changed. 2026-11-09: a URL opened as a new TAB
    /// of a browser window that was not in front (Windows keeps focus where it was) or whose title only
    /// changed in the background was reported as a failure nine times in nine.
    pub(crate) fn browser_page_changed(
        &self,
        wins: &[WindowLine],
        before: &HashMap<WindowRef, String>,
        fg0: Option<WindowRef>,
    ) -> Option<WindowRef> {
        if let Some(fg) = self.fg_root()
            && self.is_browser(fg)
        {
            let cur = wins.iter().find(|w| w.window == fg);
            let was = before.get(&fg);
            if Some(fg) != fg0 || (cur.is_some_and(|c| Some(&c.title) != was) && was.is_some()) {
                return Some(fg);
            }
        }
        let browsers = || wins.iter().filter(|w| self.is_browser(w.window));
        // a new browser window that did not take focus
        if let Some(w) = browsers().find(|w| !before.contains_key(&w.window)) {
            return Some(w.window);
        }
        // a window that was there and now shows another page
        browsers()
            .find(|w| before.get(&w.window).is_some_and(|t| *t != w.title))
            .map(|w| w.window)
    }

    /// Bring `w` to the front unless it already is, and name it.
    pub(crate) fn raise_browser(&self, w: WindowRef, wins: &[WindowLine]) -> StepResult<Target> {
        if self.fg_root() != Some(w) {
            self.check_not_protected(w)?;
            self.be().bring_to_foreground(w)?;
        }
        Ok(self.target_of(w, wins))
    }

    /// Open an http(s) URL in the default browser and wait until a browser window is in front and has
    /// changed (a new window, or a new page title). Only plain web addresses come here. Python
    /// `shortcuts.open_url`.
    #[cfg(test)]
    pub(crate) fn open_url(&self, text: &str, wait: f64) -> StepResult<String> {
        self.open_url_in(text, wait, None)
    }

    /// [`Self::open_url`] in a named browser (an app name or bundle id) instead of the system default.
    /// 2026-10-30: with Safari in front and Brave the default, every URL opened in Brave and the
    /// agent lost its Safari session.
    pub(crate) fn open_url_in(
        &self,
        text: &str,
        wait: f64,
        browser: Option<&str>,
    ) -> StepResult<String> {
        let url = text.trim();
        // the owner's hard line
        self.o.policy.check_command(url, "the URL")?;
        let (target, args) = match browser.map(str::trim).filter(|b| !b.is_empty()) {
            Some(name) => {
                self.o.policy.check_command(name, "the browser")?;
                match self.be().resolve_app(name).ok().flatten() {
                    Some(t) if !matches!(t, AppTarget::Other(_)) => (t, url.to_string()),
                    _ => {
                        return Err(StepError::target(format!(
                            "no browser named {} (an app name like Safari, or a bundle id); \
                             leave browser out to use the system default",
                            py_repr(name)
                        )));
                    }
                }
            }
            None => (AppTarget::Other(url.to_string()), String::new()),
        };
        let fg0 = self.fg_root();
        let before: HashMap<WindowRef, String> = self
            .c
            .window_refs()
            .list(false)?
            .into_iter()
            .map(|w| (w.window, w.title))
            .collect();
        // the default browser must not run with the daemon's admin rights: `deelevate` starts it at
        // the user's normal level
        self.be().launch(&LaunchSpec {
            target,
            args,
            deelevate: true,
        })?;
        let arrived = || -> Option<StepResult<Target>> {
            let wins = self.c.window_refs().list(false).ok()?;
            let w = self.browser_page_changed(&wins, &before, fg0)?;
            Some(self.raise_browser(w, &wins))
        };
        let (found, _) = self.poll(arrived, wait, 0.25);
        self.remember_target();
        if let Some(found) = found {
            let found = found?;
            return Ok(format!(
                "opened {url} in {} ({})",
                found.label(),
                found.process
            ));
        }
        // 2026-10-30: an unattended run lost steps to navigation that silently did nothing. A browser
        // in front whose title never changed is not "probably loading": say it did not navigate.
        if let Some(fg) = self.fg_root().filter(|w| self.is_browser(*w)) {
            let title = self.info(fg).map(|i| i.title).unwrap_or_default();
            return Err(StepError::target(format!(
                "sent {url} to the browser in front, but its window title is still {} after {}s, so \
                 it did not navigate. If that page was already showing, nothing is wrong; otherwise \
                 screenshot to see what the browser did (a dialog, a blocked pop-up, no network) and \
                 do not assume the page loaded",
                py_repr(&title),
                fmt_g(wait)
            )));
        }
        Err(StepError::target(format!(
            "started {url} but no browser window came to the front within {}s; list_windows or \
             screenshot shows where it went. Do not open it again",
            fmt_g(wait)
        )))
    }
}

#[cfg(test)]
mod pure_tests {
    use super::*;

    // ---- ported from test_only_plain_web_addresses_take_the_url_route
    #[test]
    fn only_plain_web_addresses_take_the_url_route() {
        for (text, yes) in [
            ("https://example.com/a?b=1", true),
            ("HTTP://x.org", true),
            ("https://", false),
            ("notepad", false),
            ("ms-settings:", false),
            ("file:///C:/x.exe", false),
            ("https://a b", false),
            ("javascript:alert(1)", false),
            ("C:\\x.exe", false),
        ] {
            assert_eq!(is_url(text), yes, "{text}");
        }
    }

    // ---- ported from test_a_whole_title_segment_is_not_ambiguous_with_substrings
    #[test]
    fn a_whole_title_segment_is_not_ambiguous_with_substrings() {
        assert_eq!(title_segments("c - file explorer"), ["c", "file explorer"]);
        assert_eq!(title_segments("page | site"), ["page", "site"]);
        assert_eq!(title_segments("editor v4.0.8"), ["editor v4.0.8"]);
        assert_eq!(title_segments("a \u{2013} b \u{2014} c"), ["a", "b", "c"]);
    }

    #[test]
    fn the_op_aliases_map_to_real_ops() {
        for (alias, op) in [
            ("min", "minimize"),
            ("max", "maximize"),
            ("activate", "focus"),
            ("front", "focus"),
            ("raise", "focus"),
            ("shut", "close"),
            ("move", "move"),
        ] {
            assert_eq!(op_alias(alias), op);
        }
    }

    #[test]
    fn absolute_paths_of_either_kind_are_recognised() {
        assert!(is_abs("C:\\Users\\x\\a.png") && is_abs("c:/x.png") && is_abs("/tmp/a.png"));
        assert!(is_abs("\\\\server\\share\\a.png"));
        assert!(!is_abs("rel/a.png") && !is_abs("a.png") && !is_abs("C:a.png"));
    }
}
