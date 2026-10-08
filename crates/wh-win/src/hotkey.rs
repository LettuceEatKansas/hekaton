//! The global kill hotkey (Python `daemon.py:_hotkey_thread`, `HOTKEY_CANDIDATES`).
//!
//! `RegisterHotKey` belongs to the thread that registered it and its `WM_HOTKEY` arrives in that
//! thread's message queue, so one thread registers and then pumps messages for the life of the
//! process. The first chord Windows accepts wins; another program may already own a combination.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use wh_core::backend::{BackendError, BackendResult, HotkeyChord};
use wh_core::keys::{Key, windows_vk};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

/// The id this process registers its one hotkey under (Python `KILL_HOTKEY_ID`).
const KILL_HOTKEY_ID: i32 = 0x57_48;

/// Split a chord into `RegisterHotKey`'s modifier mask and virtual-key code: modifiers first, exactly
/// one other key. `None` for a chord that has no key or more than one.
pub(crate) fn chord_to_mods_vk(keys: &[Key]) -> Option<(u32, u32)> {
    let mut mods = 0u32;
    let mut vk = None;
    for key in keys {
        match key {
            Key::Ctrl => mods |= MOD_CONTROL.0,
            Key::Alt => mods |= MOD_ALT.0,
            Key::Shift => mods |= MOD_SHIFT.0,
            Key::Meta => mods |= MOD_WIN.0,
            other => {
                if vk.is_some() {
                    return None;
                }
                vk = Some(u32::from(windows_vk(*other)));
            }
        }
    }
    vk.map(|v| (mods, v))
}

/// Register the first candidate the OS accepts and call `on_press` for every press, from a thread
/// this function starts. Returns the registered chord's name, `None` when none could be registered.
pub(crate) fn register(
    candidates: &[HotkeyChord],
    on_press: Arc<dyn Fn() + Send + Sync>,
) -> BackendResult<Option<String>> {
    let candidates = candidates.to_vec();
    let (tx, rx) = mpsc::channel::<Option<String>>();
    std::thread::Builder::new()
        .name("winhands-kill-hotkey".into())
        .spawn(move || {
            let mut chosen = None;
            for chord in &candidates {
                let Some((mods, vk)) = chord_to_mods_vk(&chord.keys) else {
                    continue;
                };
                // SAFETY: registers a hotkey on this thread's queue; a refusal is an Err and we try the next one.
                let ok = unsafe {
                    RegisterHotKey(
                        None,
                        KILL_HOTKEY_ID,
                        HOT_KEY_MODIFIERS(mods) | MOD_NOREPEAT,
                        vk,
                    )
                }
                .is_ok();
                if ok {
                    chosen = Some(chord.name.clone());
                    break;
                }
            }
            let registered = chosen.is_some();
            let _ = tx.send(chosen);
            if !registered {
                return;
            }
            let mut msg = MSG::default();
            // SAFETY: `msg` is a live MSG; GetMessageW blocks until a message and returns <= 0 to end the loop.
            while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
                if msg.message == WM_HOTKEY && msg.wParam.0 == KILL_HOTKEY_ID as usize {
                    // A panic in the callback must not end the pump and leave the hotkey dead.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_press()));
                }
            }
        })
        .map_err(|e| BackendError::Os(format!("could not start the hotkey thread: {e}")))?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| BackendError::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_become_a_mask_and_the_last_key_the_vk() {
        let f12 = chord_to_mods_vk(&[Key::Ctrl, Key::Alt, Key::Shift, Key::F(12)]);
        assert_eq!(f12, Some((MOD_CONTROL.0 | MOD_ALT.0 | MOD_SHIFT.0, 0x7B)));
        let pause = chord_to_mods_vk(&[Key::Ctrl, Key::Alt, Key::Shift, Key::Pause]);
        assert_eq!(pause, Some((MOD_CONTROL.0 | MOD_ALT.0 | MOD_SHIFT.0, 0x13)));
        let win_k = chord_to_mods_vk(&[Key::Ctrl, Key::Shift, Key::Meta, Key::Char('k')]);
        assert_eq!(win_k, Some((MOD_CONTROL.0 | MOD_SHIFT.0 | MOD_WIN.0, 0x4B)));
    }

    #[test]
    fn the_key_may_come_anywhere_in_the_list() {
        assert_eq!(
            chord_to_mods_vk(&[Key::F(12), Key::Ctrl]),
            Some((MOD_CONTROL.0, 0x7B))
        );
        assert_eq!(chord_to_mods_vk(&[Key::Escape]), Some((0, 0x1B)));
    }

    #[test]
    fn chords_without_exactly_one_key_are_rejected() {
        assert_eq!(chord_to_mods_vk(&[]), None);
        assert_eq!(chord_to_mods_vk(&[Key::Ctrl, Key::Alt]), None);
        assert_eq!(
            chord_to_mods_vk(&[Key::Ctrl, Key::Char('a'), Key::Char('b')]),
            None
        );
    }

    #[test]
    fn the_python_candidates_all_map() {
        // The three chords daemon.py tried, in its order.
        let chords = [
            [Key::Ctrl, Key::Alt, Key::Shift, Key::F(12)].as_slice(),
            [Key::Ctrl, Key::Alt, Key::Shift, Key::Pause].as_slice(),
            [Key::Ctrl, Key::Shift, Key::Meta, Key::Char('k')].as_slice(),
        ];
        assert!(chords.iter().all(|c| chord_to_mods_vk(c).is_some()));
    }
}
