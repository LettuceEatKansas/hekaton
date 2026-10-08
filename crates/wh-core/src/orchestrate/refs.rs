//! The two ref tables a model works with. Port of `core._WIN_REFS`, `core.list_windows`,
//! `core.resolve_window_hwnd`, `core._ref_cache`, `core.get_cached_ref` and
//! `core._prune_closed_ref_windows`.
//!
//! **Window refs** (`w3`). 2026-09-19: refs were list positions, so opening or closing any window
//! silently renumbered every other one (w1 was Notepad, then Claude) and an agent's next action hit the
//! wrong app. A ref now stays with its window handle until that window dies. A ref the table does not
//! know is an error, never a silent substitution of some other window.
//!
//! **Element refs** (`4` in a `read` listing) live per window: a snapshot of window A never evicts
//! window B's refs (regression fix 2026-09-17), an entry is replaced only when that window is
//! re-snapshotted, and dropped when that window closes.

use std::collections::{BTreeSet, HashMap};

use super::Computer;
use super::error::{StepError, StepResult};
use crate::backend::{ElementId, Rect, WindowInfo, WindowRef};
use crate::textutil::py_repr;

/// One row of the window list (Python `WinInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowLine {
    /// The stable `wN` number.
    pub reference: u32,
    /// The window.
    pub window: WindowRef,
    /// Its title.
    pub title: String,
    /// Its process as the OS reports it.
    pub process: String,
    /// Its process runs elevated.
    pub elevated: bool,
    /// It is the window in front.
    pub focused: bool,
    /// Where it is and how big (x, y, width, height).
    pub bounds: (i32, i32, i32, i32),
    /// Its process id.
    pub pid: u32,
    /// Minimised.
    pub minimized: bool,
    /// Present but not composited anywhere (DWM-cloaked).
    pub cloaked: bool,
}

/// `w3 "Save" notepad.exe [focused] 1,2,3,4` for each row (Python `core.format_windows`).
pub fn format_windows(wins: &[WindowLine]) -> Vec<String> {
    wins.iter()
        .map(|w| {
            let mut flags = String::new();
            if w.elevated {
                flags.push_str(" [elevated]");
            }
            if w.focused {
                flags.push_str(" [focused]");
            }
            if w.minimized {
                flags.push_str(" [minimized]");
            }
            if w.cloaked {
                flags.push_str(" [cloaked]");
            }
            let process = if w.process.is_empty() {
                "?"
            } else {
                &w.process
            };
            format!(
                "w{} \"{}\" {}{} {},{},{},{}",
                w.reference,
                w.title,
                process,
                flags,
                w.bounds.0,
                w.bounds.1,
                w.bounds.2,
                w.bounds.3
            )
        })
        .collect()
}

/// One element a `read` recorded under a window.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedElement {
    /// The element's position in its window's list.
    pub reference: usize,
    /// The backend's handle on it.
    pub id: ElementId,
    /// The platform role name (`ButtonControl`).
    pub native_role: String,
    /// Its accessible name.
    pub name: String,
    /// Disabled.
    pub disabled: bool,
    /// Centre of its bounds.
    pub center: (i32, i32),
    /// Its value.
    pub value: Option<String>,
    /// Its bounds.
    pub rect: Option<Rect>,
    /// `[on]`, `[selected]`, `[expanded]`: what a click changes.
    pub state: String,
}

#[derive(Default)]
pub(crate) struct RefTables {
    next_window: u32,
    windows: HashMap<WindowRef, u32>,
    elements: HashMap<WindowRef, Vec<CachedElement>>,
    last_snapshot: Option<WindowRef>,
}

/// The window ref table of one [`Computer`].
pub struct WindowRefs<'a>(&'a Computer);

impl<'a> WindowRefs<'a> {
    pub(crate) fn new(c: &'a Computer) -> Self {
        Self(c)
    }

    fn lock(&self) -> std::sync::MutexGuard<'a, RefTables> {
        self.0
            .refs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The ref of a window, if it has been listed.
    pub fn ref_of(&self, w: WindowRef) -> Option<u32> {
        self.lock().windows.get(&w).copied()
    }

    /// Top-level windows with their stable refs, topmost first. DWM-cloaked windows are hidden by
    /// default (they duplicate titles and cannot be foregrounded); pass `include_cloaked` to list
    /// them. Python `core.list_windows`.
    pub fn list(&self, include_cloaked: bool) -> StepResult<Vec<WindowLine>> {
        let all = self.0.backend.list_windows(true)?;
        let live: BTreeSet<WindowRef> = all.iter().map(|w| w.window).collect();
        let mut tables = self.lock();
        let stale: Vec<WindowRef> = tables
            .windows
            .keys()
            .filter(|h| !live.contains(h) && !self.0.backend.window_exists(**h))
            .copied()
            .collect();
        for h in stale {
            tables.windows.remove(&h);
        }
        let mut out = Vec::new();
        for info in all {
            if info.hidden && !include_cloaked {
                continue;
            }
            let reference = match tables.windows.get(&info.window) {
                Some(r) => *r,
                None => {
                    let r = tables.next_window;
                    tables.next_window += 1;
                    tables.windows.insert(info.window, r);
                    r
                }
            };
            out.push(line_of(reference, &info));
        }
        Ok(out)
    }

    /// `w3` ref, `pid:N`, `hwnd:N`, a title (exact beats substring), or nothing = the window in
    /// front. Python `core.resolve_window_hwnd`.
    pub fn resolve(&self, spec: Option<&str>) -> StepResult<WindowRef> {
        self.resolve_in(spec, None)
    }

    pub(crate) fn resolve_in(
        &self,
        spec: Option<&str>,
        listed: Option<&[WindowLine]>,
    ) -> StepResult<WindowRef> {
        let gone = || StepError::target("window handle unavailable; refresh windows");
        let Some(spec) = spec.filter(|s| !s.is_empty()) else {
            return self
                .0
                .backend
                .foreground_window()
                .ok()
                .flatten()
                .ok_or_else(gone);
        };
        let spec_l = spec.trim();
        let lower = spec_l.to_lowercase();
        if let Some(rest) = lower.strip_prefix("hwnd:") {
            let n: u64 = rest
                .trim()
                .parse()
                .map_err(|_| StepError::value("hwnd spec requires an integer"))?;
            let w = WindowRef::from_raw(n);
            if n == 0 || !self.0.backend.window_exists(w) {
                return Err(gone());
            }
            return Ok(w);
        }
        let owned;
        let wins: &[WindowLine] = match listed {
            Some(w) => w,
            None => {
                owned = self.list(false)?;
                &owned
            }
        };
        if let Some(rest) = lower.strip_prefix("pid:") {
            let pid: u32 = rest
                .trim()
                .parse()
                .map_err(|_| StepError::value(format!("bad pid spec: {}", py_repr(spec))))?;
            return wins
                .iter()
                .find(|w| w.pid == pid)
                .map(|w| w.window)
                .ok_or_else(|| StepError::value(format!("no window for pid {pid}")));
        }
        if lower.starts_with('w')
            && !lower[1..].is_empty()
            && lower[1..].chars().all(|c| c.is_ascii_digit())
        {
            let idx: u32 = lower[1..].parse().unwrap_or(u32::MAX);
            return wins
                .iter()
                .find(|w| w.reference == idx)
                .map(|w| w.window)
                .ok_or_else(|| StepError::value(format!("no window ref {}", py_repr(spec))));
        }
        let mut matches: Vec<&WindowLine> = wins
            .iter()
            .filter(|w| w.title.to_lowercase() == lower)
            .collect();
        if matches.is_empty() {
            matches = wins
                .iter()
                .filter(|w| w.title.to_lowercase().contains(&lower))
                .collect();
        }
        // Defect 3 (2026-09-17): only genuinely usable windows count towards ambiguity. A cloaked
        // window duplicates the title but cannot be foregrounded, so it never makes a name ambiguous.
        let real: Vec<&WindowLine> = matches.iter().copied().filter(|w| !w.cloaked).collect();
        if !real.is_empty() {
            matches = real;
        }
        if matches.len() > 1 {
            // UWP: the ApplicationFrameHost frame window is the real action target.
            let frames: Vec<&WindowLine> = matches
                .iter()
                .copied()
                .filter(|w| w.process.to_lowercase() == "applicationframehost.exe")
                .collect();
            if frames.len() == 1 {
                matches = frames;
            }
        }
        match matches.as_slice() {
            [one] => Ok(one.window),
            [] => Err(StepError::value(format!(
                "no window matching {}",
                py_repr(spec)
            ))),
            _ => Err(StepError::value(format!(
                "ambiguous window {}; use a wN ref from windows",
                py_repr(spec)
            ))),
        }
    }
}

fn line_of(reference: u32, info: &WindowInfo) -> WindowLine {
    WindowLine {
        reference,
        window: info.window,
        title: info.title.clone(),
        process: info.app.clone(),
        elevated: info.elevated,
        focused: info.foreground,
        bounds: (
            info.rect.left,
            info.rect.top,
            info.rect.width(),
            info.rect.height(),
        ),
        pid: info.pid,
        minimized: info.minimized,
        cloaked: info.hidden,
    }
}

/// The element ref cache of one [`Computer`].
pub struct ElementRefs<'a>(&'a Computer);

impl<'a> ElementRefs<'a> {
    pub(crate) fn new(c: &'a Computer) -> Self {
        Self(c)
    }

    fn lock(&self) -> std::sync::MutexGuard<'a, RefTables> {
        self.0
            .refs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replace one window's refs with a fresh snapshot (and remember it as the latest).
    pub fn store(&self, window: WindowRef, items: Vec<CachedElement>) {
        let mut t = self.lock();
        t.elements.insert(window, items);
        t.last_snapshot = Some(window);
    }

    /// Look up a cached ref. With a window, scoped to that window only. Without, the most recently
    /// snapshotted window wins, then any other. 2026-09-19: it used to return the first window in dict
    /// order that had ANY element with that number, and refs are small integers that every window has,
    /// so `click 4` after snapshotting window A could hit window B's element 4. Python
    /// `core.get_cached_ref`.
    pub fn get(&self, reference: usize, window: Option<WindowRef>) -> Option<CachedElement> {
        let t = self.lock();
        if let Some(w) = window {
            return t.elements.get(&w).and_then(|v| v.get(reference)).cloned();
        }
        if let Some(last) = t.last_snapshot
            && let Some(item) = t.elements.get(&last).and_then(|v| v.get(reference))
        {
            return Some(item.clone());
        }
        t.elements.values().find_map(|v| v.get(reference)).cloned()
    }

    /// Drop every window that is no longer live. Python `_prune_closed_ref_windows`.
    pub fn prune(&self, live: &BTreeSet<WindowRef>) {
        let mut t = self.lock();
        t.elements.retain(|w, _| live.contains(w));
    }

    /// How many windows have cached refs.
    pub fn windows_cached(&self) -> usize {
        self.lock().elements.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrate::testkit::Kit;

    fn win(title: &str, app: &str, pid: u32) -> WindowInfo {
        WindowInfo::basic(title, app, pid, Rect::from_xywh(0, 0, 10, 10))
    }

    fn wl(reference: u32, handle: u64, title: &str, process: &str) -> WindowLine {
        WindowLine {
            reference,
            window: WindowRef::from_raw(handle),
            title: title.into(),
            process: process.into(),
            elevated: false,
            focused: false,
            bounds: (0, 0, 10, 10),
            pid: handle as u32,
            minimized: false,
            cloaked: false,
        }
    }

    fn element(reference: usize, name: &str) -> CachedElement {
        CachedElement {
            reference,
            id: ElementId(format!("e{reference}")),
            native_role: "ButtonControl".into(),
            name: name.into(),
            disabled: false,
            center: (10, 20),
            value: None,
            rect: None,
            state: String::new(),
        }
    }

    // ---- ported from tests/test_core.py ----

    #[test]
    fn format_windows_line() {
        let mut w = wl(3, 123, "Save", "notepad.exe");
        w.focused = true;
        w.bounds = (1, 2, 3, 4);
        assert_eq!(
            format_windows(&[w]),
            ["w3 \"Save\" notepad.exe [focused] 1,2,3,4"]
        );
    }

    #[test]
    fn format_windows_elevated_and_focused() {
        let mut w = wl(0, 1, "Admin", "cmd.exe");
        w.elevated = true;
        w.focused = true;
        w.bounds = (0, 0, 100, 100);
        assert_eq!(
            format_windows(&[w]),
            ["w0 \"Admin\" cmd.exe [elevated] [focused] 0,0,100,100"]
        );
    }

    #[test]
    fn resolve_window_by_pid_ref_and_no_match() {
        let kit = Kit::new();
        let wins = vec![wl(2, 777, "X", "x.exe")];
        let refs = kit.computer.window_refs();
        assert_eq!(
            refs.resolve_in(Some("w2"), Some(&wins)).unwrap(),
            WindowRef::from_raw(777)
        );
        assert_eq!(
            refs.resolve_in(Some("pid:777"), Some(&wins)).unwrap(),
            WindowRef::from_raw(777)
        );
        assert!(matches!(
            refs.resolve_in(Some("pid:99999999"), Some(&[])),
            Err(StepError::Value(_))
        ));
        let err = refs.resolve_in(Some("w9"), Some(&wins)).unwrap_err();
        assert_eq!(err.message(), "no window ref 'w9'");
    }

    // ---- ported from tests/test_defects_20260917.py ----

    #[test]
    fn cloaked_windows_hidden_by_default_and_listed_on_request() {
        let kit = Kit::new();
        let mut cloaked = win("Calculator", "CalculatorApp.exe", 11);
        cloaked.hidden = true;
        let mut frame = win("Calculator", "ApplicationFrameHost.exe", 22);
        frame.minimized = true;
        kit.fake.add_window(frame);
        kit.fake.add_window(cloaked);
        let refs = kit.computer.window_refs();
        let shown = refs.list(false).unwrap();
        assert_eq!(shown.len(), 1);
        assert!(shown[0].minimized && !shown[0].cloaked);
        let all = refs.list(true).unwrap();
        assert_eq!(all.len(), 2);
        let mut ids: Vec<u32> = all.iter().map(|w| w.reference).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 2);
        assert!(all.iter().any(|w| w.cloaked));
    }

    #[test]
    fn window_refs_are_stable_across_listings() {
        // 2026-09-19: refs were list positions, so any window opening or closing renumbered the rest.
        let kit = Kit::new();
        let mut first = win("Calculator", "CalculatorApp.exe", 11);
        first.hidden = true;
        let a = kit.fake.add_window(first);
        let b = kit
            .fake
            .add_window(win("Calculator", "ApplicationFrameHost.exe", 22));
        let refs = kit.computer.window_refs();
        let before = refs.list(true).unwrap();
        let ref_b = before.iter().find(|w| w.window == b).unwrap().reference;
        kit.fake.close_window(a);
        let after = refs.list(true).unwrap();
        assert_eq!(
            after.iter().find(|w| w.window == b).unwrap().reference,
            ref_b
        );
        // a new window never takes a dead one's number
        let c = kit.fake.add_window(win("New", "n.exe", 33));
        let again = refs.list(true).unwrap();
        let ref_c = again.iter().find(|w| w.window == c).unwrap().reference;
        assert!(before.iter().all(|w| w.reference != ref_c));
    }

    #[test]
    fn format_windows_tags_minimized_and_cloaked() {
        let mut a = wl(0, 11, "Calculator", "CalculatorApp.exe");
        a.cloaked = true;
        let mut b = wl(1, 22, "Calculator", "ApplicationFrameHost.exe");
        b.minimized = true;
        let lines = format_windows(&[a, b]);
        assert!(lines[0].contains("[cloaked]") && !lines[0].contains("[minimized]"));
        assert!(lines[1].contains("[minimized]") && !lines[1].contains("[cloaked]"));
    }

    #[test]
    fn uwp_frame_window_is_preferred() {
        let kit = Kit::new();
        let refs = kit.computer.window_refs();
        let mut cloaked = wl(0, 11, "Calculator", "CalculatorApp.exe");
        cloaked.cloaked = true;
        let frame = wl(1, 22, "Calculator", "ApplicationFrameHost.exe");
        let got = refs
            .resolve_in(Some("Calculator"), Some(&[cloaked, frame.clone()]))
            .unwrap();
        assert_eq!(got.raw(), 22);
        let two = [wl(0, 11, "Calculator", "CalculatorApp.exe"), frame];
        assert_eq!(
            refs.resolve_in(Some("Calculator"), Some(&two))
                .unwrap()
                .raw(),
            22
        );
    }

    #[test]
    fn ambiguity_counts_only_real_windows() {
        let kit = Kit::new();
        let refs = kit.computer.window_refs();
        let mut hidden = wl(0, 11, "Notes", "notes.exe");
        hidden.cloaked = true;
        let real = wl(1, 22, "Notes", "notes.exe");
        assert_eq!(
            refs.resolve_in(Some("Notes"), Some(&[hidden, real.clone()]))
                .unwrap()
                .raw(),
            22
        );
        let twin = wl(0, 11, "Notes", "notes.exe");
        let err = refs
            .resolve_in(Some("Notes"), Some(&[twin, real]))
            .unwrap_err();
        assert!(err.message().contains("ambiguous"), "{err}");
        let backup = wl(0, 11, "Notes backup", "notes.exe");
        let exact = wl(1, 22, "Notes", "notes.exe");
        assert_eq!(
            refs.resolve_in(Some("Notes"), Some(&[backup, exact]))
                .unwrap()
                .raw(),
            22
        );
    }

    #[test]
    fn hwnd_specs_pin_a_live_window_and_an_empty_spec_is_the_foreground() {
        let kit = Kit::new();
        let w = kit.fake.add_window(win("A", "a.exe", 1));
        let refs = kit.computer.window_refs();
        assert_eq!(refs.resolve(Some(&w.spec())).unwrap(), w);
        let gone = refs.resolve(Some("hwnd:424242")).unwrap_err();
        assert_eq!(gone.message(), "window handle unavailable; refresh windows");
        assert!(refs.resolve(Some("hwnd:abc")).is_err());
        assert!(refs.resolve(None).is_err(), "nothing in front");
        kit.fake.set_foreground(Some(w));
        assert_eq!(refs.resolve(None).unwrap(), w);
        assert_eq!(refs.resolve(Some("")).unwrap(), w);
    }

    // ---- ported from tests/test_focus_and_cache.py ----

    #[test]
    fn ref_cache_survives_across_requests_and_other_windows_are_untouched() {
        let kit = Kit::new();
        let cache = kit.computer.element_refs();
        let (a, b) = (WindowRef::from_raw(100), WindowRef::from_raw(200));
        cache.store(a, vec![element(0, "Save")]);
        // a later, unrelated request still resolves it, with or without naming the window
        assert_eq!(cache.get(0, Some(a)).unwrap().name, "Save");
        assert_eq!(cache.get(0, None).unwrap().name, "Save");
        // regression: a snapshot of window B must not evict window A's refs
        cache.store(b, vec![element(0, "Cancel")]);
        assert_eq!(cache.get(0, Some(a)).unwrap().name, "Save");
        assert_eq!(cache.get(0, Some(b)).unwrap().name, "Cancel");
        // with no window named, the latest snapshot wins
        assert_eq!(cache.get(0, None).unwrap().name, "Cancel");
        assert!(cache.get(5, None).is_none());
    }

    #[test]
    fn ref_cache_invalidates_on_resnapshot_and_on_window_close() {
        let kit = Kit::new();
        let cache = kit.computer.element_refs();
        let (a, b) = (WindowRef::from_raw(100), WindowRef::from_raw(200));
        cache.store(a, vec![element(0, "Save")]);
        cache.store(a, vec![element(0, "SaveAs")]);
        assert_eq!(cache.get(0, Some(a)).unwrap().name, "SaveAs");
        cache.store(b, vec![element(0, "Cancel")]);
        cache.prune(&BTreeSet::from([b]));
        assert!(cache.get(0, Some(a)).is_none());
        assert_eq!(cache.get(0, Some(b)).unwrap().name, "Cancel");
        assert_eq!(cache.windows_cached(), 1);
    }
}
