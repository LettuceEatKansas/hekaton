//! Single-element UI Automation operations: re-reading an element, the element under the pointer or
//! with focus, acting on an element, the file dialog's name box, and the items of an open menu.
//!
//! Python reference: `core.py:_fresh_element`, `_try_patterns`, `computer.py:_describe_point`,
//! `_irreversible_name_at`, `_file_dialog`, `_menu_items_once`, `_menu_items_under`.

use std::time::{Duration, Instant};

use hekaton_core::backend::{
    BackendError, BackendResult, Element, ElementAction, ElementHit, ElementId, MenuItem, Point,
    WindowRef,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    IUIAutomationElement, IUIAutomationExpandCollapsePattern, IUIAutomationInvokePattern,
    IUIAutomationLegacyIAccessiblePattern, IUIAutomationSelectionItemPattern,
    IUIAutomationTogglePattern, IUIAutomationValuePattern, TreeScope_Children,
    TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_ExpandCollapsePatternId,
    UIA_InvokePatternId, UIA_LegacyIAccessiblePatternId, UIA_SelectionItemPatternId,
    UIA_TogglePatternId, UIA_ValuePatternId,
};
use windows::core::BSTR;

use crate::model::{self, ct};
use crate::uia::{
    self, Uia, build_element, read_cached, refresh, remember, resolve, top_level_of, uia, win_point,
};
use crate::util::{hwnd_of, os_err};
use crate::wnd;

/// Bound the caller's wait. Native UIA calls may still be running after this expires.
const ELEMENT_TIMEOUT: Duration = Duration::from_millis(1500);

// ------------------------------------------------------------------------------------ elements

/// A fresh read of one element (`_fresh_element`): the same id, current properties.
pub(crate) fn element(id: &ElementId) -> BackendResult<Element> {
    let id = id.clone();
    uia::with_deadline(ELEMENT_TIMEOUT, move || {
        let u = uia()?;
        let (el, parts) = resolve(u, &id)?;
        let fresh = refresh(u, &el)?;
        let mut out = build_element(parts.hwnd, &fresh);
        out.id = id;
        Ok(out)
    })?
}

/// The top-level window an element lives in (`GetTopLevelControl().NativeWindowHandle`).
pub(crate) fn element_window(id: &ElementId) -> BackendResult<WindowRef> {
    let id = id.clone();
    uia::with_deadline(ELEMENT_TIMEOUT, move || {
        let u = uia()?;
        let (el, parts) = resolve(u, &id)?;
        let recorded = hwnd_of(WindowRef::from_raw(parts.hwnd));
        let top = top_level_of(u, &el, Some(recorded)).unwrap_or(recorded);
        Ok(crate::util::window_ref(top))
    })?
}

/// The element at a screen point and up to `max_ancestors` parents, within `budget`.
pub(crate) fn element_at(
    p: Point,
    max_ancestors: usize,
    budget: Duration,
) -> BackendResult<Option<ElementHit>> {
    let run = uia::with_deadline(budget, move || -> BackendResult<Option<ElementHit>> {
        let u = uia()?;
        // SAFETY: a hit test with the shared cache request; an empty spot is an error, not a crash.
        let Ok(el) = (unsafe { u.auto.ElementFromPointBuildCache(win_point(p), &u.cache) }) else {
            return Ok(None);
        };
        let fallback = wnd::window_at_point(p.x, p.y).map(hwnd_of);
        let hwnd = top_level_of(u, &el, fallback).map_or(0, |h| h.0 as usize as u64);
        let element = build_element(hwnd, &el);
        let mut ancestors = Vec::new();
        let mut cur = el;
        for _ in 0..max_ancestors {
            // SAFETY: walking to the parent of a live element, with the cache request.
            match unsafe { u.raw.GetParentElementBuildCache(&cur, &u.cache) } {
                Ok(parent) => {
                    ancestors.push(build_element(hwnd, &parent));
                    cur = parent;
                }
                Err(_) => break,
            }
        }
        Ok(Some(ElementHit { element, ancestors }))
    });
    match run {
        Ok(answer) => answer,
        // "answering within budget or returning None"
        Err(BackendError::Timeout) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The element with keyboard focus (`auto.GetFocusedControl()`), password fields without a value.
pub(crate) fn focused_element() -> BackendResult<Option<Element>> {
    uia::with_deadline(
        Duration::from_millis(1500),
        || -> BackendResult<Option<Element>> {
            let u = uia()?;
            // SAFETY: asks the OS for the focused element with the shared cache request.
            let Ok(el) = (unsafe { u.auto.GetFocusedElementBuildCache(&u.cache) }) else {
                return Ok(None);
            };
            let fallback = Some(wnd::foreground());
            let hwnd = top_level_of(u, &el, fallback).map_or(0, |h| h.0 as usize as u64);
            let element = build_element(hwnd, &el);
            remember(None, [(element.id.clone(), el)]);
            Ok(Some(element))
        },
    )?
}

// ------------------------------------------------------------------------------------ acting

fn pattern<T: windows::core::Interface>(
    el: &IUIAutomationElement,
    id: windows::Win32::UI::Accessibility::UIA_PATTERN_ID,
) -> Option<T> {
    // SAFETY: asks a live element for a pattern; an unsupported one is an error.
    unsafe { el.GetCurrentPatternAs::<T>(id) }.ok()
}

fn action_failed(what: &str, e: &windows::core::Error) -> BackendError {
    BackendError::Os(format!(
        "UIA action failed ({what}: {e}); outcome uncertain, inspect before retrying"
    ))
}

/// Do the element's primary action: Invoke, else Toggle, else Select, else Expand
/// (`_try_patterns`), and last the legacy default action when the element names one. `false` when it
/// has none of them, so the caller can click it instead. A failure while acting is an error.
fn require_action_time(deadline: Instant) -> BackendResult<()> {
    if Instant::now() >= deadline {
        Err(BackendError::Timeout)
    } else {
        Ok(())
    }
}

fn press(el: &IUIAutomationElement, deadline: Instant) -> BackendResult<bool> {
    if let Some(p) = pattern::<IUIAutomationInvokePattern>(el, UIA_InvokePatternId) {
        require_action_time(deadline)?;
        // SAFETY: invoking the pattern just fetched.
        return unsafe { p.Invoke() }
            .map(|()| true)
            .map_err(|e| action_failed("Invoke", &e));
    }
    if let Some(p) = pattern::<IUIAutomationTogglePattern>(el, UIA_TogglePatternId) {
        require_action_time(deadline)?;
        // SAFETY: toggling the pattern just fetched.
        return unsafe { p.Toggle() }
            .map(|()| true)
            .map_err(|e| action_failed("Toggle", &e));
    }
    if let Some(p) = pattern::<IUIAutomationSelectionItemPattern>(el, UIA_SelectionItemPatternId) {
        require_action_time(deadline)?;
        // SAFETY: selecting through the pattern just fetched.
        return unsafe { p.Select() }
            .map(|()| true)
            .map_err(|e| action_failed("Select", &e));
    }
    if let Some(p) = pattern::<IUIAutomationExpandCollapsePattern>(el, UIA_ExpandCollapsePatternId)
    {
        require_action_time(deadline)?;
        // SAFETY: expanding through the pattern just fetched.
        return unsafe { p.Expand() }
            .map(|()| true)
            .map_err(|e| action_failed("Expand", &e));
    }
    if let Some(p) =
        pattern::<IUIAutomationLegacyIAccessiblePattern>(el, UIA_LegacyIAccessiblePatternId)
    {
        // SAFETY: reading the legacy pattern's default action name.
        let named = unsafe { p.CurrentDefaultAction() }.is_ok_and(|a| !a.is_empty());
        if named {
            require_action_time(deadline)?;
            // SAFETY: running the default action the element itself named.
            return unsafe { p.DoDefaultAction() }
                .map(|()| true)
                .map_err(|e| action_failed("DoDefaultAction", &e));
        }
    }
    Ok(false)
}

/// Act on an element (`_try_patterns`, `_file_dialog`'s `SetFocus`).
pub(crate) fn perform(id: &ElementId, action: ElementAction) -> BackendResult<bool> {
    let id = id.clone();
    let deadline = Instant::now() + ELEMENT_TIMEOUT;
    let result = uia::with_deadline(ELEMENT_TIMEOUT, move || {
        let u = uia()?;
        let (el, _) = resolve(u, &id)?;
        // A slow resolution must not begin a new mutation after the caller gave up.
        if Instant::now() >= deadline {
            return Err(BackendError::Timeout);
        }
        match action {
            ElementAction::Press => press(&el, deadline),
            ElementAction::Focus => {
                // SAFETY: gives keyboard focus to a live element.
                unsafe { el.SetFocus() }
                    .map(|()| true)
                    .map_err(|e| os_err("could not focus the element", &e))
            }
            ElementAction::SetValue(text) => {
                let Some(p) = pattern::<IUIAutomationValuePattern>(&el, UIA_ValuePatternId) else {
                    return Err(BackendError::Unsupported(
                        "the element has no settable value".into(),
                    ));
                };
                require_action_time(deadline)?;
                // SAFETY: sets the value through the pattern just fetched.
                unsafe { p.SetValue(&BSTR::from(text.as_str())) }
                    .map(|()| true)
                    .map_err(|e| action_failed("SetValue", &e))
            }
        }
    });
    match result {
        // UIA cannot cancel an action already sent to another process. A retry could repeat it.
        Err(BackendError::Timeout) => Err(BackendError::Os(
            "UI Automation action timed out; outcome uncertain, inspect before retrying".into(),
        )),
        other => other?,
    }
}

// ------------------------------------------------------------------------------------ file dialog

/// The "File name" edit of a standard Save As / Open dialog in `window`. A UIA search for Edit
/// elements, then the name test (`computer._file_dialog`'s WalkControl to depth 12).
pub(crate) fn find_file_dialog_field(window: WindowRef) -> BackendResult<Option<ElementId>> {
    let raw = window.raw();
    uia::with_deadline(
        Duration::from_millis(1500),
        move || -> BackendResult<Option<ElementId>> {
            let u = uia()?;
            let hwnd = hwnd_of(WindowRef::from_raw(raw));
            if !wnd::is_window(hwnd) {
                return Ok(None);
            }
            // SAFETY: a condition and a search of a window that exists.
            let found = unsafe {
                let Ok(root) = u.auto.ElementFromHandle(hwnd) else {
                    return Ok(None);
                };
                let cond = u
                    .auto
                    .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(ct::EDIT))
                    .map_err(|e| os_err("UI Automation condition", &e))?;
                match root.FindAllBuildCache(TreeScope_Descendants, &cond, &u.cache) {
                    Ok(f) => f,
                    Err(_) => return Ok(None),
                }
            };
            // SAFETY: indexes inside the array just returned.
            let n = unsafe { found.Length() }.unwrap_or(0);
            for i in 0..n {
                // SAFETY: as above.
                let Ok(el) = (unsafe { found.GetElement(i) }) else {
                    continue;
                };
                let props = read_cached(&el);
                if model::is_file_name_label(&props.name) {
                    let top = wnd::root_hwnd(hwnd).unwrap_or(hwnd);
                    let element = build_element(top.0 as usize as u64, &el);
                    remember(None, [(element.id.clone(), el)]);
                    return Ok(Some(element.id));
                }
            }
            Ok(None)
        },
    )?
}

// ------------------------------------------------------------------------------------ menus

/// MenuItem (and, inside a XAML popup, Button: Windows 11's Cut/Copy/Rename icon row) elements under
/// `parent`, a few levels deep (`_menu_items_under`). Returns whether any was a real MenuItem.
struct MenuScan<'a> {
    u: &'a Uia,
    deadline: Instant,
    found: Vec<(MenuItem, bool)>,
}

impl MenuScan<'_> {
    fn under(&mut self, parent: &IUIAutomationElement, depth: u32) {
        if depth > 7 || Instant::now() > self.deadline {
            return;
        }
        // SAFETY: a child query on a live element with the shared cache request.
        let Ok(kids) = (unsafe {
            parent.FindAllBuildCache(TreeScope_Children, &self.u.true_cond, &self.u.cache)
        }) else {
            return;
        };
        // SAFETY: reading the length of the array just returned.
        let n = unsafe { kids.Length() }.unwrap_or(0);
        for i in 0..n {
            if Instant::now() > self.deadline {
                break;
            }
            // SAFETY: an index inside the array's length.
            let Ok(child) = (unsafe { kids.GetElement(i) }) else {
                continue;
            };
            let p = read_cached(&child);
            let name = p.name.trim().to_string();
            let is_item = p.control_type == ct::MENU_ITEM;
            if (is_item || p.control_type == ct::BUTTON) && !name.is_empty() {
                self.found.push((
                    MenuItem {
                        name,
                        rect: p.rect.unwrap_or_default(),
                    },
                    is_item,
                ));
                if is_item {
                    // an open submenu is its own popup; do not descend into items
                    continue;
                }
            }
            self.under(&child, depth + 1);
        }
    }
}

/// The items of the open context menu, within `budget`: popup windows first, then the desktop's
/// direct children that look like menus (`_menu_items_once`).
pub(crate) fn menu_items(budget: Duration) -> BackendResult<Vec<MenuItem>> {
    let popups: Vec<u64> = wnd::popup_menus().into_iter().map(WindowRef::raw).collect();
    let deadline = Instant::now() + budget;
    // The scan checks the deadline between branches; this is the stop for a query stuck in one.
    let hard = budget + Duration::from_millis(400);
    let answer = uia::with_deadline(hard, move || -> BackendResult<Vec<MenuItem>> {
        let u = uia()?;
        for raw in popups {
            let hwnd = hwnd_of(WindowRef::from_raw(raw));
            // SAFETY: the popup window was just listed; a vanished one is an error and is skipped.
            let Ok(root) = (unsafe { u.auto.ElementFromHandle(hwnd) }) else {
                continue;
            };
            let mut scan = MenuScan {
                u,
                deadline,
                found: Vec::new(),
            };
            scan.under(&root, 0);
            if scan.found.iter().any(|(_, is_item)| *is_item) {
                return Ok(scan.found.into_iter().map(|(m, _)| m).collect());
            }
        }
        desktop_menus(u, deadline)
    });
    match answer {
        Ok(items) => items,
        Err(BackendError::Timeout) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// A menu drawn in a plain top-level window (Chromium and Electron do): the desktop's children of type
/// Menu, Pane or Window whose own children include named MenuItems, topmost window first.
fn desktop_menus(u: &Uia, deadline: Instant) -> BackendResult<Vec<MenuItem>> {
    // SAFETY: the desktop element and its direct children with the shared cache request.
    let tops = unsafe {
        let root = u
            .auto
            .GetRootElement()
            .map_err(|e| os_err("UI Automation root", &e))?;
        root.FindAllBuildCache(TreeScope_Children, &u.true_cond, &u.cache)
            .map_err(|e| os_err("UI Automation desktop", &e))?
    };
    // SAFETY: reading the length of the array just returned.
    let n = unsafe { tops.Length() }.unwrap_or(0);
    for i in 0..n {
        if Instant::now() > deadline {
            break;
        }
        // SAFETY: an index inside the array's length.
        let Ok(top) = (unsafe { tops.GetElement(i) }) else {
            continue;
        };
        let p = read_cached(&top);
        if !matches!(p.control_type, ct::MENU | ct::PANE | ct::WINDOW) {
            continue;
        }
        // A window with no box (minimised, parked off screen) cannot hold an open menu.
        if p.rect.is_none_or(|r| r.is_empty() || r.left <= -30000) {
            continue;
        }
        // SAFETY: a child query on a live element with the shared cache request.
        let Ok(kids) =
            (unsafe { top.FindAllBuildCache(TreeScope_Children, &u.true_cond, &u.cache) })
        else {
            continue;
        };
        let mut items = Vec::new();
        // SAFETY: reading the length of the array just returned.
        let m = unsafe { kids.Length() }.unwrap_or(0);
        for j in 0..m {
            if Instant::now() > deadline {
                break;
            }
            // SAFETY: an index inside the array's length.
            let Ok(kid) = (unsafe { kids.GetElement(j) }) else {
                continue;
            };
            let kp = read_cached(&kid);
            let name = kp.name.trim();
            if kp.control_type == ct::MENU_ITEM && !name.is_empty() {
                items.push(MenuItem {
                    name: name.to_string(),
                    rect: kp.rect.unwrap_or_default(),
                });
            }
        }
        if !items.is_empty() {
            return Ok(items);
        }
    }
    Ok(Vec::new())
}
