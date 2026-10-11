//! The small helpers every action leans on: window facts, the display the model sees, the human-input
//! wait, the remembered target window, key combos and held modifiers, polling.
//! Python: `display_rect`, `_fg_monitor`, `_fg_root`, `_class`, `_title`, `_wait_for_human`,
//! `_mark_ours`, `_remember_target`, `_restore_target`, `_holding`, `_with_modifiers`, `_poll`.

use std::time::Duration;

use super::error::StepResult;
use super::{Ctx, secs};
use crate::backend::{Backend, Display, Rect, WindowInfo, WindowKind, WindowRef, no_guard};
use crate::clock::Clock;
use crate::config::Config;
use crate::human::{DEFAULT_MAX_WAIT, DEFAULT_QUIET};
use crate::input::{TypeOptions, type_text};
use crate::keys::{Key, Platform, parse_key_combo, parse_modifiers};
use crate::scale::{parse_display_spec, sort_displays};

/// How long the monitor list is trusted (`_MON_CACHE`, 2 s).
const MONITOR_TTL: Duration = Duration::from_secs(2);

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
        self.clock().sleep(secs(s));
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
                self.be().restore_window(target, Duration::from_secs(2))?;
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
        self.be().key_chord(&keys, repeat.max(1), &no_guard)?;
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
        for k in keys {
            self.be().key_down(*k)?;
        }
        let out = f();
        let mut release: StepResult<()> = Ok(());
        for k in keys.iter().rev() {
            if let Err(e) = self.be().key_up(*k)
                && release.is_ok()
            {
                release = Err(e.into());
            }
        }
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

    /// Type text the way that survives every app: short text key by key, longer text pasted with the
    /// owner's clipboard put back. Returns " (pasted)" or "" (Python `_type_text`).
    pub(crate) fn type_text(&self, text: &str) -> StepResult<&'static str> {
        let paste_over = usize::try_from(self.cfg().paste_over()).unwrap_or(0);
        // 2026-10-11 (Brave address bar): a pasted trailing newline never submits a one-line
        // field, so "url\n" sat there unsent. Type the body, then press Enter for real.
        let body = text.trim_end_matches(['\n', '\r']);
        let enters = text[body.len()..].matches('\n').count()
            + usize::from(text.ends_with('\r') && !text.ends_with('\n'));
        let mut suffix = "";
        if !body.is_empty() {
            let outcome = type_text(
                self.be(),
                self.o.policy,
                body,
                &TypeOptions::new(paste_over),
            )?;
            suffix = outcome.note_suffix();
        }
        if enters > 0 {
            self.sleep(0.05);
            self.send_combo("enter", enters as u32)?;
        }
        Ok(suffix)
    }
}
