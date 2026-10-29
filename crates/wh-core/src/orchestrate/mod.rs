//! The `computer` tool's orchestration: one call, one action or a batch, over the [`Backend`] trait.
//! Port of `winhands/computer.py` (`run`, `run_many` and everything they call), the computer half of
//! `winhands/daemon.py` (`_execute_tool`: acknowledgement, pause, rate cap, audit, failure fields) and
//! the read formatting of `winhands/fast.py` and `winhands/core.py` (`snapshot`, `format_windows`,
//! `resolve_window_hwnd`, the window and element ref caches).
//!
//! # Shape
//!
//! * [`Computer`] is the long-lived object a daemon owns: the backend, the clock, the config and the
//!   cross-call memory the Python build kept in module globals (`_TARGET`, `_LAST_POINT`, `_PRE_KEY`,
//!   the window refs, the element ref cache, the monitor cache).
//! * [`Computer::run_many`] is the `computer` tool entry; [`Computer::execute`] wraps it with the
//!   per-call gates and the audit entry, and returns what the daemon sends back.
//! * Everything that was a module global *per call* in Python (`COORDS`, `LONG_EDGE`,
//!   `_DISPLAY_OVERRIDE`, `_ONLY_APPS`, `_SNAP_CALL`, the destructive opt-in) lives in a `CallOpts`
//!   that `run_many` builds fresh from the call's own arguments, so nothing leaks from one caller to
//!   the next (D1.1, 2026-09-21).
//! * All waiting goes through the injected [`Clock`], so the tests run the 3 s dialog waits and the
//!   settle loops without sleeping.
//!
//! Where the Python build raised an exception the Rust returns a [`StepError`] of the same class,
//! because `run_many` decides whether to attach a screenshot to a failure from that class.

mod actions;
mod args;
mod ctx;
mod error;
mod files;
mod find;
mod folder;
mod guard;
mod many;
mod open;
mod recipe;
mod refs;
mod repeat;
mod scope;
mod sessions;
mod shortcuts;
mod shots;
mod snapshot;

#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests_actions;
#[cfg(test)]
mod tests_find;
#[cfg(test)]
mod tests_guard;
#[cfg(test)]
mod tests_improvements;
#[cfg(test)]
mod tests_input_safety;
#[cfg(test)]
mod tests_many;
#[cfg(test)]
mod tests_open;
#[cfg(test)]
mod tests_recipes;
#[cfg(test)]
mod tests_scope;
#[cfg(test)]
mod tests_sessions;
#[cfg(test)]
mod tests_shortcuts;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::backend::{Backend, Display, Point, Rect, WindowRef};
use crate::caller::Caller;
use crate::clock::Clock;
use crate::config::Config;
use crate::destructive::Policy;
use crate::human::HumanGate;
use crate::recipes::RecipeStore;
use crate::scale::{Coords, Scale};
use crate::settle::UiaProbe;
use crate::state::{IdleTimer, PauseFlag, RateLimiter};

pub use args::Args;
pub(crate) use args::flag;
pub use error::{StepError, StepResult};
pub(crate) use files::fmt_size;
pub(crate) use guard::normalize_only_app;
pub use many::{ACTION_LIMIT, ManyResult, StepFailure, ToolOutcome};
pub use refs::{ElementRefs, WindowRefs};
pub use repeat::{FailureCounter, REPEAT_FAIL_LIMIT, REPEAT_WINDOW, RepeatNote};
pub use snapshot::{WindowLine, format_windows};

/// What a call remembers about the world between calls (the Python module globals that
/// `run_many` deliberately does NOT reset).
#[derive(Debug, Default, Clone)]
pub(crate) struct Memory {
    /// Screen point of the last pointer action, drawn on the next screenshot (`_LAST_POINT`).
    pub last_point: Option<Point>,
    /// The app window that was in front when the model last looked (`_TARGET`).
    pub target: Option<WindowRef>,
    /// Foreground window and focus token just before the last key combo (`_PRE_KEY`).
    pub pre_key: (Option<WindowRef>, u64),
    /// `[when, monitors]` so the display lookup does not re-enumerate (`_MON_CACHE`).
    pub monitors: Option<(Duration, Vec<Display>)>,
}

/// The `computer` tool over one backend. See the [module docs](self).
pub struct Computer {
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) cfg: Arc<Config>,
    pub(crate) pause: Arc<PauseFlag>,
    pub(crate) idle: Arc<IdleTimer>,
    pub(crate) rate: RateLimiter,
    pub(crate) human: HumanGate,
    pub(crate) probe: UiaProbe,
    pub(crate) mem: Mutex<Memory>,
    pub(crate) refs: Mutex<refs::RefTables>,
    pub(crate) fails: FailureCounter,
    pub(crate) repeats: RepeatNote,
    /// The bundled recipes and the owner's, for the `recipe` actions.
    pub(crate) recipes: RecipeStore,
    sessions: Mutex<sessions::Sessions>,
    observation_epoch: std::sync::atomic::AtomicU64,
    /// Ownership of the raw left-button hold; never includes human-held input.
    pub(crate) owned_mouse: Mutex<bool>,
    tool_lock: Mutex<()>,
}

impl Computer {
    /// Cheap capabilities and limits without capturing or targeting the desktop.
    pub fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "backend": self.backend.name(), "version": env!("CARGO_PKG_VERSION"),
            "paused": self.pause.is_paused(), "read_only": crate::config::truthy(&self.cfg.get("read_only")),
            "max_actions": ACTION_LIMIT, "max_images": 8, "budget_ms": 60000,
            "max_budget_ms": 120000, "compact": true, "sessions": true,
            "snapshot_scope": "single unscoped screenshot; tokens consumed per attempted request",
            "native_validation": "not established by this status call"
        })
    }

    /// A tool over `backend`, with its own pause flag, idle timer and rate cap.
    pub fn new(backend: Arc<dyn Backend>, clock: Arc<dyn Clock>, cfg: Arc<Config>) -> Self {
        let idle = Arc::new(IdleTimer::new(cfg.idle_timeout_seconds(), clock.as_ref()));
        Self::with_state(backend, clock, cfg, Arc::new(PauseFlag::new()), idle)
    }

    /// A tool that shares the daemon's pause flag and idle timer (the kill hotkey toggles the former,
    /// every handled call touches the latter).
    pub fn with_state(
        backend: Arc<dyn Backend>,
        clock: Arc<dyn Clock>,
        cfg: Arc<Config>,
        pause: Arc<PauseFlag>,
        idle: Arc<IdleTimer>,
    ) -> Self {
        let recipes = RecipeStore::standard(cfg.paths().recipes_dir());
        Self {
            backend,
            clock,
            cfg,
            pause,
            idle,
            rate: RateLimiter::new(),
            human: HumanGate::new(),
            probe: UiaProbe::new(),
            mem: Mutex::new(Memory::default()),
            refs: Mutex::new(refs::RefTables::default()),
            fails: FailureCounter::default(),
            repeats: RepeatNote::default(),
            recipes,
            sessions: Mutex::new(sessions::Sessions::default()),
            observation_epoch: std::sync::atomic::AtomicU64::new(0),
            owned_mouse: Mutex::new(false),
            tool_lock: Mutex::new(()),
        }
    }

    /// The pause flag the kill hotkey toggles.
    pub fn pause_flag(&self) -> &PauseFlag {
        &self.pause
    }

    /// The idle-shutdown timer.
    pub fn idle_timer(&self) -> &IdleTimer {
        &self.idle
    }

    /// The window ref table (`w3` handles stable for a window's life).
    pub fn window_refs(&self) -> WindowRefs<'_> {
        WindowRefs::new(self)
    }

    /// The element ref cache a `read` fills.
    pub fn element_refs(&self) -> ElementRefs<'_> {
        ElementRefs::new(self)
    }

    /// The "same result as Ns ago" memory used by the read routes.
    pub fn repeat_note(&self) -> &RepeatNote {
        &self.repeats
    }

    /// The identical-failure counter.
    pub fn failure_counter(&self) -> &FailureCounter {
        &self.fails
    }

    /// The lock the daemon holds around anything that can move focus or send input, so two agents
    /// (or a batch and a stray click) never interleave mouse and keyboard input.
    pub fn tool_lock(&self) -> &Mutex<()> {
        &self.tool_lock
    }

    /// Take [`Self::tool_lock`]. A call that can send input never queues (it could fire after its
    /// caller gave up, 2026-10-28), so `wait = false` answers `None` at once when busy. A read-only
    /// call (`wait = true`) waits its turn, up to its request deadline or 30 s, on real time: the
    /// other holder is a real thread, not the injected clock.
    pub fn lock_tool(&self, wait: bool) -> Option<std::sync::MutexGuard<'_, ()>> {
        let most = std::time::Duration::from_secs(30);
        let give_up =
            std::time::Instant::now() + crate::request::remaining().map_or(most, |d| d.min(most));
        loop {
            match self.tool_lock.try_lock() {
                Ok(guard) => return Some(guard),
                Err(std::sync::TryLockError::Poisoned(e)) => return Some(e.into_inner()),
                Err(std::sync::TryLockError::WouldBlock)
                    if wait && std::time::Instant::now() < give_up =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(std::sync::TryLockError::WouldBlock) => return None,
            }
        }
    }

    /// The backend this tool drives.
    pub fn backend(&self) -> &dyn Backend {
        self.backend.as_ref()
    }

    /// The config this tool reads.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub(crate) fn mem(&self) -> std::sync::MutexGuard<'_, Memory> {
        self.mem
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The options one call carries: this call's own arguments layered on the configured defaults.
/// Built fresh by every `run_many`; nothing here survives into the next call.
#[derive(Debug, Clone)]
pub(crate) struct CallOpts {
    /// Pause generation captured before running any steps. Resuming cannot revive cancelled work.
    pub cancel_generation: u64,
    /// Paths this call saved through `file_dialog`: wait_for_file may watch them even under
    /// only_apps (shared with recipe steps, which clone these options).
    pub saved_paths: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Absolute deadline on the injected clock.
    pub deadline: Option<Duration>,
    /// Refuse any operation that can mutate desktop or clipboard state.
    pub read_only: bool,
    /// Whether optional OCR helper pictures may be generated.
    pub helper_images: bool,
    /// Pixels of the screenshot or 0 to 1000.
    pub coords: Coords,
    /// The screenshot's long edge for this call.
    pub long_edge: u32,
    /// The display this call chose with `display`, if it did.
    pub display: Option<Rect>,
    /// The `only_apps` allowlist, entries normalised (process part lowercased). Empty = no limit.
    pub only_apps: BTreeSet<String>,
    /// `snap: true` for this call.
    pub snap_call: bool,
    /// The destructive opt-in for this call.
    pub policy: Policy,
    /// The window this call is scoped to, if any (`scope`). Shared by every context made from these
    /// options, so a recipe step's narrowed context sees the same one.
    pub scope: scope::ScopeCell,
}

/// A call in flight: the long-lived tool, this call's options and who is calling.
pub(crate) struct Ctx<'a> {
    pub c: &'a Computer,
    pub o: CallOpts,
    pub who: &'a Caller,
}

impl Ctx<'_> {
    /// The screen the model sees, the convention and the screenshot size for this call.
    pub(crate) fn scale(&self) -> Scale {
        Scale::new(self.display_rect(), self.o.coords, self.o.long_edge)
    }
}

/// `Duration::from_secs_f64` that never panics on a negative or NaN input.
pub(crate) fn secs(s: f64) -> Duration {
    if s.is_finite() && s > 0.0 {
        Duration::from_secs_f64(s)
    } else {
        Duration::ZERO
    }
}
