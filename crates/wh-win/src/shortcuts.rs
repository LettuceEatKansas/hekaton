//! Window operations for the `window` action (Python `shortcuts.py`: `_post_close`, `_show_async`,
//! `_set_pos`, `_state`, `_window_rect`, `_client_rect`, `_visible_rect`).
//!
//! Each is one Win32 call, so the core can poll the result and a hung window never blocks the call:
//! `PostMessageW(WM_CLOSE)` (the polite close, the same as the X button; the process is never killed),
//! `ShowWindowAsync` (does not wait for the window's message loop) and `SetWindowPos` with
//! `SWP_NOZORDER | SWP_NOACTIVATE` (place it, do not steal focus or change stacking).

use std::ffi::c_void;
use std::mem::size_of;

use wh_core::backend::{
    BackendError, BackendResult, Rect, WindowGeometry, WindowOp, WindowRef, WindowState,
};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetWindowRect, IsIconic, IsWindow, IsZoomed, PostMessageW, SHOW_WINDOW_CMD,
    SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos,
    ShowWindowAsync, WM_CLOSE,
};

use crate::util::{hwnd_of, os_err};

fn live(window: WindowRef) -> BackendResult<HWND> {
    let hwnd = hwnd_of(window);
    // SAFETY: IsWindow accepts any handle value, live or not.
    if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        Ok(hwnd)
    } else {
        Err(BackendError::WindowGone)
    }
}

fn rect_of(r: RECT) -> Rect {
    Rect::new(r.left, r.top, r.right, r.bottom)
}

/// The DWM extended frame (no invisible resize border) when it answers and is not empty, else `None`.
fn extended_frame(hwnd: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    // SAFETY: the out-buffer is a live RECT and its exact size is passed.
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            std::ptr::from_mut(&mut r).cast::<c_void>(),
            size_of::<RECT>() as u32,
        )
    };
    (hr.is_ok() && r.right > r.left && r.bottom > r.top).then(|| rect_of(r))
}

/// The client area in screen coordinates, `None` when the window has none.
fn client_rect(hwnd: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    // SAFETY: `r` is a live RECT out-parameter.
    unsafe { GetClientRect(hwnd, &mut r) }.ok()?;
    let mut origin = POINT { x: 0, y: 0 };
    // SAFETY: `origin` is a live POINT; the handle was just read.
    if !unsafe { ClientToScreen(hwnd, &mut origin) }.as_bool() {
        return None;
    }
    let rect = Rect::new(
        origin.x,
        origin.y,
        origin.x + (r.right - r.left),
        origin.y + (r.bottom - r.top),
    );
    (!rect.is_empty()).then_some(rect)
}

/// A window's frame, visible bounds, client area and state (Python `_window_rect`, `_visible_rect`,
/// `_client_rect`, `_state`).
pub(crate) fn window_geometry(window: WindowRef) -> BackendResult<WindowGeometry> {
    let hwnd = live(window)?;
    let mut r = RECT::default();
    // SAFETY: `r` is a live RECT out-parameter.
    unsafe { GetWindowRect(hwnd, &mut r) }.map_err(|e| os_err("window unavailable", &e))?;
    let frame = rect_of(r);
    // SAFETY: plain queries on a handle.
    let state = if unsafe { IsIconic(hwnd) }.as_bool() {
        WindowState::Minimized
    } else if unsafe { IsZoomed(hwnd) }.as_bool() {
        WindowState::Maximized
    } else {
        WindowState::Normal
    };
    Ok(WindowGeometry {
        frame,
        visible: extended_frame(hwnd).unwrap_or(frame),
        client: client_rect(hwnd),
        state,
    })
}

/// Ask a window to close, minimise, maximise, restore or move. Returns when the request is made,
/// not when the window has obeyed (Python `_post_close`, `_show_async`, `_set_pos`).
pub(crate) fn window_op(window: WindowRef, op: WindowOp) -> BackendResult<()> {
    let hwnd = live(window)?;
    match op {
        WindowOp::Close => {
            // SAFETY: posting a message to a live window; WM_CLOSE carries no pointers.
            unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }
                .map_err(|e| os_err("could not ask the window to close", &e))
        }
        WindowOp::Minimize => show(hwnd, SW_MINIMIZE),
        WindowOp::Maximize => show(hwnd, SW_MAXIMIZE),
        WindowOp::Restore => show(hwnd, SW_RESTORE),
        WindowOp::Move(rect) => {
            // SAFETY: placing a live window; SWP_NOZORDER | SWP_NOACTIVATE keep stacking and focus.
            unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    rect.left,
                    rect.top,
                    rect.width(),
                    rect.height(),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            }
            .map_err(|e| os_err("could not move the window", &e))
        }
    }
}

fn show(hwnd: HWND, cmd: SHOW_WINDOW_CMD) -> BackendResult<()> {
    // SAFETY: ShowWindowAsync only posts the request to the window's own thread. Its return value is
    // whether the call was accepted, which a dead window answers with false; the core polls the state.
    let _ = unsafe { ShowWindowAsync(hwnd, cmd) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_that_is_not_there_is_gone_not_a_crash() {
        let nowhere = WindowRef::from_raw(0x7fff_fff0);
        assert_eq!(
            window_geometry(nowhere).unwrap_err(),
            BackendError::WindowGone
        );
        for op in [
            WindowOp::Close,
            WindowOp::Minimize,
            WindowOp::Maximize,
            WindowOp::Restore,
            WindowOp::Move(Rect::from_xywh(0, 0, 100, 100)),
        ] {
            assert_eq!(
                window_op(nowhere, op).unwrap_err(),
                BackendError::WindowGone,
                "{op:?}"
            );
        }
    }
}
