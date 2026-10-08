//! `read`: a window's accessibility elements as text. Port of `core.snapshot` (the line format and the
//! ref bookkeeping), `core.snapshot_wake_policy` and `core.WAKE_HINT`, and `fast.do_snapshot` (junk
//! pruning, filtering, capping, compaction).
//!
//! Local models pay for every token on every turn, so the default is 40 useful items, and refs stay
//! valid because they number the full list the backend returned, not the trimmed one.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

use super::error::{StepError, StepResult};
use super::refs::CachedElement;
use super::{Computer, Ctx};
use crate::backend::{Element, TreeLimits, WakePolicy, WindowRef};
use crate::textutil::py_repr;

pub use super::refs::{WindowLine, format_windows};

/// What `read` appends when a Chromium window looks empty and was left alone
/// (`core.WAKE_HINT`).
pub const WAKE_HINT: &str = "(accessibility tree looks empty: pass wake=true to wake this window's \
accessibility tree; it is brought to the front briefly)";

/// Elements a model can never use: parked off screen at 0,0, or nameless AND disabled. A nameless
/// element that is enabled and on screen (an icon button, a slider) is kept: it can still be clicked
/// by number. In a browser these are most of the list. Dropping them (2026-09-19) roughly halves
/// snapshot tokens.
static JUNK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"^\d+ \w+ ".*?"(?: \[\w+\])* @0,0$|^\d+ \w+ "" (?:\[\w+\] )*\[disabled\](?: \[\w+\])* @-?\d+,-?\d+$"#,
    )
    .expect("JUNK")
});
static TYPE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+) (\w+?)Control ").expect("TYPE_RE"));
static COORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" @-?\d+,-?\d+$").expect("COORD_RE"));

/// `4 ButtonControl "Button A" @177,304` becomes `4 Button "Button A"`. 2026-09-20: the "Control"
/// suffix and the coordinates were about 40 percent of every snapshot's tokens, and the model clicks by
/// number or name, never by those coordinates. Pass `coords=true` when you really need them. Python
/// `fast._compact_line`.
pub(crate) fn compact_line(line: &str) -> String {
    let line = TYPE_RE.replace(line, "${1} ${2} ");
    COORD_RE.replace(&line, "").into_owned()
}

/// One element as the list prints it (`core.snapshot`).
pub(crate) fn element_line(reference: usize, e: &Element) -> String {
    let mut flags = String::new();
    if !e.enabled {
        flags.push_str(" [disabled]");
    }
    for f in e.state.flags() {
        flags.push_str(&format!(" [{f}]"));
    }
    let val = match &e.value {
        Some(v) => {
            let shown = if v.chars().count() > 60 {
                format!("{}...", v.chars().take(60).collect::<String>())
            } else {
                v.clone()
            };
            format!(" =\"{shown}\"")
        }
        None => String::new(),
    };
    let (cx, cy) = center_of(e);
    format!(
        "{reference} {} \"{}\"{flags}{val} @{cx},{cy}",
        e.native_role, e.name
    )
}

fn center_of(e: &Element) -> (i32, i32) {
    e.rect.map_or((0, 0), |r| {
        let c = r.center();
        (c.x, c.y)
    })
}

/// Prune, filter, cap and compact a full snapshot (`fast.do_snapshot` after `core.snapshot`).
pub(crate) fn render_snapshot(
    lines: &[String],
    max_items: usize,
    filter: &str,
    coords: bool,
) -> String {
    let max_items = max_items.max(1);
    let head: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("window "))
        .take(1)
        .collect();
    let mut body: Vec<&String> = lines
        .iter()
        .filter(|l| !l.starts_with("window ") && l.as_str() != "(truncated)")
        .filter(|l| !JUNK.is_match(l))
        .collect();
    if !filter.is_empty() {
        let f = filter.to_lowercase();
        body.retain(|l| l.to_lowercase().contains(&f));
    }
    let mut out: Vec<String> = head.iter().map(|s| (*s).clone()).collect();
    out.extend(body.iter().take(max_items).map(|s| (*s).clone()));
    if body.len() > max_items {
        out.push(format!(
            "(truncated, {} more; pass filter= to narrow)",
            body.len() - max_items
        ));
    }
    if body.is_empty() && filter.is_empty() {
        out.push(
            "(this app exposes no clickable elements to Windows: it is drawn by a game engine or an \
             embedded browser. Use read_text for its words, look for a picture with numbered boxes, \
             or click by 'x,y'. For a browser page use read_page.)"
                .to_string(),
        );
    }
    if !filter.is_empty() && body.is_empty() {
        out.push(format!(
            "(no element matches {}; try a shorter word, or read_page/read_text for content)",
            py_repr(filter)
        ));
    }
    if !coords {
        out = out.iter().map(|l| compact_line(l)).collect();
    }
    out.join("\n")
}

impl Ctx<'_> {
    /// `core.snapshot`: the window's interactive elements as lines, and whether a Chromium window
    /// looked empty and was left alone (`wake` false). The refs it prints are remembered for the
    /// window until it is read again or closes.
    pub(crate) fn snapshot_lines(
        &self,
        spec: Option<&str>,
        max_items: usize,
        wake: bool,
    ) -> StepResult<(Vec<String>, bool)> {
        let refs = self.c.window_refs();
        let wins = refs.list(true)?;
        let window = refs.resolve_in(spec, Some(&wins))?;
        let live: BTreeSet<WindowRef> = wins.iter().map(|w| w.window).collect();
        self.c.element_refs().prune(&live);
        let title = wins
            .iter()
            .find(|w| w.window == window)
            .map_or_else(|| "?".to_string(), |w| w.title.clone());
        let mut limits = TreeLimits::snapshot(max_items);
        limits.wake = if wake {
            WakePolicy::IfEmpty
        } else {
            WakePolicy::Never
        };
        let tree = self.be().element_tree(window, &limits)?;
        let items = &tree.root.children;
        let mut lines = vec![format!("window \"{title}\"")];
        let mut cached = Vec::with_capacity(items.len());
        for (i, e) in items.iter().enumerate() {
            lines.push(element_line(i, e));
            cached.push(CachedElement {
                reference: i,
                id: e.id.clone(),
                native_role: e.native_role.clone(),
                name: e.name.clone(),
                disabled: !e.enabled,
                center: center_of(e),
                value: e.value.clone(),
                rect: e.rect,
                state: e.state.flags().iter().map(|f| format!(" [{f}]")).collect(),
            });
        }
        self.c.element_refs().store(window, cached);
        if tree.truncated {
            lines.push("(truncated)".to_string());
        }
        Ok((lines, !wake && tree.wake_hint))
    }

    /// `fast.do_snapshot`: the trimmed, compact listing a model reads. The bool is the wake hint.
    pub(crate) fn do_snapshot(
        &self,
        spec: Option<&str>,
        max_items: i64,
        filter: &str,
        coords: bool,
        wake: bool,
    ) -> StepResult<(String, bool)> {
        let max_items = max_items.max(1) as usize;
        let ask = if filter.is_empty() {
            80.max(300.min(max_items.saturating_mul(4)))
        } else {
            500
        };
        let (lines, hint) = self.snapshot_lines(spec, ask, wake)?;
        Ok((render_snapshot(&lines, max_items, filter, coords), hint))
    }
}

impl Computer {
    /// `fast.do_snapshot` for a caller outside a `computer` call (the daemon's `snapshot` route):
    /// element list of `window` (a `wN` ref, title, `pid:N`, `hwnd:N`, or the window in front).
    /// `wake` lets an empty Chromium tree be woken by raising the window, as `core.snapshot` does
    /// outside a `read`. The text is returned as it would print; when the tree looked empty and was
    /// left alone the wake hint is appended.
    pub fn read_snapshot(
        &self,
        window: Option<&str>,
        max_items: i64,
        filter: &str,
        coords: bool,
        wake: bool,
    ) -> Result<String, StepError> {
        let who = crate::caller::Caller::unknown();
        let ctx = self.read_ctx(&who);
        let (mut text, hint) = ctx.do_snapshot(window, max_items, filter, coords, wake)?;
        if hint {
            text.push('\n');
            text.push_str(WAKE_HINT);
        }
        Ok(text)
    }

    /// `core.format_windows(core.list_windows(...))` joined by newlines, as the `windows` route and
    /// the `list_windows` action print it.
    pub fn windows_text(&self, include_cloaked: bool) -> Result<String, StepError> {
        let wins = self.window_refs().list(include_cloaked)?;
        Ok(format_windows(&wins).join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Affordance, Expand, Rect, Role, Toggle, WindowInfo};
    use crate::orchestrate::testkit::Kit;

    // ---- ported from tests/test_20260919_fixes.py ----

    #[test]
    fn snapshot_filter_applies_before_cap() {
        let mut lines = vec!["window \"X\"".to_string()];
        lines.extend((0..300).map(|i| format!("{i} ButtonControl \"Item {i}\" @5,5")));
        lines.push("300 ButtonControl \"Save as\" @10,10".to_string());
        let out = render_snapshot(&lines, 40, "save", false);
        assert!(
            out.contains("300 Button ") && out.starts_with("window \"X\""),
            "{out}"
        );
    }

    #[test]
    fn snapshot_default_is_small() {
        let lines: Vec<String> = (0..80).map(|i| format!("{i} x")).collect();
        let out = render_snapshot(&lines, 40, "", false);
        assert_eq!(out.lines().count(), 41, "40 items plus the truncation note");
        assert!(out.ends_with("(truncated, 40 more; pass filter= to narrow)"));
    }

    #[test]
    fn snapshot_drops_unusable_elements() {
        let lines: Vec<String> = [
            "window \"B\"",
            "0 ButtonControl \"\" @0,0",
            "1 ButtonControl \"\" [disabled] @0,0",
            "2 ButtonControl \"Maximize\" @0,0",
            "3 EditControl \"Address bar\" =\"x\" @10,60",
            "4 SliderControl \"\" @300,200",
            "5 ButtonControl \"\" [disabled] @400,60",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = render_snapshot(&lines, 40, "", false);
        assert!(
            out.contains("Address bar") && !out.contains("Maximize"),
            "{out}"
        );
        assert!(
            out.contains("4 Slider"),
            "a nameless but enabled on-screen element must survive"
        );
        assert!(!out.contains("[disabled]"));
    }

    #[test]
    fn empty_snapshot_says_what_to_do_instead() {
        let out = render_snapshot(&["window \"Steam\"".to_string()], 40, "", false);
        assert!(out.contains("read_text") && out.contains("x,y"), "{out}");
        let none = render_snapshot(&["window \"Steam\"".to_string()], 40, "zzz", false);
        assert!(none.contains("no element matches 'zzz'"), "{none}");
    }

    #[test]
    fn snapshot_lines_are_compact_by_default() {
        let lines: Vec<String> = [
            "window \"B\"",
            "3 EditControl \"Address bar\" =\"x\" @10,60",
            "7 CheckBoxControl \"Check me\" [on] @197,341",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = render_snapshot(&lines, 40, "", false);
        assert!(
            out.contains("3 Edit \"Address bar\" =\"x\"")
                && out.contains("7 CheckBox \"Check me\" [on]")
        );
        assert!(!out.contains('@'));
        assert!(render_snapshot(&lines, 40, "", true).contains("@197,341"));
    }

    // ---- element line format (core.snapshot) ----

    #[test]
    fn an_element_prints_role_name_flags_value_and_centre() {
        let mut e =
            Element::new("e", Role::CheckBox, "Remember me").at(Rect::new(100, 200, 140, 220));
        e.state.toggle = Some(Toggle::On);
        e.state.selected = true;
        e.state.expanded = Some(Expand::Collapsed);
        e.enabled = false;
        e.value = Some("v".repeat(70));
        let line = element_line(7, &e);
        assert_eq!(
            line,
            format!(
                "7 CheckBoxControl \"Remember me\" [disabled] [on] [selected] [collapsed] =\"{}...\" @120,210",
                "v".repeat(60)
            )
        );
        let bare = Element::new("b", Role::Button, "OK");
        assert_eq!(element_line(0, &bare), "0 ButtonControl \"OK\" @0,0");
    }

    #[test]
    fn read_lists_a_window_through_the_backend_and_remembers_the_refs() {
        let kit = Kit::new();
        let w = kit.fake.add_window(WindowInfo::basic(
            "Notepad",
            "notepad.exe",
            5,
            Rect::from_xywh(0, 0, 800, 600),
        ));
        let tree = Element::new("root", Role::Window, "Notepad")
            .with_child(Element::new("save", Role::Button, "Save").at(Rect::new(10, 10, 50, 30)))
            .with_child(Element::new("pane", Role::Pane, "chrome"))
            .with_child(
                Element::new("name", Role::Edit, "File name")
                    .at(Rect::new(0, 40, 100, 60))
                    .valued("a.txt"),
            );
        kit.fake.set_tree(w, tree);
        let text = kit
            .computer
            .read_snapshot(Some("Notepad"), 40, "", false, false)
            .unwrap();
        assert_eq!(
            text,
            "window \"Notepad\"\n0 Button \"Save\"\n1 Edit \"File name\" =\"a.txt\""
        );
        let cached = kit.computer.element_refs().get(1, Some(w)).unwrap();
        assert_eq!(cached.name, "File name");
        assert_eq!(cached.center, (50, 50));
        assert_eq!(
            kit.computer.element_refs().get(0, None).unwrap().id.0,
            "save"
        );
        // a filter narrows what is printed but refs still number the full list
        let narrowed = kit
            .computer
            .read_snapshot(Some("Notepad"), 40, "file", false, false)
            .unwrap();
        assert_eq!(
            narrowed,
            "window \"Notepad\"\n1 Edit \"File name\" =\"a.txt\""
        );
    }

    #[test]
    fn a_pane_with_a_pattern_counts_as_interactive() {
        let kit = Kit::new();
        let w = kit.fake.add_window(WindowInfo::basic(
            "App",
            "app.exe",
            5,
            Rect::from_xywh(0, 0, 80, 60),
        ));
        let mut pane = Element::new("p", Role::Pane, "Clickable").at(Rect::new(0, 0, 10, 10));
        pane.affordances.push(Affordance::Invoke);
        kit.fake.set_tree(
            w,
            Element::new("root", Role::Window, "App").with_child(pane),
        );
        let text = kit
            .computer
            .read_snapshot(Some("App"), 40, "", false, false)
            .unwrap();
        assert!(text.contains("0 Pane \"Clickable\""), "{text}");
    }

    #[test]
    fn reading_a_vanished_window_is_a_target_error() {
        let kit = Kit::new();
        let err = kit
            .computer
            .read_snapshot(Some("hwnd:99999"), 40, "", false, false)
            .unwrap_err();
        assert!(matches!(err, StepError::Target(_)));
        assert_eq!(err.message(), "window handle unavailable; refresh windows");
    }
}
