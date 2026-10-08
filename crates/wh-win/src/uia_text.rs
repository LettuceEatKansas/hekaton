//! Text documents through the UI Automation TextPattern: a web page in a browser, a rich editor.
//! The document knows its real characters, so a search or a line read here is exact where OCR
//! guesses (2026-09-21 h-longsite: the page said TII5ML, OCR read T115LML).
//!
//! Python reference: `computer.py:_text_docs`, `_doc_text_uia`, `_doc_text_hits`, `_exact_line_at`.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use wh_core::backend::{BackendResult, DocumentHit, Point, WindowRef};
use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Ole::{
    SafeArrayDestroy, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    IUIAutomationElement, IUIAutomationTextPattern, IUIAutomationTextRange,
    IUIAutomationValuePattern, TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start,
    TextUnit_Line, TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_TextPatternId,
    UIA_ValuePatternId,
};
use windows::core::BSTR;

use crate::model::{self, ct};
use crate::uia::{self, Uia, uia, win_point};
use crate::util::hwnd_of;
use crate::wnd;

/// Longest line text read for a hit or a line lookup (`GetText(400)`).
const LINE_CHARS: i32 = 400;

// ------------------------------------------------------------------------------------ small reads

fn text_pattern(el: &IUIAutomationElement) -> Option<IUIAutomationTextPattern> {
    // SAFETY: asks a live element for its text pattern; absent is an error.
    unsafe { el.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) }.ok()
}

fn document_range(tp: &IUIAutomationTextPattern) -> Option<IUIAutomationTextRange> {
    // SAFETY: the document range of a live text pattern.
    unsafe { tp.DocumentRange() }.ok()
}

/// A range's text, up to `max` characters; "" when it cannot say.
fn range_text(range: &IUIAutomationTextRange, max: i32) -> String {
    // SAFETY: reads text from a live range.
    unsafe { range.GetText(max) }
        .map(|b| b.to_string())
        .unwrap_or_default()
}

/// The `left, top, width, height` doubles of a range's on-screen boxes, empty when it is scrolled
/// out of view.
fn range_rects(range: &IUIAutomationTextRange) -> Vec<f64> {
    // SAFETY: the call returns a SAFEARRAY we own.
    let Ok(sa) = (unsafe { range.GetBoundingRectangles() }) else {
        return Vec::new();
    };
    if sa.is_null() {
        return Vec::new();
    }
    // SAFETY: `sa` is a live one-dimensional array of doubles, read once and destroyed once.
    let out = unsafe { safe_array_doubles(sa) };
    // SAFETY: as above; nothing reads the array after this.
    let _ = unsafe { SafeArrayDestroy(sa) };
    out
}

/// Copy a one-dimensional array of doubles out of a SAFEARRAY.
///
/// # Safety
/// `sa` must point to a live one-dimensional SAFEARRAY of `f64`.
unsafe fn safe_array_doubles(sa: *const SAFEARRAY) -> Vec<f64> {
    // SAFETY: the caller guarantees a live one-dimensional array.
    let (lo, hi) = unsafe {
        (
            SafeArrayGetLBound(sa, 1).unwrap_or(0),
            SafeArrayGetUBound(sa, 1).unwrap_or(-1),
        )
    };
    let mut out = Vec::new();
    for i in lo..=hi {
        let mut v = 0.0f64;
        // SAFETY: `i` is inside the bounds just read and `v` is room for one double.
        if unsafe { SafeArrayGetElement(sa, &i, std::ptr::from_mut(&mut v).cast::<c_void>()) }
            .is_ok()
        {
            out.push(v);
        }
    }
    out
}

/// A copy of `range` widened to the line it sits on.
fn line_range(range: &IUIAutomationTextRange) -> Option<IUIAutomationTextRange> {
    // SAFETY: clones a live range.
    let line = unsafe { range.Clone() }.ok()?;
    // SAFETY: widens the clone, not the original. A refusal leaves it as it was.
    let _ = unsafe { line.ExpandToEnclosingUnit(TextUnit_Line) };
    Some(line)
}

/// The document's own text of the line a range sits on, whitespace collapsed; the range's own text
/// when the line cannot be read.
fn line_text(range: &IUIAutomationTextRange) -> String {
    let widened = line_range(range).map(|l| range_text(&l, LINE_CHARS));
    let text = match widened {
        Some(t) if !t.is_empty() => t,
        _ => range_text(range, LINE_CHARS),
    };
    model::collapse_ws(&text)
}

fn cap(max_chars: usize) -> i32 {
    i32::try_from(max_chars).unwrap_or(i32::MAX)
}

/// TextPatterns of the Document elements in a window, found by UIA's own search (it runs inside UIA,
/// so a page deep under a browser's toolbars is reached at once). The cap counts documents found,
/// as Python's did.
fn text_docs(u: &Uia, window: WindowRef, max_n: usize) -> Vec<IUIAutomationTextPattern> {
    let hwnd = hwnd_of(window);
    // SAFETY: the window may have closed; a failure just means no documents.
    let Ok(root) = (unsafe { u.auto.ElementFromHandle(hwnd) }) else {
        return Vec::new();
    };
    // SAFETY: builds a condition for the Document control type.
    let Ok(cond) = (unsafe {
        u.auto
            .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(ct::DOCUMENT))
    }) else {
        return Vec::new();
    };
    // SAFETY: a descendant search of a live element.
    let Ok(found) = (unsafe { root.FindAll(TreeScope_Descendants, &cond) }) else {
        return Vec::new();
    };
    // SAFETY: reading the length of the array just returned.
    let n = unsafe { found.Length() }.unwrap_or(0).max(0);
    (0..n.min(i32::try_from(max_n).unwrap_or(i32::MAX)))
        .filter_map(|i| {
            // SAFETY: an index inside the array's length.
            unsafe { found.GetElement(i) }.ok()
        })
        .filter_map(|el| text_pattern(&el))
        .collect()
}

// ------------------------------------------------------------------------------------ public

/// The text of the focused element, if it has any: its TextPattern, else its value.
fn focused_text(u: &Uia, window: WindowRef, max_chars: usize) -> Option<String> {
    // SAFETY: the focused element, if any.
    let focus = unsafe { u.auto.GetFocusedElement() }.ok()?;
    // Only this window's own focused control counts. A browser's page element has no window handle
    // of its own, so its window is found through its parents (Python let it through unchecked).
    let wanted = wnd::root_hwnd(hwnd_of(window)).unwrap_or_else(|| hwnd_of(window));
    if let Some(top) = uia::top_level_of(u, &focus, None)
        && top != wanted
    {
        return None;
    }
    if let Some(range) = text_pattern(&focus).and_then(|tp| document_range(&tp)) {
        let text = range_text(&range, cap(max_chars));
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    // SAFETY: asks the focused element for its value pattern.
    let vp = unsafe { focus.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
        .ok()?;
    // SAFETY: reads the value through the pattern just fetched.
    let value = unsafe { vp.CurrentValue() }.ok()?.to_string();
    (!value.trim().is_empty()).then_some(value)
}

/// The whole text of a window's first document or, failing that, of the focused text control; `None`
/// when the window exposes no text (`_doc_text_uia`).
pub(crate) fn document_text(window: WindowRef, max_chars: usize) -> BackendResult<Option<String>> {
    let raw = window.raw();
    uia::with_deadline(
        Duration::from_secs(4),
        move || -> BackendResult<Option<String>> {
            let u = uia()?;
            let window = WindowRef::from_raw(raw);
            for tp in text_docs(u, window, 1) {
                if let Some(range) = document_range(&tp) {
                    let text = range_text(&range, cap(max_chars));
                    if !text.trim().is_empty() {
                        return Ok(Some(text));
                    }
                }
            }
            Ok(focused_text(u, window, max_chars))
        },
    )?
}

/// Matches for `query` in one document, from its own text. Stops at `max_hits` or the deadline.
fn search_document(
    tp: &IUIAutomationTextPattern,
    needle: &BSTR,
    max_hits: usize,
    deadline: Instant,
) -> Vec<DocumentHit> {
    let mut hits = Vec::new();
    let Some(doc) = document_range(tp) else {
        return hits;
    };
    // SAFETY: a clone of the document range to search forward through.
    let Ok(search) = (unsafe { doc.Clone() }) else {
        return hits;
    };
    let mut rounds = 0;
    while hits.len() < max_hits && Instant::now() < deadline && rounds < max_hits + 64 {
        rounds += 1;
        // SAFETY: a forward, case-insensitive search; no match is an error.
        let Ok(hit) = (unsafe { search.FindText(needle, false, true) }) else {
            break;
        };
        let line = line_range(&hit);
        let text = match &line {
            Some(l) => {
                let t = range_text(l, LINE_CHARS);
                if t.is_empty() {
                    range_text(&hit, LINE_CHARS)
                } else {
                    t
                }
            }
            None => range_text(&hit, LINE_CHARS),
        };
        hits.push(DocumentHit {
            line: model::collapse_ws(&text),
            match_rect: model::first_rect(&range_rects(&hit)),
            line_rect: line.and_then(|l| model::first_rect(&range_rects(&l))),
        });
        // carry on after this match
        // SAFETY: moves the search range's start to the end of the hit.
        let moved = unsafe {
            search.MoveEndpointByRange(
                TextPatternRangeEndpoint_Start,
                &hit,
                TextPatternRangeEndpoint_End,
            )
        };
        if moved.is_err() {
            break;
        }
    }
    hits
}

/// Matches for `query` in the window's documents, from their own text (`_doc_text_hits`). The first
/// document that has any wins. A hit scrolled out of view has no boxes.
pub(crate) fn find_in_documents(
    window: WindowRef,
    query: &str,
    max_hits: usize,
    budget: Duration,
) -> BackendResult<Vec<DocumentHit>> {
    if query.is_empty() || max_hits == 0 {
        return Ok(Vec::new());
    }
    let raw = window.raw();
    let query = query.to_string();
    let deadline = Instant::now() + budget;
    uia::with_deadline(
        budget + Duration::from_millis(500),
        move || -> BackendResult<Vec<DocumentHit>> {
            let u = uia()?;
            let needle = BSTR::from(query.as_str());
            for tp in text_docs(u, WindowRef::from_raw(raw), 4) {
                let hits = search_document(&tp, &needle, max_hits, deadline);
                if !hits.is_empty() {
                    return Ok(hits);
                }
            }
            Ok(Vec::new())
        },
    )?
}

/// The document's own text of the line at a screen point (`_exact_line_at`).
pub(crate) fn document_line_at(window: WindowRef, p: Point) -> BackendResult<Option<String>> {
    let raw = window.raw();
    uia::with_deadline(
        Duration::from_millis(1500),
        move || -> BackendResult<Option<String>> {
            let u = uia()?;
            for tp in text_docs(u, WindowRef::from_raw(raw), 4) {
                // SAFETY: asks a live text pattern for the range at a point.
                let Ok(range) = (unsafe { tp.RangeFromPoint(win_point(p)) }) else {
                    continue;
                };
                let text = line_text(&range);
                if !text.is_empty() {
                    return Ok(Some(text));
                }
            }
            Ok(None)
        },
    )?
}
