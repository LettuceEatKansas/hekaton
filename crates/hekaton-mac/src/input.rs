//! Synthetic input through CGEvent, posted at the HID event tap. Mouse coordinates are global
//! points (origin top left of the main display), the same space every other method uses.
//!
//! The backend keeps its own picture of what is held: modifier flags (so `cmd+click` and chords
//! carry the right flags on every event), mouse buttons (so a move while a button is down is a
//! drag, not a hover), and the click chain (so three single clicks become a triple click).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use hekaton_core::backend::{
    BackendError, BackendResult, InputGuard, MouseButton, Point, ScrollDirection,
};
use hekaton_core::keys::Key;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventType, CGMouseButton, CGScrollEventUnit,
};

use crate::cf::Shared;
use crate::keymap::{
    FLAG_CAPS_LOCK, FLAG_SHIFT, VK_RETURN, VK_TAB, implied_flags, keycode, modifier_flag,
};
use crate::perm;
use crate::textops::{
    ClickChain, DRAG_DWELL, DRAG_PRESS_SETTLE, DRAG_STEP_GAP, TYPE_CHUNK_UNITS, TypeStep,
    drag_path, plan_typing, scroll_step,
};

/// Pause between the parts of a click so apps see distinct events.
const CLICK_GAP: Duration = Duration::from_millis(14);
/// Pause after moving the pointer so hover state settles before the press.
const HOVER_GAP: Duration = Duration::from_millis(10);
/// Pause between scroll notches.
const SCROLL_GAP: Duration = Duration::from_millis(12);
/// Pause between typed chunks.
const TYPE_GAP: Duration = Duration::from_millis(6);

#[derive(Default)]
struct State {
    /// Modifier flags held down by `key_down`.
    flags: u64,
    /// Left, right, middle.
    buttons: [bool; 3],
    chain: ClickChain,
}

/// The event poster.
pub(crate) struct Input {
    source: Option<Shared<objc2_core_foundation::CFRetained<CGEventSource>>>,
    state: Mutex<State>,
    events: AtomicU64,
    epoch: Instant,
}

fn button_index(b: MouseButton) -> usize {
    match b {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
    }
}

fn cg_button(b: MouseButton) -> CGMouseButton {
    match b {
        MouseButton::Left => CGMouseButton::Left,
        MouseButton::Right => CGMouseButton::Right,
        MouseButton::Middle => CGMouseButton::Center,
    }
}

fn cg_point(p: Point) -> CGPoint {
    CGPoint {
        x: f64::from(p.x),
        y: f64::from(p.y),
    }
}

fn down_type(b: MouseButton) -> CGEventType {
    match b {
        MouseButton::Left => CGEventType::LeftMouseDown,
        MouseButton::Right => CGEventType::RightMouseDown,
        MouseButton::Middle => CGEventType::OtherMouseDown,
    }
}

fn up_type(b: MouseButton) -> CGEventType {
    match b {
        MouseButton::Left => CGEventType::LeftMouseUp,
        MouseButton::Right => CGEventType::RightMouseUp,
        MouseButton::Middle => CGEventType::OtherMouseUp,
    }
}

fn drag_type(b: MouseButton) -> CGEventType {
    match b {
        MouseButton::Left => CGEventType::LeftMouseDragged,
        MouseButton::Right => CGEventType::RightMouseDragged,
        MouseButton::Middle => CGEventType::OtherMouseDragged,
    }
}

fn os(msg: &str) -> BackendError {
    BackendError::Os(msg.to_string())
}

impl Input {
    pub(crate) fn new() -> Self {
        Self {
            source: CGEventSource::new(CGEventSourceStateID::HIDSystemState).map(Shared),
            state: Mutex::new(State::default()),
            events: AtomicU64::new(0),
            epoch: Instant::now(),
        }
    }

    fn src(&self) -> Option<&CGEventSource> {
        self.source.as_ref().map(|s| &*s.0)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn post(&self, event: &CGEvent) {
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(event));
        self.events.fetch_add(1, Ordering::Relaxed);
    }

    /// Count an action done through Accessibility (a direct insert, a press) as input sent: it
    /// changed the app as surely as a posted event.
    pub(crate) fn count_ax_action(&self) {
        self.events.fetch_add(1, Ordering::Relaxed);
    }

    /// How many events were posted since start.
    pub(crate) fn events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }

    fn mouse_event(
        &self,
        kind: CGEventType,
        at: Point,
        button: MouseButton,
        click_state: Option<i64>,
        flags: u64,
    ) -> BackendResult<()> {
        let ev = CGEvent::new_mouse_event(self.src(), kind, cg_point(at), cg_button(button))
            .ok_or_else(|| os("could not create a mouse event"))?;
        if flags != 0 {
            CGEvent::set_flags(Some(&ev), CGEventFlags(flags));
        }
        if let Some(n) = click_state {
            CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventClickState, n);
        }
        if button == MouseButton::Middle {
            CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventButtonNumber, 2);
        }
        self.post(&ev);
        Ok(())
    }

    /// Where the pointer is.
    pub(crate) fn cursor(&self) -> BackendResult<Point> {
        let ev =
            CGEvent::new(self.src()).ok_or_else(|| os("could not read the pointer position"))?;
        let p = CGEvent::location(Some(&ev));
        Ok(Point {
            x: p.x.round() as i32,
            y: p.y.round() as i32,
        })
    }

    /// Time since the last input from any source.
    pub(crate) fn idle(&self) -> Duration {
        let secs = CGEventSource::seconds_since_last_event_type(
            CGEventSourceStateID::HIDSystemState,
            // kCGAnyInputEventType is (CGEventType)~0.
            CGEventType(u32::MAX),
        );
        idle_from_secs(secs)
    }

    /// Move the pointer. A held button turns the move into a drag event.
    pub(crate) fn mouse_move(&self, to: Point) -> BackendResult<()> {
        perm::require_post_events("move the mouse")?;
        self.move_inner(to)
    }

    fn move_inner(&self, to: Point) -> BackendResult<()> {
        let (kind, button, flags) = {
            let st = self.state();
            let held = [MouseButton::Left, MouseButton::Right, MouseButton::Middle]
                .into_iter()
                .find(|b| st.buttons[button_index(*b)]);
            match held {
                Some(b) => (drag_type(b), b, st.flags),
                None => (CGEventType::MouseMoved, MouseButton::Left, st.flags),
            }
        };
        self.mouse_event(kind, to, button, None, flags)
    }

    /// Press or release a button where the pointer is.
    pub(crate) fn mouse_button(&self, button: MouseButton, down: bool) -> BackendResult<()> {
        perm::require_post_events("press a mouse button")?;
        let at = self.cursor()?;
        let (click_state, flags) = {
            let mut st = self.state();
            st.buttons[button_index(button)] = down;
            let n = if down {
                st.chain.next_state(self.epoch.elapsed(), at, button)
            } else {
                1
            };
            (n, st.flags)
        };
        let kind = if down {
            down_type(button)
        } else {
            up_type(button)
        };
        self.mouse_event(kind, at, button, Some(click_state), flags)
    }

    /// Move to `at`, then click `count` times.
    pub(crate) fn click(
        &self,
        at: Point,
        button: MouseButton,
        count: u8,
        guard: InputGuard<'_>,
    ) -> BackendResult<()> {
        perm::require_post_events("click")?;
        guard()?;
        self.move_inner(at)?;
        sleep(HOVER_GAP);
        for _ in 0..count.max(1) {
            guard()?;
            let (click_state, flags) = {
                let mut st = self.state();
                let n = st.chain.next_state(self.epoch.elapsed(), at, button);
                (n, st.flags)
            };
            self.mouse_event(down_type(button), at, button, Some(click_state), flags)?;
            sleep(CLICK_GAP);
            // The release always follows the press, whatever the guard thinks by now.
            self.mouse_event(up_type(button), at, button, Some(click_state), flags)?;
            sleep(CLICK_GAP);
        }
        Ok(())
    }

    /// Move to `at` and scroll `notches` notches.
    pub(crate) fn scroll(
        &self,
        at: Point,
        direction: ScrollDirection,
        notches: u32,
        guard: InputGuard<'_>,
    ) -> BackendResult<()> {
        perm::require_post_events("scroll")?;
        guard()?;
        self.move_inner(at)?;
        sleep(HOVER_GAP);
        let (dy, dx) = scroll_step(direction);
        for _ in 0..notches.max(1) {
            guard()?;
            let ev = CGEvent::new_scroll_wheel_event2(
                self.src(),
                CGScrollEventUnit::Pixel,
                2,
                dy,
                dx,
                0,
            )
            .ok_or_else(|| os("could not create a scroll event"))?;
            let flags = self.state().flags;
            if flags != 0 {
                CGEvent::set_flags(Some(&ev), CGEventFlags(flags));
            }
            self.post(&ev);
            sleep(SCROLL_GAP);
        }
        Ok(())
    }

    /// Press at `from`, wait for the press to register, move to `to` in twelve steps, stay there for a
    /// few frames, release at `to`. The release happens even if the guard stops the drag. Same timing
    /// as the Windows build (`core.drag`, 2026-10-08): a game that samples input once a frame needs to
    /// see the item over its target before the button comes up.
    pub(crate) fn drag(&self, from: Point, to: Point, guard: InputGuard<'_>) -> BackendResult<()> {
        perm::require_post_events("drag")?;
        guard()?;
        self.move_inner(from)?;
        sleep(HOVER_GAP);
        let flags = self.state().flags;
        {
            let mut st = self.state();
            st.chain.reset();
            st.buttons[0] = true;
        }
        self.mouse_event(
            CGEventType::LeftMouseDown,
            from,
            MouseButton::Left,
            Some(1),
            flags,
        )?;
        sleep(DRAG_PRESS_SETTLE);
        let mut at = from;
        let mut outcome = Ok(());
        for p in drag_path(from, to) {
            if let Err(e) = guard() {
                outcome = Err(e);
                break;
            }
            if let Err(e) = self.mouse_event(
                CGEventType::LeftMouseDragged,
                p,
                MouseButton::Left,
                None,
                flags,
            ) {
                outcome = Err(e);
                break;
            }
            at = p;
            sleep(DRAG_STEP_GAP);
        }
        if outcome.is_ok() {
            sleep(DRAG_DWELL);
        }
        self.state().buttons[0] = false;
        let up = self.mouse_event(
            CGEventType::LeftMouseUp,
            at,
            MouseButton::Left,
            Some(1),
            flags,
        );
        outcome.and(up)
    }

    fn key_event(&self, code: u16, down: bool, flags: u64) -> BackendResult<()> {
        let ev = CGEvent::new_keyboard_event(self.src(), code, down)
            .ok_or_else(|| os("could not create a key event"))?;
        CGEvent::set_flags(Some(&ev), CGEventFlags(flags));
        self.post(&ev);
        Ok(())
    }

    /// Press a key and leave it down.
    pub(crate) fn key_down(&self, key: Key) -> BackendResult<()> {
        self.key(key, true)
    }

    /// Release a key.
    pub(crate) fn key_up(&self, key: Key) -> BackendResult<()> {
        self.key(key, false)
    }

    fn key(&self, key: Key, down: bool) -> BackendResult<()> {
        perm::require_post_events("press keys")?;
        let code = keycode(key).ok_or_else(|| {
            BackendError::Unsupported(format!("the key {key} has no macOS keycode"))
        })?;
        let held_flag = modifier_flag(key).filter(|f| *f != FLAG_CAPS_LOCK);
        if held_flag.is_none() && down {
            perm::check_secure_typing()?;
        }
        let flags = {
            let mut st = self.state();
            if let Some(f) = held_flag {
                if down {
                    st.flags |= f;
                } else {
                    st.flags &= !f;
                }
            }
            st.flags
                | if held_flag.is_none() {
                    implied_flags(key)
                } else {
                    0
                }
        };
        self.key_event(code, down, flags)
    }

    /// Type text as unicode key events, newline and tab as real keys.
    pub(crate) fn type_text(&self, text: &str, guard: InputGuard<'_>) -> BackendResult<()> {
        perm::require_post_events("type")?;
        perm::check_secure_typing()?;
        for step in plan_typing(text, TYPE_CHUNK_UNITS) {
            guard()?;
            let flags = self.state().flags;
            match step {
                TypeStep::Return => {
                    self.key_event(VK_RETURN, true, flags)?;
                    self.key_event(VK_RETURN, false, flags)?;
                }
                TypeStep::Tab => {
                    self.key_event(VK_TAB, true, flags)?;
                    self.key_event(VK_TAB, false, flags)?;
                }
                TypeStep::Key { code, shift, unit } => {
                    let flags = if shift { flags | FLAG_SHIFT } else { flags };
                    for down in [true, false] {
                        let ev = CGEvent::new_keyboard_event(self.src(), code, down)
                            .ok_or_else(|| os("could not create a key event"))?;
                        CGEvent::set_flags(Some(&ev), CGEventFlags(flags));
                        // SAFETY: `unit` outlives the call and the length passed is 1.
                        unsafe {
                            CGEvent::keyboard_set_unicode_string(Some(&ev), 1, &unit);
                        }
                        self.post(&ev);
                    }
                }
                TypeStep::Units(units) => {
                    for down in [true, false] {
                        let ev = CGEvent::new_keyboard_event(self.src(), 0, down)
                            .ok_or_else(|| os("could not create a key event"))?;
                        CGEvent::set_flags(Some(&ev), CGEventFlags(flags));
                        // SAFETY: `units` outlives the call and the length passed is its length.
                        unsafe {
                            CGEvent::keyboard_set_unicode_string(
                                Some(&ev),
                                units.len() as _,
                                units.as_ptr(),
                            );
                        }
                        self.post(&ev);
                    }
                }
            }
            sleep(TYPE_GAP);
        }
        Ok(())
    }
}

/// The OS counter's reading as a duration. An unreadable counter (NaN, negative) is not "a person
/// just touched the mouse": reading it as zero idle refused every acting step (2026-10-30).
fn idle_from_secs(secs: f64) -> Duration {
    if secs.is_finite() && secs >= 0.0 {
        Duration::from_secs_f64(secs.min(1.0e9))
    } else {
        Duration::from_secs(3600)
    }
}

#[cfg(test)]
mod idle_tests {
    use super::*;

    #[test]
    fn a_bad_idle_reading_is_not_a_person() {
        assert_eq!(idle_from_secs(f64::NAN), Duration::from_secs(3600));
        assert_eq!(idle_from_secs(-1.0), Duration::from_secs(3600));
        assert_eq!(idle_from_secs(f64::INFINITY), Duration::from_secs(3600));
        assert_eq!(idle_from_secs(0.25), Duration::from_millis(250));
    }
}
