//! The pure half of the UI Automation backend: control-type mapping, element ids, filtering and
//! truncation rules. Nothing here touches COM, so all of it is unit-tested without a desktop.
//!
//! Python reference: `core.py:_snapshot_batched`, `_is_interactive`, `_item_state`, `_refind_element`,
//! `computer.py:_uia_elements`.

use std::time::Instant;

use wh_core::backend::{
    Affordance, Element, ElementFilter, ElementId, ElementState, Expand, Rect, Role, Toggle,
};

// ------------------------------------------------------------------------------------ control types

/// UIA `ControlType` ids (`UIA_ButtonControlTypeId` is 50000 and so on) to the neutral role and the
/// `uiautomation` package's name for the type, which snapshots print (`ButtonControl`). Python
/// printed `Control` for an id it did not know.
pub(crate) fn role_for(control_type: i32) -> (Role, &'static str) {
    match control_type {
        50000 => (Role::Button, "ButtonControl"),
        50001 => (Role::Other("Calendar".into()), "CalendarControl"),
        50002 => (Role::CheckBox, "CheckBoxControl"),
        50003 => (Role::ComboBox, "ComboBoxControl"),
        50004 => (Role::Edit, "EditControl"),
        50005 => (Role::Hyperlink, "HyperlinkControl"),
        50006 => (Role::Other("Image".into()), "ImageControl"),
        50007 => (Role::ListItem, "ListItemControl"),
        50008 => (Role::List, "ListControl"),
        50009 => (Role::Menu, "MenuControl"),
        50010 => (Role::Other("MenuBar".into()), "MenuBarControl"),
        50011 => (Role::MenuItem, "MenuItemControl"),
        50012 => (Role::Other("ProgressBar".into()), "ProgressBarControl"),
        50013 => (Role::RadioButton, "RadioButtonControl"),
        50014 => (Role::Other("ScrollBar".into()), "ScrollBarControl"),
        50015 => (Role::Slider, "SliderControl"),
        50016 => (Role::Other("Spinner".into()), "SpinnerControl"),
        50017 => (Role::Other("StatusBar".into()), "StatusBarControl"),
        50018 => (Role::Other("Tab".into()), "TabControl"),
        50019 => (Role::TabItem, "TabItemControl"),
        50020 => (Role::Text, "TextControl"),
        50021 => (Role::Other("ToolBar".into()), "ToolBarControl"),
        50022 => (Role::Other("ToolTip".into()), "ToolTipControl"),
        50023 => (Role::Tree, "TreeControl"),
        50024 => (Role::TreeItem, "TreeItemControl"),
        50025 => (Role::Other("Custom".into()), "CustomControl"),
        // A group is not a Pane: Python described "(on group 'x')" but stayed silent for a pane.
        50026 => (Role::Other("Group".into()), "GroupControl"),
        50027 => (Role::Other("Thumb".into()), "ThumbControl"),
        50028 => (Role::DataGrid, "DataGridControl"),
        50029 => (Role::DataItem, "DataItemControl"),
        50030 => (Role::Document, "DocumentControl"),
        50031 => (Role::SplitButton, "SplitButtonControl"),
        50032 => (Role::Window, "WindowControl"),
        50033 => (Role::Pane, "PaneControl"),
        50034 => (Role::Other("Header".into()), "HeaderControl"),
        50035 => (Role::Other("HeaderItem".into()), "HeaderItemControl"),
        50036 => (Role::Table, "TableControl"),
        50037 => (Role::Other("TitleBar".into()), "TitleBarControl"),
        50038 => (Role::Other("Separator".into()), "SeparatorControl"),
        50039 => (Role::Other("SemanticZoom".into()), "SemanticZoomControl"),
        50040 => (Role::Other("AppBar".into()), "AppBarControl"),
        _ => (Role::Other("Control".into()), "Control"),
    }
}

/// `UIA_ButtonControlTypeId` and the other ids the code compares against.
pub(crate) mod ct {
    pub const BUTTON: i32 = 50000;
    pub const EDIT: i32 = 50004;
    pub const MENU: i32 = 50009;
    pub const MENU_ITEM: i32 = 50011;
    pub const DOCUMENT: i32 = 50030;
    pub const WINDOW: i32 = 50032;
    pub const PANE: i32 = 50033;
}

// ------------------------------------------------------------------------------------ raw properties

/// Which patterns an element supports, from the `Is<Pattern>Available` properties.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Patterns {
    pub invoke: bool,
    pub value: bool,
    pub toggle: bool,
    pub select: bool,
    pub expand: bool,
    pub text: bool,
}

/// What one cached read of an element returns, before it becomes an [`Element`].
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Props {
    pub control_type: i32,
    pub name: String,
    pub rect: Option<Rect>,
    pub enabled: bool,
    pub offscreen: bool,
    pub password: bool,
    pub focused: bool,
    pub patterns: Patterns,
    /// The `Value.Value` property; only meaningful when `patterns.value`.
    pub value: Option<String>,
    /// `Toggle.ToggleState`: 0 off, 1 on, 2 indeterminate.
    pub toggle: Option<i32>,
    pub selected: bool,
    /// `ExpandCollapse.ExpandCollapseState`: 0 collapsed, 1 expanded, 2 partial, 3 leaf.
    pub expand: Option<i32>,
}

/// An element from its properties. State is filled for the roles whose state a click changes
/// (`core._STATEFUL`), as Python did, and the value of a password field is never carried.
pub(crate) fn element_from_props(id: ElementId, p: &Props) -> Element {
    let (role, native) = role_for(p.control_type);
    let mut affordances = Vec::new();
    for (has, a) in [
        (p.patterns.invoke, Affordance::Invoke),
        (p.patterns.toggle, Affordance::Toggle),
        (p.patterns.select, Affordance::Select),
        (p.patterns.expand, Affordance::ExpandCollapse),
        (p.patterns.value, Affordance::Value),
        (p.patterns.text, Affordance::Text),
    ] {
        if has {
            affordances.push(a);
        }
    }
    let mut state = ElementState::default();
    if role.is_stateful() {
        if p.patterns.toggle {
            state.toggle = match p.toggle {
                Some(0) => Some(Toggle::Off),
                Some(1) => Some(Toggle::On),
                Some(2) => Some(Toggle::Mixed),
                _ => None,
            };
        }
        state.selected = p.patterns.select && p.selected;
        if p.patterns.expand {
            // 3 is a leaf node: not shown
            state.expanded = match p.expand {
                Some(0) => Some(Expand::Collapsed),
                Some(1) => Some(Expand::Expanded),
                Some(2) => Some(Expand::Partial),
                _ => None,
            };
        }
    }
    let value = if p.password || !p.patterns.value {
        None
    } else {
        Some(p.value.clone().unwrap_or_default())
    };
    Element {
        id,
        role,
        native_role: native.to_string(),
        name: p.name.clone(),
        value,
        rect: p.rect,
        enabled: p.enabled,
        focused: p.focused,
        offscreen: p.offscreen,
        password: p.password,
        state,
        affordances,
        children: Vec::new(),
    }
}

/// Does a read with `filter` keep `e`? `Named` is `computer._uia_elements` (a name and a real box).
pub(crate) fn keeps(filter: ElementFilter, e: &Element) -> bool {
    match filter {
        ElementFilter::All => true,
        ElementFilter::Interactive => e.is_interactive(),
        ElementFilter::Named => !e.name.trim().is_empty() && e.rect.is_some_and(|r| !r.is_empty()),
    }
}

// ------------------------------------------------------------------------------------ limits

/// Counts what a read has kept and says when a limit has been reached.
#[derive(Debug)]
pub(crate) struct Budget {
    max_items: usize,
    deadline: Instant,
    kept: usize,
    truncated: bool,
}

impl Budget {
    pub(crate) fn new(max_items: usize, deadline: Instant) -> Self {
        Self {
            max_items,
            deadline,
            kept: 0,
            truncated: false,
        }
    }

    /// May the read look at one more element? Marks the read truncated when it may not: the item
    /// cap is reached or the deadline has passed (Python's `truncated = True` at both).
    pub(crate) fn may_continue(&mut self) -> bool {
        if self.kept >= self.max_items || Instant::now() >= self.deadline {
            self.truncated = true;
            return false;
        }
        true
    }

    /// One more element was kept.
    pub(crate) fn kept_one(&mut self) {
        self.kept += 1;
    }

    #[cfg(test)]
    pub(crate) fn kept(&self) -> usize {
        self.kept
    }

    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Keep the elements of an in-order list that pass `filter`, up to the cap, the way
/// `_snapshot_batched` walks `FindAllBuildCache`'s answer. A `None` item could not be read and is
/// skipped. Each kept element travels with a payload (the live UIA element, for the id cache).
pub(crate) fn select_flat<T>(
    nodes: impl Iterator<Item = Option<(Element, T)>>,
    filter: ElementFilter,
    budget: &mut Budget,
) -> Vec<(Element, T)> {
    let mut out = Vec::new();
    for node in nodes {
        if !budget.may_continue() {
            break;
        }
        let Some((e, payload)) = node else {
            continue;
        };
        if keeps(filter, &e) {
            budget.kept_one();
            out.push((e, payload));
        }
    }
    out
}

// ------------------------------------------------------------------------------------ element ids

/// What an [`ElementId`] remembers about an element, enough to find it again on another thread:
/// the window it was read under, its control type, name and last box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdParts {
    pub hwnd: u64,
    pub control_type: i32,
    pub rect: Rect,
    pub name: String,
}

const ID_TAG: &str = "uia1";

/// Mint the id: `uia1|hwnd|controlType|l,t,r,b|name`. The name goes last so it may hold any character.
pub(crate) fn encode_id(parts: &IdParts) -> ElementId {
    ElementId(format!(
        "{ID_TAG}|{}|{}|{},{},{},{}|{}",
        parts.hwnd,
        parts.control_type,
        parts.rect.left,
        parts.rect.top,
        parts.rect.right,
        parts.rect.bottom,
        parts.name
    ))
}

/// Read an id made by [`encode_id`]; `None` for anything else.
pub(crate) fn decode_id(id: &ElementId) -> Option<IdParts> {
    let mut it = id.0.splitn(5, '|');
    if it.next()? != ID_TAG {
        return None;
    }
    let hwnd = it.next()?.parse().ok()?;
    let control_type = it.next()?.parse().ok()?;
    let mut edges = it.next()?.split(',').map(str::parse::<i32>);
    let rect = Rect::new(
        edges.next()?.ok()?,
        edges.next()?.ok()?,
        edges.next()?.ok()?,
        edges.next()?.ok()?,
    );
    if edges.next().is_some() {
        return None;
    }
    let name = it.next()?.to_string();
    Some(IdParts {
        hwnd,
        control_type,
        rect,
        name,
    })
}

/// Of several elements that match by name and type, which one is the one the id meant? The nearest to
/// where it was; but with look-alikes and none close, none (Python `_refind_element`: do not guess,
/// 80 px of Manhattan distance).
pub(crate) fn pick_refind(want: Rect, candidates: &[Rect]) -> Option<usize> {
    let c = want.center();
    let (best, dist) = candidates
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let m = r.center();
            (
                i,
                i64::from((m.x - c.x).abs()) + i64::from((m.y - c.y).abs()),
            )
        })
        .min_by_key(|&(_, d)| d)?;
    if candidates.len() > 1 && dist > REFIND_MAX_DISTANCE {
        return None;
    }
    Some(best)
}

/// How far a look-alike may have drifted before it is not trusted to be the same element.
pub(crate) const REFIND_MAX_DISTANCE: i64 = 80;

// ------------------------------------------------------------------------------------ text helpers

/// Python's `" ".join(text.split())`: whitespace runs collapse to one space.
pub(crate) fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A UIA rectangle array (`left, top, width, height` per rectangle, in doubles) to the first box, the
/// way `computer._rect_tuple` took `rects[0]`. `None` when the text is not on screen.
pub(crate) fn first_rect(rects: &[f64]) -> Option<Rect> {
    let r = rects.get(0..4)?;
    Some(Rect::new(
        r[0].round() as i32,
        r[1].round() as i32,
        (r[0] + r[2]).round() as i32,
        (r[1] + r[3]).round() as i32,
    ))
}

/// Is this a "File name" edit of a common file dialog? `computer._file_dialog`: the name, stripped
/// of blanks and one trailing colon, compares equal to "file name" in any case.
pub(crate) fn is_file_name_label(name: &str) -> bool {
    name.trim().trim_end_matches(':').to_lowercase() == "file name"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn props(ct: i32, name: &str) -> Props {
        Props {
            control_type: ct,
            name: name.into(),
            rect: Some(Rect::new(10, 20, 110, 60)),
            enabled: true,
            ..Props::default()
        }
    }

    #[test]
    fn control_types_map_to_the_neutral_roles_and_python_names() {
        assert_eq!(role_for(50000), (Role::Button, "ButtonControl"));
        assert_eq!(role_for(50004).0, Role::Edit);
        assert_eq!(role_for(50002).0, Role::CheckBox);
        assert_eq!(role_for(50007).0, Role::ListItem);
        assert_eq!(role_for(50011).0, Role::MenuItem);
        assert_eq!(role_for(50030).0, Role::Document);
        assert_eq!(role_for(50032).0, Role::Window);
        assert_eq!(role_for(50033).0, Role::Pane);
        // a group stays its own kind so "(on group ...)" still reads
        assert_eq!(
            role_for(50026),
            (Role::Other("Group".into()), "GroupControl")
        );
        assert_eq!(role_for(50099), (Role::Other("Control".into()), "Control"));
    }

    #[test]
    fn every_known_id_has_a_distinct_python_name_ending_in_control() {
        let mut seen = std::collections::HashSet::new();
        for id in 50000..=50040 {
            let (role, native) = role_for(id);
            assert!(native.ends_with("Control"), "{id}");
            assert!(seen.insert(native), "duplicate name {native}");
            if let Role::Other(name) = &role {
                assert_eq!(format!("{name}Control"), native, "{id}");
            }
        }
        assert_eq!(seen.len(), 41);
    }

    #[test]
    fn the_thirteen_interactive_types_are_the_ones_python_listed() {
        let interactive: Vec<i32> = (50000..=50040)
            .filter(|&id| role_for(id).0.is_interactive())
            .collect();
        // Button, CheckBox, ComboBox, Edit, Hyperlink, ListItem, MenuItem, RadioButton, Slider,
        // TabItem, TreeItem, DataItem, SplitButton
        assert_eq!(
            interactive,
            [
                50000, 50002, 50003, 50004, 50005, 50007, 50011, 50013, 50015, 50019, 50024, 50029,
                50031
            ]
        );
    }

    #[test]
    fn elements_carry_value_state_and_affordances() {
        let mut p = props(50002, "Remember me");
        p.patterns.toggle = true;
        p.patterns.invoke = true;
        p.toggle = Some(1);
        let e = element_from_props(ElementId("x".into()), &p);
        assert_eq!(e.native_role, "CheckBoxControl");
        assert_eq!(e.state.toggle, Some(Toggle::On));
        assert_eq!(e.affordances, [Affordance::Invoke, Affordance::Toggle]);
        assert_eq!(e.state.flags(), ["on"]);

        let mut tree = props(50024, "src");
        tree.patterns.expand = true;
        tree.patterns.select = true;
        tree.expand = Some(0);
        tree.selected = true;
        let e = element_from_props(ElementId("t".into()), &tree);
        assert_eq!(e.state.flags(), ["selected", "collapsed"]);
        tree.expand = Some(3);
        assert_eq!(
            element_from_props(ElementId("t".into()), &tree)
                .state
                .expanded,
            None,
            "a leaf shows no expand state"
        );
    }

    #[test]
    fn state_is_only_shown_for_the_stateful_roles() {
        let mut p = props(50000, "Mute");
        p.patterns.toggle = true;
        p.toggle = Some(1);
        let e = element_from_props(ElementId("b".into()), &p);
        assert!(
            e.state.flags().is_empty(),
            "Python printed state for stateful types only"
        );
        assert!(e.affordances.contains(&Affordance::Toggle));
    }

    #[test]
    fn a_value_is_carried_for_a_value_pattern_and_never_for_a_password() {
        let mut p = props(50004, "Address");
        p.patterns.value = true;
        p.value = Some("example.com".into());
        let e = element_from_props(ElementId("e".into()), &p);
        assert_eq!(e.value.as_deref(), Some("example.com"));
        p.value = None;
        assert_eq!(
            element_from_props(ElementId("e".into()), &p)
                .value
                .as_deref(),
            Some(""),
            "an empty edit still shows ="
        );
        p.password = true;
        p.value = Some("hunter2".into());
        let e = element_from_props(ElementId("e".into()), &p);
        assert_eq!(e.value, None);
        assert!(e.password);
        let mut plain = props(50020, "label");
        plain.value = Some("ignored".into());
        assert_eq!(
            element_from_props(ElementId("l".into()), &plain).value,
            None
        );
    }

    #[test]
    fn interactive_means_role_or_pattern() {
        let button = element_from_props(ElementId("b".into()), &props(50000, "OK"));
        let text = element_from_props(ElementId("t".into()), &props(50020, "hello"));
        assert!(keeps(ElementFilter::Interactive, &button));
        assert!(!keeps(ElementFilter::Interactive, &text));
        let mut p = props(50033, "");
        p.patterns.invoke = true;
        assert!(keeps(
            ElementFilter::Interactive,
            &element_from_props(ElementId("p".into()), &p)
        ));
        let mut doc = props(50030, "page");
        assert!(!keeps(
            ElementFilter::Interactive,
            &element_from_props(ElementId("d".into()), &doc)
        ));
        doc.patterns.text = true;
        assert!(keeps(
            ElementFilter::Interactive,
            &element_from_props(ElementId("d".into()), &doc)
        ));
        assert!(keeps(ElementFilter::All, &text));
    }

    #[test]
    fn named_needs_a_name_and_a_real_box() {
        let named = element_from_props(ElementId("a".into()), &props(50020, " Save "));
        assert!(keeps(ElementFilter::Named, &named));
        let nameless = element_from_props(ElementId("b".into()), &props(50020, "  "));
        assert!(!keeps(ElementFilter::Named, &nameless));
        let mut flat = props(50020, "x");
        flat.rect = Some(Rect::new(5, 5, 5, 40));
        assert!(!keeps(
            ElementFilter::Named,
            &element_from_props(ElementId("c".into()), &flat)
        ));
        flat.rect = None;
        assert!(!keeps(
            ElementFilter::Named,
            &element_from_props(ElementId("d".into()), &flat)
        ));
    }

    fn numbered(n: usize) -> Vec<Option<(Element, ())>> {
        (0..n)
            .map(|i| {
                Some((
                    element_from_props(ElementId(format!("e{i}")), &props(50000, &format!("b{i}"))),
                    (),
                ))
            })
            .collect()
    }

    #[test]
    fn a_flat_read_stops_at_the_item_cap_and_says_so() {
        let mut budget = Budget::new(3, Instant::now() + Duration::from_secs(60));
        let kept = select_flat(
            numbered(10).into_iter(),
            ElementFilter::Interactive,
            &mut budget,
        );
        assert_eq!(kept.len(), 3);
        assert!(budget.truncated());
        assert_eq!(kept[2].0.name, "b2");
    }

    #[test]
    fn a_flat_read_that_fits_is_not_truncated() {
        let mut budget = Budget::new(3, Instant::now() + Duration::from_secs(60));
        let kept = select_flat(
            numbered(3).into_iter(),
            ElementFilter::Interactive,
            &mut budget,
        );
        assert_eq!(kept.len(), 3);
        assert!(!budget.truncated(), "exactly the cap, nothing left over");
    }

    #[test]
    fn the_cap_counts_kept_elements_not_looked_at_ones() {
        let mut nodes: Vec<Option<(Element, ())>> = (0..5)
            .map(|i| {
                Some((
                    element_from_props(ElementId(format!("t{i}")), &props(50020, "static")),
                    (),
                ))
            })
            .collect();
        nodes.push(None); // an unreadable element is skipped
        nodes.extend(numbered(2));
        let mut budget = Budget::new(2, Instant::now() + Duration::from_secs(60));
        let kept = select_flat(nodes.into_iter(), ElementFilter::Interactive, &mut budget);
        assert_eq!(kept.len(), 2);
        assert!(!budget.truncated());
    }

    #[test]
    fn a_passed_deadline_truncates_the_read() {
        let mut budget = Budget::new(100, Instant::now());
        let kept = select_flat(numbered(5).into_iter(), ElementFilter::All, &mut budget);
        assert!(kept.is_empty());
        assert!(budget.truncated());
        assert_eq!(budget.kept(), 0);
    }

    #[test]
    fn ids_round_trip_with_any_name() {
        for name in [
            "Save",
            "",
            "a|b|c",
            "x, y",
            "name with \"quotes\" and \u{1F600}",
        ] {
            let parts = IdParts {
                hwnd: 0x1_02A4,
                control_type: 50004,
                rect: Rect::new(-5, 10, 300, 44),
                name: name.to_string(),
            };
            let id = encode_id(&parts);
            assert_eq!(decode_id(&id), Some(parts), "{name:?}");
        }
    }

    #[test]
    fn foreign_or_broken_ids_do_not_decode() {
        for bad in [
            "",
            "e17",
            "uia1|x|50000|0,0,1,1|n",
            "uia1|5|50000|0,0,1|n",
            "uia1|5|50000|0,0,1,1,2|n",
            "uia2|5|50000|0,0,1,1|n",
            "uia1|5|50000|0,0,1,1",
        ] {
            assert_eq!(decode_id(&ElementId(bad.into())), None, "{bad:?}");
        }
    }

    #[test]
    fn refind_picks_the_nearest_and_refuses_to_guess_among_lookalikes() {
        let want = Rect::new(100, 100, 200, 140);
        // one match anywhere is taken (it may have moved a lot)
        assert_eq!(
            pick_refind(want, &[Rect::new(900, 900, 1000, 940)]),
            Some(0)
        );
        // several: nearest wins when it is close
        let near = Rect::new(110, 105, 210, 145);
        let far = Rect::new(600, 100, 700, 140);
        assert_eq!(pick_refind(want, &[far, near]), Some(1));
        // several and none close: none
        assert_eq!(
            pick_refind(want, &[far, Rect::new(100, 400, 200, 440)]),
            None
        );
        assert_eq!(pick_refind(want, &[]), None);
    }

    #[test]
    fn text_helpers_follow_the_python_ones() {
        assert_eq!(collapse_ws("  hello \t world\r\n x "), "hello world x");
        assert_eq!(
            first_rect(&[10.0, 20.0, 30.0, 8.0, 1.0, 2.0, 3.0, 4.0]),
            Some(Rect::new(10, 20, 40, 28))
        );
        assert_eq!(first_rect(&[]), None);
        assert_eq!(first_rect(&[1.0, 2.0, 3.0]), None);
        assert!(is_file_name_label("File name:"));
        assert!(is_file_name_label("  file NAME "));
        assert!(!is_file_name_label("File name filter"));
        assert!(!is_file_name_label("Name"));
    }
}
