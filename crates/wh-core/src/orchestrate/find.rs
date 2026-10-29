//! Finding and reading text on screen. Port of the text half of `computer.py`: `_uia_elements`,
//! `_locate_text`, `_find_text`, `_doc_text_hits`, `_doc_find_note`, `_lookalike_note`,
//! `_zoom_text`, `_ocr_zoom_items`, `_group_into_rows`, `_read_field_uia`, `_read_text`,
//! `_doc_text_by_copy`, `_snap_to_word`, and the context-menu helpers (`_menu_items`, `_wait_popup`,
//! `_right_click_menu_items`, `_menu_note`).
//!
//! Order of trust, everywhere: the document's own characters (exact), then the accessibility tree's
//! names (exact), then OCR (a guess, flagged when it could be confusing 1, l and I).

use std::sync::LazyLock;

use regex::Regex;

use super::error::{StepError, StepResult};
use super::{Ctx, secs};
use crate::backend::{ClipboardContent, Element, MenuItem, Point, Rect, TreeLimits, WindowRef};
use crate::textutil::py_repr;

// ------------------------------------------------------------------------------------ element names

/// A named accessibility element with bounds and, when it has one, a value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UiaElem {
    pub name: String,
    pub rect: Rect,
    pub value: Option<String>,
    pub has_value: bool,
}

/// A place on screen that matches a query (Python `(cx, cy, source, label)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cand {
    pub x: i32,
    pub y: i32,
    pub source: &'static str,
    pub label: String,
}

/// One line of a zoom's text: `(cy, cx, height, label)`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RowItem {
    pub cy: f64,
    pub cx: f64,
    pub h: f64,
    pub label: String,
}

fn keep(e: &Element, out: &mut Vec<UiaElem>) {
    let name = e.name.trim();
    let Some(rect) = e.rect.filter(|r| !r.is_empty()) else {
        return;
    };
    if name.is_empty() {
        return;
    }
    out.push(UiaElem {
        name: name.to_string(),
        rect,
        value: e.value.clone(),
        has_value: e.value.is_some() || e.affordances.contains(&crate::backend::Affordance::Value),
    });
}

/// OCR swaps these; a code copied off the screen needs a second look at them.
fn lookalike_group(ch: char) -> Option<&'static str> {
    Some(match ch {
        'I' | 'l' | '1' | '|' => "I/l/1",
        'O' | '0' => "O/0",
        'S' | '5' => "S/5",
        'B' | '8' => "B/8",
        _ => return None,
    })
}

static CODE_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9|]+(?:-[A-Za-z0-9|]+)*").expect("CODE_TOKEN"));

/// A caution line naming code-like words (letters mixed with digits, or long all-capitals) whose
/// characters OCR commonly confuses, or "" when there are none. Only for OCR output: UIA text is exact.
/// Python `_lookalike_note`.
pub(crate) fn lookalike_note(texts: &[String]) -> String {
    let mut flagged: Vec<(String, Vec<&'static str>)> = Vec::new();
    for t in texts {
        for m in CODE_TOKEN.find_iter(t) {
            let tok = m.as_str();
            let bare: String = tok.chars().filter(|c| *c != '-').collect();
            if bare.chars().count() < 4 {
                continue;
            }
            let has_digit = bare.chars().any(|c| c.is_ascii_digit());
            let has_alpha = bare.chars().any(|c| c.is_alphabetic());
            let mut groups: Vec<&'static str> = bare.chars().filter_map(lookalike_group).collect();
            groups.sort_unstable();
            groups.dedup();
            let caps = bare.chars().any(|c| c.is_alphabetic())
                && !bare.chars().any(|c| c.is_lowercase())
                && bare.chars().count() >= 5
                && bare
                    .chars()
                    .filter(|c| lookalike_group(*c).is_some())
                    .count()
                    >= 2;
            if !groups.is_empty()
                && ((has_digit && has_alpha) || caps)
                && !flagged.iter().any(|(seen, _)| seen == tok)
            {
                flagged.push((tok.to_string(), groups));
            }
        }
    }
    if flagged.is_empty() {
        return String::new();
    }
    let parts = flagged
        .iter()
        .take(6)
        .map(|(tok, g)| format!("{tok} ({})", g.join(", ")))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "\nOCR caution: these code-like words hold characters OCR often swaps: {parts}. Before \
         copying one, zoom a tight box around just that word (it renders larger) and compare; if the \
         reads disagree, say which characters are uncertain."
    )
}

/// Total size of the blocks `difflib.SequenceMatcher(None, a, b, autojunk=False)` finds: the longest
/// common block, then the same on each side of it.
fn matching_chars(a: &[char], b: &[char]) -> usize {
    fn longest(
        a: &[char],
        b: &[char],
        alo: usize,
        ahi: usize,
        blo: usize,
        bhi: usize,
    ) -> (usize, usize, usize) {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut prev = vec![0usize; bhi - blo + 1];
        for (i, ca) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut cur = vec![0usize; bhi - blo + 1];
            for (j, cb) in b.iter().enumerate().take(bhi).skip(blo) {
                if ca == cb {
                    let k = prev[j - blo] + 1;
                    cur[j - blo + 1] = k;
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            prev = cur;
        }
        (besti, bestj, bestsize)
    }
    fn total(a: &[char], b: &[char], alo: usize, ahi: usize, blo: usize, bhi: usize) -> usize {
        if alo >= ahi || blo >= bhi {
            return 0;
        }
        let (i, j, k) = longest(a, b, alo, ahi, blo, bhi);
        if k == 0 {
            return 0;
        }
        k + total(a, b, alo, i, blo, j) + total(a, b, i + k, ahi, j + k, bhi)
    }
    total(a, b, 0, a.len(), 0, b.len())
}

/// Does the exact line contain (most of) what OCR read, in order? Guards against `RangeFromPoint`
/// landing on a neighbouring line. Python `_same_line`.
pub(crate) fn same_line(ocr: &str, exact: &str) -> bool {
    let a: Vec<char> = ocr.to_lowercase().trim().chars().collect();
    let b: Vec<char> = exact.to_lowercase().chars().collect();
    if a.is_empty() {
        return false;
    }
    matching_chars(&a, &b) as f64 / a.len() as f64 >= 0.7
}

/// Items in reading order, one screen row per line. OCR (and a UIA tree walk) can return a table
/// column by column, so a list of names came back before its dates and a model paired "echo" with the
/// wrong date (2026-09-21 eval). Python `_group_into_rows`.
pub(crate) fn group_into_rows(items: &[RowItem]) -> Vec<String> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&RowItem> = items.iter().collect();
    sorted.sort_by(|a, b| {
        a.cy.partial_cmp(&b.cy)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cx.partial_cmp(&b.cx).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.h.partial_cmp(&b.h).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.label.cmp(&b.label))
    });
    let mut table: Vec<Vec<&RowItem>> = Vec::new();
    let mut row: Vec<&RowItem> = vec![sorted[0]];
    for it in &sorted[1..] {
        let last = row.last().copied().unwrap_or(sorted[0]);
        if (it.cy - last.cy).abs() < 0.6 * last.h.max(8.0) {
            row.push(it);
        } else {
            table.push(std::mem::replace(&mut row, vec![it]));
        }
    }
    table.push(row);
    table
        .into_iter()
        .map(|mut r| {
            r.sort_by(|a, b| a.cx.partial_cmp(&b.cx).unwrap_or(std::cmp::Ordering::Equal));
            r.iter()
                .map(|t| t.label.as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .collect()
}

impl Ctx<'_> {
    /// `[x, y]` of a screen point in the model's coordinates, as it prints (Python
    /// `from_screen(x, y)` inside an f-string).
    pub(crate) fn pt(&self, x: i32, y: i32) -> String {
        let [a, b] = self.scale().from_screen(x, y);
        format!("[{a}, {b}]")
    }

    // ------------------------------------------------------------------ accessibility names

    /// Every named UI Automation element under `window` as (name, bounds), time-boxed so a huge tree
    /// (a browser, a dense dialog) cannot stall a click. UIA runs before OCR for ordinary controls,
    /// which it names and locates exactly; OCR stays as the fallback for canvas-drawn text (games,
    /// custom-rendered UI) that exposes nothing to the tree. Python `_uia_elements`.
    pub(crate) fn uia_elements(
        &self,
        window: Option<WindowRef>,
        max_s: Option<f64>,
    ) -> Vec<UiaElem> {
        let Some(w) = window else {
            return Vec::new();
        };
        let budget = max_s.unwrap_or_else(|| self.cfg().uia_time_box_s());
        let Ok(tree) = self.be().element_tree(w, &TreeLimits::named(secs(budget))) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        keep(&tree.root, &mut out);
        for e in &tree.root.children {
            keep(e, &mut out);
        }
        out
    }

    // ------------------------------------------------------------------ context menus

    /// Visible top-level popup windows that can hold a menu. One cheap enumeration, safe to poll.
    pub(crate) fn popup_menus(&self) -> Vec<WindowRef> {
        self.be().popup_menus().unwrap_or_default()
    }

    /// Items of an open context menu, via UIA, polling up to `wait_s` for one to appear. Windows draws
    /// a right-click menu as its own top-level popup (not a child of the app's window), so this looks
    /// at popup windows, then the desktop's direct children.
    ///
    /// 2026-09-21: after right_click a model always had to zoom and read menu labels off the picture.
    /// The file menu in Explorer came back with no items at all, because this read once, immediately
    /// after the click, before a menu with shell extensions had appeared. Python `_menu_items`.
    pub(crate) fn menu_items(&self, wait_s: f64, max_s: f64) -> Vec<MenuItem> {
        let end = self.now() + wait_s.max(0.0);
        loop {
            if self.check_cancelled().is_err() {
                return Vec::new();
            }
            let items = self.menu_items_once(max_s);
            if !items.is_empty() || self.now() >= end {
                return items;
            }
            self.sleep(0.08);
        }
    }

    pub(crate) fn menu_items_once(&self, max_s: f64) -> Vec<MenuItem> {
        self.be().menu_items(secs(max_s)).unwrap_or_default()
    }

    /// Poll for a popup-menu window for up to `wait_s`. One window enumeration, cheap, unlike a UIA
    /// read of a menu. Python `_wait_popup`.
    pub(crate) fn wait_popup(&self, wait_s: f64, step_s: f64) -> bool {
        let end = self.now() + wait_s;
        loop {
            if self.check_cancelled().is_err() {
                return false;
            }
            if !self.popup_menus().is_empty() {
                return true;
            }
            if self.now() >= end {
                return false;
            }
            self.sleep(step_s);
        }
    }

    /// Items of the menu a right_click opened, without paying for a menu that never came.
    ///
    /// 2026-10-08 (speed fix W5): right_click used to wait a full second for items even when no menu
    /// opened (a right-click on a game surface or a blank canvas). Now it waits about 200 ms for a
    /// popup window; if one shows up it keeps the old behaviour (Explorer's file menu builds its
    /// shell-extension items first, so read with up to 1 s of patience). If none shows up, one quick
    /// UIA look at the desktop's top-level children still catches a menu drawn in a window whose class
    /// is not popup-like (Chromium and Electron draw theirs as plain top-level windows). Python
    /// `_right_click_menu_items`.
    pub(crate) fn right_click_menu_items(&self) -> Vec<MenuItem> {
        if self.wait_popup(0.2, 0.03) {
            self.menu_items(1.0, 0.4)
        } else {
            self.menu_items_once(0.1)
        }
    }

    /// `Open @[x, y], Copy @[x, y], ...` with click points in the model's coordinates.
    pub(crate) fn menu_note(&self, items: &[MenuItem]) -> String {
        items
            .iter()
            .take(24)
            .map(|m| {
                let name = m.name.replace('\t', " ");
                let r = m.rect;
                if r.right > r.left && r.bottom > r.top {
                    let c = r.center();
                    format!("{name} @{}", self.pt(c.x, c.y))
                } else {
                    name
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    // ------------------------------------------------------------------ locating text

    /// Every on-screen candidate for `query`: the open context menu first (it is on top of everything
    /// and is what a person reads first; it is its own window, so the foreground-window walk never
    /// sees it), then UIA element names in the foreground window, then OCR words if UIA found
    /// nothing. Python `_locate_text`.
    pub(crate) fn locate_text(&self, query: &str) -> StepResult<Vec<Cand>> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        if !self.popup_menus().is_empty() {
            let items: Vec<MenuItem> = self
                .menu_items(0.0, 0.3)
                .into_iter()
                .filter(|m| m.rect.right > m.rect.left && m.rect.bottom > m.rect.top)
                .collect();
            // a classic item reads "Undo Rename<TAB>Ctrl+Z": the label is the part before the tab
            let mut hits: Vec<&MenuItem> = items
                .iter()
                .filter(|m| {
                    m.name
                        .split('\t')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_lowercase()
                        == q
                })
                .collect();
            if hits.is_empty() {
                hits = items
                    .iter()
                    .filter(|m| m.name.to_lowercase().contains(&q))
                    .collect();
            }
            let cands: Vec<Cand> = hits
                .iter()
                .map(|m| {
                    let c = m.rect.center();
                    Cand {
                        x: c.x,
                        y: c.y,
                        source: "menu",
                        label: m.name.clone(),
                    }
                })
                .collect();
            if !cands.is_empty() {
                return Ok(cands);
            }
        }
        let cands: Vec<Cand> = self
            .uia_elements(self.fg_root(), None)
            .into_iter()
            .filter(|e| e.name.to_lowercase().contains(&q))
            // scoped: the title bar is not in view
            .filter(|e| {
                let c = e.rect.center();
                self.in_view(f64::from(c.x), f64::from(c.y))
            })
            .map(|e| {
                let c = e.rect.center();
                Cand {
                    x: c.x,
                    y: c.y,
                    source: "uia",
                    label: e.name,
                }
            })
            .collect();
        if !cands.is_empty() {
            return Ok(cands);
        }
        let display = self.display_rect();
        let rows = self
            .grab(display)
            .ok()
            .and_then(|img| {
                let (origin, scale) = (img.origin(), img.scale());
                self.ocr_rows_of(&img, true)
                    .map(|rows| (origin, scale, rows))
            })
            .unwrap_or((Point::default(), 1.0, Vec::new()));
        let (origin, scale, rows) = rows;
        let mut out = Vec::new();
        for row in rows {
            let word = row.text.trim();
            if word.to_lowercase().contains(&q) {
                let mx = f64::from(row.rect.left + row.rect.right) / 2.0 / scale;
                let my = f64::from(row.rect.top + row.rect.bottom) / 2.0 / scale;
                out.push(Cand {
                    x: origin.x + mx as i32,
                    y: origin.y + my as i32,
                    source: "ocr",
                    label: word.to_string(),
                });
            }
        }
        Ok(out)
    }

    /// Verify an action's effect: `expect={"text":.., "gone_text":.., "title":..}`. Any mismatch is a
    /// FAIL, so a batch can stop there instead of building on a wrong screen state. Python
    /// `_check_expect`.
    #[cfg(test)]
    pub(crate) fn check_expect(&self, expect: &serde_json::Value) -> StepResult<String> {
        Ok(match self.check_expect_result(expect)? {
            None => "expect: PASS".into(),
            Some(reason) => format!("expect: FAIL ({reason})"),
        })
    }

    /// The verdict is data: None means pass, Some carries the mismatch reason.
    /// Callers must not recover success or failure by parsing the rendered explanation.
    pub(crate) fn check_expect_result(
        &self,
        expect: &serde_json::Value,
    ) -> StepResult<Option<String>> {
        let Some(map) = expect.as_object() else {
            return Ok(Some(
                "expect must be an object with text/gone_text/title".into(),
            ));
        };
        let pick = |key: &str| -> Option<String> {
            map.get(key)
                .filter(|v| crate::config::truthy(v))
                .map(crate::textutil::py_str)
        };
        let (title, text, gone) = (pick("title"), pick("text"), pick("gone_text"));
        let mut fails: Vec<String> = Vec::new();
        if let Some(t) = &title {
            let cur = self.fg_root().map(|w| self.title_of(w)).unwrap_or_default();
            if !cur.contains(&t.trim().to_lowercase()) {
                fails.push(format!("title is {}, wanted {}", py_repr(&cur), py_repr(t)));
            }
        }
        if let Some(t) = &text
            && self.locate_text(t)?.is_empty()
        {
            fails.push(format!("text {} not found", py_repr(t)));
        }
        if let Some(t) = &gone
            && !self.locate_text(t)?.is_empty()
        {
            fails.push(format!("text {} is still there", py_repr(t)));
        }
        if title.is_none() && text.is_none() && gone.is_none() {
            return Ok(Some(
                "expect needs at least one of text, gone_text, title".into(),
            ));
        }
        if fails.is_empty() {
            Ok(None)
        } else {
            Ok(Some(fails.join("; ")))
        }
    }

    // ------------------------------------------------------------------ documents

    /// Is the box's centre on the model's screen and inside the foreground window? (`_visible_box`)
    pub(crate) fn visible_box(&self, rect: Option<Rect>) -> bool {
        let Some(r) = rect else {
            return false;
        };
        let (cx, cy) = (
            f64::from(r.left + r.right) / 2.0,
            f64::from(r.top + r.bottom) / 2.0,
        );
        let d = self.display_rect();
        if !(f64::from(d.left) <= cx
            && cx < f64::from(d.left + d.width())
            && f64::from(d.top) <= cy
            && cy < f64::from(d.top + d.height()))
        {
            return false;
        }
        if let Some(info) = self.fg_root().and_then(|w| self.info(w))
            && info.rect.right > info.rect.left
        {
            let w = info.rect;
            return f64::from(w.left) <= cx
                && cx < f64::from(w.right)
                && f64::from(w.top) <= cy
                && cy < f64::from(w.bottom);
        }
        true
    }

    /// The "found N line(s)" note for matches taken straight from a document's text, and a picture
    /// around the first one that is on screen. Python `_doc_find_note`.
    fn doc_find_note(
        &self,
        query: &str,
        hits: &[crate::backend::DocumentHit],
    ) -> (String, Option<String>) {
        let mut lines = Vec::new();
        let mut first: Option<Rect> = None;
        for h in hits {
            let text = &h.line;
            if self.visible_box(h.match_rect) {
                let box_ = h.match_rect.unwrap_or_default();
                let anchor = if self.visible_box(h.line_rect) {
                    h.line_rect.unwrap_or(box_)
                } else {
                    box_
                };
                let ac = anchor.center();
                let bc = box_.center();
                lines.push(format!(
                    "{text} @{}   <- {} itself @{}",
                    self.pt(ac.x, ac.y),
                    py_repr(query.trim()),
                    self.pt(bc.x, bc.y)
                ));
                first = first.or(Some(anchor));
            } else {
                lines.push(format!(
                    "{text}   (not on screen now: scroll to it before clicking)"
                ));
            }
        }
        let img = first.and_then(|f| {
            let d = self.display_rect();
            let (w, h) = (f64::from(d.width()), f64::from(d.height()));
            let a = self.scale().from_screen(
                d.left.max(f.left - (0.03 * w) as i32),
                d.top.max(f.top - (0.04 * h) as i32),
            );
            let b = self.scale().from_screen(
                (d.left + d.width() - 1).min(f.left + (0.35 * w) as i32),
                (d.top + d.height() - 1).min(f.bottom + (0.04 * h) as i32),
            );
            self.screenshot_b64(Some([
                f64::from(a[0]),
                f64::from(a[1]),
                f64::from(b[0]),
                f64::from(b[1]),
            ]))
            .ok()
        });
        let note = format!(
            "found {} line(s) with {} (uia text: the document's own characters, exact, including \
             lines scrolled out of view); click points in full-screenshot coordinates:\n{}",
            hits.len(),
            py_repr(query),
            lines.join("\n")
        );
        (note, img)
    }

    /// Read the whole screen and return every line containing `query`, together with the text on the
    /// same row to its right, each with a click point, plus a zoom image around the first match.
    ///
    /// 2026-09-21 eval: to read "Developer: Mega Crit" off a Wikipedia infobox, a model aimed four
    /// zooms at the wrong height and finally typed its guess "MegaCrit". A person scans for the label
    /// and reads across; this does the same in one step. Python `_find_text`.
    pub(crate) fn find_text(&self, query: &str) -> StepResult<(String, Option<String>)> {
        const MAX_HITS: usize = 6;
        let d = self.display_rect();
        let (left, top, w) = (d.left, d.top, f64::from(d.width()));
        let h = f64::from(d.height());
        let q = query.trim().to_lowercase();
        let doc_hits = if q.is_empty() {
            Vec::new()
        } else {
            self.doc_text_hits(query.trim(), MAX_HITS)
        };
        if !doc_hits.is_empty() {
            return Ok(self.doc_find_note(query, &doc_hits));
        }
        let mut source = "ocr";
        // (left, top, right, bottom, text)
        let mut items: Vec<(f64, f64, f64, f64, String)> = self
            .uia_elements(self.fg_root(), None)
            .into_iter()
            .filter(|e| {
                // scoped: only what the window's picture shows
                self.in_view(
                    f64::from(e.rect.left + e.rect.right) / 2.0,
                    f64::from(e.rect.top + e.rect.bottom) / 2.0,
                )
            })
            .map(|e| {
                (
                    f64::from(e.rect.left),
                    f64::from(e.rect.top),
                    f64::from(e.rect.right),
                    f64::from(e.rect.bottom),
                    e.name,
                )
            })
            .collect();
        let mut hits: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.4.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .take(MAX_HITS)
            .collect();
        if !hits.is_empty() {
            source = "uia";
        } else {
            items.clear();
            if let Ok(img) = self.grab(d) {
                let (origin, scale) = (img.origin(), img.scale());
                for row in self.ocr_rows_of(&img, false).unwrap_or_default() {
                    let words = row.text.trim().to_string();
                    if !words.is_empty() {
                        items.push((
                            f64::from(origin.x) + f64::from(row.rect.left) / scale,
                            f64::from(origin.y) + f64::from(row.rect.top) / scale,
                            f64::from(origin.x) + f64::from(row.rect.right) / scale,
                            f64::from(origin.y) + f64::from(row.rect.bottom) / scale,
                            words,
                        ));
                    }
                }
            }
            hits = items
                .iter()
                .enumerate()
                .filter(|(_, it)| it.4.to_lowercase().contains(&q))
                .map(|(i, _)| i)
                .take(MAX_HITS)
                .collect();
        }
        if hits.is_empty() {
            return Ok((
                format!(
                    "no text on screen matches {} (tried UIA then OCR, may miss stylised text). Only \
                     what is on screen was searched: in a long document or page, read_text with the \
                     same text searches all of it. Or try a shorter word, scroll, or zoom",
                    py_repr(query)
                ),
                None,
            ));
        }
        let _ = (left, top);
        let mut out = Vec::new();
        for &i in &hits {
            let (x0, y0, x1, y1, words) = &items[i];
            let cy = (y0 + y1) / 2.0;
            let lh = (y1 - y0).max(8.0);
            let mut row: Vec<&(f64, f64, f64, f64, String)> = items
                .iter()
                .filter(|it| {
                    ((it.1 + it.3) / 2.0 - cy).abs() < 0.6 * lh
                        && it.0 >= x0 - 4.0
                        && it.0 < x0 + 0.45 * w
                })
                .collect();
            row.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let parts: Vec<String> = row
                .iter()
                .map(|it| {
                    format!(
                        "{} @{}",
                        it.4,
                        self.pt(((it.0 + it.2) / 2.0) as i32, ((it.1 + it.3) / 2.0) as i32)
                    )
                })
                .collect();
            let mut line = parts.join(" | ");
            let words_l = words.to_lowercase();
            let wlen = words.chars().count();
            let qlen = q.chars().count();
            if let Some(byte) = words_l.find(&q)
                && qlen < wlen
            {
                // The match inside a longer line: estimate its own spot from its place in the line, so
                // a double_click lands on the word. 2026-09-21: a model's double-click on "draft" in
                // "status: draft" hit the space before it and it typed over the wrong text.
                let idx = words_l[..byte].chars().count();
                let mx = x0 + (x1 - x0) * (idx as f64 + qlen as f64 / 2.0) / (wlen.max(1) as f64);
                line.push_str(&format!(
                    "   <- {} itself @{}",
                    py_repr(query.trim()),
                    self.pt(mx as i32, cy as i32)
                ));
            }
            out.push(line);
        }
        let (x0, y0, _x1, y1, _) = &items[hits[0]];
        let sc = self.scale();
        let a = sc.from_screen(
            left.max((*x0 as i32) - (0.03 * w) as i32),
            top.max((*y0 as i32) - (0.04 * h) as i32),
        );
        let b = sc.from_screen(
            (left + d.width() - 1).min((*x0 as i32) + (0.35 * w) as i32),
            (top + d.height() - 1).min((*y1 as i32) + (0.04 * h) as i32),
        );
        // the text below is the answer; never lose it to a bad picture box
        let img = if self.o.helper_images {
            self.screenshot_b64(Some([
                f64::from(a[0]),
                f64::from(a[1]),
                f64::from(b[0]),
                f64::from(b[1]),
            ]))
            .ok()
        } else {
            None
        };
        let mut note = format!(
            "found {} line(s) with {} ({source}); each row reads left to right with click points in \
             full-screenshot coordinates:\n{}",
            hits.len(),
            py_repr(query),
            out.join("\n")
        );
        if source == "ocr" {
            let texts: Vec<String> = hits.iter().map(|&i| items[i].4.clone()).collect();
            note.push_str(&lookalike_note(&texts));
        }
        Ok((note, img))
    }

    /// Every match for `query` in the foreground window's documents, straight from the document's text.
    /// A missing box means the match is not on screen (scrolled away); the text is still exact.
    /// Python `_doc_text_hits`.
    pub(crate) fn doc_text_hits(
        &self,
        query: &str,
        max_hits: usize,
    ) -> Vec<crate::backend::DocumentHit> {
        let Some(root) = self.fg_root() else {
            return Vec::new();
        };
        self.be()
            .find_in_documents(root, query, max_hits, secs(1.0))
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------ zoom text

    /// UIA elements (foreground window) whose centre falls inside the screen box, ready for
    /// [`group_into_rows`]. Python `_uia_items_in_box`.
    fn uia_items_in_box(&self, x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<RowItem> {
        self.uia_elements(self.fg_root(), None)
            .into_iter()
            .filter_map(|e| {
                let (cx, cy) = (
                    f64::from(e.rect.left + e.rect.right) / 2.0,
                    f64::from(e.rect.top + e.rect.bottom) / 2.0,
                );
                if f64::from(x0) <= cx
                    && cx <= f64::from(x1)
                    && f64::from(y0) <= cy
                    && cy <= f64::from(y1)
                {
                    Some(RowItem {
                        cy,
                        cx,
                        h: f64::from((e.rect.bottom - e.rect.top).max(8)),
                        label: format!("{} @{}", e.name, self.pt(cx as i32, cy as i32)),
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    /// The document's own text for the line at a screen point, or "". Python `_exact_line_at`.
    fn exact_line_at(&self, root: Option<WindowRef>, x: i32, y: i32) -> String {
        let Some(root) = root else {
            return String::new();
        };
        self.be()
            .document_line_at(root, Point { x, y })
            .ok()
            .flatten()
            .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    }

    /// OCR of a zoom box: (items for [`group_into_rows`], source label, caution text). Each line gets
    /// its click point in FULL-screenshot coordinates. 2026-09-21 eval: a model read Calculator's
    /// button positions off the zoomed picture and clicked them as if they were full-screen points,
    /// closing the window by accident. Python `_ocr_zoom_items`.
    fn ocr_zoom_items(
        &self,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
    ) -> (Vec<RowItem>, &'static str, String) {
        let none = || (Vec::new(), "ocr", String::new());
        let Ok(mut img) = self.grab(Rect::new(x0, y0, x1, y1)) else {
            return none();
        };
        if img.height() < 300 {
            let k = (300 / img.height().max(1)).clamp(2, 4);
            match crate::imaging::resize_bicubic(&img, img.width() * k, img.height() * k) {
                Ok(big) => img = big,
                Err(_) => return none(),
            }
        }
        let origin = img.origin();
        let f = img.scale();
        let Some(rows) = self.ocr_rows_of(&img, false) else {
            return none();
        };
        let root = self.fg_root();
        let caret = if rows.is_empty() {
            None
        } else {
            self.be().caret().ok().flatten()
        };
        let mut items = Vec::new();
        let (mut fixed, mut unsure, mut at_caret): (usize, Vec<String>, Vec<String>) =
            (0, Vec::new(), Vec::new());
        for row in rows {
            let mut words = row.text.trim().to_string();
            if words.is_empty() {
                continue;
            }
            let r = row.rect;
            let cx = f64::from(origin.x) + f64::from(r.left + r.right) / 2.0 / f;
            let cy = f64::from(origin.y) + f64::from(r.top + r.bottom) / 2.0 / f;
            let lh = f64::from(r.bottom - r.top) / f;
            // A page or rich editor knows its own characters: swap the OCR guess for the real line
            // when it is clearly the same line (h-longsite: OCR said T115YIL for TII5ML).
            let exact = self.exact_line_at(
                root,
                (f64::from(origin.x) + f64::from(r.left) / f + 2.0) as i32,
                cy as i32,
            );
            if !exact.is_empty() && same_line(&words, &exact) {
                words = exact;
                fixed += 1;
            } else {
                unsure.push(words.clone());
                if let Some(c) = caret
                    && words.ends_with(['l', 'I', '|', '1', '!'])
                {
                    // 2026-09-21 openedit: the blinking cursor right after "final" was read as a
                    // letter, "status: finall", and the model "fixed" it by deleting a real
                    // character elsewhere. Say where the cursor is when a thin glyph ends there.
                    let top_ = f64::from(origin.y) + f64::from(r.top) / f;
                    let bot_ = f64::from(origin.y) + f64::from(r.bottom) / f;
                    let ccy = f64::from(c.y);
                    let on_line = top_ - 4.0 <= ccy + f64::from(c.height).min(lh) / 2.0
                        && ccy + f64::from(c.height).min(lh) / 2.0 <= bot_ + 4.0;
                    let end_x = f64::from(origin.x) + f64::from(r.right) / f;
                    if on_line && (end_x - f64::from(c.x)).abs() <= 6.0_f64.max(0.6 * lh) {
                        at_caret.push(words.clone());
                    }
                }
            }
            items.push(RowItem {
                cy,
                cx,
                h: lh,
                label: format!("{words} @{}", self.pt(cx as i32, cy as i32)),
            });
        }
        let source = if fixed > 0 {
            if unsure.is_empty() {
                "uia text"
            } else {
                "uia text + ocr"
            }
        } else {
            "ocr"
        };
        let mut caution = lookalike_note(&unsure);
        for words in at_caret {
            let last: String = words.chars().last().map(String::from).unwrap_or_default();
            caution.push_str(&format!(
                "\ntext cursor: it sits right after {}; the last {} there may be the blinking cursor, \
                 not a letter. Check with read_text before correcting it.",
                py_repr(&words),
                py_repr(&last)
            ));
        }
        (items, source, caution)
    }

    /// The text inside a zoom box, UIA element names first, OCR as the fallback, as a line after the
    /// image note. A person just reads it; handing back the words with the picture ends the zoom loop
    /// (2026-09-21 eval: a local model spent 9 of 20 steps zooming smaller and smaller strips of a
    /// paragraph trying to read one name). With fewer than 3 names, OCR runs too and is merged in
    /// (openedit: a lone title-bar element used to hide the text). `region` is in model coordinates.
    /// Python `_zoom_text`.
    pub(crate) fn zoom_text(&self, region: [i32; 4]) -> StepResult<String> {
        if !self.cfg().zoom_ocr() {
            return Ok(String::new());
        }
        let sc = self.scale();
        let (x0, y0) = sc.to_screen(f64::from(region[0]), f64::from(region[1]))?;
        let (x1, y1) = sc.to_screen(f64::from(region[2]), f64::from(region[3]))?;
        let mut items = self.uia_items_in_box(x0, y0, x1, y1);
        let mut source = "uia".to_string();
        let mut caution = String::new();
        if items.len() < 3 {
            let (ocr_items, ocr_source, c) = self.ocr_zoom_items(x0, y0, x1, y1);
            caution = c;
            let seen: Vec<String> = items
                .iter()
                .map(|it| {
                    it.label
                        .split(" @")
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_lowercase()
                })
                .collect();
            let fresh: Vec<RowItem> = ocr_items
                .into_iter()
                .filter(|it| {
                    !seen.contains(
                        &it.label
                            .split(" @")
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_lowercase(),
                    )
                })
                .collect();
            if !fresh.is_empty() {
                source = if items.is_empty() {
                    ocr_source.to_string()
                } else {
                    format!("uia + {ocr_source}")
                };
            }
            items.extend(fresh);
        }
        if items.is_empty() {
            return Ok(
                "\nno text found in the zoom box: it may be off target. Take a bigger box \
                       around where you think the text is."
                    .to_string(),
            );
        }
        let mut text = group_into_rows(&items).join("\n");
        if text.chars().count() > 1400 {
            text = format!("{} ...", text.chars().take(1400).collect::<String>());
        }
        Ok(format!(
            "\ntext in the zoom box ({source}), one screen row per line, left to right, click points \
             in full-screenshot coordinates:\n{text}{caution}"
        ))
    }

    /// The value shown next to `label`, via UIA: an edit/text element whose own name is the label (a
    /// properly wired accessible name), else the nearest element to the right of a caption that reads
    /// `label` on the same row (a plain Win32 dialog with no LabeledBy link). `None` if UIA finds
    /// nothing, so the caller can fall back to the OCR row reader. Python `_read_field_uia`.
    pub(crate) fn read_field_uia(&self, label: &str) -> Option<String> {
        let root = self.fg_root()?;
        let q = label.trim().to_lowercase();
        let q = q.trim_end_matches(':').trim().to_string();
        let elems = self.uia_elements(Some(root), None);
        for e in &elems {
            if e.name.to_lowercase().contains(&q) && e.has_value {
                return Some(e.value.clone().unwrap_or_default());
            }
        }
        for e in &elems {
            if e.name.to_lowercase().trim_end_matches(':').trim() != q {
                continue;
            }
            let (r, cy) = (e.rect, f64::from(e.rect.top + e.rect.bottom) / 2.0);
            let mut best: Option<(i32, String)> = None;
            for other in &elems {
                if other.rect == e.rect {
                    continue;
                }
                let cy2 = f64::from(other.rect.top + other.rect.bottom) / 2.0;
                if (cy2 - cy).abs() < 8.0 && other.rect.left >= r.right - 4 {
                    let val = if other.has_value {
                        other.value.clone().unwrap_or_default()
                    } else {
                        other.name.clone()
                    };
                    if best.as_ref().is_none_or(|b| other.rect.left < b.0) {
                        best = Some((other.rect.left, val));
                    }
                }
            }
            if let Some((_, v)) = best {
                return Some(v);
            }
        }
        None
    }

    // ------------------------------------------------------------------ double-click aim

    /// A double-click selects a word, so when it lands in the gap next to one (a space, a colon), move
    /// it onto the nearest word on that line within `reach` screen pixels. Off by default: in a game,
    /// snapping a double-click onto the nearest OCR'd label is wrong (adversarial review, 2026-09-21);
    /// a runner turns it on per call with `snap: true`.
    ///
    /// 2026-09-21 evals: a model double-clicked between "status" and ":" and at the line's edge three
    /// times while trying to select "draft"; each time it typed over the wrong text. Python
    /// `_snap_to_word`. Returns the point and the word it moved onto ("" when it did not move).
    pub(crate) fn snap_to_word(&self, x: i32, y: i32) -> (i32, i32, String) {
        const REACH: f64 = 28.0;
        let stay = (x, y, String::new());
        if !(self.cfg().snap() || self.o.snap_call) {
            return stay;
        }
        let area = Rect::from_xywh(x - 160, y - 30, 320, 60);
        let Ok(img) = self.grab(area) else {
            return stay;
        };
        let Ok(img) = crate::imaging::resize_bicubic(&img, 960, 180) else {
            return stay;
        };
        let (origin, f) = (img.origin(), img.scale());
        let Some(rows) = self.ocr_rows_of(&img, true) else {
            return stay;
        };
        let (px, py) = (f64::from(x), f64::from(y));
        let mut best: Option<(f64, i32, i32, String)> = None;
        for row in rows {
            let (l, t) = (
                f64::from(origin.x) + f64::from(row.rect.left) / f,
                f64::from(origin.y) + f64::from(row.rect.top) / f,
            );
            let (r, b) = (
                f64::from(origin.x) + f64::from(row.rect.right) / f,
                f64::from(origin.y) + f64::from(row.rect.bottom) / f,
            );
            if l - 1.0 <= px && px <= r + 1.0 && t - 2.0 <= py && py <= b + 2.0 {
                return stay; // already on a word
            }
            if !(t - 4.0 <= py && py <= b + 4.0) {
                continue;
            }
            let d = if px < l { l - px } else { px - r };
            if d <= REACH && best.as_ref().is_none_or(|bst| d < bst.0) {
                best = Some((
                    d,
                    ((l + r) / 2.0) as i32,
                    ((t + b) / 2.0) as i32,
                    row.text.clone(),
                ));
            }
        }
        match best {
            Some((_, bx, by, word)) => (bx, by, word),
            None => stay,
        }
    }

    // ------------------------------------------------------------------ read_text

    /// The whole text of the window's document or focused edit box via UIA, or "" if it exposes none
    /// (a Tk or canvas-drawn editor). Python `_doc_text_uia`.
    fn doc_text_uia(&self, root: WindowRef) -> String {
        self.be()
            .document_text(root, 500_000)
            .ok()
            .flatten()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_default()
    }

    /// Select all + copy in the front window, read the clipboard, put the old clipboard text back and
    /// clear the selection (so a later keystroke cannot replace the whole document). Python
    /// `_doc_text_by_copy`.
    fn doc_text_by_copy(&self) -> StepResult<String> {
        if matches!(self.be().clipboard()?, ClipboardContent::Files(_)) {
            return Err(StepError::target(
                "read_text needs the clipboard, which holds copied files right now; paste them first, \
                 then read",
            ));
        }
        let prior = self.be().clipboard_text().unwrap_or_default();
        let marker = format!("winhands-read-{}", self.clock().now().as_nanos());
        self.be().set_clipboard_text(&marker)?;
        let copied = (|| -> StepResult<String> {
            self.send_combo(self.select_all(), 1)?;
            self.sleep(0.05);
            self.send_combo(self.copy_combo(), 1)?;
            let (got, _took) = self.poll(
                || {
                    let v = self.be().clipboard_text().unwrap_or_default();
                    (!v.is_empty() && v != marker).then_some(v)
                },
                1.0,
                0.05,
            );
            Ok(got.unwrap_or_default())
        })();
        // put the owner's clipboard back and drop the select-all, whatever happened
        let _ = self.be().set_clipboard_text(&prior);
        let dropped = self.send_combo("left", 1);
        let text = copied?;
        dropped?;
        Ok(text)
    }

    /// The front window's whole document as numbered lines, or with `query` only the lines that
    /// contain it. UIA text first; editors that expose none are read by select-all + copy.
    ///
    /// 2026-09-21 h-longdoc: the line with the answer was in a 180-line file in a Tk editor with no
    /// UIA text and no Find. find_text and zoom only see what is on screen, so a model scrolled for 20
    /// steps, skipped past the line and ran out. A person presses ctrl+f; this is the tool's ctrl+f
    /// for any document. Python `_read_text`.
    pub(crate) fn read_text(&self, query: &str) -> StepResult<String> {
        const MAX_CHARS: usize = 6000;
        let Some(root) = self.fg_root() else {
            return Err(StepError::target("no window is in front to read"));
        };
        self.check_not_protected(root)?;
        self.refuse_host(root)?;
        self.check_allowed(root)?;
        if self
            .info(root)
            .as_ref()
            .is_some_and(super::ctx::is_file_manager)
        {
            return Err(StepError::target(
                "read_text reads a document or page; for the files in a folder use list_files, for \
                 what is on screen use find_text or zoom",
            ));
        }
        let mut text = self.doc_text_uia(root);
        let mut source = "uia";
        if text.trim().is_empty() {
            text = self.doc_text_by_copy()?;
            source = "select-all + copy; selection cleared";
        }
        if text.trim().is_empty() {
            return Ok(
                "read_text: the front window gave no text (it may draw its text as pictures); use zoom"
                    .to_string(),
            );
        }
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let lines: Vec<&str> = normalized.split('\n').collect();
        let head = format!(
            "read_text ({source}, exact characters, {} lines)",
            lines.len()
        );
        let q = query.trim().to_lowercase();
        if !q.is_empty() {
            let found: Vec<String> = lines
                .iter()
                .enumerate()
                .filter(|(_, ln)| ln.to_lowercase().contains(&q))
                .map(|(i, ln)| format!("{}: {}", i + 1, ln.trim()))
                .collect();
            if found.is_empty() {
                return Ok(format!(
                    "{head}: no line contains {}",
                    py_repr(query.trim())
                ));
            }
            let more = if found.len() > 30 {
                format!("\n({} more matching lines)", found.len() - 30)
            } else {
                String::new()
            };
            return Ok(format!(
                "{head}, {} contain {}:\n{}{more}",
                found.len(),
                py_repr(query.trim()),
                found
                    .iter()
                    .take(30)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        let mut out = lines
            .iter()
            .enumerate()
            .map(|(i, ln)| format!("{}: {}", i + 1, ln.trim_end()))
            .collect::<Vec<_>>()
            .join("\n");
        if out.chars().count() > MAX_CHARS {
            out = format!(
                "{}\n(cut here; pass text = a word to get only the lines that contain it)",
                out.chars().take(MAX_CHARS).collect::<String>()
            );
        }
        Ok(format!("{head}:\n{out}"))
    }
}
