//! Text through Accessibility: a window's document text, matches with their screen boxes, the line
//! under a point, the caret. `AXValue` strings and `AXBoundsForRange` both count UTF-16 units, the
//! same unit the pure helpers in `textops` use.

use std::time::{Duration, Instant};

use wh_core::backend::{BackendError, BackendResult, Caret, DocumentHit, Point, Rect, WindowRef};

use crate::apps;
use crate::ax::Ax;
use crate::cf;
use crate::geom::rect_from_f64;
use crate::perm;
use crate::roles::{is_dialog_like, is_secure_field, is_text_input};
use crate::textops::{
    WebNode, find_matches_checked, line_range, page_with_dialogs, remaining_budget, slice16,
};
use crate::tree::{ax_window, find_first, focused_ax, window_of};
use crate::wininfo::window_parts;
use crate::winlist;

/// Largest document text read, in characters.
const MAX_DOC_CHARS: usize = 2_000_000;

/// The element holding the window's document text, with the text.
fn text_element(
    window: WindowRef,
    displays: &[Rect],
    deadline: Instant,
) -> BackendResult<Option<(Ax, String)>> {
    let remaining = || remaining_budget(deadline, Instant::now()).ok_or(BackendError::Timeout);
    remaining()?;
    let (pid, _) = window_parts(window);
    let in_window = |ax: &Ax| window_of(ax) == Some(window);
    // The focused text control of this window is the best guess for "the document".
    if let Some(focus) = Ax::app(pid).element("AXFocusedUIElement")
        && in_window(&focus)
        && let Ok(p) = focus.fetch()
        && is_text_input(&p.role)
        && !is_secure_field(&p.role, p.subrole.as_deref())
        && let Some(text) = p.value_text()
        && !text.is_empty()
    {
        remaining()?;
        return Ok(Some((focus, text)));
    }
    remaining()?;
    let win = ax_window(window, displays)?;
    let found = find_first(
        &win,
        12,
        remaining()?.min(Duration::from_millis(700)),
        &|p| p.role == "AXTextArea" && p.value_text().is_some_and(|t| !t.is_empty()),
    );
    remaining()?;
    Ok(found.and_then(|(ax, p)| p.value_text().map(|t| (ax, t))))
}

/// How long the walk of a web page's tree may take. A page is thousands of elements; past this the
/// caller falls back to select-all + copy rather than get half a page.
const WEB_TEXT_BUDGET: Duration = Duration::from_millis(3000);

/// Most elements of a web page read as text.
const WEB_TEXT_NODES: usize = 60_000;

/// One element and its children as a [`WebNode`], `None` when the budget ran out. A dialog, sheet or
/// alert found on the way is not left in the page: it goes into `dialogs` (once, outermost only) and
/// the page gets an empty node in its place.
fn web_node(
    ax: &Ax,
    deadline: Instant,
    seen: &mut usize,
    dialogs: &mut Vec<WebNode>,
    in_dialog: bool,
) -> Option<WebNode> {
    if Instant::now() >= deadline || *seen >= WEB_TEXT_NODES {
        return None;
    }
    *seen += 1;
    let p = ax.fetch().ok()?;
    if is_secure_field(&p.role, p.subrole.as_deref()) {
        return Some(WebNode::default());
    }
    let dialog = is_dialog_like(&p.role, p.subrole.as_deref(), p.role_description.as_deref());
    let mut children = Vec::with_capacity(p.children.len());
    for c in &p.children {
        children.push(web_node(c, deadline, seen, dialogs, in_dialog || dialog)?);
    }
    // static text keeps its value; a control or a leaf speaks through its label
    let text = if p.role == "AXStaticText" {
        p.value_text().or_else(|| p.title.clone())
    } else if is_text_input(&p.role) {
        // a field's label, never what was typed into it
        p.title.clone().or_else(|| p.description.clone())
    } else {
        p.title
            .clone()
            .or_else(|| p.description.clone())
            .or_else(|| p.value_text())
    };
    let node = WebNode {
        role: p.role,
        text,
        children,
    };
    if dialog && !in_dialog {
        dialogs.push(node);
        return Some(WebNode::default());
    }
    Some(node)
}

/// Dialogs, sheets and alerts under `ax` that are outside the page (a browser's own sheet, a
/// permission bubble), found without going into a web view or into a dialog. Outermost first.
fn outside_dialogs(ax: &Ax, depth: u32, deadline: Instant, found: &mut Vec<Ax>) {
    if depth > 10 || Instant::now() >= deadline {
        return;
    }
    let Ok(p) = ax.fetch() else { return };
    if p.role == "AXWebArea" {
        return;
    }
    if depth > 0 && is_dialog_like(&p.role, p.subrole.as_deref(), p.role_description.as_deref()) {
        found.push(ax.clone());
        return;
    }
    for c in &p.children {
        outside_dialogs(c, depth + 1, deadline, found);
    }
}

/// The text of the page in the window's web view, read from the accessibility tree, with any open
/// dialog, sheet or alert listed first (in the page or around it). Selecting the
/// page and copying it leaves the whole page highlighted (2026-10-30 unattended run: an agent clicked
/// blank space to clear that and flipped a radio answer). 2026-10-30: the same run missed a "Goal
/// Complete" modal that was on screen, because only the web view's plain content was read. `None`
/// when the window has no web view, the view exposes no text, or the walk ran out of time.
fn web_text(window: WindowRef, displays: &[Rect]) -> BackendResult<Option<String>> {
    let win = ax_window(window, displays)?;
    let deadline = Instant::now() + WEB_TEXT_BUDGET;
    let Some((area, _)) = find_first(&win, 14, Duration::from_millis(900), &|p| {
        p.role == "AXWebArea"
    }) else {
        return Ok(None);
    };
    let mut seen = 0;
    let mut dialogs = Vec::new();
    let Some(root) = web_node(&area, deadline, &mut seen, &mut dialogs, false) else {
        return Ok(None);
    };
    // a dialog drawn outside the web view (the browser's own, or an AXSheet on the window)
    let mut outer = Vec::new();
    outside_dialogs(&win, 0, deadline, &mut outer);
    let mut all = Vec::new();
    for ax in &outer {
        if let Some(n) = web_node(ax, deadline, &mut seen, &mut Vec::new(), true) {
            all.push(n);
        }
    }
    all.extend(dialogs);
    let text = page_with_dialogs(&all, &root);
    Ok((!text.trim().is_empty()).then_some(text))
}

/// The window's document text, capped at `max_chars`.
pub(crate) fn document_text(
    window: WindowRef,
    max_chars: usize,
    displays: &[Rect],
) -> BackendResult<Option<String>> {
    perm::require_accessibility("read document text")?;
    let text = match text_element(
        window,
        displays,
        Instant::now() + Duration::from_millis(1500),
    )? {
        Some((_, t)) => Some(t),
        // a browser page: its words are in the web view's tree, not in a text area
        None => web_text(window, displays)?,
    };
    Ok(text.map(|t| t.chars().take(max_chars.min(MAX_DOC_CHARS)).collect()))
}

fn visible_rect(ax: &Ax, location: usize, length: usize, win: Rect) -> Option<Rect> {
    let r = ax.bounds_for_range(location as isize, length as isize)?;
    let rect = rect_from_f64(r.origin.x, r.origin.y, r.size.width, r.size.height);
    (!rect.is_empty() && rect.intersect(&win).is_some()).then_some(rect)
}

/// Matches for `query` in the window's document text.
pub(crate) fn find_in_documents(
    window: WindowRef,
    query: &str,
    max_hits: usize,
    displays: &[Rect],
    budget: Duration,
) -> BackendResult<Vec<DocumentHit>> {
    let deadline = Instant::now() + budget;
    let remaining = || remaining_budget(deadline, Instant::now()).ok_or(BackendError::Timeout);
    remaining()?;
    perm::require_accessibility("search document text")?;
    let Some((ax, text)) = text_element(window, displays, deadline)? else {
        return Ok(Vec::new());
    };
    remaining()?;
    let win_rect = winlist::info(window, displays)?.rect;
    let matches = find_matches_checked(&text, query, max_hits, &|| remaining().is_ok())
        .ok_or(BackendError::Timeout)?;
    let mut hits = Vec::with_capacity(matches.len());
    for m in matches {
        remaining()?;
        let match_rect = visible_rect(&ax, m.start, m.len, win_rect);
        remaining()?;
        let line_rect = visible_rect(&ax, m.line_start, m.line_end - m.line_start, win_rect);
        remaining()?;
        hits.push(DocumentHit {
            match_rect,
            line_rect,
            line: m.line,
        });
    }
    Ok(hits)
}

/// The document's own text of the line at a screen point.
pub(crate) fn line_at(window: WindowRef, p: Point) -> BackendResult<Option<String>> {
    perm::require_accessibility("read the line under the pointer")?;
    let sys = Ax::system_wide();
    let Ok(mut el) = sys.element_at(p.x as f32, p.y as f32) else {
        return Ok(None);
    };
    for _ in 0..4 {
        if window_of(&el).is_some_and(|w| w != window) {
            return Ok(None);
        }
        let role = el.string("AXRole").unwrap_or_default();
        if is_text_input(&role) {
            break;
        }
        match el.element("AXParent") {
            Some(parent) => el = parent,
            None => return Ok(None),
        }
    }
    let Some(text) = el
        .copy("AXValue")
        .ok()
        .and_then(|v| cf::string_of(&v))
        .filter(|t| !t.is_empty())
    else {
        return Ok(None);
    };
    let Some(range) = el.range_for_position(objc2_core_foundation::CGPoint {
        x: f64::from(p.x),
        y: f64::from(p.y),
    }) else {
        return Ok(None);
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    let (start, end) = line_range(&units, range.location.max(0) as usize);
    let line = slice16(&units, start, end);
    let line = line.trim();
    Ok((!line.is_empty()).then(|| line.to_string()))
}

/// The text cursor on screen.
pub(crate) fn caret() -> BackendResult<Option<Caret>> {
    perm::require_accessibility("locate the text cursor")?;
    let Some(ax) = focused_ax() else {
        return Ok(None);
    };
    let role = ax.string("AXRole").unwrap_or_default();
    if !is_text_input(&role) {
        return Ok(None);
    }
    let Some(range) = ax
        .copy("AXSelectedTextRange")
        .ok()
        .and_then(|v| cf::ax_range(&v))
    else {
        return Ok(None);
    };
    let at = (range.location + range.length).max(0);
    let rect = |loc: isize, len: isize| {
        ax.bounds_for_range(loc, len)
            .filter(|r| r.size.height > 0.0)
    };
    // An empty range often reports nothing: fall back to the glyph before or after the caret.
    if let Some(r) = rect(at, 0) {
        return Ok(Some(caret_at(r.origin.x, r.origin.y, r.size.height)));
    }
    if at > 0
        && let Some(r) = rect(at - 1, 1)
    {
        return Ok(Some(caret_at(
            r.origin.x + r.size.width,
            r.origin.y,
            r.size.height,
        )));
    }
    Ok(rect(at, 1).map(|r| caret_at(r.origin.x, r.origin.y, r.size.height)))
}

fn caret_at(x: f64, y: f64, h: f64) -> Caret {
    Caret {
        x: x.round() as i32,
        y: y.round() as i32,
        height: h.round() as i32,
    }
}

/// Does a text control have focus (in `window`'s app, or anywhere)?
pub(crate) fn has_caret(window: Option<WindowRef>) -> BackendResult<bool> {
    perm::require_accessibility("check for a text cursor")?;
    let focus = match window {
        Some(w) => {
            let (pid, _) = window_parts(w);
            Ax::app(pid).element("AXFocusedUIElement")
        }
        None => focused_ax(),
    };
    Ok(focus
        .and_then(|f| f.string("AXRole"))
        .is_some_and(|r| is_text_input(&r)))
}

/// A value that changes when keyboard focus moves to another control.
pub(crate) fn focus_token() -> u64 {
    if !crate::ax::trusted() {
        return apps::frontmost_pid().map_or(0, |p| p as u64);
    }
    focused_ax().map_or(0, |a| a.identity_hash())
}

/// Put `text` at the caret of the focused native text control through Accessibility, the way a
/// paste lands but with no clipboard, no key events and no focus needed. Returns false, having
/// changed nothing, when the control is not a plain native text box (web content, Electron, a
/// password field, a single-line box asked for a newline) or the app ignored the write; the caller
/// then types or pastes. 2026-10-29: 3 chars took 142 ms keyed, ~10 ms this way.
pub(crate) fn insert_text(text: &str) -> BackendResult<bool> {
    if text.is_empty() || !crate::ax::trusted() {
        return Ok(false);
    }
    let Some(ax) = focused_ax() else {
        return Ok(false);
    };
    let Ok(p) = ax.fetch() else {
        return Ok(false);
    };
    if !matches!(
        p.role.as_str(),
        "AXTextArea" | "AXTextField" | "AXSearchField" | "AXComboBox"
    ) || is_secure_field(&p.role, p.subrole.as_deref())
        || (text.contains('\n') && p.role != "AXTextArea")
        || in_web_content(&ax)
        || !ax.pid().is_some_and(appkit_text_app)
        || !ax.is_settable("AXSelectedText")
    {
        return Ok(false);
    }
    let count = |a: &Ax| {
        a.copy("AXNumberOfCharacters")
            .ok()
            .and_then(|v| cf::i64_of(&v))
    };
    let Some(before) = count(&ax) else {
        return Ok(false);
    };
    let selected = ax
        .copy("AXSelectedTextRange")
        .ok()
        .and_then(|v| cf::ax_range(&v))
        .map_or(0, |r| r.length as i64);
    if ax.set_string("AXSelectedText", text) != objc2_application_services::AXError::Success {
        return Ok(false);
    }
    let want = before - selected + crate::textops::utf16_len(text) as i64;
    match count(&ax) {
        Some(after) if after == want => Ok(true),
        // Unchanged: the app took the write and dropped it (some custom views). Nothing went in, so
        // the keyboard path is safe to try.
        Some(after) if after == before && selected == 0 => Ok(false),
        // Something changed: typing again would double it.
        _ => Ok(true),
    }
}

/// Does this app draw its text boxes with AppKit, whose `AXSelectedText` write is a real edit?
/// Chromium and Electron apps (their own views even outside a page: Brave's address bar took the
/// write and kept its old URL, 2026-10-29) and Firefox get real keys. Cached per pid.
fn appkit_text_app(pid: i32) -> bool {
    static CACHE: std::sync::Mutex<Vec<(i32, bool)>> = std::sync::Mutex::new(Vec::new());
    if let Some(&(_, ok)) = CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(p, _)| *p == pid)
    {
        return ok;
    }
    let ok = crate::apps::bundle_path(pid).is_some_and(|path| {
        !crate::textops::own_text_engine(
            &crate::apps::bundle_id(pid),
            std::fs::read_dir(std::path::Path::new(&path).join("Contents/Frameworks"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned()),
        )
    });
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() > 64 {
        cache.clear();
    }
    cache.push((pid, ok));
    ok
}

/// Is this element inside a web page (a browser tab, an Electron app)? Their editors take an
/// `AXSelectedText` write without firing the page's input events, so they get real keys.
fn in_web_content(ax: &Ax) -> bool {
    let mut cur = ax.element("AXParent");
    for _ in 0..20 {
        let Some(el) = cur else {
            return false;
        };
        if el.string("AXRole").as_deref() == Some("AXWebArea") {
            return true;
        }
        cur = el.element("AXParent");
    }
    false
}
