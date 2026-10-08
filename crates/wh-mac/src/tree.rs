//! The accessibility tree: reading a window's elements under limits, minting stable element ids,
//! acting on elements, and the small queries built on the same walk (menus, dialog text, the file
//! dialog field).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

use objc2_application_services::AXError;
use wh_core::backend::{
    Affordance, BackendError, BackendResult, Element, ElementAction, ElementFilter, ElementHit,
    ElementId, ElementState, Expand, MenuItem, Rect, Role, TreeLimits, TreeSnapshot, WakePolicy,
    WindowRef,
};

use crate::apps;
use crate::ax::{self, Ax, Props};
use crate::geom::rect_from_f64;
use crate::perm;
use crate::roles::{
    PRESS_ACTIONS, has_toggle_value, is_secure_field, role_from_ax, toggle_from_value,
};
use crate::wininfo::{window_parts, window_ref};
use crate::winlist;

/// Most element handles kept alive; the oldest go first.
const REGISTRY_CAP: usize = 40_000;
/// Most elements one read visits, whatever the limits say.
const VISIT_CAP: usize = 30_000;
/// A walk that finds no more than this many elements in a Chromium window counts as empty.
const EMPTY_TREE: usize = 30;
/// Longest value text an element carries.
const MAX_VALUE_CHARS: usize = 4000;

struct Entry {
    ax: Ax,
    window: Option<WindowRef>,
}

#[derive(Default)]
struct Inner {
    next: u64,
    map: HashMap<u64, Entry>,
    order: VecDeque<u64>,
}

/// Live element references behind the string ids the core sees.
#[derive(Default)]
pub(crate) struct Registry {
    inner: Mutex<Inner>,
    woken: Mutex<HashSet<i32>>,
}

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn insert(&self, ax: Ax, window: Option<WindowRef>) -> ElementId {
        let mut g = self.lock();
        g.next += 1;
        let n = g.next;
        g.map.insert(n, Entry { ax, window });
        g.order.push_back(n);
        while g.order.len() > REGISTRY_CAP {
            if let Some(old) = g.order.pop_front() {
                g.map.remove(&old);
            }
        }
        ElementId(format!("ax{n}"))
    }

    fn get(&self, id: &ElementId) -> Option<(Ax, Option<WindowRef>)> {
        let n: u64 = id.0.strip_prefix("ax")?.parse().ok()?;
        self.lock().map.get(&n).map(|e| (e.ax.clone(), e.window))
    }
}

/// Map an AX failure onto the backend's error kinds.
pub(crate) fn map_err(err: AXError) -> BackendError {
    match err {
        AXError::InvalidUIElement => BackendError::ElementGone,
        AXError::APIDisabled => perm::require_accessibility("read the screen")
            .err()
            .unwrap_or_else(|| BackendError::Os(ax::describe(err).into())),
        _ => BackendError::Os(ax::describe(err).to_string()),
    }
}

fn cap_chars(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        s
    } else {
        s.chars().take(max).collect()
    }
}

/// Build the neutral element (without children) from fetched properties.
pub(crate) fn make_element(id: ElementId, props: &Props, window_rect: Option<Rect>) -> Element {
    let role = role_from_ax(
        &props.role,
        props.subrole.as_deref(),
        props.disclosing.is_some(),
    );
    let password = is_secure_field(&props.role, props.subrole.as_deref());
    let value_text = if password { None } else { props.value_text() };
    let non_empty = |s: &Option<String>| s.clone().filter(|t| !t.trim().is_empty());
    let name = match role {
        Role::Text => non_empty(&value_text)
            .or_else(|| non_empty(&props.title))
            .or_else(|| non_empty(&props.description)),
        _ => non_empty(&props.title)
            .or_else(|| non_empty(&props.description))
            .or_else(|| {
                if role == Role::Edit {
                    non_empty(&props.placeholder)
                } else {
                    None
                }
            }),
    }
    .map(|s| cap_chars(s, 300))
    .unwrap_or_default();

    let rect = match (props.position, props.size) {
        (Some(p), Some(s)) => Some(rect_from_f64(p.x, p.y, s.width, s.height)),
        _ => None,
    };
    let offscreen = match (rect, window_rect) {
        (Some(r), Some(w)) => r.intersect(&w).is_none(),
        _ => false,
    };

    let mut state = ElementState::default();
    let num = props.value.as_deref().and_then(crate::cf::i64_of);
    if has_toggle_value(&props.role, props.subrole.as_deref()) {
        match role {
            Role::RadioButton => state.selected = num == Some(1),
            _ => state.toggle = Some(toggle_from_value(num.unwrap_or(0))),
        }
    }
    if matches!(role, Role::TabItem) {
        state.selected = num == Some(1) || props.selected;
    }
    if matches!(
        role,
        Role::ListItem | Role::TreeItem | Role::DataItem | Role::MenuItem
    ) {
        state.selected = props.selected;
    }
    if let Some(open) = props.expanded.or(props.disclosing) {
        state.expanded = Some(if open {
            Expand::Expanded
        } else {
            Expand::Collapsed
        });
    }

    let mut affordances = Vec::new();
    match role {
        Role::Button | Role::MenuItem | Role::Hyperlink | Role::SplitButton => {
            affordances.push(Affordance::Invoke);
        }
        Role::CheckBox => affordances.push(Affordance::Toggle),
        Role::RadioButton | Role::ListItem | Role::TabItem | Role::TreeItem | Role::DataItem => {
            affordances.push(Affordance::Select);
        }
        Role::Edit | Role::Slider | Role::ComboBox => affordances.push(Affordance::Value),
        Role::Document => affordances.push(Affordance::Text),
        _ => {}
    }
    if state.expanded.is_some() {
        affordances.push(Affordance::ExpandCollapse);
    }

    let show_value = !matches!(role, Role::Text | Role::CheckBox | Role::RadioButton)
        && !has_toggle_value(&props.role, props.subrole.as_deref());
    Element {
        id,
        role,
        native_role: props.role.clone(),
        name,
        value: if show_value {
            value_text.map(|v| cap_chars(v, MAX_VALUE_CHARS))
        } else {
            None
        },
        rect,
        enabled: props.enabled.unwrap_or(true),
        focused: props.focused,
        offscreen,
        password,
        state,
        affordances,
        children: Vec::new(),
    }
}

struct Node {
    el: Element,
    ax: Ax,
    kept: bool,
    kids: Vec<Node>,
}

struct Walk<'a> {
    limits: &'a TreeLimits,
    deadline: Instant,
    window_rect: Option<Rect>,
    visited: usize,
    kept: usize,
    saw_document: bool,
    truncated: bool,
}

fn keeps(el: &Element, filter: ElementFilter) -> bool {
    let visible = el.rect.is_none_or(|r| !r.is_empty());
    match filter {
        ElementFilter::All => true,
        ElementFilter::Interactive => el.is_interactive() && visible,
        ElementFilter::Named => !el.name.is_empty() && el.rect.is_some_and(|r| !r.is_empty()),
    }
}

/// The first text found under a node, for items whose own name is empty.
fn first_text(kids: &[Node], depth: u32) -> Option<String> {
    if depth > 3 {
        return None;
    }
    for k in kids {
        if k.el.role == Role::Text && !k.el.name.is_empty() {
            return Some(k.el.name.clone());
        }
        if let Some(t) = first_text(&k.kids, depth + 1) {
            return Some(t);
        }
    }
    None
}

impl Walk<'_> {
    fn stop(&mut self) -> bool {
        if Instant::now() >= self.deadline || self.visited >= VISIT_CAP {
            self.truncated = true;
            return true;
        }
        if self.kept >= self.limits.max_items {
            self.truncated = true;
            return true;
        }
        false
    }

    fn visit(&mut self, ax: Ax, depth: u32) -> Option<Node> {
        let props = ax.fetch().ok()?;
        self.visited += 1;
        let mut el = make_element(ElementId(String::new()), &props, self.window_rect);
        self.saw_document |= el.role == Role::Document;
        let mut kids = Vec::new();
        if depth < self.limits.max_depth {
            for child in props.children.iter().cloned() {
                if self.stop() {
                    break;
                }
                if let Some(n) = self.visit(child, depth + 1) {
                    kids.push(n);
                }
            }
        } else if !props.children.is_empty() {
            self.truncated = true;
        }
        if el.name.is_empty()
            && matches!(
                el.role,
                Role::ListItem
                    | Role::DataItem
                    | Role::TreeItem
                    | Role::TabItem
                    | Role::Button
                    | Role::CheckBox
                    | Role::RadioButton
                    | Role::Hyperlink
                    | Role::MenuItem
            )
            && let Some(t) = first_text(&kids, 0)
        {
            el.name = t;
        }
        // A leaf group or image that can be pressed is worth listing as clickable.
        if matches!(el.role, Role::Pane | Role::Other(_))
            && kids.is_empty()
            && self.limits.filter == ElementFilter::Interactive
            && el.rect.is_some_and(|r| !r.is_empty())
            && el.enabled
            && ax.actions().iter().any(|a| a == "AXPress")
        {
            el.affordances.push(Affordance::Invoke);
        }
        let kept = keeps(&el, self.limits.filter);
        if kept {
            self.kept += 1;
        }
        Some(Node { el, ax, kept, kids })
    }
}

fn collect_flat(node: Node, reg: &Registry, window: WindowRef, out: &mut Vec<Element>) {
    let Node {
        mut el,
        ax,
        kept,
        kids,
    } = node;
    if kept {
        el.id = reg.insert(ax, Some(window));
        out.push(el);
    }
    for k in kids {
        collect_flat(k, reg, window, out);
    }
}

fn prune(node: Node, reg: &Registry, window: WindowRef) -> Option<Element> {
    let Node {
        mut el,
        ax,
        kept,
        kids,
    } = node;
    let children: Vec<Element> = kids
        .into_iter()
        .filter_map(|k| prune(k, reg, window))
        .collect();
    if !kept && children.is_empty() {
        return None;
    }
    el.id = reg.insert(ax, Some(window));
    el.children = children;
    Some(el)
}

/// What one walk of a window produced.
struct Walked {
    root: Element,
    /// Elements visited, kept or not.
    visited: usize,
    /// A web or document area was seen (a Chromium tree without one has not woken up).
    saw_document: bool,
    truncated: bool,
}

fn walk_window(
    reg: &Registry,
    win: &Ax,
    window: WindowRef,
    limits: &TreeLimits,
    remaining: Duration,
) -> BackendResult<Walked> {
    let props = win.fetch().map_err(map_err)?;
    let window_rect = match (props.position, props.size) {
        (Some(p), Some(s)) => Some(rect_from_f64(p.x, p.y, s.width, s.height)),
        _ => None,
    };
    let mut w = Walk {
        limits,
        deadline: Instant::now() + remaining,
        window_rect,
        visited: 0,
        kept: 0,
        saw_document: false,
        truncated: false,
    };
    let root = w.visit(win.clone(), 0).ok_or(BackendError::ElementGone)?;
    let (visited, saw_document, truncated) = (w.visited, w.saw_document, w.truncated);
    let Node {
        mut el, ax, kids, ..
    } = root;
    el.id = reg.insert(ax, Some(window));
    el.role = Role::Window;
    if limits.flatten {
        let mut flat = Vec::new();
        for k in kids {
            collect_flat(k, reg, window, &mut flat);
        }
        el.children = flat;
    } else {
        el.children = kids
            .into_iter()
            .filter_map(|k| prune(k, reg, window))
            .collect();
    }
    Ok(Walked {
        root: el,
        visited,
        saw_document,
        truncated,
    })
}

/// Turn on the accessibility tree of a Chromium or Electron app, once per app. Setting the
/// attributes does not raise the window or move focus. True when this call did it.
fn wake_app(reg: &Registry, pid: i32) -> bool {
    let mut woken = reg.woken.lock().unwrap_or_else(|e| e.into_inner());
    if !woken.insert(pid) {
        return false;
    }
    let app = Ax::app(pid);
    app.set_bool("AXManualAccessibility", true);
    app.set_bool("AXEnhancedUserInterface", true);
    true
}

/// Read a window's tree under `limits`.
pub(crate) fn element_tree(
    reg: &Registry,
    window: WindowRef,
    limits: &TreeLimits,
    displays: &[Rect],
) -> BackendResult<TreeSnapshot> {
    perm::require_accessibility("read app windows")?;
    let started = Instant::now();
    let (pid, id) = window_parts(window);
    let info = winlist::info(window, displays)?;
    let found = winlist::find_ax_window(pid, id, Some(info.rect)).ok_or_else(|| {
        BackendError::Os(
            "the app does not expose this window to Accessibility (it may be a system surface)"
                .into(),
        )
    })?;
    let win = found.el;
    let chromium = apps::is_chromium_like(pid);
    let mut wake_hint = false;
    let mut walked = walk_window(reg, &win, window, limits, limits.deadline)?;
    // A Chromium or Electron window whose page has not appeared in the tree is asleep.
    if chromium && (!walked.saw_document || walked.visited <= EMPTY_TREE) {
        match limits.wake {
            WakePolicy::Never => wake_hint = true,
            // Already woken once: a window with no document is simply not a web window.
            WakePolicy::IfEmpty if wake_app(reg, pid) => {
                // Chromium builds the tree asynchronously; give it a moment, then read again.
                let settle_until = started + limits.deadline.mul_f32(0.5);
                while Instant::now() < settle_until {
                    sleep(Duration::from_millis(120));
                    if win.elements("AXChildren").len() > 3 {
                        break;
                    }
                }
                let left = limits.deadline.saturating_sub(started.elapsed());
                walked = walk_window(
                    reg,
                    &win,
                    window,
                    limits,
                    left.max(Duration::from_millis(300)),
                )?;
                // `wake_hint` stays false: it means "left alone because the policy said never".
            }
            WakePolicy::IfEmpty => {}
        }
    }
    Ok(TreeSnapshot {
        window,
        title: info.title,
        root: walked.root,
        truncated: walked.truncated,
        wake_hint,
    })
}

/// A fresh read of one element.
pub(crate) fn element(reg: &Registry, id: &ElementId) -> BackendResult<Element> {
    let (ax, window) = reg.get(id).ok_or(BackendError::ElementGone)?;
    let props = ax.fetch().map_err(map_err)?;
    let _ = window;
    Ok(make_element(id.clone(), &props, None))
}

/// The window an element lives in.
pub(crate) fn element_window(reg: &Registry, id: &ElementId) -> BackendResult<WindowRef> {
    let (ax, window) = reg.get(id).ok_or(BackendError::ElementGone)?;
    if let Some(w) = window {
        return Ok(w);
    }
    window_of(&ax).ok_or(BackendError::WindowGone)
}

/// The window an arbitrary element belongs to (its own `AXWindow`, or itself if it is one).
pub(crate) fn window_of(ax: &Ax) -> Option<WindowRef> {
    let pid = ax.pid()?;
    if ax.string("AXRole").as_deref() == Some("AXWindow") {
        return ax.window_id().map(|id| window_ref(pid, id));
    }
    ax.element("AXWindow")
        .and_then(|w| w.window_id())
        .map(|id| window_ref(pid, id))
}

/// The element at a screen point with its ancestors.
pub(crate) fn element_at(
    reg: &Registry,
    p: wh_core::backend::Point,
    max_ancestors: usize,
    budget: Duration,
) -> BackendResult<Option<ElementHit>> {
    perm::require_accessibility("inspect what is under the pointer")?;
    let sys = Ax::system_wide();
    sys.set_timeout(budget.as_secs_f32().clamp(0.05, 5.0));
    let hit = sys.element_at(p.x as f32, p.y as f32);
    sys.set_timeout(1.0);
    let ax = match hit {
        Ok(ax) => ax,
        Err(AXError::APIDisabled) => return Err(map_err(AXError::APIDisabled)),
        Err(_) => return Ok(None),
    };
    let props = ax.fetch().map_err(map_err)?;
    let window = window_of(&ax);
    let element = make_element(reg.insert(ax.clone(), window), &props, None);
    let mut ancestors = Vec::new();
    let mut cur = ax;
    while ancestors.len() < max_ancestors {
        let Some(parent) = cur.element("AXParent") else {
            break;
        };
        let Ok(pp) = parent.fetch() else { break };
        ancestors.push(make_element(reg.insert(parent.clone(), window), &pp, None));
        cur = parent;
    }
    Ok(Some(ElementHit { element, ancestors }))
}

/// The focused element, system-wide first and then through the frontmost app.
pub(crate) fn focused_ax() -> Option<Ax> {
    let sys = Ax::system_wide();
    if let Some(el) = sys.element("AXFocusedUIElement") {
        return Some(el);
    }
    let pid = apps::frontmost_pid()?;
    Ax::app(pid).element("AXFocusedUIElement")
}

/// The focused element as a neutral record (no value when it is a password field).
pub(crate) fn focused_element(reg: &Registry) -> BackendResult<Option<Element>> {
    perm::require_accessibility("read the focused control")?;
    let Some(ax) = focused_ax() else {
        return Ok(None);
    };
    let props = ax.fetch().map_err(map_err)?;
    let window = window_of(&ax);
    Ok(Some(make_element(reg.insert(ax, window), &props, None)))
}

/// Act on an element.
pub(crate) fn perform(
    reg: &Registry,
    id: &ElementId,
    action: ElementAction,
) -> BackendResult<bool> {
    perm::require_accessibility("act on a control")?;
    let (ax, _) = reg.get(id).ok_or(BackendError::ElementGone)?;
    match action {
        ElementAction::Press => press(&ax),
        ElementAction::Focus => match ax.set_bool("AXFocused", true) {
            AXError::Success => Ok(true),
            e => Err(map_err(e)),
        },
        ElementAction::SetValue(text) => match ax.set_string("AXValue", &text) {
            AXError::Success => Ok(true),
            e => Err(map_err(e)),
        },
    }
}

fn press(ax: &Ax) -> BackendResult<bool> {
    let names = ax.actions();
    for want in PRESS_ACTIONS {
        if !names.iter().any(|n| n == want) {
            continue;
        }
        return match ax.perform(want) {
            AXError::Success => Ok(true),
            AXError::InvalidUIElement => Err(BackendError::ElementGone),
            AXError::ActionUnsupported => Ok(false),
            AXError::CannotComplete => Err(BackendError::Os(
                "the app did not answer the press (it may still have happened: a menu or dialog \
                 may have opened)"
                    .into(),
            )),
            e => Err(map_err(e)),
        };
    }
    // No press action: rows, tabs and radio buttons are chosen by selecting them.
    let role = ax.string("AXRole").unwrap_or_default();
    if matches!(
        role.as_str(),
        "AXRow" | "AXCell" | "AXRadioButton" | "AXTab"
    ) && ax.is_settable("AXSelected")
    {
        return match ax.set_bool("AXSelected", true) {
            AXError::Success => Ok(true),
            e => Err(map_err(e)),
        };
    }
    Ok(false)
}

/// Depth-first search under `root` for the first element `pred` accepts.
pub(crate) fn find_first(
    root: &Ax,
    max_depth: u32,
    budget: Duration,
    pred: &dyn Fn(&Props) -> bool,
) -> Option<(Ax, Props)> {
    let deadline = Instant::now() + budget;
    fn go(
        ax: &Ax,
        depth: u32,
        max_depth: u32,
        deadline: Instant,
        pred: &dyn Fn(&Props) -> bool,
    ) -> Option<(Ax, Props)> {
        if Instant::now() >= deadline {
            return None;
        }
        let props = ax.fetch().ok()?;
        if pred(&props) {
            return Some((ax.clone(), props));
        }
        if depth >= max_depth {
            return None;
        }
        for c in &props.children {
            if let Some(f) = go(c, depth + 1, max_depth, deadline, pred) {
                return Some(f);
            }
        }
        None
    }
    go(root, 0, max_depth, deadline, pred)
}

/// Visit elements under `root` depth-first, handing each to `f`, within a depth and time limit.
pub(crate) fn visit_all(
    root: &Ax,
    max_depth: u32,
    budget: Duration,
    f: &mut dyn FnMut(&Props, u32),
) {
    let deadline = Instant::now() + budget;
    fn go(ax: &Ax, depth: u32, max_depth: u32, deadline: Instant, f: &mut dyn FnMut(&Props, u32)) {
        if Instant::now() >= deadline {
            return;
        }
        let Ok(props) = ax.fetch() else { return };
        f(&props, depth);
        if depth < max_depth {
            for c in &props.children {
                go(c, depth + 1, max_depth, deadline, f);
            }
        }
    }
    go(root, 0, max_depth, deadline, f);
}

/// The AX window element for a window handle.
pub(crate) fn ax_window(window: WindowRef, displays: &[Rect]) -> BackendResult<Ax> {
    let (pid, id) = window_parts(window);
    let info = winlist::info(window, displays)?;
    winlist::find_ax_window(pid, id, Some(info.rect))
        .map(|w| w.el)
        .ok_or(BackendError::WindowGone)
}

/// A dialog's title and static text lines.
pub(crate) fn window_text(window: WindowRef, displays: &[Rect]) -> BackendResult<String> {
    perm::require_accessibility("read a dialog")?;
    let info = winlist::info(window, displays)?;
    let win = ax_window(window, displays)?;
    let mut lines: Vec<String> = Vec::new();
    if !info.title.is_empty() {
        lines.push(info.title.clone());
    }
    visit_all(&win, 8, Duration::from_millis(600), &mut |p, _| {
        if p.role == "AXStaticText"
            && let Some(t) = p.value_text().or_else(|| p.title.clone())
        {
            let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
            if !t.is_empty() && !lines.contains(&t) {
                lines.push(t);
            }
        }
    });
    Ok(lines.join("\n"))
}

/// The filename field of a standard Save or Open dialog.
pub(crate) fn find_file_dialog_field(
    reg: &Registry,
    window: WindowRef,
    displays: &[Rect],
) -> BackendResult<Option<ElementId>> {
    perm::require_accessibility("find the file name field")?;
    let win = ax_window(window, displays)?;
    let found = find_first(&win, 10, Duration::from_millis(600), &|p| {
        matches!(p.role.as_str(), "AXTextField" | "AXComboBox")
            && (p.identifier.as_deref() == Some("saveAsNameTextField")
                || p.description
                    .as_deref()
                    .or(p.title.as_deref())
                    .is_some_and(|d| {
                        let d = d.to_lowercase();
                        d.contains("save as") || d.contains("go to the folder")
                    }))
    });
    Ok(found.map(|(ax, _)| reg.insert(ax, Some(window))))
}

/// The items of the open context menu.
pub(crate) fn menu_items(budget: Duration) -> BackendResult<Vec<MenuItem>> {
    perm::require_accessibility("read menus")?;
    let deadline = Instant::now() + budget;
    let sys = Ax::system_wide();
    let mut seen: Vec<u64> = Vec::new();
    let mut out = Vec::new();
    for (_, rect) in winlist::popups() {
        for frac in [2, 4, 3] {
            if Instant::now() >= deadline {
                return Ok(out);
            }
            let x = (rect.left + rect.width() / 2) as f32;
            let y = (rect.top + rect.height() / frac) as f32;
            let Ok(mut cur) = sys.element_at(x, y) else {
                continue;
            };
            // Climb to the menu that holds the item under the probe.
            let mut menu = None;
            for _ in 0..6 {
                if cur.string("AXRole").as_deref() == Some("AXMenu") {
                    menu = Some(cur.clone());
                    break;
                }
                match cur.element("AXParent") {
                    Some(p) => cur = p,
                    None => break,
                }
            }
            let Some(menu) = menu else { continue };
            let h = menu.identity_hash();
            if seen.contains(&h) {
                continue;
            }
            seen.push(h);
            for item in menu.elements("AXChildren") {
                let Ok(p) = item.fetch() else { continue };
                if p.role != "AXMenuItem" {
                    continue;
                }
                let Some(name) = p.title.clone().filter(|t| !t.is_empty()) else {
                    continue;
                };
                let (Some(pos), Some(size)) = (p.position, p.size) else {
                    continue;
                };
                out.push(MenuItem {
                    name,
                    rect: rect_from_f64(pos.x, pos.y, size.width, size.height),
                });
            }
            break;
        }
    }
    Ok(out)
}
