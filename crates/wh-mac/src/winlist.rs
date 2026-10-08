//! Windows: the window server's list (`CGWindowListCopyWindowInfo`) for identity, geometry and
//! z-order, plus Accessibility for what the list lacks (minimised, subrole, titles when Screen
//! Recording is off) and for raising, restoring and focusing.

use std::collections::HashMap;
use std::thread::sleep;
use std::time::{Duration, Instant};

use objc2_core_foundation::CFDictionary;
use objc2_core_graphics::{CGWindowListCopyWindowInfo, CGWindowListOption};
use wh_core::backend::{
    BackendError, BackendResult, Point, Rect, WindowInfo, WindowKind, WindowRef,
};

use crate::apps;
use crate::ax::{self, Ax};
use crate::cf;
use crate::geom::rect_from_f64;
use crate::perm;
use crate::wininfo::{
    RawWindow, WindowContext, frontmost_app_window, to_info, window_at_point, window_parts,
    window_ref,
};

const ON_SCREEN: CGWindowListOption = CGWindowListOption(
    CGWindowListOption::OptionOnScreenOnly.0 | CGWindowListOption::ExcludeDesktopElements.0,
);
const EVERYTHING: CGWindowListOption = CGWindowListOption(
    CGWindowListOption::OptionAll.0 | CGWindowListOption::ExcludeDesktopElements.0,
);
const POLL: Duration = Duration::from_millis(30);

fn parse_window(d: &CFDictionary) -> Option<RawWindow> {
    let id = cf::dict_get(d, "kCGWindowNumber").and_then(|v| cf::i64_of(&v))? as u32;
    let pid = cf::dict_get(d, "kCGWindowOwnerPID").and_then(|v| cf::i64_of(&v))? as i32;
    let layer = cf::dict_get(d, "kCGWindowLayer")
        .and_then(|v| cf::i64_of(&v))
        .unwrap_or(0) as i32;
    let alpha = cf::dict_get(d, "kCGWindowAlpha")
        .and_then(|v| cf::f64_of(&v))
        .unwrap_or(1.0);
    let bounds = cf::dict_get(d, "kCGWindowBounds")
        .and_then(|v| cf::dict_of(&v).map(bounds_of))
        .unwrap_or_default();
    Some(RawWindow {
        id,
        pid,
        layer,
        alpha,
        bounds,
        title: cf::dict_get(d, "kCGWindowName").and_then(|v| cf::string_of(&v)),
        owner_name: cf::dict_get(d, "kCGWindowOwnerName")
            .and_then(|v| cf::string_of(&v))
            .unwrap_or_default(),
        on_screen: cf::dict_get(d, "kCGWindowIsOnscreen")
            .and_then(|v| cf::bool_of(&v))
            .unwrap_or(false),
    })
}

fn bounds_of(b: &CFDictionary) -> Rect {
    let n = |k: &str| {
        cf::dict_get(b, k)
            .and_then(|v| cf::f64_of(&v))
            .unwrap_or(0.0)
    };
    rect_from_f64(n("X"), n("Y"), n("Width"), n("Height"))
}

/// The window server's list, front to back.
pub(crate) fn copy_list(opts: CGWindowListOption, relative: u32) -> Vec<RawWindow> {
    let Some(arr) = CGWindowListCopyWindowInfo(opts, relative) else {
        return Vec::new();
    };
    cf::array_items(&arr)
        .iter()
        .filter_map(|item| cf::dict_of(item).and_then(parse_window))
        .collect()
}

/// An application window as Accessibility sees it.
pub(crate) struct AxWin {
    pub(crate) el: Ax,
    pub(crate) id: Option<u32>,
    pub(crate) minimized: bool,
    pub(crate) title: String,
    pub(crate) subrole: Option<String>,
    pub(crate) rect: Option<Rect>,
}

/// Every window an app lists in `AXWindows`.
pub(crate) fn ax_windows(pid: i32) -> Vec<AxWin> {
    Ax::app(pid)
        .elements("AXWindows")
        .into_iter()
        .map(|el| {
            let rect = match (el.position(), el.size()) {
                (Some(p), Some(s)) => Some(rect_from_f64(p.x, p.y, s.width, s.height)),
                _ => None,
            };
            AxWin {
                id: el.window_id(),
                minimized: el.bool("AXMinimized").unwrap_or(false),
                title: el.string("AXTitle").unwrap_or_default(),
                subrole: el.string("AXSubrole"),
                rect,
                el,
            }
        })
        .collect()
}

/// Find the AX window for a window-server window: by `CGWindowID`, else by identical bounds.
pub(crate) fn find_ax_window(pid: i32, id: u32, hint: Option<Rect>) -> Option<AxWin> {
    let wins = ax_windows(pid);
    if wins.iter().any(|w| w.id.is_some()) {
        return wins.into_iter().find(|w| w.id == Some(id));
    }
    let hint = hint?;
    wins.into_iter().find(|w| {
        w.rect.is_some_and(|r| {
            (r.left - hint.left).abs() <= 2
                && (r.top - hint.top).abs() <= 2
                && (r.right - hint.right).abs() <= 2
                && (r.bottom - hint.bottom).abs() <= 2
        })
    })
}

/// `(pid, CGWindowID)` of the focused window of the frontmost app.
pub(crate) fn foreground_id(on_screen: &[RawWindow]) -> Option<(i32, u32)> {
    let pid = apps::frontmost_pid()?;
    if ax::trusted()
        && let Some(id) = Ax::app(pid)
            .element("AXFocusedWindow")
            .and_then(|w| w.window_id())
        && on_screen.iter().any(|r| r.pid == pid && r.id == id)
    {
        return Some((pid, id));
    }
    frontmost_app_window(on_screen, pid).map(|r| (r.pid, r.id))
}

/// The on-screen windows, front to back.
pub(crate) fn on_screen_list() -> Vec<RawWindow> {
    copy_list(ON_SCREEN, 0)
}

/// The neutral foreground window.
pub(crate) fn foreground() -> Option<WindowRef> {
    let list = copy_list(ON_SCREEN, 0);
    foreground_id(&list).map(|(pid, id)| window_ref(pid, id))
}

/// Top-level application windows in z-order.
pub(crate) fn list(include_hidden: bool, displays: &[Rect]) -> Vec<WindowInfo> {
    let raws = copy_list(EVERYTHING, 0);
    let on_screen: Vec<RawWindow> = raws.iter().filter(|r| r.on_screen).cloned().collect();
    let front = foreground_id(&on_screen);
    let have_titles = perm::screen_capture_granted();
    let have_ax = ax::trusted();
    let mut bundles: HashMap<i32, String> = HashMap::new();
    let mut ax_by_pid: HashMap<i32, Vec<AxWin>> = HashMap::new();
    let mut out = Vec::new();
    for raw in raws.iter().filter(|r| r.is_app_window()) {
        let bundle = bundles
            .entry(raw.pid)
            .or_insert_with(|| apps::bundle_id(raw.pid))
            .clone();
        let mut ctx = WindowContext {
            bundle_id: bundle,
            foreground: front == Some((raw.pid, raw.id)),
            ..Default::default()
        };
        let needs_ax = !raw.on_screen || (!have_titles && raw.title.is_none());
        if needs_ax && have_ax {
            let wins = ax_by_pid
                .entry(raw.pid)
                .or_insert_with(|| ax_windows(raw.pid));
            match wins.iter().find(|w| w.id == Some(raw.id)) {
                Some(w) => {
                    ctx.minimized = w.minimized;
                    ctx.subrole = w.subrole.clone();
                    ctx.ax_title = Some(w.title.clone()).filter(|t| !t.is_empty());
                }
                // Off screen and unknown to Accessibility: a helper surface, not a window.
                None if !raw.on_screen => continue,
                None => {}
            }
        } else if !raw.on_screen {
            // Cannot tell a minimised window from a leftover surface without Accessibility.
            continue;
        }
        let info = to_info(raw, &ctx, displays);
        if info.hidden && !include_hidden {
            continue;
        }
        out.push(info);
    }
    out
}

/// A live read of one window.
pub(crate) fn info(window: WindowRef, displays: &[Rect]) -> BackendResult<WindowInfo> {
    let (pid, id) = window_parts(window);
    let raw = copy_list(CGWindowListOption::OptionIncludingWindow, id)
        .into_iter()
        .find(|r| r.id == id && r.pid == pid)
        .ok_or(BackendError::WindowGone)?;
    let on_screen = copy_list(ON_SCREEN, 0);
    let mut ctx = WindowContext {
        bundle_id: apps::bundle_id(pid),
        foreground: foreground_id(&on_screen) == Some((pid, id)),
        ..Default::default()
    };
    if raw.layer == 0
        && ax::trusted()
        && let Some(w) = find_ax_window(pid, id, Some(raw.bounds))
    {
        ctx.minimized = w.minimized;
        ctx.subrole = w.subrole;
        ctx.ax_title = Some(w.title).filter(|t| !t.is_empty());
    }
    let mut info = to_info(&raw, &ctx, displays);
    if matches!(info.kind, WindowKind::Dialog | WindowKind::PopupMenu) || raw.layer > 0 {
        info.owner = on_screen
            .iter()
            .find(|r| r.pid == pid && r.id != id && r.is_app_window())
            .map(|r| window_ref(r.pid, r.id));
    }
    Ok(info)
}

/// The topmost window under a point.
pub(crate) fn at_point(p: Point) -> Option<WindowRef> {
    let list = copy_list(ON_SCREEN, 0);
    window_at_point(&list, p).map(|r| window_ref(r.pid, r.id))
}

/// Pop-up and context menu surfaces.
pub(crate) fn popups() -> Vec<(WindowRef, Rect)> {
    copy_list(ON_SCREEN, 0)
        .into_iter()
        .filter(RawWindow::is_popup_menu)
        .map(|r| (window_ref(r.pid, r.id), r.bounds))
        .collect()
}

fn is_on_screen(pid: i32, id: u32) -> bool {
    copy_list(CGWindowListOption::OptionIncludingWindow, id)
        .iter()
        .any(|r| r.id == id && r.pid == pid && r.on_screen)
}

/// Bring a minimised or hidden window back and wait, bounded, for the window server to show it.
pub(crate) fn restore(window: WindowRef, timeout: Duration) -> BackendResult<()> {
    let (pid, id) = window_parts(window);
    let raw = copy_list(CGWindowListOption::OptionIncludingWindow, id)
        .into_iter()
        .find(|r| r.id == id && r.pid == pid)
        .ok_or(BackendError::WindowGone)?;
    if raw.on_screen {
        return Ok(());
    }
    perm::require_accessibility("restore a minimised window")?;
    if apps::is_hidden(pid) {
        apps::unhide(pid);
    }
    if let Some(w) = find_ax_window(pid, id, Some(raw.bounds))
        && w.minimized
    {
        let err = w.el.set_bool("AXMinimized", false);
        if err != objc2_application_services::AXError::Success {
            return Err(BackendError::Os(format!(
                "could not un-minimise the window: {}",
                ax::describe(err)
            )));
        }
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if is_on_screen(pid, id) {
            return Ok(());
        }
        sleep(POLL);
    }
    if is_on_screen(pid, id) {
        Ok(())
    } else {
        Err(BackendError::Os(
            "the window did not come back on screen (it may be on another Space)".into(),
        ))
    }
}

/// Bring a window to the front and focus it, without synthetic key events.
pub(crate) fn bring_to_front(window: WindowRef, displays: &[Rect]) -> BackendResult<()> {
    perm::require_accessibility("bring a window to the front")?;
    let (pid, id) = window_parts(window);
    let current = info(window, displays)?;
    if current.minimized || current.hidden {
        restore(window, Duration::from_millis(1500))?;
    }
    if apps::is_hidden(pid) {
        apps::unhide(pid);
    }
    let app = Ax::app(pid);
    let ax_win = find_ax_window(pid, id, Some(current.rect));
    let deadline = Instant::now() + Duration::from_millis(900);
    let mut attempts = 0;
    loop {
        // AXFrontmost works without the cooperative-activation rules NSRunningApplication has.
        let front = app.set_bool("AXFrontmost", true);
        if front != objc2_application_services::AXError::Success {
            apps::activate(pid);
        }
        if let Some(w) = &ax_win {
            w.el.perform("AXRaise");
            w.el.set_bool("AXMain", true);
            w.el.set_bool("AXFocused", true);
        }
        sleep(Duration::from_millis(60));
        let on_screen = copy_list(ON_SCREEN, 0);
        if foreground_id(&on_screen) == Some((pid, id)) {
            return Ok(());
        }
        attempts += 1;
        if Instant::now() >= deadline || attempts >= 12 {
            break;
        }
    }
    if apps::frontmost_pid() == Some(pid) {
        // The app is in front but another of its windows has focus (a modal sheet, say).
        return Err(BackendError::Os(
            "the app is in front but this window did not take focus (a dialog may be open)".into(),
        ));
    }
    Err(BackendError::Os(format!(
        "could not bring {} to the front",
        apps::name(pid)
    )))
}

/// Wait, bounded, for a window to have real bounds and some accessibility children.
pub(crate) fn wait_ready(window: WindowRef, timeout: Duration) {
    let (pid, id) = window_parts(window);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let sized = copy_list(CGWindowListOption::OptionIncludingWindow, id)
            .iter()
            .any(|r| r.id == id && r.pid == pid && r.on_screen && !r.bounds.is_empty());
        let populated = !ax::trusted()
            || find_ax_window(pid, id, None)
                .is_some_and(|w| !w.el.elements("AXChildren").is_empty());
        if sized && populated {
            return;
        }
        sleep(POLL);
    }
}
