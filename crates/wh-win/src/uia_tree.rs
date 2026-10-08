//! `element_tree`: read a window's accessibility tree under `TreeLimits`.
//!
//! Two ways to read, both with every printed property cached so no element costs a round trip of its
//! own (Python `core.py:_snapshot_batched`, 49 ms for 160 items against 157 ms for the old walk):
//!
//! * batched: one `FindAllBuildCache(Descendants)` for the whole tree, then the cached answers are
//!   filtered and capped in memory. Fastest for a full read; the query itself cannot be cut short.
//! * walk: depth first, one `FindAllBuildCache(Children)` per branch. Checks the deadline and the item
//!   cap between branches, so it can stop early and hand back what it has (time-boxed text lookups,
//!   tiny probes such as the settle floor's "does this window expose UIA at all", nested output).
//!
//! An empty Chromium tree is woken the way Python did: walk two levels (it builds its tree once a
//! client has walked it), and for a window that is still empty and covered, raise it, wait for the
//! tree, and hand focus back, but only when the caller allowed it.

use std::thread::sleep;
use std::time::{Duration, Instant};

use wh_core::backend::{
    BackendError, BackendResult, Element, ElementFilter, ElementId, TreeLimits, TreeSnapshot,
    WakePolicy, WindowRef,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Accessibility::{
    IUIAutomationElement, TreeScope_Children, TreeScope_Descendants,
};

use crate::model::{self, Budget};
use crate::uia::{self, Shared, Uia, build_element, uia};
use crate::util::{hwnd_of, os_err, window_ref};
use crate::wnd;

/// At or below this many wanted items the walk is used: it stops as soon as it has them, where the
/// batched query always reads the whole window. Measured on this PC (7 runs each, median): Hermes
/// (667 interactive elements) 120 items 139 ms walk against 385 ms batched, ChatGPT 58 against 208.
/// A full read is 20 to 30 percent slower walking (Hermes 491 ms against 412 ms), so unbounded reads
/// stay batched. Both visit the same elements in the same order.
const WALK_BELOW_ITEMS: usize = 400;

/// A window whose read finds this few kept elements or fewer may be an empty Chromium tree
/// (`len(items) <= 3`: the three title-bar buttons).
const EMPTY_TREE_ITEMS: usize = 3;

/// How a read gets at the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Strategy {
    Batched,
    Walk,
}

/// Which way to read under these limits.
pub(crate) fn choose_strategy(limits: &TreeLimits) -> Strategy {
    if limits.filter == ElementFilter::Named
        || !limits.flatten
        || limits.max_items <= WALK_BELOW_ITEMS
    {
        Strategy::Walk
    } else {
        Strategy::Batched
    }
}

/// How long to wait for the helper in total. The deadline is cooperative (checked between
/// branches), so this is the hard stop for a query that cannot be interrupted.
pub(crate) fn hard_limit(limits: &TreeLimits) -> Duration {
    let base = limits.deadline.max(Duration::from_millis(500));
    let wake = if limits.wake == WakePolicy::IfEmpty {
        Duration::from_secs(3)
    } else {
        Duration::ZERO
    };
    base * 2 + wake
}

/// What a read produced.
struct Items {
    items: Vec<Element>,
    truncated: bool,
    held: Vec<(ElementId, Shared<IUIAutomationElement>)>,
}

/// The answer of one helper run.
struct Read {
    title: String,
    root: Element,
    items: Vec<Element>,
    truncated: bool,
    wake_hint: bool,
}

pub(crate) fn element_tree(window: WindowRef, limits: &TreeLimits) -> BackendResult<TreeSnapshot> {
    if !wnd::is_window(hwnd_of(window)) {
        return Err(BackendError::WindowGone);
    }
    let limits = *limits;
    let raw = window.raw();
    let read = uia::with_deadline(hard_limit(&limits), move || read_window(raw, &limits))??;
    let Read {
        title,
        mut root,
        items,
        truncated,
        wake_hint,
    } = read;
    root.children = items;
    Ok(TreeSnapshot {
        window,
        title,
        root,
        truncated,
        wake_hint,
    })
}

fn read_window(raw: u64, limits: &TreeLimits) -> BackendResult<Read> {
    let u = uia()?;
    let hwnd = hwnd_of(WindowRef::from_raw(raw));
    // SAFETY: ElementFromHandle on a window that was just checked to exist.
    let root_el = unsafe { u.auto.ElementFromHandleBuildCache(hwnd, &u.cache) }.map_err(|e| {
        if wnd::is_window(hwnd) {
            os_err("UI Automation cannot read this window", &e)
        } else {
            BackendError::WindowGone
        }
    })?;
    let root = build_element(raw, &root_el);
    let title = if root.name.is_empty() {
        wnd::title_of(hwnd)
    } else {
        root.name.clone()
    };
    let strategy = choose_strategy(limits);
    let mut items = read_items(u, &root_el, raw, limits, strategy)?;
    let mut wake_hint = false;
    if limits.filter == ElementFilter::Interactive
        && items.items.len() <= EMPTY_TREE_ITEMS
        && wnd::is_chromium(hwnd)
    {
        // Chromium builds its tree only after a client has walked it once.
        wake_walk(u, &root_el);
        if let Ok(again) = read_items(u, &root_el, raw, limits, strategy) {
            items = again;
        }
        if items.items.len() <= EMPTY_TREE_ITEMS {
            if limits.wake == WakePolicy::IfEmpty {
                if let Some(woken) = wake_by_raise(u, &root_el, raw, limits) {
                    items = woken;
                }
            } else {
                // Only worth a hint when raising would change something: a window already in front
                // and still empty has nothing to wake.
                wake_hint = !is_front(hwnd);
            }
        }
    }
    if limits.filter == ElementFilter::Interactive {
        // A fresh read of a window supersedes its last one: these are the elements its refs mean.
        // Done here, in the MTA, where the live elements were made and are safe to hold.
        uia::remember(Some(raw), items.held.into_iter().map(|(id, el)| (id, el.0)));
    }
    Ok(Read {
        title,
        root,
        items: items.items,
        truncated: items.truncated,
        wake_hint,
    })
}

fn read_items(
    u: &Uia,
    root: &IUIAutomationElement,
    raw: u64,
    limits: &TreeLimits,
    strategy: Strategy,
) -> BackendResult<Items> {
    match strategy {
        Strategy::Batched => {
            read_batched(u, root, raw, limits).or_else(|_| read_walk(u, root, raw, limits))
        }
        Strategy::Walk => read_walk(u, root, raw, limits),
    }
}

// ------------------------------------------------------------------------------------ batched

fn read_batched(
    u: &Uia,
    root: &IUIAutomationElement,
    raw: u64,
    limits: &TreeLimits,
) -> BackendResult<Items> {
    // SAFETY: one search of a live window's descendants with the shared cache request.
    let found = unsafe { root.FindAllBuildCache(TreeScope_Descendants, &u.true_cond, &u.cache) }
        .map_err(|e| os_err("UI Automation search failed", &e))?;
    // SAFETY: reading the length of the array just returned.
    let n = unsafe { found.Length() }.map_err(|e| os_err("UI Automation search failed", &e))?;
    // The query is done; this only converts cached answers, so give it its own, fresh budget.
    let mut budget = Budget::new(
        limits.max_items,
        Instant::now() + limits.deadline.max(Duration::from_secs(1)),
    );
    let nodes = (0..n).map(|i| {
        // SAFETY: an index inside the array's length.
        unsafe { found.GetElement(i) }
            .ok()
            .map(|e| (build_element(raw, &e), e))
    });
    let kept = model::select_flat(nodes, limits.filter, &mut budget);
    let mut items = Vec::with_capacity(kept.len());
    let mut held = Vec::with_capacity(kept.len());
    for (el, live) in kept {
        held.push((el.id.clone(), Shared(live)));
        items.push(el);
    }
    Ok(Items {
        items,
        truncated: budget.truncated(),
        held,
    })
}

// ------------------------------------------------------------------------------------ walk

struct Walk<'a> {
    u: &'a Uia,
    raw: u64,
    filter: ElementFilter,
    flatten: bool,
    max_depth: u32,
    budget: Budget,
    stopped: bool,
    flat: Vec<Element>,
    held: Vec<(ElementId, Shared<IUIAutomationElement>)>,
}

impl Walk<'_> {
    /// Visit `parent`'s children depth first. Flattened reads push into `self.flat` in document
    /// order and return nothing; nested reads return the kept subtrees, with the children of a
    /// dropped element lifted to the nearest kept ancestor.
    fn visit(&mut self, parent: &IUIAutomationElement, depth: u32) -> Vec<Element> {
        let mut here = Vec::new();
        if depth >= self.max_depth {
            return here;
        }
        // SAFETY: one query for a live element's direct children with the shared cache request.
        let Ok(kids) = (unsafe {
            parent.FindAllBuildCache(TreeScope_Children, &self.u.true_cond, &self.u.cache)
        }) else {
            return here;
        };
        // SAFETY: reading the length of the array just returned.
        let n = unsafe { kids.Length() }.unwrap_or(0);
        for i in 0..n {
            if self.stopped || !self.budget.may_continue() {
                self.stopped = true;
                break;
            }
            // SAFETY: an index inside the array's length.
            let Ok(child) = (unsafe { kids.GetElement(i) }) else {
                continue;
            };
            let mut el = build_element(self.raw, &child);
            let keep = model::keeps(self.filter, &el);
            if keep {
                self.budget.kept_one();
                self.held.push((el.id.clone(), Shared(child.clone())));
            }
            if keep && self.flatten {
                self.flat.push(el);
                self.visit(&child, depth + 1);
            } else {
                let sub = self.visit(&child, depth + 1);
                if keep {
                    el.children = sub;
                    here.push(el);
                } else {
                    here.extend(sub);
                }
            }
        }
        here
    }
}

fn read_walk(
    u: &Uia,
    root: &IUIAutomationElement,
    raw: u64,
    limits: &TreeLimits,
) -> BackendResult<Items> {
    let mut walk = Walk {
        u,
        raw,
        filter: limits.filter,
        flatten: limits.flatten,
        // Python's walk expanded depths 0 through max_depth below the window (`if depth > 25: return`).
        max_depth: limits.max_depth.saturating_add(1),
        budget: Budget::new(limits.max_items, Instant::now() + limits.deadline),
        stopped: false,
        flat: Vec::new(),
        held: Vec::new(),
    };
    let nested = walk.visit(root, 0);
    let items = if limits.flatten {
        std::mem::take(&mut walk.flat)
    } else {
        nested
    };
    Ok(Items {
        items,
        truncated: walk.budget.truncated(),
        held: walk.held,
    })
}

// ------------------------------------------------------------------------------------ waking

/// Walk two levels, which is what makes Chromium build its accessibility tree (Python `snapshot`).
fn wake_walk(u: &Uia, root: &IUIAutomationElement) {
    // SAFETY: plain child queries on a live element; the answers are only dropped.
    unsafe {
        if let Ok(kids) = root.FindAll(TreeScope_Children, &u.true_cond) {
            for i in 0..kids.Length().unwrap_or(0) {
                if let Ok(kid) = kids.GetElement(i) {
                    let _ = kid.FindAll(TreeScope_Children, &u.true_cond);
                }
            }
        }
    }
}

/// Is `hwnd` the window in front, or inside the one that is (`same_window(GetForegroundWindow(), hwnd)`)?
fn is_front(hwnd: HWND) -> bool {
    let front = wnd::foreground();
    !front.0.is_null()
        && (front == hwnd
            || wnd::root_hwnd(front)
                .zip(wnd::root_hwnd(hwnd))
                .is_some_and(|(a, b)| a == b))
}

/// Raise an occluded Chromium window until its tree appears, then hand focus back to whoever had
/// it (Python `_wake_chromium_by_raise`). `None` when the window is already in front (nothing to
/// wake) or the tree did not appear. This is the one place a read changes the screen, and it only
/// runs for [`WakePolicy::IfEmpty`].
fn wake_by_raise(
    u: &Uia,
    root: &IUIAutomationElement,
    raw: u64,
    limits: &TreeLimits,
) -> Option<Items> {
    let hwnd = hwnd_of(WindowRef::from_raw(raw));
    let previous = wnd::foreground();
    if is_front(hwnd) {
        return None;
    }
    wnd::restore_window(WindowRef::from_raw(raw), Duration::from_millis(1000)).ok()?;
    wnd::bring_to_foreground(WindowRef::from_raw(raw)).ok()?;
    let end = Instant::now() + Duration::from_millis(1500);
    let mut result = None;
    while Instant::now() < end {
        sleep(Duration::from_millis(150));
        if let Ok(items) = read_batched(u, root, raw, limits)
            && items.items.len() > EMPTY_TREE_ITEMS
        {
            result = Some(items);
            break;
        }
    }
    if !previous.0.is_null() && wnd::is_window(previous) {
        let _ = wnd::bring_to_foreground(window_ref(previous));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(filter: ElementFilter, max: usize, flatten: bool) -> TreeLimits {
        TreeLimits {
            max_items: max,
            max_depth: 25,
            deadline: Duration::from_millis(2500),
            filter,
            flatten,
            wake: WakePolicy::Never,
        }
    }

    #[test]
    fn an_unbounded_flat_read_is_batched() {
        assert_eq!(
            choose_strategy(&limits(ElementFilter::Interactive, 5000, true)),
            Strategy::Batched
        );
        assert_eq!(
            choose_strategy(&limits(ElementFilter::All, usize::MAX, true)),
            Strategy::Batched
        );
    }

    #[test]
    fn capped_probe_nested_and_named_reads_walk() {
        for cap in [4, 160, 300, 400] {
            assert_eq!(
                choose_strategy(&limits(ElementFilter::Interactive, cap, true)),
                Strategy::Walk,
                "{cap}"
            );
        }
        assert_eq!(
            choose_strategy(&limits(ElementFilter::Interactive, 5000, false)),
            Strategy::Walk
        );
        assert_eq!(
            choose_strategy(&TreeLimits::named(Duration::from_millis(300))),
            Strategy::Walk
        );
    }

    #[test]
    fn the_hard_stop_leaves_room_for_the_query_and_for_a_wake() {
        let mut l = limits(ElementFilter::Interactive, 300, true);
        assert_eq!(hard_limit(&l), Duration::from_secs(5));
        l.wake = WakePolicy::IfEmpty;
        assert_eq!(hard_limit(&l), Duration::from_secs(8));
        let named = TreeLimits::named(Duration::from_millis(300));
        assert_eq!(hard_limit(&named), Duration::from_secs(1));
    }
}
