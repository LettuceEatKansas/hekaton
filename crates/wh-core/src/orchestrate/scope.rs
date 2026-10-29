//! `scope=`: OBS-style window capture (2026-10-08). While a call is scoped, the "display" the model
//! sees is one window's client area, so the whole screenshot budget (`shot_long`) goes to that window
//! and every coordinate (pixels or 0 to 1000) is relative to it. Port of `computer._SCOPE`,
//! `_capture_rect`, `_scope_now`, `_scope_name`, `_in_view`, `_covered`, `set_scope`, `_step_scope`,
//! `_image_tag` and the `_SCREENLESS` rule of `run_many`.
//!
//! The capture is a crop of the screen, not a read of the window's own surface, so a covered window is
//! raised first (a minimised one restored, or an error). Reading a covered window without raising it
//! is a later improvement through the OS's own window capture.
//!
//! The scope lives in the call's [`super::CallOpts`] behind a shared cell, so it is per call (a new
//! call starts unscoped, nothing leaks to the next caller), the context a recipe step narrows is the
//! same scope, and a step's own `scope` can be put back after the step.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::Value;

use super::Ctx;
use super::actions::StepOut;
use super::args::Args;
use super::error::{StepError, StepResult};
use crate::backend::{Point, Rect, WindowRef, WindowState};
use crate::config::truthy;
use crate::textutil::{py_repr, py_str};

/// Actions that do not look at the screen: a call made of one of these alone ignores scope, so a
/// scope that no longer matches a window cannot block the call that would list what is open.
const SCREENLESS: [&str; 7] = [
    "list_windows",
    "clipboard_get",
    "clipboard_set",
    "list_files",
    "recipe_list",
    "recipe_save",
    "wait_for_file",
];

/// Where a scoped window's centre and four inset corners are looked at to see whether it is covered.
const COVER_PROBES: [(f64, f64); 5] = [(0.5, 0.5), (0.1, 0.1), (0.9, 0.1), (0.1, 0.9), (0.9, 0.9)];

/// The window a call is scoped to.
#[derive(Debug, Clone)]
pub(crate) struct ScopeState {
    /// The window.
    window: WindowRef,
    /// Its title, for notes (the label when it has none).
    title: String,
    /// The last rectangle that could be measured, in screen pixels.
    rect: Rect,
}

/// The call's scope, shared by every context made from the call's options.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScopeCell(Arc<Mutex<Option<ScopeState>>>);

impl ScopeCell {
    fn lock(&self) -> MutexGuard<'_, Option<ScopeState>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self) -> Option<ScopeState> {
        self.lock().clone()
    }

    fn set(&self, state: Option<ScopeState>) {
        *self.lock() = state;
    }

    fn remember_rect(&self, rect: Rect) {
        if let Some(state) = self.lock().as_mut() {
            state.rect = rect;
        }
    }
}

impl Ctx<'_> {
    /// Is this call scoped to a window?
    pub(crate) fn scoped(&self) -> bool {
        self.o.scope.get().is_some()
    }

    /// The rectangle, in screen pixels, that a window's capture covers, or `None` when it has no
    /// visible area now (minimised, closed, zero size or entirely off screen).
    ///
    /// The client area, not the frame: the title bar and borders are chrome the model cannot use, so
    /// every pixel of the budget goes to the content (a game's picture), and a click can never land on
    /// the close button. A window that reports no client area falls back to its visible frame (on
    /// Windows DWM's extended bounds, without the invisible resize border). The same rectangle drives
    /// the screenshot crop and the coordinate mapping, so clicks line up by construction. Clipped to
    /// the virtual screen, because a grab outside every monitor is black. Python `_capture_rect`.
    fn capture_rect(&self, window: WindowRef) -> Option<Rect> {
        let geo = self.be().window_geometry(window).ok()?;
        if geo.state == WindowState::Minimized {
            return None;
        }
        let rect = geo.client.filter(|c| !c.is_empty()).unwrap_or(geo.visible);
        let clipped = match self.be().virtual_screen() {
            Ok(desktop) => rect.intersect(&desktop)?,
            Err(_) => rect,
        };
        (!clipped.is_empty()).then_some(clipped)
    }

    /// The scoped window's rectangle right now (a window that moved or resized mid-call is followed).
    /// Its last good rectangle when the window cannot be measured, so a minimised or closed window
    /// does not turn the trailing screenshot into an error. `None` when the call is not scoped.
    /// Python `_scope_now`.
    pub(crate) fn scope_rect(&self) -> Option<Rect> {
        let state = self.o.scope.get()?;
        match self.capture_rect(state.window) {
            Some(rect) => {
                self.o.scope.remember_rect(rect);
                Some(rect)
            }
            None => Some(state.rect),
        }
    }

    /// `window "Slay the Spire 2"` for notes; the title is cut at 60 characters. Python `_scope_name`.
    pub(crate) fn scope_name(&self) -> String {
        let title = self.o.scope.get().map(|s| s.title).unwrap_or_default();
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        let shown = if title.chars().count() > 60 {
            let cut: String = title.chars().take(57).collect();
            format!("{cut}...")
        } else {
            title
        };
        format!("window \"{shown}\"")
    }

    /// Can the model name this screen point? Always when the call is not scoped. Python `_in_view`.
    pub(crate) fn in_view(&self, cx: f64, cy: f64) -> bool {
        if !self.scoped() {
            return true;
        }
        let d = self.display_rect();
        f64::from(d.left) <= cx
            && cx < f64::from(d.right)
            && f64::from(d.top) <= cy
            && cy < f64::from(d.bottom)
    }

    /// Is another window on top of any of the scoped window's centre and four inset corners? The
    /// capture is a crop of the screen, so a covered window would show the cover. Python `_covered`.
    fn covered(&self, window: WindowRef, rect: Rect) -> bool {
        let root = self.root_of(window);
        let (w, h) = (f64::from(rect.width()), f64::from(rect.height()));
        COVER_PROBES.iter().any(|(fx, fy)| {
            let p = Point::new(rect.left + (w * fx) as i32, rect.top + (h * fy) as i32);
            self.be()
                .window_at_point(p)
                .ok()
                .flatten()
                .is_some_and(|above| self.root_of(above) != root)
        })
    }

    /// Scope this call to one window: screenshots and coordinates cover only that window.
    ///
    /// `spec` has the same forms as the `window` action's target (a `wN` ref, `hwnd:N`, a title or
    /// part of one, an app `.exe` name, `front`). The window goes through the refusals every input
    /// action has (an agent host window, a protected process, the `only_apps` allowlist), is restored
    /// if minimised and raised if anything covers it, both after the wait for a person at the mouse.
    /// Returns its label. Python `set_scope`.
    pub(crate) fn set_scope(&self, spec: &str) -> StepResult<String> {
        self.check_cancelled()?;
        if self.o.read_only || *self.c.owned_mouse.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err(StepError::permission(
                "scope cannot change focus in read_only mode or while tool mouse button is held; release it first",
            ));
        }
        let (targets, _note) = self.resolve_window(spec, false)?;
        let target = targets.into_iter().next().ok_or_else(|| {
            StepError::target(format!(
                "no window matches {}; list_windows shows what is open",
                py_repr(spec)
            ))
        })?;
        let label = target.label();
        self.guard_window(&target)?;
        let window = target.window;
        let mut waited = false;
        if self.state_of(window)? == WindowState::Minimized {
            self.wait_for_human()?;
            self.check_cancelled()?;
            waited = true;
            self.be().restore_window(window, Duration::from_secs(2))?;
            if self.state_of(window)? == WindowState::Minimized {
                return Err(StepError::target(format!(
                    "scope {label} is minimized and could not be restored, so there is nothing to \
                     capture; restore it by hand, or call without scope"
                )));
            }
        }
        let Some(mut rect) = self.capture_rect(window) else {
            return Err(StepError::target(format!(
                "scope {label} has no visible area (zero size or entirely off screen); window \
                 op=move brings it back, or call without scope"
            )));
        };
        if self.covered(window, rect) {
            if !waited {
                self.wait_for_human()?;
            }
            self.check_cancelled()?;
            if let Err(e) = self.be().bring_to_foreground(window) {
                return Err(StepError::target(format!(
                    "scope {label} is covered by another window and could not be brought to the \
                     front: {e}"
                )));
            }
            rect = self.capture_rect(window).unwrap_or(rect);
        }
        let title = if target.title.is_empty() {
            label.clone()
        } else {
            target.title.clone()
        };
        self.o.scope.set(Some(ScopeState {
            window,
            title,
            rect,
        }));
        // a key press or click that follows brings this window back if it is pushed behind
        self.c.mem().target = Some(window);
        Ok(label)
    }

    /// The call's own `scope`, applied before its first step. A lone action that never looks at the
    /// screen skips it. Python `run_many` preamble.
    pub(crate) fn apply_call_scope(&self, args: &Args) -> StepResult<()> {
        if args.get("action").and_then(Value::as_str) == Some("left_mouse_up")
            && args.get("actions").is_none()
        {
            return Ok(());
        }
        let batch = args.get("actions").is_some_and(truthy);
        let action = args
            .get("action")
            .filter(|v| truthy(v))
            .map(py_str)
            .unwrap_or_default();
        let screenless = !batch && SCREENLESS.contains(&action.as_str());
        // `"scope": false` (any falsy value) is the call's choice of the full screen
        let Some(spec) = args.get("scope") else {
            if !screenless
                && !self.o.read_only
                && let Some(title) = self.pinned_window()
            {
                if let Some(state) = self.pinned_again(&title) {
                    self.c.mem().target = Some(state.window);
                    self.o.scope.set(Some(state));
                    return Ok(());
                }
                // best effort: a pinned window that is not open yet leaves the full screen
                let window = self
                    .set_scope(&title)
                    .ok()
                    .and(self.o.scope.get())
                    .map(|s| s.window);
                *self.c.pinned.lock().unwrap_or_else(PoisonError::into_inner) =
                    window.map(|w| (title, w));
            }
            return Ok(());
        };
        if !truthy(spec) || screenless {
            return Ok(());
        }
        self.set_scope(&py_str(spec)).map(|_| ())
    }

    /// The scope for the window the previous call's pin resolved to, while it still carries the
    /// pinned title and has a visible area. It already went through the allowlist checks when it was
    /// first found, so this is one title read and one rectangle instead of a search through every
    /// window (about 30 ms on Mac). A window covered by another app is not raised here: the cover is
    /// painted over in the picture, and a click on it is refused by the allowlist.
    fn pinned_again(&self, title: &str) -> Option<ScopeState> {
        let (pin, window) = self
            .c
            .pinned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        let info = self.be().window_info(window).ok()?;
        if pin != title || !info.title.to_lowercase().contains(&title.to_lowercase()) {
            return None;
        }
        let rect = self.capture_rect(window)?;
        Some(ScopeState {
            window,
            title: info.title,
            rect,
        })
    }

    /// The window title a call's allowlist pins, when it names exactly one window
    /// (`"only_apps": ["TextEdit:notes"]`). Such a call sees only that window: its pictures are
    /// cropped to it (sharper for the same tokens, nothing else on screen in them) and coordinates
    /// are read against it, exactly as if the call had passed `scope`. A pattern (`re:`) pins
    /// nothing, because it can match several windows.
    fn pinned_window(&self) -> Option<String> {
        let mut entries = self.o.only_apps.iter();
        let (_, title) = entries.next()?.split_once(':')?;
        (entries.next().is_none() && !title.is_empty() && !title.starts_with("re:"))
            .then(|| title.to_string())
    }

    /// Run one step. A step of a batch (or of a recipe) may carry its own `scope` for that step only;
    /// the call's own comes back after. The call's top-level action (`call_args`) is its own step,
    /// already scoped by [`Ctx::apply_call_scope`]. Python `_step_scope`.
    pub(crate) fn run_step_scope(&self, step: &Args, call_args: bool) -> StepResult<StepOut> {
        if step.get("action").and_then(Value::as_str) == Some("left_mouse_up") {
            return self.run(step);
        }
        let spec: Option<&Value> = if call_args {
            None
        } else {
            step.get("scope").filter(|v| truthy(v))
        };
        let Some(spec) = spec else {
            return self.run(step);
        };
        let saved = self.o.scope.get();
        let out = self.set_scope(&py_str(spec)).and_then(|_| self.run(step));
        self.o.scope.set(saved);
        out
    }

    /// What the trailing screenshot shows: `full screen`, or the scoped window and its coordinate
    /// space. Python `_image_tag`.
    pub(crate) fn image_tag(&self) -> String {
        if !self.scoped() {
            return "full screen".to_string();
        }
        let (sw, sh) = self.scale().shot_size();
        format!(
            "{} ({sw}x{sh}); coordinates in {}",
            self.scope_name(),
            self.this_image_space()
        )
    }

    /// How coordinates are read against a scoped picture.
    pub(crate) fn this_image_space(&self) -> &'static str {
        if self.o.coords == crate::scale::Coords::Norm1000 {
            "0-1000 of THIS image"
        } else {
            "pixels of THIS image"
        }
    }
}
