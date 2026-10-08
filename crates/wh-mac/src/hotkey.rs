//! The kill hotkey, through the `global-hotkey` crate (Carbon `RegisterEventHotKey`). Unlike an
//! event tap it needs no Input Monitoring grant and keeps working under Secure Input.
//!
//! Carbon delivers hotkey events through the main thread's run loop, so the process must keep the
//! main thread turning it (see [`crate::run_main_run_loop`]); the callback itself runs on a thread
//! this module owns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use dispatch2::DispatchQueue;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use wh_core::backend::{BackendError, BackendResult, HotkeyChord};
use wh_core::keys::Key;

use crate::keymap::w3c_code_name;

type Callback = Arc<dyn Fn() + Send + Sync>;

/// How long a hop to the main thread may take before we conclude its run loop is not turning.
const MAIN_HOP_TIMEOUT: Duration = Duration::from_secs(3);

struct Active {
    /// Kept so the registration lives as long as the backend.
    _manager: GlobalHotKeyManager,
}

/// The registered kill hotkey, if any.
#[derive(Default)]
pub(crate) struct Hotkeys {
    active: Mutex<Option<Active>>,
    target: Arc<Mutex<Option<(u32, Callback)>>>,
    listener: AtomicBool,
}

/// Split a chord into modifiers and its one key.
pub(crate) fn chord_parts(keys: &[Key]) -> Option<(Modifiers, Code)> {
    let mut mods = Modifiers::empty();
    let mut main = None;
    for k in keys {
        match k {
            Key::Ctrl => mods |= Modifiers::CONTROL,
            Key::Alt => mods |= Modifiers::ALT,
            Key::Shift => mods |= Modifiers::SHIFT,
            Key::Meta => mods |= Modifiers::SUPER,
            other => {
                if main.is_some() {
                    return None;
                }
                main = Some(w3c_code_name(*other)?.parse::<Code>().ok()?);
            }
        }
    }
    Some((mods, main?))
}

/// Run `work` on the main thread. Carbon wants the hotkey manager created there, and the main
/// thread must be turning its run loop for the hop to complete.
fn on_main_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> BackendResult<T> {
    // SAFETY: pthread_main_np has no preconditions.
    if unsafe { libc::pthread_main_np() } != 0 {
        return Ok(work());
    }
    let (tx, rx) = mpsc::channel();
    DispatchQueue::main().exec_async(move || {
        let _ = tx.send(work());
    });
    rx.recv_timeout(MAIN_HOP_TIMEOUT).map_err(|_| {
        BackendError::Os(
            "the main thread is not running its run loop, so the kill hotkey cannot be              registered: the daemon must call wh_mac::run_main_run_loop on its main thread"
                .into(),
        )
    })
}

/// Register the first chord that works; the manager and hotkey come back with its name.
fn register_first(
    candidates: Vec<HotkeyChord>,
) -> BackendResult<Option<(GlobalHotKeyManager, HotKey, String)>> {
    let manager = GlobalHotKeyManager::new()
        .map_err(|e| BackendError::Os(format!("could not start the hotkey manager: {e}")))?;
    for chord in candidates {
        let Some((mods, code)) = chord_parts(&chord.keys) else {
            continue;
        };
        let hotkey = HotKey::new(Some(mods), code);
        if manager.register(hotkey).is_ok() {
            return Ok(Some((manager, hotkey, chord.name)));
        }
    }
    Ok(None)
}

impl Hotkeys {
    /// Register the first chord the system accepts. `Ok(None)` when none could be registered.
    pub(crate) fn register(
        &self,
        candidates: &[HotkeyChord],
        on_press: Callback,
    ) -> BackendResult<Option<String>> {
        let candidates = candidates.to_vec();
        let Some((manager, hotkey, name)) = on_main_thread(move || register_first(candidates))??
        else {
            return Ok(None);
        };
        *self.target.lock().unwrap_or_else(|e| e.into_inner()) = Some((hotkey.id(), on_press));
        self.ensure_listener();
        *self.active.lock().unwrap_or_else(|e| e.into_inner()) = Some(Active { _manager: manager });
        Ok(Some(name))
    }

    fn ensure_listener(&self) {
        if self.listener.swap(true, Ordering::SeqCst) {
            return;
        }
        let target = Arc::clone(&self.target);
        let spawned = std::thread::Builder::new()
            .name("winhands-kill-hotkey".into())
            .spawn(move || {
                let rx = GlobalHotKeyEvent::receiver();
                while let Ok(ev) = rx.recv() {
                    if ev.state() != HotKeyState::Pressed {
                        continue;
                    }
                    let cb = target
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        .filter(|(id, _)| *id == ev.id())
                        .map(|(_, cb)| Arc::clone(cb));
                    if let Some(cb) = cb {
                        cb();
                    }
                }
            });
        if spawned.is_err() {
            self.listener.store(false, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_split_into_modifiers_and_a_key() {
        let (m, c) = chord_parts(&[Key::Ctrl, Key::Alt, Key::Shift, Key::F(12)]).unwrap();
        assert_eq!(m, Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT);
        assert_eq!(c, Code::F12);
        let (m, c) = chord_parts(&[Key::Meta, Key::Char('k')]).unwrap();
        assert_eq!((m, c), (Modifiers::SUPER, Code::KeyK));
    }

    #[test]
    fn bad_chords_are_rejected() {
        assert!(chord_parts(&[Key::Ctrl]).is_none());
        assert!(chord_parts(&[Key::Char('a'), Key::Char('b')]).is_none());
        assert!(chord_parts(&[Key::Ctrl, Key::Char('é')]).is_none());
    }
}
