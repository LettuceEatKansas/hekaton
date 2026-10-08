//! Synthetic input: `SendInput` for the mouse and keyboard (Python `core.py` SendInput plumbing,
//! `_send_inputs`, `_key_input`, `move_mouse`, `click_at`, `scroll_at`, `drag`, `send_unicode_text`).
//!
//! The pointer is moved with absolute coordinates over the whole virtual desktop
//! (`MOUSEEVENTF_VIRTUALDESK`), so a second monitor to the left or above works. Text is typed as
//! `KEYEVENTF_UNICODE` events, one UTF-16 unit at a time, so layouts and dead keys do not matter.

use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::Duration;

use wh_core::backend::{
    BackendError, BackendResult, InputGuard, MouseButton, Point, Rect, ScrollDirection,
};
use wh_core::keys::{Key, windows_vk};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
    VIRTUAL_KEY,
};

use crate::display;

/// One wheel notch (`WHEEL_DELTA`).
const WHEEL_DELTA: i32 = 120;

/// How many interpolated moves a drag makes (Python `core.drag`: `range(1, 13)`).
const DRAG_STEPS: i32 = 12;
/// After the press, before the first move: lets the app register the button going down.
const DRAG_PRESS_SETTLE: Duration = Duration::from_millis(50);
/// Between two moves, about one frame of a game capped at 60 FPS.
const DRAG_STEP_GAP: Duration = Duration::from_millis(15);
/// On the target before the release, a few frames, so a drop target sees the item over it.
///
/// 2026-10-08 live run (Slay the Spire 2 capped at 24 FPS): an 80 ms drag released on the same frame
/// it arrived, so the game often never saw the card over its target and the drop did nothing. About
/// 0.35 s per drag now.
const DRAG_DWELL: Duration = Duration::from_millis(120);

// ------------------------------------------------------------------------------------ pure helpers

/// Map a screen point to the 0..65535 range `SendInput` wants over the virtual desktop. Matches
/// Python `_normalize`: scaled by `width - 1` (never zero) and truncated towards zero.
pub(crate) fn normalize(p: Point, desktop: Rect) -> (i32, i32) {
    let scale = |v: i32, origin: i32, extent: i32| -> i32 {
        let num = (i64::from(v) - i64::from(origin)) * 65535;
        let den = i64::from((extent - 1).max(1));
        // Truncation towards zero is Python's int(); the result fits i32 for any on-screen point.
        (num / den) as i32
    };
    (
        scale(p.x, desktop.left, desktop.width()),
        scale(p.y, desktop.top, desktop.height()),
    )
}

/// The `i`-th of [`DRAG_STEPS`] points on the way from `from` to `to`, rounding down like Python's `//`.
pub(crate) fn drag_point(from: Point, to: Point, i: i32) -> Point {
    Point {
        x: from.x + (to.x - from.x).saturating_mul(i).div_euclid(DRAG_STEPS),
        y: from.y + (to.y - from.y).saturating_mul(i).div_euclid(DRAG_STEPS),
    }
}

/// Keys that need `KEYEVENTF_EXTENDEDKEY` so the arrow and navigation block is not read as the
/// numeric keypad (Python `_key_input`).
pub(crate) fn is_extended_vk(vk: u16) -> bool {
    matches!(
        vk,
        0x25..=0x28 // left, up, right, down
        | 0x24 // home
        | 0x23 // end
        | 0x21 // page up
        | 0x22 // page down
        | 0x2D // insert
        | 0x2E // delete
    )
}

/// The wheel event for a scroll: (horizontal?, signed delta). Up and right are positive.
pub(crate) fn wheel_for(direction: ScrollDirection, notches: u32) -> (bool, i32) {
    let magnitude = i32::try_from(notches)
        .unwrap_or(i32::MAX / WHEEL_DELTA)
        .saturating_mul(WHEEL_DELTA);
    match direction {
        ScrollDirection::Up => (false, magnitude),
        ScrollDirection::Down => (false, -magnitude),
        ScrollDirection::Right => (true, magnitude),
        ScrollDirection::Left => (true, -magnitude),
    }
}

/// The down and up flags of a button.
pub(crate) fn button_flags(button: MouseButton) -> (MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS) {
    match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    }
}

fn key_input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn vk_input(vk: u16, up: bool) -> INPUT {
    let mut flags = if up {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    if is_extended_vk(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    key_input(vk, 0, flags)
}

fn unicode_input(unit: u16, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    key_input(0, unit, flags)
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, data: i32, dx: i32, dy: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                // The wheel delta is a signed quantity carried in an unsigned field.
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

// ------------------------------------------------------------------------------------ the injector

/// Sends input and counts the batches it sent.
#[derive(Default)]
pub(crate) struct Injector {
    batches: AtomicU64,
}

impl Injector {
    /// How many `SendInput` calls have been made (Python `INPUT_EVENTS`).
    pub(crate) fn batches(&self) -> u64 {
        self.batches.load(Ordering::SeqCst)
    }

    /// One `SendInput` call. Counted before the call: a failed call may still have moved something.
    fn send(&self, inputs: &[INPUT]) -> BackendResult<()> {
        self.batches.fetch_add(1, Ordering::SeqCst);
        let size = i32::try_from(size_of::<INPUT>()).unwrap_or(40);
        // SAFETY: `inputs` is a live slice of fully initialised INPUT structs and `size` is their size.
        let sent = unsafe { SendInput(inputs, size) } as usize;
        if sent == inputs.len() {
            Ok(())
        } else {
            Err(BackendError::Os(format!(
                "SendInput sent {sent}/{} events; outcome uncertain, inspect before retrying",
                inputs.len()
            )))
        }
    }

    pub(crate) fn mouse_event(&self, flags: MOUSE_EVENT_FLAGS, data: i32) -> BackendResult<()> {
        self.send(&[mouse_input(flags, data, 0, 0)])
    }

    pub(crate) fn move_to(&self, p: Point) -> BackendResult<()> {
        let (nx, ny) = normalize(p, display::virtual_screen_rect());
        self.send(&[mouse_input(
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            0,
            nx,
            ny,
        )])
    }

    pub(crate) fn button(&self, button: MouseButton, down: bool) -> BackendResult<()> {
        let (d, u) = button_flags(button);
        self.mouse_event(if down { d } else { u }, 0)
    }

    pub(crate) fn click(
        &self,
        at: Point,
        button: MouseButton,
        count: u8,
        guard: InputGuard<'_>,
    ) -> BackendResult<()> {
        guard()?;
        self.move_to(at)?;
        sleep(Duration::from_millis(10));
        let (down, up) = button_flags(button);
        for _ in 0..count.max(1) {
            guard()?;
            // 15 ms between down and up is plenty for every app tried (Win32, WinUI, Electron, Chromium).
            self.mouse_event(down, 0)?;
            sleep(Duration::from_millis(15));
            self.mouse_event(up, 0)?;
            sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    pub(crate) fn scroll(
        &self,
        at: Point,
        direction: ScrollDirection,
        notches: u32,
        guard: InputGuard<'_>,
    ) -> BackendResult<()> {
        guard()?;
        self.move_to(at)?;
        sleep(Duration::from_millis(20));
        guard()?;
        let (horizontal, delta) = wheel_for(direction, notches);
        let flags = if horizontal {
            MOUSEEVENTF_HWHEEL
        } else {
            MOUSEEVENTF_WHEEL
        };
        self.mouse_event(flags, delta)
    }

    pub(crate) fn drag(&self, from: Point, to: Point, guard: InputGuard<'_>) -> BackendResult<()> {
        guard()?;
        self.move_to(from)?;
        sleep(Duration::from_millis(30));
        guard()?;
        self.mouse_event(MOUSEEVENTF_LEFTDOWN, 0)?;
        let moved = (|| -> BackendResult<()> {
            sleep(DRAG_PRESS_SETTLE);
            for i in 1..=DRAG_STEPS {
                guard()?;
                self.move_to(drag_point(from, to, i))?;
                sleep(DRAG_STEP_GAP);
            }
            sleep(DRAG_DWELL);
            Ok(())
        })();
        // Always release, even if focus changed or the guard stopped the drag.
        let released = self.mouse_event(MOUSEEVENTF_LEFTUP, 0);
        moved.and(released)
    }

    pub(crate) fn key(&self, key: Key, up: bool) -> BackendResult<()> {
        self.send(&[vk_input(windows_vk(key), up)])
    }

    /// Press `keys` in order and release them in reverse as one `SendInput` batch, `repeat` times.
    pub(crate) fn chord(
        &self,
        keys: &[Key],
        repeat: u32,
        guard: InputGuard<'_>,
    ) -> BackendResult<()> {
        let mut batch: Vec<INPUT> = Vec::with_capacity(keys.len() * 2);
        batch.extend(keys.iter().map(|k| vk_input(windows_vk(*k), false)));
        batch.extend(keys.iter().rev().map(|k| vk_input(windows_vk(*k), true)));
        for _ in 0..repeat.max(1) {
            guard()?;
            self.send(&batch)?;
            sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    /// Type text one UTF-16 unit at a time (an emoji is two units: a surrogate pair).
    pub(crate) fn type_text(&self, text: &str, guard: InputGuard<'_>) -> BackendResult<()> {
        for unit in text.encode_utf16() {
            guard()?;
            self.send(&[unicode_input(unit, false), unicode_input(unit, true)])?;
        }
        sleep(Duration::from_millis(10));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desktop() -> Rect {
        Rect::from_xywh(-1920, 0, 1920 + 2560, 1440)
    }

    #[test]
    fn normalisation_spans_the_whole_virtual_desktop() {
        let d = desktop();
        assert_eq!(normalize(Point::new(-1920, 0), d), (0, 0));
        assert_eq!(normalize(Point::new(2559, 1439), d), (65535, 65535));
        // Matches Python: int((x - left) * 65535 / (width - 1)).
        assert_eq!(normalize(Point::new(0, 720), d), (28092, 32790));
    }

    #[test]
    fn normalisation_survives_degenerate_and_off_screen_input() {
        let tiny = Rect::from_xywh(0, 0, 1, 1);
        assert_eq!(normalize(Point::new(0, 0), tiny), (0, 0));
        // Negative numerators truncate towards zero, like Python's int().
        assert_eq!(
            normalize(Point::new(-1, 0), Rect::from_xywh(0, 0, 1001, 11)).0,
            -65
        );
        assert!(normalize(Point::new(i32::MAX, 0), desktop()).0 > 65535);
    }

    #[test]
    fn drag_points_floor_like_python_integer_division() {
        let (a, b) = (Point::new(100, 100), Point::new(20, 140));
        let points: Vec<Point> = (1..=DRAG_STEPS).map(|i| drag_point(a, b, i)).collect();
        assert_eq!(points.len(), 12);
        assert_eq!(points[11], b);
        // (20 - 100) * 1 // 12 is -7 in Python, and (-80 * 3) // 12 is -20.
        assert_eq!(points[0], Point::new(93, 103));
        assert_eq!(points[2], Point::new(80, 110));
        // A step that is not a multiple of 12 floors: (-7 * 1) // 12 is -1 in Python.
        assert_eq!(
            drag_point(Point::new(10, 10), Point::new(3, 10), 1),
            Point::new(9, 10)
        );
    }

    // Ported from core.drag (7e3838c): press, 50 ms, 12 moves 15 ms apart, 120 ms on the target,
    // then release. About a third of a second per drag.
    #[test]
    fn a_drag_takes_about_a_third_of_a_second() {
        assert_eq!(DRAG_STEPS, 12);
        assert_eq!(DRAG_PRESS_SETTLE, Duration::from_millis(50));
        assert_eq!(DRAG_STEP_GAP, Duration::from_millis(15));
        assert_eq!(DRAG_DWELL, Duration::from_millis(120));
        let total = DRAG_PRESS_SETTLE + DRAG_STEP_GAP * DRAG_STEPS as u32 + DRAG_DWELL;
        assert_eq!(total, Duration::from_millis(350));
    }

    #[test]
    fn navigation_keys_are_extended_and_letters_are_not() {
        for vk in [0x25, 0x26, 0x27, 0x28, 0x24, 0x23, 0x21, 0x22, 0x2D, 0x2E] {
            assert!(is_extended_vk(vk), "{vk:#x}");
        }
        for vk in [0x41, 0x0D, 0x09, 0x1B, 0x70, 0x5B, 0x20, 0x08] {
            assert!(!is_extended_vk(vk), "{vk:#x}");
        }
    }

    #[test]
    fn key_events_carry_the_right_flags() {
        let down = vk_input(0x41, false);
        let up = vk_input(0x25, true);
        // SAFETY: both inputs were built as keyboard events, so the `ki` arm of the union is the live one.
        let (d, u) = unsafe { (down.Anonymous.ki, up.Anonymous.ki) };
        assert_eq!((d.wVk.0, d.dwFlags.0), (0x41, 0));
        assert_eq!(u.wVk.0, 0x25);
        assert_eq!(u.dwFlags, KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY);
    }

    #[test]
    fn unicode_events_carry_the_unit_as_the_scan_code() {
        let down = unicode_input(0x20AC, false);
        let up = unicode_input(0xD83D, true);
        // SAFETY: both inputs were built as keyboard events, so the `ki` arm of the union is the live one.
        let (d, u) = unsafe { (down.Anonymous.ki, up.Anonymous.ki) };
        assert_eq!(
            (d.wVk.0, d.wScan, d.dwFlags),
            (0, 0x20AC, KEYEVENTF_UNICODE)
        );
        assert_eq!(u.wScan, 0xD83D);
        assert_eq!(u.dwFlags, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
    }

    #[test]
    fn emoji_become_surrogate_pairs() {
        let units: Vec<u16> = "a\u{1F600}".encode_utf16().collect();
        assert_eq!(units, [0x61, 0xD83D, 0xDE00]);
    }

    #[test]
    fn wheel_events_follow_direction_and_size() {
        assert_eq!(wheel_for(ScrollDirection::Up, 3), (false, 360));
        assert_eq!(wheel_for(ScrollDirection::Down, 1), (false, -120));
        assert_eq!(wheel_for(ScrollDirection::Right, 2), (true, 240));
        assert_eq!(wheel_for(ScrollDirection::Left, 2), (true, -240));
        // Absurd counts clamp instead of overflowing.
        assert!(wheel_for(ScrollDirection::Up, u32::MAX).1 > 0);
    }

    #[test]
    fn buttons_map_to_their_flag_pairs() {
        assert_eq!(
            button_flags(MouseButton::Left),
            (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)
        );
        assert_eq!(
            button_flags(MouseButton::Right),
            (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)
        );
        assert_eq!(
            button_flags(MouseButton::Middle),
            (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP)
        );
    }

    #[test]
    fn a_guard_that_refuses_stops_before_anything_is_sent() {
        // Nothing here reaches SendInput: the guard fails first, which is exactly what is tested.
        let inj = Injector::default();
        let refuse = || -> BackendResult<()> { Err(BackendError::Denied("no".into())) };
        assert!(
            inj.click(Point::new(1, 1), MouseButton::Left, 1, &refuse)
                .is_err()
        );
        assert!(
            inj.scroll(Point::new(1, 1), ScrollDirection::Up, 1, &refuse)
                .is_err()
        );
        assert!(
            inj.drag(Point::new(1, 1), Point::new(2, 2), &refuse)
                .is_err()
        );
        assert!(inj.chord(&[Key::Ctrl, Key::Char('a')], 1, &refuse).is_err());
        assert!(inj.type_text("x", &refuse).is_err());
        assert_eq!(inj.batches(), 0);
    }
}
