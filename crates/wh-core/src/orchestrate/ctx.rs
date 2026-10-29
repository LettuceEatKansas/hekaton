//! The small helpers every action leans on: window facts, the display the model sees, the human-input
//! wait, the remembered target window, key combos and held modifiers, polling.
//! Python: `display_rect`, `_fg_monitor`, `_fg_root`, `_class`, `_title`, `_wait_for_human`,
//! `_mark_ours`, `_remember_target`, `_restore_target`, `_holding`, `_with_modifiers`, `_poll`.

use std::cell::Cell;
use std::time::Duration;

use super::error::StepResult;
use super::{Computer, Ctx, secs};
use crate::backend::{
    Backend, BackendError, BackendResult, Display, HeldKeys, Rect, WindowInfo, WindowKind,
    WindowRef,
};
use crate::clock::Clock;
use crate::config::Config;
use crate::human::{DEFAULT_MAX_WAIT, DEFAULT_QUIET};
use crate::input::{TypeOptions, type_text_guarded};
use crate::keys::{Key, Platform, parse_key_combo, parse_modifiers};
use crate::scale::{parse_display_spec, sort_displays};

/// How long the monitor list is trusted (`_MON_CACHE`, 2 s).
const MONITOR_TTL: Duration = Duration::from_secs(2);

/// Text up to this long goes into a browser as keys, not a paste (about 0.7 s of keys at most).
const BROWSER_KEYS_UP_TO: usize = 60;

/// Windows' file manager, by raw class or by neutral kind.
pub(crate) fn is_file_manager(info: &WindowInfo) -> bool {
    info.class == "CabinetWClass" || info.kind == WindowKind::FileManager
}

/// The desktop or a taskbar: `progman`, `workerw`, `shell_traywnd`, `shell_secondarytraywnd`
/// (`_DESKTOP_TASKBAR_CLASSES`).
pub(crate) fn is_desktop_or_taskbar(info: &WindowInfo) -> bool {
    matches!(info.kind, WindowKind::Desktop | WindowKind::Taskbar)
        || matches!(
            info.class.to_lowercase().as_str(),
            "progman" | "workerw" | "shell_traywnd" | "shell_secondarytraywnd"
        )
}

/// The Run dialog (`_is_run_dialog`): a standard dialog titled "Run".
pub(crate) fn is_run_dialog(info: &WindowInfo) -> bool {
    info.class == "#32770" && info.title.trim().eq_ignore_ascii_case("run")
}

impl Computer {
    /// Stop accepting input and release only the split mouse press this tool owns.
    /// The primitive lock never spans a held-key wait, drag, or whole tool request.
    /// A failed release keeps ownership so the next stop can retry cleanup.
    pub fn pause_and_release(&self) -> BackendResult<()> {
        self.pause.set(true);
        let mut owned = self
            .owned_mouse
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *owned {
            self.backend
                .mouse_button(crate::backend::MouseButton::Left, false)?;
            *owned = false;
        }
        Ok(())
    }
}

impl<'a> Ctx<'a> {
    pub(crate) fn be(&self) -> &'a dyn Backend {
        self.c.backend.as_ref()
    }

    pub(crate) fn clock(&self) -> &'a dyn Clock {
        self.c.clock.as_ref()
    }

    pub(crate) fn cfg(&self) -> &'a Config {
        &self.c.cfg
    }

    /// Wait `s` seconds on the call's clock.
    pub(crate) fn sleep(&self, s: f64) {
        let _ = self.sleep_checked(s);
    }

    /// Refuse an interrupted request even after the user resumes newer work.
    pub(crate) fn check_cancelled(&self) -> StepResult<()> {
        if self.c.pause.is_paused() || self.c.pause.generation() != self.o.cancel_generation {
            return Err(super::error::StepError::permission("paused by user hotkey"));
        }
        if self
            .o
            .deadline
            .is_some_and(|deadline| self.clock().now() >= deadline)
        {
            return Err(super::error::StepError::permission(
                "request deadline exceeded; inspect current state before retrying",
            ));
        }
        Ok(())
    }

    pub(crate) fn input_guard(&self) -> BackendResult<()> {
        if self.o.read_only {
            return Err(BackendError::Denied(
                "read-only mode refuses desktop input".into(),
            ));
        }
        if let Some(why) = self.c.game_lock() {
            return Err(BackendError::Denied(format!("gaming lock: {why}")));
        }
        self.check_cancelled()
            .map_err(|error| BackendError::Denied(error.to_string()))
    }

    /// Wait cooperatively. Held-input cleanup runs when this returns an interruption.
    pub(crate) fn sleep_checked(&self, s: f64) -> StepResult<()> {
        let until = self.clock().now().saturating_add(secs(s));
        loop {
            self.check_cancelled()?;
            let now = self.clock().now();
            if now >= until {
                return Ok(());
            }
            self.clock()
                .sleep((until - now).min(Duration::from_millis(25)));
        }
    }

    /// Seconds on the call's clock (`time.monotonic()`).
    pub(crate) fn now(&self) -> f64 {
        self.clock().now().as_secs_f64()
    }

    /// Call `check` until it yields something or `timeout` seconds pass. Returns the last result and
    /// the seconds taken. Python `_poll`.
    pub(crate) fn poll<T>(
        &self,
        mut check: impl FnMut() -> Option<T>,
        timeout: f64,
        interval: f64,
    ) -> (Option<T>, f64) {
        let start = self.now();
        loop {
            if self.check_cancelled().is_err() {
                return (None, self.now() - start);
            }
            let found = check();
            let took = self.now() - start;
            if found.is_some() || took >= timeout {
                return (found, took);
            }
            self.sleep(interval);
        }
    }

    // ------------------------------------------------------------------ windows

    /// A live read of one window, or `None` when it is gone.
    pub(crate) fn info(&self, w: WindowRef) -> Option<WindowInfo> {
        self.be().window_info(w).ok()
    }

    /// The window's process as the guards compare it: lowercased executable name, `?` when it cannot
    /// be read (Python `window_process`).
    pub(crate) fn app_of(&self, w: WindowRef) -> Option<String> {
        let info = self.info(w)?;
        Some(if info.app.is_empty() {
            "?".to_string()
        } else {
            info.app.to_lowercase()
        })
    }

    /// The raw class, "" when the window is gone (Python `_class`).
    pub(crate) fn class_of(&self, w: WindowRef) -> String {
        self.info(w).map(|i| i.class).unwrap_or_default()
    }

    /// The title, lowercased, "" when the window is gone (Python `_title`).
    pub(crate) fn title_of(&self, w: WindowRef) -> String {
        self.info(w)
            .map(|i| i.title.to_lowercase())
            .unwrap_or_default()
    }

    /// The top-level window of `w` (itself when that cannot be read). Python `root_hwnd`.
    pub(crate) fn root_of(&self, w: WindowRef) -> WindowRef {
        self.be().root_window(w).unwrap_or(w)
    }

    /// Top-level window of whatever is in front, `None` when nothing is (Python `_fg_root`).
    pub(crate) fn fg_root(&self) -> Option<WindowRef> {
        let fg = self.be().foreground_window().ok().flatten()?;
        Some(self.root_of(fg))
    }

    // ------------------------------------------------------------------ the display

    fn monitors_cached(&self) -> Vec<Display> {
        let now = self.clock().now();
        let mut mem = self.c.mem();
        if let Some((at, list)) = &mem.monitors
            && now.saturating_sub(*at) <= MONITOR_TTL
        {
            return list.clone();
        }
        let mut list = self.be().displays().unwrap_or_default();
        sort_displays(&mut list);
        mem.monitors = Some((now, list.clone()));
        list
    }

    /// The monitor containing the centre of the foreground window, or `None` (`_fg_monitor`).
    ///
    /// 2026-10-08 live run: Hermes (the agent's own app) was in front on monitor 2 while the game it
    /// was playing sat on monitor 1, so every screenshot showed Hermes and every drag was refused as
    /// aimed at the agent itself. When the window in front is the agent's own (its app, terminal or
    /// their parents), the monitor is the one of the topmost window that is not.
    pub(crate) fn fg_monitor(&self) -> Option<Rect> {
        let root = self.fg_root()?;
        let behind_the_agent = if self.is_host(root) {
            self.be().list_windows(false).ok().and_then(|list| {
                list.into_iter()
                    .find(|w| !w.hidden && !w.minimized && !self.is_host(w.window))
                    .map(|w| w.rect)
            })
        } else {
            None
        };
        let rect = match behind_the_agent {
            Some(r) => r,
            None => self.info(root)?.rect,
        };
        let c = rect.center();
        self.monitors_cached()
            .into_iter()
            .find(|d| d.rect.contains(c))
            .map(|d| d.rect)
    }

    /// The primary monitor's rect (`GetSystemMetrics` size at 0,0).
    pub(crate) fn primary_rect(&self) -> Rect {
        let list = self.monitors_cached();
        list.iter()
            .find(|d| d.primary)
            .or_else(|| list.first())
            .map(|d| d.rect)
            .unwrap_or_else(|| Rect::from_xywh(0, 0, 1920, 1080))
    }

    /// The screen the model sees: the window this call is scoped to, else this call's `display`, else
    /// the configured `x,y,w,h`, else the monitor holding the window in front, else the primary one
    /// (Python `display_rect()`).
    ///
    /// 2026-09-21: Search Console was focused in Brave on monitor 2 while every screenshot showed
    /// monitor 1, so the model never saw it. A configured spec that does not parse is ignored here
    /// (Python raised a `ValueError` from whichever action first asked for the rect).
    pub(crate) fn display_rect(&self) -> Rect {
        // A scoped call sees one window, whatever `display` or the configuration say.
        if let Some(r) = self.scope_rect() {
            return r;
        }
        if let Some(r) = self.o.display {
            return r;
        }
        let spec = self.cfg().display_spec();
        if !spec.is_empty()
            && let Ok(r) = parse_display_spec(&spec)
        {
            return r;
        }
        self.fg_monitor().unwrap_or_else(|| self.primary_rect())
    }

    /// Is the window's centre on the monitor the model is looking at? (`_centre_on_display`)
    pub(crate) fn centre_on_display(&self, w: WindowRef) -> Option<bool> {
        let info = self.info(w)?;
        Some(self.display_rect().contains(info.rect.center()))
    }

    // ------------------------------------------------------------------ human input

    /// Do not fight a person for the mouse (Python `_wait_for_human`).
    pub(crate) fn wait_for_human(&self) -> StepResult<()> {
        self.c.human.wait(
            self.be(),
            self.clock(),
            self.cfg().human_pause(),
            DEFAULT_QUIET,
            DEFAULT_MAX_WAIT,
        )?;
        Ok(())
    }

    /// Record that we just sent input (Python `_mark_ours`).
    pub(crate) fn mark_ours(&self) {
        self.c.human.mark_ours(self.clock());
    }

    // ------------------------------------------------------------------ remembered target

    /// The app in front when the model takes a screenshot is the one it means to act on
    /// (Python `_remember_target`). Only a window on the screen the model sees can be its target: the
    /// agent's own console on monitor 2 was in front at the first screenshot and got remembered
    /// (2026-09-20).
    pub(crate) fn remember_target(&self) {
        let Some(fg) = self.fg_root() else {
            // Nothing in front. Python read a zero rect for "no window" and, when that point was on
            // the display (the usual case), forgot the target.
            if self
                .display_rect()
                .contains(crate::backend::Point::new(0, 0))
            {
                self.c.mem().target = None;
            }
            return;
        };
        if self.is_host(fg) {
            return;
        }
        if self.centre_on_display(fg) == Some(true) {
            self.c.mem().target = Some(fg);
        }
    }

    /// If the app the model was using got minimised (a fullscreen game minimises when focus leaves it)
    /// or pushed behind an agent window, bring it back before acting. Returns the note prefix.
    ///
    /// Keys go to whatever window has focus, so before a key press ANY other window in front (the
    /// agent's own console, a chat app) must give focus back to the target. A click needs no such help:
    /// it lands on what is under the pointer and focuses it. Narrowed 2026-09-20: refocusing on EVERY
    /// other window broke real app switching (Win key, then typing into Start search). Only an agent
    /// host, or a window that is not on the screen the model sees, is an intruder. Python
    /// `_restore_target`.
    pub(crate) fn restore_target(&self, for_keys: bool) -> String {
        let Some(target) = self.c.mem().target else {
            return String::new();
        };
        let Some(target_info) = self.info(target) else {
            return String::new();
        };
        let fg = self.fg_root();
        let intruder = fg.is_some_and(|fg| {
            fg != target
                && (self.is_host(fg) || (for_keys && self.centre_on_display(fg) == Some(false)))
        });
        if target_info.minimized || intruder {
            let go = || -> StepResult<()> {
                self.check_cancelled()?;
                self.be().restore_window(target, Duration::from_secs(2))?;
                self.check_cancelled()?;
                self.be().bring_to_foreground(target)?;
                Ok(())
            };
            if go().is_ok() {
                self.sleep(0.25);
                return "restored the app window first; ".to_string();
            }
        }
        String::new()
    }

    // ------------------------------------------------------------------ keys

    /// Press a combo such as `ctrl+a` (Python `core.send_key_combo`).
    pub(crate) fn send_combo(&self, combo: &str, repeat: u32) -> StepResult<()> {
        let keys = parse_key_combo(combo, Platform::current())?;
        let shell_combo =
            Platform::current() == Platform::Windows && matches!(keys.first(), Some(Key::Meta));
        self.be().key_chord(&keys, repeat.max(1), &|| {
            if shell_combo {
                self.input_guard()
            } else {
                self.keyboard_guard()
            }
        })?;
        Ok(())
    }

    /// The select-all shortcut of this OS.
    pub(crate) fn select_all(&self) -> &'static str {
        match Platform::current() {
            Platform::Windows => "ctrl+a",
            Platform::Mac => "cmd+a",
        }
    }

    /// The copy shortcut of this OS.
    pub(crate) fn copy_combo(&self) -> &'static str {
        match Platform::current() {
            Platform::Windows => "ctrl+c",
            Platform::Mac => "cmd+c",
        }
    }

    /// Press `keys`, run `f`, then release them in reverse, even if `f` fails (Python `_holding`).
    pub(crate) fn holding<T>(
        &self,
        keys: &[Key],
        f: impl FnOnce() -> StepResult<T>,
    ) -> StepResult<T> {
        let mut held = HeldKeys::new(self.be());
        for k in keys {
            self.check_cancelled()?;
            held.press(*k)?;
        }
        let out = f();
        let release = held.release().map_err(super::error::StepError::from);
        match (out, release) {
            (Ok(v), Ok(())) => Ok(v),
            (Ok(_), Err(e)) => Err(e),
            (Err(e), _) => Err(e),
        }
    }

    /// Hold modifier keys (`shift`, `ctrl+shift`) around `f` (Python `_with_modifiers`).
    pub(crate) fn with_modifiers<T>(
        &self,
        mods: &str,
        f: impl FnOnce() -> StepResult<T>,
    ) -> StepResult<T> {
        let keys = parse_modifiers(mods, Platform::current())?;
        self.holding(&keys, f)
    }

    // ------------------------------------------------------------------ typing

    /// The focused element is a dropdown or combo box, which takes no paste.
    fn focus_is_dropdown(&self) -> bool {
        matches!(self.be().focused_element(), Ok(Some(e)) if e.role == crate::backend::Role::ComboBox)
    }

    /// Type text the way that survives every app: short text key by key, longer text pasted with the
    /// owner's clipboard put back. Returns " (pasted)" or "" (Python `_type_text`).
    pub(crate) fn type_text(&self, text: &str) -> StepResult<&'static str> {
        let paste_over = if crate::keys::is_arithmetic(text) {
            usize::MAX
        } else {
            usize::try_from(self.cfg().paste_over()).unwrap_or(0)
        };
        let target = self.typing_target()?;
        // 2026-10-29 (form job): a paste into a browser dropdown (<select>) did nothing and said
        // nothing. Keys pick the option by its first letters, so a dropdown always gets keys.
        // Brave also dropped about 1 paste in 4 into plain text boxes (caught and retyped by
        // `type_text_guarded`, at ~0.7 s each), so a browser takes keys up to a line of text.
        let paste_over = if text.chars().count() <= paste_over {
            paste_over
        } else if self.focus_is_dropdown() {
            usize::MAX
        } else if self.is_browser(target.0) {
            paste_over.max(BROWSER_KEYS_UP_TO)
        } else {
            paste_over
        };
        let last_full = Cell::new(self.clock().now());
        let title_scoped = self.o.only_apps.iter().any(|a| a.contains(':'));
        let guard = || {
            self.typing_target_guard(target)?;
            let now = self.clock().now();
            // Identity (same foreground window, same live process) is checked before every event,
            // so the window cannot change app mid-text and a plain app allowlist cannot flip; the
            // full policy read only needs refreshing every 100 ms. A title-scoped entry
            // ("msedge.exe:InPrivate") can flip when a tab changes the title, so it keeps per-event
            // checks. 2026-10-28 Mac stress test: per-event checks under a plain only_apps cost
            // ~110 ms per batch step of short text.
            if title_scoped || now.saturating_sub(last_full.get()) >= Duration::from_millis(100) {
                self.keyboard_window_guard(target.0)?;
                self.typing_target_guard(target)?;
                last_full.set(now);
            }
            Ok(())
        };
        let outcome = type_text_guarded(
            self.be(),
            self.o.policy,
            text,
            &TypeOptions::new(paste_over),
            &guard,
        )?;
        Ok(outcome.note_suffix())
    }
}
