//! Bounded, opt-in caller sessions. The legacy unnamed session remains compatible.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

use super::{Computer, Memory, StepError, StepResult, refs::RefTables};
use crate::backend::{Rect, WindowRef};
use crate::caller::Caller;
use crate::config::truthy;
use crate::scale::{
    CallScale, display_containing, parse_display_spec, pick_display, sort_displays,
};

const MAX_SESSIONS: usize = 16;
const SNAPSHOT_TTL: Duration = Duration::from_secs(60);
// Process-wide, rather than per saved session: evicting/recreating a session cannot revive an ID.
static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(1);

fn next_snapshot() -> Option<u64> {
    let mut next = NEXT_SNAPSHOT.load(Ordering::Relaxed);
    loop {
        let after = next.checked_add(1)?;
        match NEXT_SNAPSHOT.compare_exchange_weak(next, after, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => return Some(next),
            Err(actual) => next = actual,
        }
    }
}

#[derive(Default)]
struct Saved {
    memory: Memory,
    refs: RefTables,
    snapshot: Option<Snapshot>,
    touched: Duration,
    last_output: Option<[u8; 32]>,
}

struct Snapshot {
    id: String,
    window: WindowRef,
    pid: u32,
    rect: Rect,
    at: Duration,
    pause_generation: u64,
    scale: CallScale,
    display_rect: Rect,
    observation_epoch: u64,
}

#[derive(Default)]
pub(super) struct Sessions(HashMap<String, Saved>);

/// Moves, rather than clones, potentially large reference tables between sessions.
pub(super) struct SessionGuard<'a> {
    computer: &'a Computer,
    key: String,
    pub id: String,
    saved: Saved,
    previous_memory: Memory,
    previous_refs: RefTables,
}

impl<'a> SessionGuard<'a> {
    pub fn enter(computer: &'a Computer, args: &Value, who: &Caller) -> StepResult<Option<Self>> {
        let Some(id) = args.get("session_id") else {
            if args.get("snapshot_id").is_some() {
                return Err(StepError::value("snapshot_id requires session_id"));
            }
            return Ok(None);
        };
        let id = id
            .as_str()
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 64
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
            })
            .ok_or_else(|| {
                StepError::value("session_id must be 1..64 ASCII letters, digits, -_.:")
            })?;
        let owner = who.chain().first().map_or(0, |(pid, _)| *pid);
        let key = format!("{owner}:{id}");
        let mut sessions = computer.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let mut saved = sessions.0.remove(&key).unwrap_or_default();
        // One entry is active outside the map, so reserve room for it now.
        while sessions.0.len() >= MAX_SESSIONS {
            let oldest = sessions
                .0
                .iter()
                .min_by_key(|(_, s)| s.touched)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                sessions.0.remove(&oldest);
            } else {
                break;
            }
        }
        drop(sessions);
        let previous_memory =
            std::mem::replace(&mut *computer.mem(), std::mem::take(&mut saved.memory));
        let previous_refs = std::mem::replace(
            &mut *computer.refs.lock().unwrap_or_else(|e| e.into_inner()),
            std::mem::take(&mut saved.refs),
        );
        Ok(Some(Self {
            computer,
            key,
            id: id.to_string(),
            saved,
            previous_memory,
            previous_refs,
        }))
    }

    pub fn validate(&mut self, args: &Value) -> StepResult<()> {
        // Every attempted request consumes the previous observation, including failed actions.
        let snapshot = self.saved.snapshot.take();
        let Some(id) = args.get("snapshot_id") else {
            return Ok(());
        };
        let stale = || {
            StepError::target("stale snapshot; observe this session's target again before acting")
        };
        let s = snapshot.as_ref().ok_or_else(stale)?;
        if id.as_str() != Some(s.id.as_str())
            || self.computer.clock.now().saturating_sub(s.at) >= SNAPSHOT_TTL
            || self.computer.pause.generation() != s.pause_generation
            || self.computer.observation_epoch.load(Ordering::Acquire) != s.observation_epoch
        {
            return Err(stale());
        }
        let info = self
            .computer
            .backend
            .window_info(s.window)
            .map_err(|_| stale())?;
        if info.pid != s.pid
            || info.rect != s.rect
            || info.minimized
            || info.hidden
            || self.computer.backend.foreground_window().ok().flatten() != Some(s.window)
            || self.geometry(args, info.rect) != Some((s.scale, s.display_rect))
        {
            return Err(stale());
        }
        Ok(())
    }

    /// Only a simple, unscoped screenshot currently carries an unambiguous coordinate frame.
    /// Text reads of other windows and zoom/batch output must not claim the foreground's identity.
    pub fn observe(&mut self, args: &Value) -> Option<String> {
        self.saved.snapshot = None;
        if args.get("actions").is_some()
            || args.get("action").and_then(Value::as_str) != Some("screenshot")
            || args.get("save_to").is_some_and(truthy)
        {
            return None;
        }
        let window = self.computer.backend.foreground_window().ok().flatten()?;
        if self.computer.mem().target != Some(window) {
            return None;
        }
        let info = self.computer.backend.window_info(window).ok()?;
        if info.minimized || info.hidden || info.rect.is_empty() {
            return None;
        }
        let (scale, display_rect) = self.geometry(args, info.rect)?;
        // A screenshot of another display does not observe this window, even if memory is old.
        if !display_rect.contains(info.rect.center()) {
            return None;
        }
        let sequence = next_snapshot()?;
        let id = format!("s{sequence}");
        self.saved.snapshot = Some(Snapshot {
            id: id.clone(),
            window,
            pid: info.pid,
            rect: info.rect,
            at: self.computer.clock.now(),
            pause_generation: self.computer.pause.generation(),
            scale,
            display_rect,
            observation_epoch: self.computer.observation_epoch.load(Ordering::Acquire),
        });
        Some(id)
    }

    /// Resolve this request's own defaults. Never copy screenshot settings into global/session memory.
    fn geometry(&self, args: &Value, window_rect: Rect) -> Option<(CallScale, Rect)> {
        fn scoped(args: &Value) -> bool {
            args.get("scope").is_some_and(truthy)
                || args
                    .get("actions")
                    .and_then(Value::as_array)
                    .is_some_and(|steps| steps.iter().any(scoped))
        }
        if scoped(args) {
            return None;
        }
        let scale = CallScale::from_args(&self.computer.cfg, args).ok()?;
        let mut displays = self.computer.backend.displays().ok()?;
        sort_displays(&mut displays);
        let display_rect = if let Some(index) = scale.display_index {
            pick_display(&displays, index).ok()?
        } else {
            let configured = self.computer.cfg.display_spec();
            if let Ok(rect) = parse_display_spec(&configured) {
                rect
            } else {
                let point = window_rect.center();
                display_containing(&displays, point.x, point.y)
                    .or_else(|| displays.first().map(|d| d.rect))?
            }
        };
        Some((scale, display_rect))
    }

    pub fn unchanged(&mut self, digest: [u8; 32]) -> bool {
        let same = self.saved.last_output == Some(digest);
        self.saved.last_output = Some(digest);
        same
    }
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        self.saved.memory = std::mem::replace(
            &mut *self.computer.mem(),
            std::mem::take(&mut self.previous_memory),
        );
        self.saved.refs = std::mem::replace(
            &mut *self.computer.refs.lock().unwrap_or_else(|e| e.into_inner()),
            std::mem::take(&mut self.previous_refs),
        );
        self.saved.touched = self.computer.clock.now();
        self.computer
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .insert(self.key.clone(), std::mem::take(&mut self.saved));
    }
}
