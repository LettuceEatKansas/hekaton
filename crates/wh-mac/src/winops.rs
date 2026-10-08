//! Window operations for the `window` action on macOS: close, minimise, maximise, restore, move.
//!
//! All of it is Accessibility, so it needs the Accessibility grant and never touches the process:
//! a close presses the window's own close button (`AXCloseButton`, the same as a click, so an app
//! with unsaved work asks first); minimise sets `AXMinimized`; maximise enters full screen
//! (`AXFullScreen`) and falls back to the green zoom button for a window that cannot; restore
//! leaves full screen and un-minimises; move writes `AXPosition` and `AXSize`.

use objc2_application_services::AXError;
use objc2_core_foundation::{CGPoint, CGSize};
use wh_core::backend::{
    BackendError, BackendResult, Rect, WindowGeometry, WindowOp, WindowRef, WindowState,
};

use crate::ax;
use crate::cf;
use crate::perm;
use crate::wininfo::window_parts;
use crate::winlist::{self, AxWin};

/// The window's Accessibility element, or why there is none.
fn ax_window(window: WindowRef, what: &str) -> BackendResult<AxWin> {
    perm::require_accessibility(what)?;
    let (pid, id) = window_parts(window);
    let raw = winlist::lookup(pid, id).ok_or(BackendError::WindowGone)?;
    winlist::find_ax_window(pid, id, Some(raw.bounds)).ok_or_else(|| {
        BackendError::Os(
            "Accessibility does not list that window (another Space, or a helper surface)".into(),
        )
    })
}

fn check(err: AXError, what: &str) -> BackendResult<()> {
    if err == AXError::Success {
        Ok(())
    } else {
        Err(BackendError::Os(format!(
            "could not {what}: {}",
            ax::describe(err)
        )))
    }
}

/// Frame, visible bounds (the same here: macOS draws no invisible border), no client area, and
/// whether the window is minimised or in full screen.
pub(crate) fn geometry(window: WindowRef, displays: &[Rect]) -> BackendResult<WindowGeometry> {
    let info = winlist::info(window, displays)?;
    let mut state = if info.minimized {
        WindowState::Minimized
    } else {
        WindowState::Normal
    };
    if state == WindowState::Normal && ax::trusted() {
        let (pid, id) = window_parts(window);
        if winlist::find_ax_window(pid, id, Some(info.rect))
            .is_some_and(|w| w.el.bool("AXFullScreen") == Some(true))
        {
            state = WindowState::Maximized;
        }
    }
    Ok(WindowGeometry {
        frame: info.rect,
        visible: info.rect,
        client: None,
        state,
    })
}

/// Do one window operation. Returns when the request is made; the core polls the result.
pub(crate) fn op(window: WindowRef, op: WindowOp) -> BackendResult<()> {
    match op {
        WindowOp::Close => {
            let w = ax_window(window, "close a window")?;
            let Some(button) = w.el.element("AXCloseButton") else {
                return Err(BackendError::Unsupported(
                    "this window has no close button to press".into(),
                ));
            };
            check(button.perform("AXPress"), "press the close button")
        }
        WindowOp::Minimize => {
            let w = ax_window(window, "minimise a window")?;
            check(w.el.set_bool("AXMinimized", true), "minimise the window")
        }
        WindowOp::Maximize => {
            let w = ax_window(window, "maximise a window")?;
            if w.el.set_bool("AXFullScreen", true) == AXError::Success {
                return Ok(());
            }
            // not every window can go full screen: the green zoom button still zooms it
            match w.el.element("AXZoomButton") {
                Some(zoom) => check(zoom.perform("AXPress"), "press the zoom button"),
                None => Err(BackendError::Unsupported(
                    "this window can be neither put in full screen nor zoomed".into(),
                )),
            }
        }
        WindowOp::Restore => {
            let w = ax_window(window, "restore a window")?;
            if w.el.bool("AXFullScreen") == Some(true) {
                check(w.el.set_bool("AXFullScreen", false), "leave full screen")?;
            }
            if w.minimized {
                check(
                    w.el.set_bool("AXMinimized", false),
                    "un-minimise the window",
                )?;
            }
            Ok(())
        }
        WindowOp::Move(rect) => {
            let w = ax_window(window, "move a window")?;
            let (Some(pos), Some(size)) = (
                cf::make_ax_point(CGPoint {
                    x: f64::from(rect.left),
                    y: f64::from(rect.top),
                }),
                cf::make_ax_size(CGSize {
                    width: f64::from(rect.width()),
                    height: f64::from(rect.height()),
                }),
            ) else {
                return Err(BackendError::Os("could not describe the new bounds".into()));
            };
            // size first, then position, then size again: a window that was against a screen edge
            // clamps the first size to where it was
            check(w.el.set_value("AXSize", &size), "resize the window")?;
            check(w.el.set_value("AXPosition", &pos), "move the window")?;
            check(w.el.set_value("AXSize", &size), "resize the window")
        }
    }
}
