//! Top-level windows: listing, live reads, z-order lookups, foreground switching, restore
//! (Python `core.py:list_windows`, `is_cloaked`, `root_hwnd`, `_hwnd_at`, `bring_to_foreground`,
//! `restore_window`, `wait_window_ready`, `computer.py:_popup_menu_hwnds`, `_owner_chain`,
//! `_uwp_app_name`, `_window_text`, `_gui_thread_info`).
//!
//! A `WindowRef` is the `HWND` value itself, so a window keeps its ref for its whole life.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::thread::sleep;
use std::time::{Duration, Instant};

use wh_core::backend::{
    BackendError, BackendResult, Caret, Rect, WindowInfo, WindowKind, WindowRef,
};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, BringWindowToTop, EnumChildWindows, EnumWindows, GA_ROOT,
    GA_ROOTOWNER, GUITHREADINFO, GW_OWNER, GWL_EXSTYLE, GetAncestor, GetClassNameW,
    GetForegroundWindow, GetGUIThreadInfo, GetWindow, GetWindowLongW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, SPI_GETFOREGROUNDLOCKTIMEOUT, SPI_SETFOREGROUNDLOCKTIMEOUT, SW_RESTORE,
    SW_SHOW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetForegroundWindow, ShowWindow,
    SystemParametersInfoW, WS_EX_TOOLWINDOW, WindowFromPoint,
};
use windows::core::BOOL;

use crate::process;
use crate::util::{hwnd_of, opt_ref, window_ref};

/// A window below this x is parked off screen by the shell (a minimised-to-tray app), not usable.
const OFFSCREEN_LEFT: i32 = -30000;

// ------------------------------------------------------------------------------------ pure rules

/// What kind of surface a window class names. Python matched these class names in the guards.
pub(crate) fn kind_for_class(class: &str) -> WindowKind {
    let lower = class.to_ascii_lowercase();
    match lower.as_str() {
        "progman" | "workerw" => WindowKind::Desktop,
        "shell_traywnd" | "shell_secondarytraywnd" => WindowKind::Taskbar,
        "cabinetwclass" => WindowKind::FileManager,
        "#32770" => WindowKind::Dialog,
        _ if is_popup_class(&lower) => WindowKind::PopupMenu,
        _ => WindowKind::Normal,
    }
}

/// The classic menu class and the XAML popup hosts Windows 11 draws its context menu in.
pub(crate) fn is_popup_class(class: &str) -> bool {
    let lower = class.to_ascii_lowercase();
    lower == "#32768" || lower.contains("popup")
}

/// Does `list_windows` show a window: visible, titled (not just blanks), not a tool window.
pub(crate) fn is_listable(visible: bool, title: &str, tool_window: bool) -> bool {
    visible && !title.trim().is_empty() && !tool_window
}

/// Does the window cover the whole monitor it is on (a fullscreen game or video)?
pub(crate) fn covers_monitor(window: Rect, monitor: Rect) -> bool {
    window.covers(&monitor)
}

/// Are the bounds those of a window that is really on screen?
pub(crate) fn has_real_bounds(r: Rect) -> bool {
    r.right > r.left && r.bottom > r.top && r.left > OFFSCREEN_LEFT
}

/// The classes whose text is worth reporting from a small dialog.
pub(crate) fn is_text_class(class: &str) -> bool {
    matches!(class, "Static" | "DirectUIHWND" | "CtrlNotifySink")
}

/// Title plus child texts joined with " / ", the way a popup's question is reported. Texts of two
/// characters or fewer are noise. Capped at 200 characters.
pub(crate) fn join_dialog_text(title: &str, children: &[String]) -> String {
    let mut parts: Vec<&str> = vec![title];
    parts.extend(
        children
            .iter()
            .filter(|t| t.chars().count() > 2)
            .map(String::as_str),
    );
    let joined = parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    joined.chars().take(200).collect()
}

// ------------------------------------------------------------------------------------ raw reads

/// Every top-level window, topmost first.
fn top_level() -> Vec<HWND> {
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the address of the Vec that `top_level` keeps alive for the call.
        let out = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        out.push(hwnd);
        BOOL(1)
    }
    let mut out: Vec<HWND> = Vec::new();
    // SAFETY: the callback only pushes to the Vec behind `lparam`, which outlives this synchronous call.
    let _ = unsafe { EnumWindows(Some(each), LPARAM(std::ptr::from_mut(&mut out) as isize)) };
    out
}

/// Direct and indirect children of a window.
fn children(hwnd: HWND) -> Vec<HWND> {
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the address of the Vec that `children` keeps alive for the call.
        let out = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        out.push(hwnd);
        BOOL(1)
    }
    let mut out: Vec<HWND> = Vec::new();
    // SAFETY: as in `top_level`; the parent handle is only read by the OS.
    let _ = unsafe {
        EnumChildWindows(
            Some(hwnd),
            Some(each),
            LPARAM(std::ptr::from_mut(&mut out) as isize),
        )
    };
    out
}

pub(crate) fn is_window(hwnd: HWND) -> bool {
    // SAFETY: IsWindow accepts any handle value, live or not.
    unsafe { IsWindow(Some(hwnd)) }.as_bool()
}

fn is_iconic(hwnd: HWND) -> bool {
    // SAFETY: IsIconic accepts any handle value.
    unsafe { IsIconic(hwnd) }.as_bool()
}

fn is_visible(hwnd: HWND) -> bool {
    // SAFETY: IsWindowVisible accepts any handle value.
    unsafe { IsWindowVisible(hwnd) }.as_bool()
}

pub(crate) fn foreground() -> HWND {
    // SAFETY: no arguments, no preconditions.
    unsafe { GetForegroundWindow() }
}

pub(crate) fn title_of(hwnd: HWND) -> String {
    // SAFETY: plain query on a handle; a dead window reports length 0.
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    // SAFETY: `buf` has room for `len + 1` UTF-16 units including the NUL.
    let got = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..got.max(0) as usize])
}

pub(crate) fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: `buf` has room for 256 UTF-16 units.
    let got = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..got.max(0) as usize])
}

fn rect_of(hwnd: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    // SAFETY: `r` is a live RECT out-parameter.
    unsafe { GetWindowRect(hwnd, &mut r) }.ok()?;
    Some(Rect::new(r.left, r.top, r.right, r.bottom))
}

fn pid_of(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: `pid` is a live out-parameter.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

fn thread_of(hwnd: HWND) -> u32 {
    // SAFETY: a null process-id pointer is allowed; a null or dead handle returns 0.
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

/// A DWM-cloaked window is present and nominally visible but not composited on any desktop. UWP
/// apps keep one per process; it duplicates the real title and cannot be foregrounded.
fn is_cloaked(hwnd: HWND) -> bool {
    let mut value = 0u32;
    // SAFETY: the out-buffer is a live u32 and its exact size is passed.
    let r = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            std::ptr::from_mut(&mut value).cast::<c_void>(),
            size_of::<u32>() as u32,
        )
    };
    r.is_ok() && value != 0
}

fn monitor_rect(hwnd: HWND) -> Option<Rect> {
    // SAFETY: a plain query; the nearest monitor is returned even for an off-screen window.
    let mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a live MONITORINFO with cbSize set.
    if !unsafe { GetMonitorInfoW(mon, &mut info) }.as_bool() {
        return None;
    }
    let r = info.rcMonitor;
    Some(Rect::new(r.left, r.top, r.right, r.bottom))
}

fn is_tool_window(hwnd: HWND) -> bool {
    // SAFETY: plain query on a handle.
    let ex = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
    ex & WS_EX_TOOLWINDOW.0 != 0
}

/// Process facts looked up once per pid during one listing.
#[derive(Default)]
pub(crate) struct ProcCache {
    seen: HashMap<u32, (String, bool)>,
}

impl ProcCache {
    fn get(&mut self, pid: u32) -> (String, bool) {
        self.seen
            .entry(pid)
            .or_insert_with(|| {
                (
                    process::process_name(pid).unwrap_or_else(|| "?".to_string()),
                    process::is_elevated(pid),
                )
            })
            .clone()
    }
}

/// Everything `WindowInfo` carries, read live. The caller has already decided the window qualifies.
fn read_info(
    hwnd: HWND,
    fg: HWND,
    cache: &mut ProcCache,
    cloaked: bool,
    visible: bool,
) -> WindowInfo {
    let pid = pid_of(hwnd);
    let (app, elevated) = cache.get(pid);
    let rect = rect_of(hwnd).unwrap_or_default();
    let class = class_of(hwnd);
    let fullscreen = monitor_rect(hwnd).is_some_and(|m| covers_monitor(rect, m));
    // SAFETY: plain query; a window with no owner returns an error, which becomes None.
    let owner = unsafe { GetWindow(hwnd, GW_OWNER) }.ok().and_then(opt_ref);
    WindowInfo {
        window: window_ref(hwnd),
        title: title_of(hwnd),
        app,
        pid,
        rect,
        minimized: is_iconic(hwnd),
        fullscreen,
        foreground: hwnd == fg,
        hidden: cloaked || !visible,
        elevated,
        kind: kind_for_class(&class),
        class,
        owner,
    }
}

// ------------------------------------------------------------------------------------ queries

/// Top-level application windows, topmost first (Python `list_windows`).
pub(crate) fn list_windows(include_hidden: bool) -> Vec<WindowInfo> {
    let fg = foreground();
    let mut cache = ProcCache::default();
    let mut out = Vec::new();
    for hwnd in top_level() {
        if !is_visible(hwnd) {
            continue;
        }
        // SAFETY: plain query on a handle.
        if unsafe { GetWindowTextLengthW(hwnd) } == 0 {
            continue;
        }
        let title = title_of(hwnd);
        if !is_listable(true, &title, is_tool_window(hwnd)) {
            continue;
        }
        let cloaked = is_cloaked(hwnd);
        if cloaked && !include_hidden {
            continue;
        }
        out.push(read_info(hwnd, fg, &mut cache, cloaked, true));
    }
    out
}

/// A live read of one window (Python `window_process`, `window_pid`, `window_class`, `_window_rect`).
pub(crate) fn window_info(window: WindowRef) -> BackendResult<WindowInfo> {
    let hwnd = hwnd_of(window);
    if !is_window(hwnd) {
        return Err(BackendError::WindowGone);
    }
    let mut cache = ProcCache::default();
    Ok(read_info(
        hwnd,
        foreground(),
        &mut cache,
        is_cloaked(hwnd),
        is_visible(hwnd),
    ))
}

pub(crate) fn foreground_window() -> Option<WindowRef> {
    opt_ref(foreground())
}

/// The top-level window under a point, honouring the real z-order (Python `_hwnd_at`).
pub(crate) fn window_at_point(x: i32, y: i32) -> Option<WindowRef> {
    // SAFETY: a plain hit test on screen coordinates.
    let hit = unsafe { WindowFromPoint(POINT { x, y }) };
    if hit.0.is_null() {
        return None;
    }
    // SAFETY: plain query on a live or dead handle.
    let root = unsafe { GetAncestor(hit, GA_ROOT) };
    opt_ref(if root.0.is_null() { hit } else { root })
}

/// The top-level window above a handle (`GA_ROOT`), `None` when the OS reports none.
pub(crate) fn root_hwnd(hwnd: HWND) -> Option<HWND> {
    // SAFETY: plain query on a live or dead handle.
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    (!root.0.is_null()).then_some(root)
}

/// Window classes that mean Chromium, Electron or CEF (Python `_CHROMIUM_CLASSES`).
const CHROMIUM_CLASSES: [&str; 3] = [
    "Chrome_WidgetWin",
    "CefBrowserWindow",
    "Chrome_RenderWidgetHostHWND",
];

/// Does a class name belong to Chromium?
pub(crate) fn is_chromium_class(class: &str) -> bool {
    CHROMIUM_CLASSES.iter().any(|c| class.starts_with(c))
}

/// Is this a Chromium, Electron or CEF window (Steam, Brave, Discord, Hermes)? The window's class or
/// one of its first 40 child windows' (Python `_is_chromium`).
pub(crate) fn is_chromium(hwnd: HWND) -> bool {
    is_chromium_class(&class_of(hwnd))
        || children(hwnd)
            .into_iter()
            .take(40)
            .any(|c| is_chromium_class(&class_of(c)))
}

/// The top-level window that contains `window` (Python `root_hwnd`).
pub(crate) fn root_window(window: WindowRef) -> BackendResult<WindowRef> {
    let hwnd = hwnd_of(window);
    if !is_window(hwnd) {
        return Err(BackendError::WindowGone);
    }
    // SAFETY: plain query on a live handle.
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    Ok(if root.0.is_null() {
        window
    } else {
        window_ref(root)
    })
}

/// Owners above a window, nearest first (Python `_owner_chain`).
pub(crate) fn owner_chain(window: WindowRef) -> BackendResult<Vec<WindowRef>> {
    let hwnd = hwnd_of(window);
    if !is_window(hwnd) {
        return Err(BackendError::WindowGone);
    }
    let mut chain: Vec<WindowRef> = Vec::new();
    let mut current = hwnd;
    for _ in 0..8 {
        // SAFETY: plain query; no owner returns an error.
        let Ok(owner) = (unsafe { GetWindow(current, GW_OWNER) }) else {
            break;
        };
        let owner_ref = window_ref(owner);
        if owner.0.is_null() || owner == current || chain.contains(&owner_ref) {
            break;
        }
        chain.push(owner_ref);
        current = owner;
    }
    // SAFETY: plain query on a live handle.
    let root_owner = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
    if !root_owner.0.is_null() && root_owner != hwnd && !chain.contains(&window_ref(root_owner)) {
        chain.push(window_ref(root_owner));
    }
    Ok(chain)
}

/// The app inside an ApplicationFrameHost frame (Python `_uwp_app_name`); `None` for any other window.
pub(crate) fn hosted_app(window: WindowRef) -> BackendResult<Option<String>> {
    let hwnd = hwnd_of(window);
    if !is_window(hwnd) {
        return Err(BackendError::WindowGone);
    }
    let frame_pid = pid_of(hwnd);
    let is_frame = process::process_name(frame_pid)
        .is_some_and(|n| n.eq_ignore_ascii_case("applicationframehost.exe"));
    if !is_frame {
        return Ok(None);
    }
    for child in children(hwnd) {
        let pid = pid_of(child);
        if pid != 0 && pid != frame_pid {
            return Ok(process::process_name(pid));
        }
    }
    Ok(None)
}

/// Visible top-level popup windows that can hold a menu (Python `_popup_menu_hwnds`).
pub(crate) fn popup_menus() -> Vec<WindowRef> {
    top_level()
        .into_iter()
        .filter(|&h| is_visible(h) && is_popup_class(&class_of(h)))
        .map(window_ref)
        .collect()
}

/// A small dialog's title and static text lines (Python `_window_text`).
pub(crate) fn window_text(window: WindowRef) -> BackendResult<String> {
    let hwnd = hwnd_of(window);
    if !is_window(hwnd) {
        return Err(BackendError::WindowGone);
    }
    let texts: Vec<String> = children(hwnd)
        .into_iter()
        .filter(|&c| is_text_class(&class_of(c)))
        .map(title_of)
        .collect();
    Ok(join_dialog_text(&title_of(hwnd), &texts))
}

// ------------------------------------------------------------------------------------ text caret

fn gui_thread_info(hwnd: Option<HWND>) -> Option<GUITHREADINFO> {
    let target = hwnd.unwrap_or_else(foreground);
    let thread = thread_of(target);
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a live GUITHREADINFO with cbSize set.
    unsafe { GetGUIThreadInfo(thread, &mut info) }.ok()?;
    Some(info)
}

/// Where the foreground thread's text cursor is, in screen coordinates (Python `_caret_screen_point`).
pub(crate) fn caret() -> Option<Caret> {
    let info = gui_thread_info(None)?;
    if info.hwndCaret.0.is_null() {
        return None;
    }
    let mut pt = POINT {
        x: info.rcCaret.left,
        y: info.rcCaret.top,
    };
    // SAFETY: `hwndCaret` was just reported by the OS; `pt` is a live out-parameter.
    if !unsafe { ClientToScreen(info.hwndCaret, &mut pt) }.as_bool() {
        return None;
    }
    Some(Caret {
        x: pt.x,
        y: pt.y,
        height: (info.rcCaret.bottom - info.rcCaret.top).max(1),
    })
}

/// Does the GUI thread of `window` (the foreground one when `None`) show a caret? (Python `_has_caret`)
pub(crate) fn has_caret(window: Option<WindowRef>) -> bool {
    gui_thread_info(window.map(hwnd_of)).is_some_and(|i| !i.hwndCaret.0.is_null())
}

/// The control with keyboard focus in the foreground thread (Python `_focused_hwnd`).
pub(crate) fn focus_token() -> u64 {
    gui_thread_info(None).map_or(0, |i| i.hwndFocus.0 as usize as u64)
}

// ------------------------------------------------------------------------------------ foreground

/// Undo for the temporary state `bring_to_foreground` sets up, run even when a step fails.
struct ForegroundScope {
    current_thread: u32,
    attached: Vec<u32>,
    old_timeout: Option<u32>,
}

impl Drop for ForegroundScope {
    fn drop(&mut self) {
        for &thread in &self.attached {
            // SAFETY: detaching the input queues we attached ourselves.
            let _ = unsafe { AttachThreadInput(self.current_thread, thread, false) };
        }
        if let Some(old) = self.old_timeout {
            // SAFETY: restores the timeout we read; the value rides in the pointer argument by design.
            let _ = unsafe {
                SystemParametersInfoW(
                    SPI_SETFOREGROUNDLOCKTIMEOUT,
                    0,
                    Some(old as usize as *mut c_void),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                )
            };
        }
    }
}

/// Restore if minimised, then raise: zero the foreground-lock timeout for the call, attach to the
/// foreground and target threads, `AllowSetForegroundWindow`, `ShowWindow(SW_SHOW)` (not
/// `SW_RESTORE`, which un-maximises), `SetForegroundWindow`, `BringWindowToTop`. No synthetic keys.
/// Retries once after 250 ms, then errors (Python `bring_to_foreground`).
pub(crate) fn bring_to_foreground(window: WindowRef) -> BackendResult<()> {
    let hwnd = hwnd_of(window);
    if !is_window(hwnd) {
        return Err(BackendError::WindowGone);
    }
    if is_iconic(hwnd) {
        // SAFETY: plain call on a live handle.
        let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
        sleep(Duration::from_millis(50));
    }
    if foreground() == hwnd {
        return Ok(());
    }
    let fg_thread = thread_of(foreground());
    let target_thread = thread_of(hwnd);
    // SAFETY: no arguments, no preconditions.
    let current = unsafe { GetCurrentThreadId() };
    let mut scope = ForegroundScope {
        current_thread: current,
        attached: Vec::new(),
        old_timeout: None,
    };
    {
        let mut old = 0u32;
        // SAFETY: `old` is a live u32 that receives the setting.
        let saved = unsafe {
            SystemParametersInfoW(
                SPI_GETFOREGROUNDLOCKTIMEOUT,
                0,
                Some(std::ptr::from_mut(&mut old).cast::<c_void>()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        }
        .is_ok();
        if saved {
            scope.old_timeout = Some(old);
            // SAFETY: a zero timeout rides in the pointer argument by design; restored by the scope.
            let _ = unsafe {
                SystemParametersInfoW(
                    SPI_SETFOREGROUNDLOCKTIMEOUT,
                    0,
                    Some(std::ptr::null_mut()),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                )
            };
        }
    }
    for thread in [fg_thread, target_thread] {
        if thread != 0 && thread != current && !scope.attached.contains(&thread) {
            // SAFETY: attaching our input queue to another thread's; detached by the scope.
            if unsafe { AttachThreadInput(current, thread, true) }.as_bool() {
                scope.attached.push(thread);
            }
        }
    }
    let pid = pid_of(hwnd);
    // SAFETY: plain calls on a live handle.
    unsafe {
        let _ = AllowSetForegroundWindow(pid);
        // SW_SHOW, not SW_RESTORE: restore un-maximises a maximised window.
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);
    }
    drop(scope);
    sleep(Duration::from_millis(50));
    if foreground() != hwnd {
        // A transient popup (address-bar autocomplete, a tooltip host) can hold the foreground for a
        // moment. Wait briefly and try once more before giving up.
        sleep(Duration::from_millis(250));
        // SAFETY: plain call on a handle.
        let _ = unsafe { SetForegroundWindow(hwnd) };
        sleep(Duration::from_millis(50));
        if foreground() != hwnd {
            return Err(BackendError::Os(format!(
                "could not bring window {} to foreground; still obscured or focus unobtainable",
                window.raw()
            )));
        }
    }
    Ok(())
}

/// `SW_RESTORE` a minimised window and wait, polled, until it is neither iconic nor hidden
/// (Python `restore_window`). A window that is not minimised is left alone.
pub(crate) fn restore_window(window: WindowRef, timeout: Duration) -> BackendResult<()> {
    let hwnd = hwnd_of(window);
    if !is_window(hwnd) {
        return Err(BackendError::WindowGone);
    }
    if !is_iconic(hwnd) {
        return Ok(());
    }
    // SAFETY: plain call on a live handle.
    let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if !is_iconic(hwnd) && is_visible(hwnd) {
            return Ok(());
        }
        sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// Has the window's accessibility tree begun to populate (a UIA root with a child)? Python asked
/// `ControlFromHandle(hwnd).GetChildren()`. The query can hang on a stuck app, so it is time-boxed.
fn tree_started(hwnd: HWND) -> bool {
    crate::uia::tree_started(hwnd)
}

/// Bounded wait for a restored window to settle (Python `wait_window_ready`).
pub(crate) fn wait_window_ready(window: WindowRef, timeout: Duration) -> BackendResult<()> {
    let hwnd = hwnd_of(window);
    if !is_window(hwnd) {
        return Err(BackendError::WindowGone);
    }
    let end = Instant::now() + timeout;
    let mut last_ok = false;
    while Instant::now() < end {
        let usable =
            !is_iconic(hwnd) && is_visible(hwnd) && rect_of(hwnd).is_some_and(has_real_bounds);
        if usable {
            if tree_started(hwnd) || last_ok {
                return Ok(());
            }
            last_ok = true;
        }
        sleep(Duration::from_millis(50));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_map_to_surface_kinds() {
        assert_eq!(kind_for_class("Progman"), WindowKind::Desktop);
        assert_eq!(kind_for_class("WorkerW"), WindowKind::Desktop);
        assert_eq!(kind_for_class("Shell_TrayWnd"), WindowKind::Taskbar);
        assert_eq!(
            kind_for_class("Shell_SecondaryTrayWnd"),
            WindowKind::Taskbar
        );
        assert_eq!(kind_for_class("CabinetWClass"), WindowKind::FileManager);
        assert_eq!(kind_for_class("#32770"), WindowKind::Dialog);
        assert_eq!(kind_for_class("#32768"), WindowKind::PopupMenu);
        assert_eq!(
            kind_for_class("Microsoft.UI.Content.PopupWindowSiteBridge"),
            WindowKind::PopupMenu
        );
        assert_eq!(kind_for_class("Notepad"), WindowKind::Normal);
        assert_eq!(kind_for_class(""), WindowKind::Normal);
    }

    #[test]
    fn chromium_windows_are_told_by_class() {
        assert!(is_chromium_class("Chrome_WidgetWin_1"));
        assert!(is_chromium_class("CefBrowserWindow"));
        assert!(is_chromium_class("Chrome_RenderWidgetHostHWND"));
        assert!(!is_chromium_class("Notepad"));
        assert!(!is_chromium_class("MozillaWindowClass"));
        assert!(!is_chromium_class(""));
    }

    #[test]
    fn popup_classes_are_the_menu_class_and_xaml_hosts() {
        assert!(is_popup_class("#32768"));
        assert!(is_popup_class("Windows.UI.Core.CoreWindow.Popup"));
        assert!(is_popup_class("XamlPopupHost"));
        assert!(!is_popup_class("#32770"));
        assert!(!is_popup_class("Chrome_WidgetWin_1"));
    }

    #[test]
    fn listing_skips_untitled_blank_and_tool_windows() {
        assert!(is_listable(true, "Untitled - Notepad", false));
        assert!(!is_listable(false, "Title", false));
        assert!(!is_listable(true, "   ", false));
        assert!(!is_listable(true, "", false));
        assert!(!is_listable(true, "Tooltip", true));
    }

    #[test]
    fn fullscreen_means_covering_the_whole_monitor() {
        let monitor = Rect::new(0, 0, 1920, 1080);
        assert!(covers_monitor(Rect::new(0, 0, 1920, 1080), monitor));
        assert!(covers_monitor(Rect::new(-8, -8, 1928, 1088), monitor));
        assert!(!covers_monitor(Rect::new(0, 0, 1920, 1040), monitor));
    }

    #[test]
    fn bounds_must_be_real_and_on_screen() {
        assert!(has_real_bounds(Rect::new(0, 0, 100, 100)));
        assert!(has_real_bounds(Rect::new(-1920, 0, 0, 1080)));
        assert!(!has_real_bounds(Rect::new(10, 10, 10, 50)));
        assert!(!has_real_bounds(Rect::new(10, 50, 60, 50)));
        assert!(!has_real_bounds(Rect::new(-32000, -32000, -31840, -31972)));
    }

    #[test]
    fn dialog_text_joins_title_and_lines_and_caps_at_200() {
        let kids = vec![
            "x already exists.".to_string(),
            "ok".to_string(),
            "Replace it?".to_string(),
        ];
        assert_eq!(
            join_dialog_text("Confirm Save As", &kids),
            "Confirm Save As / x already exists. / Replace it?"
        );
        assert_eq!(join_dialog_text("", &["abc".to_string()]), "abc");
        let long = vec!["y".repeat(300)];
        assert_eq!(join_dialog_text("t", &long).chars().count(), 200);
    }

    #[test]
    fn only_dialog_text_classes_are_read() {
        assert!(is_text_class("Static"));
        assert!(is_text_class("DirectUIHWND"));
        assert!(is_text_class("CtrlNotifySink"));
        assert!(!is_text_class("Button"));
        assert!(!is_text_class("static"));
    }
}
