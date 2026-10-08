//! UI Automation plumbing: the apartment every UIA call runs in, deadlines for calls that can hang,
//! the one shared `IUIAutomation` with its cache request, cached-property reads, and the id cache that
//! turns an [`ElementId`] back into a live element on whatever thread asks.
//!
//! COM apartment. `util::ensure_com` makes an STA, which the shell objects in `launch.rs` want. UI
//! Automation clients are meant to live in the multithreaded apartment: from an STA the calls
//! marshal through the thread's message pump, and an element fetched there is dead on any other
//! thread (Python lost refs between daemon threads that way, `core._refind_element`). So every UIA
//! call here runs in the MTA: on the calling thread when that thread is (or can become) an MTA
//! thread, otherwise on a short-lived helper thread. UIA objects are then free-threaded and the one
//! `IUIAutomation`, the cache request and the live elements in the id cache are shared by all threads.
//!
//! Python reference: `core.py:_snapshot_batched` (the cache request), `_fresh_element`,
//! `_refind_element`.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use wh_core::backend::{BackendError, BackendResult, Element, ElementId, Point, Rect};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationCondition,
    IUIAutomationElement, IUIAutomationTreeWalker, TreeScope_Descendants,
    UIA_BoundingRectanglePropertyId, UIA_ControlTypePropertyId,
    UIA_ExpandCollapseExpandCollapseStatePropertyId, UIA_HasKeyboardFocusPropertyId,
    UIA_IsEnabledPropertyId, UIA_IsExpandCollapsePatternAvailablePropertyId,
    UIA_IsInvokePatternAvailablePropertyId, UIA_IsOffscreenPropertyId, UIA_IsPasswordPropertyId,
    UIA_IsSelectionItemPatternAvailablePropertyId, UIA_IsTextPatternAvailablePropertyId,
    UIA_IsTogglePatternAvailablePropertyId, UIA_IsValuePatternAvailablePropertyId,
    UIA_NamePropertyId, UIA_PROPERTY_ID, UIA_SelectionItemIsSelectedPropertyId,
    UIA_ToggleToggleStatePropertyId, UIA_ValueValuePropertyId,
};
use windows::core::BSTR;

use crate::model::{self, IdParts, Patterns, Props};
use crate::util::{hwnd_of, os_err};
use crate::wnd;

// ------------------------------------------------------------------------------------ sharing

/// A COM object that is safe to hand between threads because every UIA object here lives in the
/// multithreaded apartment (see the module docs).
pub(crate) struct Shared<T>(pub T);

// SAFETY: only ever wraps UIA objects created and used in the MTA, which are free-threaded.
unsafe impl<T> Send for Shared<T> {}
// SAFETY: as above; shared references only call free-threaded UIA methods.
unsafe impl<T> Sync for Shared<T> {}

// ------------------------------------------------------------------------------------ apartment

#[derive(Clone, Copy, PartialEq, Eq)]
enum Apartment {
    Unknown,
    Mta,
    Other,
}

thread_local! {
    static APARTMENT: Cell<Apartment> = const { Cell::new(Apartment::Unknown) };
}

/// Is the calling thread in the MTA, joining it if the thread has no COM apartment yet?
fn this_thread_is_mta() -> bool {
    APARTMENT.with(|a| {
        if a.get() == Apartment::Unknown {
            // SAFETY: plain COM init on this thread; S_FALSE means it already is in the MTA, and
            // RPC_E_CHANGED_MODE means it was made an STA earlier (then a helper thread is used).
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            a.set(if hr.is_ok() {
                Apartment::Mta
            } else {
                Apartment::Other
            });
        }
        a.get() == Apartment::Mta
    })
}

/// Run `f` in the MTA: here when this thread is an MTA thread, else on a helper thread that is, and
/// wait for it. No time limit; use [`with_deadline`] for calls that can hang.
pub(crate) fn on_mta<R: Send>(f: impl FnOnce() -> R + Send) -> BackendResult<R> {
    if this_thread_is_mta() {
        return Ok(f());
    }
    thread::scope(|s| {
        thread::Builder::new()
            .name("wh-uia".into())
            .spawn_scoped(s, || {
                let _ = this_thread_is_mta();
                f()
            })
            .map_err(|e| BackendError::Os(format!("could not start a UI Automation thread: {e}")))?
            .join()
            .map_err(|_| BackendError::Os("the UI Automation worker panicked".into()))
    })
}

/// Helper threads that have not finished. A hung app can leave a few; past this many new calls
/// answer `Timeout` at once instead of piling on more.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
const MAX_IN_FLIGHT: usize = 24;

struct FlightGuard;

impl Drop for FlightGuard {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Run `f` in the MTA on its own thread and wait at most `budget`. A call that is still running when
/// the budget ends is left to finish by itself (UIA calls cannot be cancelled) and its answer is
/// dropped: `Err(Timeout)`.
pub(crate) fn with_deadline<R: Send + 'static>(
    budget: Duration,
    f: impl FnOnce() -> R + Send + 'static,
) -> BackendResult<R> {
    if IN_FLIGHT.fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        return Err(BackendError::Timeout);
    }
    let (tx, rx) = mpsc::channel();
    let spawned = thread::Builder::new().name("wh-uia".into()).spawn(move || {
        let _flight = FlightGuard;
        let _ = this_thread_is_mta();
        // The receiver may have given up already; then nobody wants the answer.
        let _ = tx.send(f());
    });
    if let Err(e) = spawned {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        return Err(BackendError::Os(format!(
            "could not start a UI Automation thread: {e}"
        )));
    }
    match rx.recv_timeout(budget) {
        Ok(v) => Ok(v),
        Err(RecvTimeoutError::Timeout) => Err(BackendError::Timeout),
        Err(RecvTimeoutError::Disconnected) => {
            Err(BackendError::Os("the UI Automation worker failed".into()))
        }
    }
}

// ------------------------------------------------------------------------------------ the client

/// The shared UI Automation client.
pub(crate) struct Uia {
    pub auto: IUIAutomation,
    /// Every property a snapshot prints, cached with the element (`_snapshot_batched`'s CacheRequest).
    pub cache: IUIAutomationCacheRequest,
    pub true_cond: IUIAutomationCondition,
    /// The raw view: Python's `uiautomation` package walked it (`GetChildren`, `GetParentControl`).
    pub raw: IUIAutomationTreeWalker,
}

static UIA: OnceLock<Result<Shared<Uia>, String>> = OnceLock::new();

/// The properties cached with every element a read returns. Names, boxes and states come back in the
/// one cross-process query; the `Is<Pattern>Available` ones say what an element can do without
/// fetching pattern objects.
const CACHED: [UIA_PROPERTY_ID; 17] = [
    UIA_NamePropertyId,
    UIA_ControlTypePropertyId,
    UIA_IsEnabledPropertyId,
    UIA_BoundingRectanglePropertyId,
    UIA_IsOffscreenPropertyId,
    UIA_IsPasswordPropertyId,
    UIA_HasKeyboardFocusPropertyId,
    UIA_IsInvokePatternAvailablePropertyId,
    UIA_IsValuePatternAvailablePropertyId,
    UIA_IsTogglePatternAvailablePropertyId,
    UIA_IsSelectionItemPatternAvailablePropertyId,
    UIA_IsExpandCollapsePatternAvailablePropertyId,
    UIA_IsTextPatternAvailablePropertyId,
    UIA_ValueValuePropertyId,
    UIA_ToggleToggleStatePropertyId,
    UIA_SelectionItemIsSelectedPropertyId,
    UIA_ExpandCollapseExpandCollapseStatePropertyId,
];

fn create() -> windows::core::Result<Uia> {
    // SAFETY: CUIAutomation is the documented in-process class; called from an MTA thread.
    let auto: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }?;
    // SAFETY: the calls below only configure objects this function owns.
    unsafe {
        let cache = auto.CreateCacheRequest()?;
        for id in CACHED {
            cache.AddProperty(id)?;
        }
        let true_cond = auto.CreateTrueCondition()?;
        let raw = auto.RawViewWalker()?;
        Ok(Uia {
            auto,
            cache,
            true_cond,
            raw,
        })
    }
}

/// The client, created on first use. Call it only from code running in the MTA (inside
/// [`on_mta`] or [`with_deadline`]).
pub(crate) fn uia() -> BackendResult<&'static Uia> {
    let slot = UIA.get_or_init(|| create().map(Shared).map_err(|e| e.to_string()));
    match slot {
        Ok(shared) => Ok(&shared.0),
        Err(why) => Err(BackendError::Os(format!(
            "UI Automation is not available: {why}"
        ))),
    }
}

// ------------------------------------------------------------------------------------ small reads

pub(crate) fn win_point(p: Point) -> POINT {
    POINT { x: p.x, y: p.y }
}

fn cached_value(e: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<VARIANT> {
    // SAFETY: `e` is a live element; `id` was added to the cache request that produced it.
    unsafe { e.GetCachedPropertyValueEx(id, true) }.ok()
}

fn cached_bool(e: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> bool {
    cached_value(e, id)
        .and_then(|v| bool::try_from(&v).ok())
        .unwrap_or(false)
}

fn cached_int(e: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<i32> {
    cached_value(e, id).and_then(|v| i32::try_from(&v).ok())
}

fn cached_string(e: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<String> {
    cached_value(e, id)
        .and_then(|v| BSTR::try_from(&v).ok())
        .map(|b| b.to_string())
}

/// Everything a snapshot row needs from an element built with the shared cache request.
pub(crate) fn read_cached(e: &IUIAutomationElement) -> Props {
    // SAFETY: `e` is a live element built with the shared cache request, so these cached reads are
    // local to this process.
    let (control_type, name, rect, enabled, offscreen, password, focused) = unsafe {
        (
            e.CachedControlType().map_or(0, |c| c.0),
            e.CachedName().map(|b| b.to_string()).unwrap_or_default(),
            e.CachedBoundingRectangle()
                .ok()
                .map(|r| Rect::new(r.left, r.top, r.right, r.bottom)),
            e.CachedIsEnabled().is_ok_and(|b| b.as_bool()),
            e.CachedIsOffscreen().is_ok_and(|b| b.as_bool()),
            e.CachedIsPassword().is_ok_and(|b| b.as_bool()),
            e.CachedHasKeyboardFocus().is_ok_and(|b| b.as_bool()),
        )
    };
    let patterns = Patterns {
        invoke: cached_bool(e, UIA_IsInvokePatternAvailablePropertyId),
        value: cached_bool(e, UIA_IsValuePatternAvailablePropertyId),
        toggle: cached_bool(e, UIA_IsTogglePatternAvailablePropertyId),
        select: cached_bool(e, UIA_IsSelectionItemPatternAvailablePropertyId),
        expand: cached_bool(e, UIA_IsExpandCollapsePatternAvailablePropertyId),
        text: cached_bool(e, UIA_IsTextPatternAvailablePropertyId),
    };
    Props {
        control_type,
        name,
        rect,
        enabled,
        offscreen,
        password,
        focused,
        value: if patterns.value && !password {
            cached_string(e, UIA_ValueValuePropertyId)
        } else {
            None
        },
        toggle: if patterns.toggle {
            cached_int(e, UIA_ToggleToggleStatePropertyId)
        } else {
            None
        },
        selected: patterns.select && cached_bool(e, UIA_SelectionItemIsSelectedPropertyId),
        expand: if patterns.expand {
            cached_int(e, UIA_ExpandCollapseExpandCollapseStatePropertyId)
        } else {
            None
        },
        patterns,
    }
}

/// The element and its props as the neutral [`Element`], under the window `hwnd` it was read in.
pub(crate) fn build_element(hwnd: u64, e: &IUIAutomationElement) -> Element {
    let p = read_cached(e);
    let id = model::encode_id(&IdParts {
        hwnd,
        control_type: p.control_type,
        rect: p.rect.unwrap_or_default(),
        name: p.name.clone(),
    });
    model::element_from_props(id, &p)
}

/// The `HWND` a UIA element is, 0 when it is not a window of its own.
fn native_window(e: &IUIAutomationElement) -> HWND {
    // SAFETY: plain property read on a live element.
    unsafe { e.CurrentNativeWindowHandle() }.unwrap_or_default()
}

/// The top-level window an element lives in: its own window handle or the nearest ancestor's, made a
/// root; `fallback` when none is found within a few steps (`GetTopLevelControl().NativeWindowHandle`).
pub(crate) fn top_level_of(
    u: &Uia,
    e: &IUIAutomationElement,
    fallback: Option<HWND>,
) -> Option<HWND> {
    let mut cur = e.clone();
    for _ in 0..12 {
        let h = native_window(&cur);
        if !h.0.is_null() {
            return wnd::root_hwnd(h).or(Some(h));
        }
        // SAFETY: walking to the parent of a live element.
        match unsafe { u.raw.GetParentElement(&cur) } {
            Ok(parent) => cur = parent,
            Err(_) => break,
        }
    }
    fallback
}

// ------------------------------------------------------------------------------------ id cache

/// Live elements the last reads returned, by id, so a click on a ref does not search the window
/// again. Bounded; an entry that has gone stale is found again by name, type and position.
#[derive(Default)]
struct ElementCache {
    map: HashMap<String, (u64, u64, Shared<IUIAutomationElement>)>,
    seq: u64,
}

const CACHE_LIMIT: usize = 1500;

static ELEMENTS: Mutex<Option<ElementCache>> = Mutex::new(None);

fn with_cache<R>(f: impl FnOnce(&mut ElementCache) -> R) -> R {
    let mut guard = ELEMENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(ElementCache::default))
}

/// One cache entry: the window it was read under, when, and the live element.
type Entry = (u64, u64, Shared<IUIAutomationElement>);

/// Remember live elements. With `replace_window`, the entries of that window go first: a fresh read
/// of a window supersedes the last one, and its refs.
pub(crate) fn remember(
    replace_window: Option<u64>,
    items: impl IntoIterator<Item = (ElementId, IUIAutomationElement)>,
) {
    // Dropping a UIA element can call into the app that owns it, so what is evicted is dropped after
    // the lock is released, not while other threads wait for it.
    let evicted: Vec<Entry> = with_cache(|c| {
        let mut gone = Vec::new();
        if let Some(w) = replace_window {
            let stale: Vec<String> = c
                .map
                .iter()
                .filter(|(_, (hwnd, _, _))| *hwnd == w)
                .map(|(k, _)| k.clone())
                .collect();
            gone.extend(stale.iter().filter_map(|k| c.map.remove(k)));
        }
        for (id, el) in items {
            let Some(parts) = model::decode_id(&id) else {
                continue;
            };
            c.seq += 1;
            let entry = (parts.hwnd, c.seq, Shared(el));
            gone.extend(c.map.insert(id.0, entry));
        }
        if c.map.len() > CACHE_LIMIT {
            // drop the older half
            let mut seqs: Vec<u64> = c.map.values().map(|(_, s, _)| *s).collect();
            seqs.sort_unstable();
            let cut = seqs[seqs.len() / 2];
            let old: Vec<String> = c
                .map
                .iter()
                .filter(|(_, (_, s, _))| *s <= cut)
                .map(|(k, _)| k.clone())
                .collect();
            gone.extend(old.iter().filter_map(|k| c.map.remove(k)));
        }
        gone
    });
    drop(evicted);
}

fn remembered(id: &ElementId) -> Option<IUIAutomationElement> {
    with_cache(|c| c.map.get(&id.0).map(|(_, _, e)| e.0.clone()))
}

// ------------------------------------------------------------------------------------ resolving ids

/// The live element an id means, and what the id remembered. First the cached element if it still
/// answers with the same name; else a search of the window by control type and name, the nearest to
/// where it was (`_refind_element`). `Err(ElementGone)` when it is not there.
pub(crate) fn resolve(u: &Uia, id: &ElementId) -> BackendResult<(IUIAutomationElement, IdParts)> {
    let parts = model::decode_id(id).ok_or(BackendError::ElementGone)?;
    if let Some(el) = remembered(id) {
        // SAFETY: plain property read on an element that may have died; the error says so.
        let alive = unsafe { el.CurrentName() }.is_ok_and(|n| n == parts.name.as_str());
        if alive {
            return Ok((el, parts));
        }
    }
    let hwnd = hwnd_of(wh_core::backend::WindowRef::from_raw(parts.hwnd));
    if !wnd::is_window(hwnd) {
        return Err(BackendError::ElementGone);
    }
    // SAFETY: building conditions and searching a window that was just checked to exist.
    let found = unsafe {
        let root = u
            .auto
            .ElementFromHandle(hwnd)
            .map_err(|_| BackendError::ElementGone)?;
        let by_type = u
            .auto
            .CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &VARIANT::from(parts.control_type),
            )
            .map_err(|e| os_err("UI Automation condition", &e))?;
        let cond = if parts.name.is_empty() {
            by_type
        } else {
            let by_name = u
                .auto
                .CreatePropertyCondition(UIA_NamePropertyId, &VARIANT::from(parts.name.as_str()))
                .map_err(|e| os_err("UI Automation condition", &e))?;
            u.auto
                .CreateAndCondition(&by_type, &by_name)
                .map_err(|e| os_err("UI Automation condition", &e))?
        };
        root.FindAllBuildCache(TreeScope_Descendants, &cond, &u.cache)
            .map_err(|_| BackendError::ElementGone)?
    };
    // SAFETY: reading the cached boxes of the elements just returned.
    let (els, rects) = unsafe {
        let n = found.Length().unwrap_or(0);
        let mut els = Vec::new();
        let mut rects = Vec::new();
        for i in 0..n {
            if let Ok(e) = found.GetElement(i) {
                let r = e
                    .CachedBoundingRectangle()
                    .map(|r| Rect::new(r.left, r.top, r.right, r.bottom))
                    .unwrap_or_default();
                els.push(e);
                rects.push(r);
            }
        }
        (els, rects)
    };
    let pick = model::pick_refind(parts.rect, &rects).ok_or(BackendError::ElementGone)?;
    let el = els.into_iter().nth(pick).ok_or(BackendError::ElementGone)?;
    remember(None, [(id.clone(), el.clone())]);
    Ok((el, parts))
}

/// A fresh, cached read of an element's properties (one cross-process query).
pub(crate) fn refresh(u: &Uia, el: &IUIAutomationElement) -> BackendResult<IUIAutomationElement> {
    // SAFETY: re-caching the shared property set on a live element.
    unsafe { el.BuildUpdatedCache(&u.cache) }.map_err(|_| BackendError::ElementGone)
}

/// Has the window's accessibility tree a first child yet? A stuck app counts as "not yet".
pub(crate) fn tree_started(hwnd: HWND) -> bool {
    let raw = hwnd.0 as usize;
    with_deadline(Duration::from_millis(300), move || {
        let u = uia().ok()?;
        let hwnd = HWND(raw as *mut std::ffi::c_void);
        // SAFETY: asking the window's UIA root for its first child.
        unsafe {
            let root = u.auto.ElementFromHandle(hwnd).ok()?;
            u.raw.GetFirstChildElement(&root).ok()
        }
        .map(|_| ())
    })
    .ok()
    .flatten()
    .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn a_fresh_thread_runs_uia_work_on_itself() {
        thread::spawn(|| {
            let here = thread::current().id();
            let ran_on = on_mta(|| thread::current().id()).unwrap();
            assert_eq!(ran_on, here, "an MTA-capable thread needs no helper");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn an_sta_thread_runs_uia_work_on_a_helper() {
        thread::spawn(|| {
            // the shell objects in launch.rs make the thread an STA
            crate::util::ensure_com();
            let here = thread::current().id();
            let ran_on = on_mta(|| thread::current().id()).unwrap();
            assert_ne!(
                ran_on, here,
                "an STA thread must hand the work to an MTA one"
            );
            // and a result still comes back
            assert_eq!(on_mta(|| 41 + 1).unwrap(), 42);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_call_inside_its_budget_returns_its_answer() {
        assert_eq!(
            with_deadline(Duration::from_secs(5), || "done").unwrap(),
            "done"
        );
    }

    #[test]
    fn a_call_past_its_budget_times_out_and_is_left_to_finish() {
        let started = std::time::Instant::now();
        let r = with_deadline(Duration::from_millis(30), || {
            sleep(Duration::from_millis(400));
            7
        });
        assert_eq!(r, Err(BackendError::Timeout));
        assert!(started.elapsed() < Duration::from_millis(300));
    }

    #[test]
    fn a_panicking_worker_is_an_error_not_a_crash() {
        let r = with_deadline(Duration::from_secs(5), || -> u8 { panic!("boom") });
        assert!(matches!(r, Err(BackendError::Os(_))), "{r:?}");
    }
}
