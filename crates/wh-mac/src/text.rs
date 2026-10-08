//! Text through Accessibility: a window's document text, matches with their screen boxes, the line
//! under a point, the caret. `AXValue` strings and `AXBoundsForRange` both count UTF-16 units, the
//! same unit the pure helpers in `textops` use.

use std::time::Duration;

use wh_core::backend::{BackendResult, Caret, DocumentHit, Point, Rect, WindowRef};

use crate::apps;
use crate::ax::Ax;
use crate::cf;
use crate::geom::rect_from_f64;
use crate::perm;
use crate::roles::{is_secure_field, is_text_input};
use crate::textops::{find_matches, line_range, slice16};
use crate::tree::{ax_window, find_first, focused_ax, window_of};
use crate::wininfo::window_parts;
use crate::winlist;

/// Largest document text read, in characters.
const MAX_DOC_CHARS: usize = 2_000_000;

/// The element holding the window's document text, with the text.
fn text_element(window: WindowRef, displays: &[Rect]) -> BackendResult<Option<(Ax, String)>> {
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
        return Ok(Some((focus, text)));
    }
    let win = ax_window(window, displays)?;
    let found = find_first(&win, 12, Duration::from_millis(700), &|p| {
        p.role == "AXTextArea" && p.value_text().is_some_and(|t| !t.is_empty())
    });
    Ok(found.and_then(|(ax, p)| p.value_text().map(|t| (ax, t))))
}

/// The window's document text, capped at `max_chars`.
pub(crate) fn document_text(
    window: WindowRef,
    max_chars: usize,
    displays: &[Rect],
) -> BackendResult<Option<String>> {
    perm::require_accessibility("read document text")?;
    Ok(text_element(window, displays)?
        .map(|(_, t)| t.chars().take(max_chars.min(MAX_DOC_CHARS)).collect()))
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
) -> BackendResult<Vec<DocumentHit>> {
    perm::require_accessibility("search document text")?;
    let Some((ax, text)) = text_element(window, displays)? else {
        return Ok(Vec::new());
    };
    let win_rect = winlist::info(window, displays)?.rect;
    Ok(find_matches(&text, query, max_hits)
        .into_iter()
        .map(|m| DocumentHit {
            match_rect: visible_rect(&ax, m.start, m.len, win_rect),
            line_rect: visible_rect(&ax, m.line_start, m.line_end - m.line_start, win_rect),
            line: m.line,
        })
        .collect())
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
