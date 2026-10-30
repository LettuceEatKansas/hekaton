//! Finding and reading text. Ported from `tests/test_computer.py` (find_text, zoom text, locate_text,
//! read_field, the UIA walk), `tests/test_20260921_live_next.py` (h-clockdate, h-longsite, h-longdoc,
//! openedit) and the OCR plumbing of `core._ocr_winrt`.

use serde_json::json;

use super::StepError;
use super::find::{Cand, RowItem, group_into_rows, lookalike_note, same_line};
use super::testkit::{Kit, cmd_key};
use crate::backend::fake::InputEvent;
use crate::backend::{
    Caret, ClipboardContent, Display, Element, OcrLine, OcrResult, OcrWord, Rect, Role, WindowRef,
};
use crate::env::MapEnv;
use crate::keys::Key;

/// A 1280 x 720 screen read at its own size (shot_long 1280), so a screen point and a model
/// coordinate are the same number.
fn plain_kit() -> Kit {
    let kit = Kit::with_env(MapEnv::new().with("WINHANDS_SHOT_LONG", "1280"));
    set_display(&kit, 1280, 720);
    kit
}

/// A 2560 x 1440 screen read at 1280 (half size): the numbers of the Python tests.
fn half_kit() -> Kit {
    let kit = Kit::with_env(MapEnv::new().with("WINHANDS_SHOT_LONG", "1280"));
    set_display(&kit, 2560, 1440);
    kit
}

fn set_display(kit: &Kit, w: i32, h: i32) {
    kit.fake.set_displays(vec![Display {
        id: 1,
        name: String::new(),
        rect: Rect::from_xywh(0, 0, w, h),
        scale: 1.0,
        primary: true,
    }]);
    kit.fake.clear_screen();
}

fn word(text: &str, l: i32, t: i32, r: i32, b: i32) -> OcrWord {
    OcrWord {
        text: text.into(),
        rect: Rect::new(l, t, r, b),
        confidence: 1.0,
    }
}

/// One OCR line made of a single run of text with this box.
fn line(text: &str, l: i32, t: i32, r: i32, b: i32) -> OcrLine {
    OcrLine {
        text: text.into(),
        rect: Rect::new(l, t, r, b),
        words: vec![word(text, l, t, r, b)],
        confidence: 1.0,
    }
}

fn ocr(kit: &Kit, lines: Vec<OcrLine>) {
    kit.fake.set_ocr(OcrResult {
        lines,
        engine: "fake".into(),
    });
}

fn front_with(kit: &Kit, controls: &[(&str, Rect)]) -> WindowRef {
    let w = kit.front("Page", "app.exe", 3);
    kit.fake
        .update_window(w, |i| i.rect = Rect::from_xywh(0, 0, 2560, 1440));
    let mut root = Element::new("root", Role::Window, "Page");
    for (i, (name, rect)) in controls.iter().enumerate() {
        root = root.with_child(Element::new(&format!("c{i}"), Role::Text, name).at(*rect));
    }
    kit.fake.set_tree(w, root);
    w
}

// ---------------------------------------------------------------------------- find_text

// Ported from test_find_text_reads_the_row.
#[test]
fn find_text_reads_the_row() {
    let kit = plain_kit();
    ocr(
        &kit,
        vec![
            line("Developer", 683, 647, 751, 660),
            line("Mega Crit", 789, 647, 849, 660),
            line("Publishers", 683, 673, 754, 683),
        ],
    );
    let ctx = kit.ctx(json!({}));
    let (note, img) = ctx.find_text("developer").unwrap();
    assert_eq!(
        note,
        "found 1 line(s) with 'developer' (ocr); each row reads left to right with click points in \
         full-screenshot coordinates:\nDeveloper @[717, 653] | Mega Crit @[819, 653]"
    );
    assert!(img.is_some());
    let (note, img) = ctx.find_text("nothing here").unwrap();
    assert!(
        note.starts_with(
            "no text on screen matches 'nothing here' (tried UIA then OCR, may miss stylised text)."
        ),
        "{note}"
    );
    assert!(img.is_none());
    // Ported from test_find_text_miss_points_to_read_text.
    assert!(note.contains("read_text"), "{note}");
}

// Ported from test_find_text_points_at_the_word_inside_a_line.
#[test]
fn find_text_points_at_the_word_inside_a_line() {
    let kit = plain_kit();
    ocr(&kit, vec![line("status: draft", 100, 200, 230, 220)]);
    let (note, _) = kit.ctx(json!({})).find_text("draft").unwrap();
    // "draft" is characters 8-13 of 13: its centre is ~81% along the line
    assert!(note.contains("'draft' itself @[205, 210]"), "{note}");
}

// Ported from test_find_text_at_screen_corner_keeps_its_answer (h-clockdate): the bottom-right pixel
// used to round one past the shot and the answer was lost.
#[test]
fn find_text_at_the_screen_corner_keeps_its_answer() {
    let kit = half_kit();
    ocr(&kit, vec![line("9/21/2026", 2440, 1400, 2555, 1435)]);
    let (note, _) = kit.ctx(json!({})).find_text("2026").unwrap();
    assert!(note.contains("9/21/2026 @["), "{note}");
    assert!(!note.contains("outside"), "{note}");
}

// Ported from test_find_text_prefers_uia_and_labels_source.
#[test]
fn find_text_prefers_uia_and_labels_the_source() {
    let kit = plain_kit();
    front_with(&kit, &[("Developer", Rect::new(683, 647, 751, 660))]);
    ocr(&kit, vec![line("never read", 0, 0, 10, 10)]);
    let (note, _) = kit.ctx(json!({})).find_text("developer").unwrap();
    assert!(
        note.contains("found 1 line(s) with 'developer' (uia)") && note.contains("Developer @"),
        "{note}"
    );
    assert_eq!(kit.fake.ocr_calls(), 0, "OCR must not run when UIA answers");
}

const PAGE_LINES: usize = 180;

fn page() -> String {
    (1..=PAGE_LINES)
        .map(|i| {
            if i == 140 {
                "Paragraph 140: the reference value is TII5ML".to_string()
            } else {
                format!("Paragraph {i}: routine notes, nothing notable here.")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Ported from test_find_text_reads_exact_page_text_even_off_screen (h-longsite).
#[test]
fn find_text_reads_exact_page_text_even_off_screen() {
    let kit = half_kit();
    let w = front_with(&kit, &[]);
    kit.fake.set_document(w, &page());
    let (note, img) = kit
        .ctx(json!({"coords": "norm1000"}))
        .find_text("reference")
        .unwrap();
    assert!(note.contains("uia text"), "{note}");
    assert!(
        note.contains("Paragraph 140: the reference value is TII5ML"),
        "{note}"
    );
    assert!(note.contains("not on screen now"), "{note}");
    assert!(img.is_none());
    assert_eq!(kit.fake.ocr_calls(), 0);
}

// Ported from test_find_text_on_screen_document_hit_has_click_points.
#[test]
fn find_text_on_a_document_hit_that_is_on_screen_has_click_points() {
    let kit = half_kit();
    let w = front_with(&kit, &[]);
    kit.fake.set_document(w, &page());
    kit.fake.set_document_rects(
        w,
        139,
        Rect::new(300, 505, 500, 525),
        Rect::new(100, 500, 900, 530),
    );
    let (note, img) = kit
        .ctx(json!({"coords": "norm1000"}))
        .find_text("reference")
        .unwrap();
    assert!(note.contains("TII5ML @["), "{note}");
    assert!(note.contains("'reference' itself @["), "{note}");
    assert!(img.is_some(), "a picture around the first on-screen hit");
}

// ---------------------------------------------------------------------------- locate_text

// Ported from test_locate_text_prefers_uia_over_ocr.
#[test]
fn locate_text_prefers_uia_over_ocr() {
    let kit = plain_kit();
    front_with(&kit, &[("Save", Rect::new(10, 10, 50, 30))]);
    ocr(&kit, vec![line("Save", 0, 0, 10, 10)]);
    let got = kit.ctx(json!({})).locate_text("save").unwrap();
    assert_eq!(
        got,
        vec![Cand {
            x: 30,
            y: 20,
            source: "uia",
            label: "Save".into(),
            actionable: false
        }]
    );
    assert_eq!(kit.fake.ocr_calls(), 0);
}

// Ported from test_locate_text_falls_back_to_ocr: words, not lines, so one word is one candidate.
#[test]
fn locate_text_falls_back_to_ocr_words() {
    let kit = plain_kit();
    front_with(&kit, &[]);
    ocr(&kit, vec![line("Save", 100, 100, 140, 120)]);
    let got = kit.ctx(json!({})).locate_text("save").unwrap();
    assert_eq!(
        got,
        vec![Cand {
            x: 120,
            y: 110,
            source: "ocr",
            label: "Save".into(),
            actionable: false
        }]
    );
    // a blank query finds nothing; text that is not there finds nothing
    assert!(kit.ctx(json!({})).locate_text("   ").unwrap().is_empty());
    assert!(kit.ctx(json!({})).locate_text("zzz").unwrap().is_empty());
}

#[test]
fn an_ocr_engine_that_is_missing_means_no_text_not_an_error() {
    let kit = plain_kit();
    front_with(&kit, &[]);
    kit.fake.set_ocr_available(false);
    assert!(kit.ctx(json!({})).locate_text("save").unwrap().is_empty());
    let (note, _) = kit.ctx(json!({})).find_text("save").unwrap();
    assert!(note.starts_with("no text on screen matches"), "{note}");
}

// Ported from test_uia_elements_walks_and_time_boxes: the window itself counts as an element, so a
// search for the window's title finds the window.
#[test]
fn uia_elements_include_the_window_and_its_named_children() {
    let kit = plain_kit();
    let w = kit.front("Root", "app.exe", 3);
    let mut root = Element::new("root", Role::Window, "Root").at(Rect::new(0, 0, 100, 100));
    root = root
        .with_child(Element::new("save", Role::Button, "Save").at(Rect::new(10, 10, 50, 30)))
        .with_child(Element::new("anon", Role::Button, "").at(Rect::new(10, 10, 50, 30)))
        .with_child(Element::new("flat", Role::Button, "Flat").at(Rect::new(5, 5, 5, 5)));
    kit.fake.set_tree(w, root);
    let ctx = kit.ctx(json!({}));
    let names: Vec<String> = ctx
        .uia_elements(Some(w), None)
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["Root", "Save"]);
    assert!(ctx.uia_elements(None, None).is_empty());
    assert!(
        ctx.uia_elements(Some(WindowRef::from_raw(31337)), None)
            .is_empty()
    );
}

// ---------------------------------------------------------------------------- zoom text

fn zoom(kit: &Kit, region: [i32; 4]) -> String {
    kit.ctx(json!({})).zoom_text(region).unwrap()
}

// Ported from test_zoom_text_gives_full_screen_click_points.
#[test]
fn zoom_text_gives_full_screen_click_points() {
    let kit = half_kit();
    // a 100 x 40 screen box (50 x 20 in the 1280-wide shot space) is upscaled 4x for OCR
    ocr(&kit, vec![line("Save", 0, 0, 40, 20)]);
    let out = zoom(&kit, [500, 300, 550, 320]);
    let (cx, cy) = (1000.0 + 20.0 / 4.0, 600.0 + 10.0 / 4.0);
    let want = kit.ctx(json!({})).scale().from_screen(cx as i32, cy as i32);
    assert!(
        out.contains(&format!("Save @[{}, {}]", want[0], want[1])),
        "{out}"
    );
    assert!(
        out.starts_with("\ntext in the zoom box (ocr), one screen row per line"),
        "{out}"
    );
}

// Ported from test_zoom_text_groups_table_rows: OCR hands back a column of names, then a column of
// dates, and the rows must pair them up.
#[test]
fn zoom_text_groups_table_rows() {
    let kit = half_kit();
    ocr(
        &kit,
        vec![
            line("alpha", 0, 0, 40, 20),
            line("echo", 0, 40, 40, 60),
            line("9/6/2026", 200, 0, 300, 20),
            line("9/15/2026", 200, 41, 300, 61),
        ],
    );
    let out = zoom(&kit, [0, 0, 600, 300]);
    let lines: Vec<&str> = out.trim().lines().skip(1).collect();
    assert!(
        lines[0].starts_with("alpha @") && lines[0].contains("| 9/6/2026 @"),
        "{out}"
    );
    assert!(
        lines[1].starts_with("echo @") && lines[1].contains("| 9/15/2026 @"),
        "{out}"
    );
}

// Ported from test_zoom_swaps_ocr_line_for_the_documents_own_text (h-longsite).
#[test]
fn zoom_swaps_an_ocr_line_for_the_documents_own_text() {
    let kit = half_kit();
    let w = front_with(&kit, &[]);
    kit.fake.set_document(w, &page());
    ocr(
        &kit,
        vec![line("the reference value is T115YIL", 0, 0, 300, 20)],
    );
    // the zoom box is 1200 x 600 screen px, so OCR runs unscaled; the line is at (2, 10)
    kit.fake.set_document_line_at(
        w,
        crate::backend::Point::new(2, 10),
        "Paragraph 140: the reference value is TII5ML",
    );
    let out = zoom(&kit, [0, 0, 600, 300]);
    assert!(
        out.contains("Paragraph 140: the reference value is TII5ML @"),
        "{out}"
    );
    assert!(out.contains("(uia text)"), "{out}");
    assert!(!out.contains("OCR caution"), "{out}");
}

// Ported from test_ocr_only_zoom_flags_lookalike_code.
#[test]
fn an_ocr_only_zoom_flags_lookalike_code() {
    let kit = half_kit();
    ocr(
        &kit,
        vec![line("the reference value is T115LML", 0, 0, 300, 20)],
    );
    let out = zoom(&kit, [0, 0, 600, 300]);
    assert!(out.contains("OCR caution"), "{out}");
    assert!(out.contains("T115LML (I/l/1, S/5)"), "{out}");
}

// Ported from test_lookalike_note_ignores_plain_words_and_numbers.
#[test]
fn the_lookalike_note_ignores_plain_words_and_numbers() {
    let none = lookalike_note(&[
        "Paragraph 183: routine notes, nothing notable here.".into(),
        "Build 26200.9445".into(),
    ]);
    assert_eq!(none, "");
    assert!(lookalike_note(&["access code: KX-6968".into()]).contains("KX-6968"));
    assert!(lookalike_note(&["value TIISML".into()]).contains("TIISML"));
    let many: Vec<String> = (0..9).map(|i| format!("AB{i}1B8S5")).collect();
    assert_eq!(
        lookalike_note(&many).matches("AB").count(),
        6,
        "at most six words are named"
    );
}

// Ported from test_zoom_still_reads_text_when_only_a_title_bar_element_is_in_the_box (openedit).
#[test]
fn a_lone_title_bar_element_does_not_hide_the_text() {
    let kit = half_kit();
    front_with(&kit, &[("System", Rect::new(10, 10, 30, 30))]);
    ocr(&kit, vec![line("status: final", 0, 100, 300, 130)]);
    let out = zoom(&kit, [0, 0, 600, 300]);
    assert!(out.contains("System @[10, 10]"), "{out}");
    assert!(out.contains("status: final @"), "{out}");
    assert!(out.contains("(uia + ocr)"), "{out}");
}

// Ported from test_zoom_names_the_text_cursor_after_a_thin_letter and
// test_zoom_says_nothing_about_the_cursor_when_it_is_elsewhere (openedit).
#[test]
fn zoom_names_the_text_cursor_after_a_thin_letter() {
    let kit = half_kit();
    ocr(
        &kit,
        vec![
            line("status: finall", 100, 100, 400, 140),
            line("owner: evals", 100, 160, 380, 200),
        ],
    );
    kit.fake.set_caret(Some(Caret {
        x: 402,
        y: 104,
        height: 30,
    }));
    let out = zoom(&kit, [0, 0, 600, 300]);
    assert!(
        out.contains("text cursor: it sits right after 'status: finall'"),
        "{out}"
    );
    assert!(
        out.contains("the last 'l' there may be the blinking cursor, not a letter"),
        "{out}"
    );
    assert!(!out.contains("after 'owner: evals'"), "{out}");
    kit.fake.set_caret(Some(Caret {
        x: 900,
        y: 500,
        height: 30,
    }));
    ocr(&kit, vec![line("status: final", 100, 100, 400, 140)]);
    assert!(!zoom(&kit, [0, 0, 600, 300]).contains("text cursor"));
}

#[test]
fn zoom_says_so_when_there_is_no_text_and_can_be_turned_off() {
    let kit = half_kit();
    ocr(&kit, vec![]);
    assert!(
        zoom(&kit, [0, 0, 600, 300])
            .starts_with("\nno text found in the zoom box: it may be off target.")
    );
    let off = Kit::with_env(MapEnv::new().with("WINHANDS_ZOOM_OCR", "0"));
    assert_eq!(zoom(&off, [0, 0, 100, 60]), "");
    let long: Vec<OcrLine> = (0..80)
        .map(|i| {
            line(
                &format!("row number {i} with filler"),
                0,
                i * 3,
                100,
                i * 3 + 2,
            )
        })
        .collect();
    let big = half_kit();
    ocr(&big, long);
    assert!(
        zoom(&big, [0, 0, 600, 300]).trim_end().ends_with(" ..."),
        "long text is cut at 1400 chars"
    );
}

#[test]
fn row_grouping_pairs_names_with_their_dates() {
    let item = |cy: f64, cx: f64, label: &str| RowItem {
        cy,
        cx,
        h: 20.0,
        label: label.into(),
    };
    let rows = group_into_rows(&[
        item(10.0, 200.0, "9/6/2026"),
        item(10.0, 5.0, "alpha"),
        item(50.0, 5.0, "echo"),
        item(51.0, 200.0, "9/15/2026"),
    ]);
    assert_eq!(rows, ["alpha | 9/6/2026", "echo | 9/15/2026"]);
    assert!(group_into_rows(&[]).is_empty());
}

#[test]
fn a_line_is_the_same_line_when_most_of_it_is_there_in_order() {
    assert!(same_line(
        "the reference value is T115YIL",
        "Paragraph 140: the reference value is TII5ML"
    ));
    assert!(!same_line(
        "completely different words",
        "Paragraph 140: the reference value is TII5ML"
    ));
    assert!(!same_line("", "anything"));
}

// ---------------------------------------------------------------------------- read_field

// Ported from test_read_field_uia_direct_name.
#[test]
fn read_field_finds_a_value_by_the_elements_own_name() {
    let kit = plain_kit();
    let w = kit.front("Form", "app.exe", 3);
    kit.fake.set_tree(
        w,
        Element::new("r", Role::Window, "Form").with_child(
            Element::new("email", Role::Edit, "Email address")
                .at(Rect::new(0, 0, 10, 10))
                .valued("jon@x.com"),
        ),
    );
    assert_eq!(
        kit.ctx(json!({}))
            .read_field_uia("Email address")
            .as_deref(),
        Some("jon@x.com")
    );
    let (note, img) = kit
        .step(json!({"action": "read_field", "text": "Email address"}))
        .unwrap();
    assert_eq!(note, "Email address: 'jon@x.com' (uia)");
    assert!(img.is_none());
}

// Ported from test_read_field_uia_caption_row.
#[test]
fn read_field_finds_the_value_to_the_right_of_a_caption() {
    let kit = plain_kit();
    let w = kit.front("Form", "app.exe", 3);
    kit.fake.set_tree(
        w,
        Element::new("r", Role::Window, "Form")
            .with_child(Element::new("cap", Role::Text, "Status:").at(Rect::new(10, 10, 60, 30)))
            .with_child(
                Element::new("far", Role::Edit, "Other")
                    .at(Rect::new(300, 10, 350, 30))
                    .valued("Far"),
            )
            .with_child(
                Element::new("val", Role::Edit, "Draft")
                    .at(Rect::new(70, 10, 120, 30))
                    .valued("Draft"),
            ),
    );
    assert_eq!(
        kit.ctx(json!({})).read_field_uia("Status").as_deref(),
        Some("Draft")
    );
    assert_eq!(kit.ctx(json!({})).read_field_uia("Nothing"), None);
}

// Ported from test_read_field_action_falls_back_to_find_text.
#[test]
fn read_field_falls_back_to_the_ocr_row() {
    let kit = plain_kit();
    ocr(
        &kit,
        vec![
            line("Total", 10, 10, 60, 30),
            line("42.00", 100, 10, 160, 30),
        ],
    );
    let (note, img) = kit
        .step(json!({"action": "read_field", "label": "Total"}))
        .unwrap();
    assert!(note.starts_with("no labelled field found via UIA for 'Total'; nearest OCR row:\nfound 1 line(s) with 'Total' (ocr)"), "{note}");
    assert!(
        note.contains("Total @[35, 20] | 42.00 @[130, 20]"),
        "{note}"
    );
    assert!(img.is_some());
    let err = kit.step(json!({"action": "read_field"})).unwrap_err();
    assert_eq!(
        err.message(),
        "read_field needs text: the field's label, e.g. Email"
    );
}

// ---------------------------------------------------------------------------- read_text

fn log_text() -> String {
    (1..=PAGE_LINES)
        .map(|i| {
            if i == 97 {
                "097 access code: KX-6968".to_string()
            } else {
                format!("{i:03} routine log entry")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tk_kit() -> (Kit, WindowRef) {
    let kit = plain_kit();
    let w = kit.front("EvalPad", "pythonw.exe", 3);
    kit.fake.update_window(w, |i| i.class = "TkTopLevel".into());
    (kit, w)
}

// Ported from test_read_text_finds_the_line_in_a_long_uia_document (h-longdoc).
#[test]
fn read_text_finds_the_line_in_a_long_uia_document() {
    let (kit, w) = tk_kit();
    kit.fake.set_document(w, &log_text());
    let chords = kit.fake.events();
    let out = kit.ctx(json!({})).read_text("access code", "").unwrap();
    assert!(
        out.contains("180 lines") && out.contains("97: 097 access code: KX-6968"),
        "{out}"
    );
    assert!(!out.contains("routine"), "{out}");
    assert_eq!(
        kit.fake.events(),
        chords,
        "no keystrokes: the document gave its own text"
    );
    let whole = kit.ctx(json!({})).read_text("", "").unwrap();
    assert!(
        whole
            .starts_with("read_text (uia, exact characters, 180 lines):\n1: 001 routine log entry"),
        "{whole}"
    );
    assert!(
        whole.ends_with("180: 180 routine log entry"),
        "short enough to come back whole: {whole}"
    );
    let none = kit.ctx(json!({})).read_text("zzz", "").unwrap();
    assert_eq!(
        none,
        "read_text (uia, exact characters, 180 lines): no line contains 'zzz'"
    );
}

#[test]
fn a_long_document_is_cut_with_a_pointer_to_the_search() {
    let (kit, w) = tk_kit();
    let long: String = (1..=400)
        .map(|i| format!("{i:03} {}\n", "x".repeat(40)))
        .collect();
    kit.fake.set_document(w, &long);
    let out = kit.ctx(json!({})).read_text("", "").unwrap();
    // the start and the end stay (a page's pop-up text comes last); the middle is cut
    assert!(
        out.contains(
            " lines cut from the middle; pass text = a word to get only the lines that contain it)"
        ),
        "{out}"
    );
    assert!(
        out.ends_with(&format!("400: 400 {}", "x".repeat(40))),
        "{out}"
    );
    assert!(out.chars().count() < 6400, "{}", out.chars().count());
    assert!(
        out.starts_with("read_text (uia, exact characters, 401 lines):\n1: 001 xxx"),
        "{}",
        &out[..80]
    );
}

#[test]
fn read_text_counts_extra_matches() {
    let (kit, w) = tk_kit();
    kit.fake.set_document(w, &log_text());
    let out = kit.ctx(json!({})).read_text("routine", "").unwrap();
    assert!(out.contains("179 contain 'routine':"), "{out}");
    assert!(
        out.ends_with("(149 more matching lines; pass limit= up to 400 to see more)"),
        "{out}"
    );
    assert_eq!(
        out.lines()
            .filter(|l| l.contains("routine log entry"))
            .count(),
        30
    );
}

// Ported from test_read_text_falls_back_to_copy_for_editors_without_uia and
// test_read_text_by_copy_restores_clipboard_and_clears_selection.
#[test]
fn read_text_copies_for_editors_without_uia_text_and_puts_everything_back() {
    let (kit, _w) = tk_kit();
    kit.fake
        .set_clipboard(ClipboardContent::Text("jon's text".into()));
    let body = log_text();
    let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<Key>>::new()));
    let seen = sink.clone();
    kit.fake.set_chord_hook(move |fake, keys| {
        seen.lock().unwrap().push(keys.to_vec());
        if keys == [cmd_key(), Key::Char('c')] {
            fake.set_clipboard(ClipboardContent::Text(body.clone()));
        }
    });
    let out = kit.ctx(json!({})).read_text("", "").unwrap();
    assert!(
        out.starts_with(
            "read_text (select-all + copy; selection cleared, exact characters, 180 lines)"
        ),
        "{out}"
    );
    assert!(out.contains("\n97: 097 access code"), "{out}");
    assert_eq!(
        sink.lock().unwrap().clone(),
        vec![
            vec![cmd_key(), Key::Char('a')],
            vec![cmd_key(), Key::Char('c')],
            vec![Key::Left]
        ]
    );
    use crate::backend::Backend;
    assert_eq!(kit.fake.clipboard_text().unwrap(), "jon's text");
}

#[test]
fn read_text_refuses_when_the_clipboard_holds_files_and_when_nothing_comes_back() {
    let (kit, _w) = tk_kit();
    kit.fake
        .set_clipboard(ClipboardContent::Files(vec!["C:\\a.txt".into()]));
    let err = kit.ctx(json!({})).read_text("", "").unwrap_err();
    assert_eq!(
        err.message(),
        "read_text needs the clipboard, which holds copied files right now; paste them first, then read"
    );
    assert!(matches!(err, StepError::Target(_)));
    // nothing copied: the window draws its text as pictures
    kit.fake
        .set_clipboard(ClipboardContent::Text("mine".into()));
    let out = kit.ctx(json!({})).read_text("", "").unwrap();
    assert_eq!(
        out,
        "read_text: the window gave no text (it is empty, or it draws its text as pictures); use zoom to look"
    );
    use crate::backend::Backend;
    assert_eq!(kit.fake.clipboard_text().unwrap(), "mine");
    let events = kit.fake.events();
    assert!(
        events.contains(&InputEvent::KeyDown(Key::Left)),
        "the select-all was dropped"
    );
}

// Ported from test_read_text_refuses_explorer_and_the_allowlist_still_applies.
#[test]
fn read_text_refuses_explorer_and_the_allowlist_still_applies() {
    let kit = plain_kit();
    let w = kit.front("Docs", "explorer.exe", 4);
    kit.fake
        .update_window(w, |i| i.class = "CabinetWClass".into());
    let err = kit.ctx(json!({})).read_text("x", "").unwrap_err();
    assert!(matches!(err, StepError::Target(_)));
    assert!(err.message().contains("list_files"), "{err}");
    let (kit, _) = tk_kit();
    let err = kit
        .ctx(json!({"only_apps": ["notepad.exe"]}))
        .read_text("x", "")
        .unwrap_err();
    assert!(
        err.message()
            .starts_with("refused: that window belongs to pythonw.exe"),
        "{err}"
    );
    let empty = plain_kit();
    assert_eq!(
        empty
            .ctx(json!({}))
            .read_text("", "")
            .unwrap_err()
            .message(),
        "no window is in front to read"
    );
    // a protected process is refused too
    let (kit, _) = tk_kit();
    std::fs::create_dir_all(kit.computer.config().paths().dir()).unwrap();
    std::fs::write(kit.computer.config().paths().protected(), "pythonw.exe").unwrap();
    assert!(matches!(
        kit.ctx(json!({})).read_text("", "").unwrap_err(),
        StepError::Permission(_)
    ));
}

#[test]
fn read_text_is_an_acting_step() {
    // it may send select-all / copy, so it waits for a person like any acting step
    let (kit, w) = tk_kit();
    kit.fake.set_document(w, "one\ntwo");
    kit.fake
        .set_human_idle(std::time::Duration::from_millis(10));
    let err = kit.step(json!({"action": "read_text"})).unwrap_err();
    assert!(
        err.message()
            .starts_with("a person is using the mouse or keyboard"),
        "{err}"
    );
}

// 2026-10-11 (macOS TextEdit): find_text saw a two-word phrase in the document while
// wait_for_text, OCR word by word, never did. locate_text now reads the document too.
#[test]
fn locate_text_finds_a_phrase_in_the_document() {
    let kit = half_kit();
    let w = front_with(&kit, &[]);
    kit.fake.set_document(w, "first line\n2nd line here\n");
    kit.fake.set_document_rects(
        w,
        1,
        Rect::new(300, 505, 500, 525),
        Rect::new(100, 500, 900, 530),
    );
    let cands = kit.ctx(json!({})).locate_text("2nd line").unwrap();
    assert_eq!(cands.len(), 1);
    assert_eq!((cands[0].x, cands[0].y), (400, 515));
}

#[test]
fn a_link_and_its_own_text_at_one_spot_are_one_match() {
    let kit = plain_kit();
    front_with(
        &kit,
        &[
            ("Learn more", Rect::new(100, 100, 180, 120)),
            ("Learn more", Rect::new(101, 101, 179, 119)),
            ("Learn more", Rect::new(100, 300, 180, 320)),
        ],
    );
    let cands = kit.ctx(json!({})).locate_text("learn more").unwrap();
    assert_eq!(cands.len(), 2, "{cands:?}");
}

// 2026-10-29 leads run: read_text(window="Untitled") with the browser in front read the browser.
#[test]
fn read_text_reads_the_named_window_not_the_front_one() {
    let kit = plain_kit();
    let doc = kit.window("Untitled", "TextEdit", 7, Rect::from_xywh(10, 10, 400, 300));
    kit.fake
        .set_document(doc, "Name | Handle\nDrew Binsky | @drewbinsky");
    let page = kit.front("YouTube", "Brave Browser", 8);
    kit.fake.set_document(page, "Subscribe\nHome");
    let out = kit
        .ctx(json!({"only_apps": ["TextEdit"]}))
        .read_text("", "Untitled")
        .unwrap();
    assert!(out.contains("2: Drew Binsky | @drewbinsky"), "{out}");
    assert!(!out.contains("Subscribe"), "{out}");
    use crate::backend::Backend;
    assert_eq!(
        kit.fake.foreground_window().unwrap(),
        Some(page),
        "UIA text needs no focus change"
    );
}

#[test]
fn read_text_query_brings_the_value_under_a_short_label_and_cuts_long_paragraphs() {
    let kit = Kit::new();
    let w = kit.front("Japan - Wikipedia", "Brave Browser", 9);
    let long = format!("The total population {}", "x".repeat(400));
    kit.fake.set_document(
        w,
        &format!("Area\n\n• Total\n377,975 km2\n{long}\n• Total\n• Total\n"),
    );
    let out = kit.ctx(json!({})).read_text("total", "").unwrap();
    assert!(out.contains("3: • Total\n   4: 377,975 km2"), "{out}");
    assert!(out.contains("5: The total population xxx"), "{out}");
    assert!(out.contains("..."), "{out}");
    assert!(out.len() < 600, "{out}");
    // a label followed by another match does not repeat it as a value
    assert!(out.contains("6: • Total\n7: • Total"), "{out}");
}

// 2026-10-29 leads rerun: read_text showed 30 matching lines whatever `limit` said.
#[test]
fn read_text_shows_as_many_matching_lines_as_limit_asks() {
    let (kit, w) = tk_kit();
    kit.fake.set_document(w, &log_text());
    let out = kit
        .run(json!({"action": "read_text", "text": "routine", "limit": 99, "shot": false}))
        .unwrap()
        .text;
    assert!(out.contains("100: 100 routine log entry"), "{out}");
    assert!(!out.contains("101: 101 routine"), "{out}");
    assert!(
        out.contains("more matching lines; pass limit= up to 400 to see more)"),
        "{out}"
    );
    let all = kit
        .run(json!({"action": "read_text", "text": "routine", "limit": 400, "shot": false}))
        .unwrap()
        .text;
    assert!(all.contains("180: 180 routine log entry"), "{all}");
    assert!(!all.contains("more matching lines"), "{all}");
}

// 2026-10-29 leads run 3: three read_text steps on one YouTube page copied it three times. Steps in
// one batch share the copy until a step that is not read_text runs.
#[test]
fn read_text_steps_in_one_batch_copy_the_page_once() {
    let (kit, _w) = tk_kit();
    let body = log_text();
    let copies = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = copies.clone();
    kit.fake.set_chord_hook(move |fake, keys| {
        if keys == [cmd_key(), Key::Char('c')] {
            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            fake.set_clipboard(ClipboardContent::Text(body.clone()));
        }
    });
    let out = kit.computer.execute(
        &json!({"actions": [
            {"action": "read_text", "text": "access code"},
            {"action": "read_text", "text": "routine"},
            {"action": "key", "text": "End"},
            {"action": "read_text", "text": "access code"}
        ], "shot": false}),
        &kit.who,
    );
    assert!(out.ok, "{:?}", out.error);
    assert!(
        out.result.as_ref().unwrap().contains("097 access code"),
        "{:?}",
        out.result
    );
    assert_eq!(copies.load(std::sync::atomic::Ordering::Relaxed), 2);
}
