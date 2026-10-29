//! `scope=` and the display fixes of 2026-10-08 (d), over a fake desktop. Ported from
//! `tests/test_20261008d_scope.py` (geometry, what the model is told, find_text and click_text,
//! batches and recipes, front/minimized/covered, the unchanged safety checks) and
//! `tests/test_20261008c_display.py` (the agent's own app in front does not pick the monitor).
//!
//! The Python numbers (a 1280x720 client area at (100, 200)) are scaled down to a 320x180 one at
//! (25, 50) so the fake screen stays small; every test keeps its Python intent. Window refs start
//! at `w0`, as in the Python build.

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::json;

use super::testkit::{Kit, png_size, textured};
use super::{ManyResult, StepError};
use crate::backend::fake::InputEvent;
use crate::backend::{
    Backend, Display, Element, OcrLine, OcrResult, OcrWord, Point, Rect, Role, WindowRef,
};
use crate::env::MapEnv;

const GAME: &str = "Slay the Spire 2";
/// A 320x180 client area at (25, 50) inside a framed window (a title bar and borders around it).
const FRAME: Rect = Rect::new(23, 20, 347, 232);
const CLIENT: Rect = Rect::new(25, 50, 345, 230);
/// The desktop of the kit's world: one 640x360 monitor.
const DESKTOP: Rect = Rect::new(0, 0, 640, 360);

struct World {
    kit: Kit,
    game: WindowRef,
}

fn monitor(id: u32, rect: Rect, primary: bool) -> Display {
    Display {
        id,
        name: String::new(),
        rect,
        scale: 1.0,
        primary,
    }
}

/// Lay out monitors (the first is the primary) with a textured screen over all of them.
fn set_monitors(kit: &Kit, rects: &[Rect]) {
    let displays: Vec<Display> = rects
        .iter()
        .enumerate()
        .map(|(i, r)| monitor(i as u32 + 1, *r, i == 0))
        .collect();
    kit.fake.set_displays(displays);
    let left = rects.iter().map(|r| r.left).min().unwrap_or(0);
    let top = rects.iter().map(|r| r.top).min().unwrap_or(0);
    let right = rects.iter().map(|r| r.right).max().unwrap_or(0);
    let bottom = rects.iter().map(|r| r.bottom).max().unwrap_or(0);
    kit.fake
        .set_screen(textured((right - left) as u32, (bottom - top) as u32));
}

fn world() -> World {
    let kit = Kit::with_env(
        MapEnv::new()
            .with("WINHANDS_IMG", "png")
            .with("WINHANDS_ZOOM_OCR", "0")
            .with("WINHANDS_MARK", "0"),
    );
    set_monitors(&kit, &[DESKTOP]);
    let game = kit.window(GAME, "slaythespire2.exe", 7, FRAME);
    kit.fake.set_client_rect(game, CLIENT);
    kit.fake.set_foreground(Some(game));
    World { kit, game }
}

fn run(w: &World, v: serde_json::Value) -> ManyResult {
    w.kit.run(v).expect("the call runs")
}

fn err_of(r: Result<ManyResult, StepError>) -> StepError {
    r.expect_err("the call is refused")
}

fn clicks(w: &World) -> Vec<Point> {
    w.kit
        .fake
        .events()
        .into_iter()
        .filter_map(|e| match e {
            InputEvent::Click { at, .. } => Some(at),
            _ => None,
        })
        .collect()
}

fn foreground(w: &World) -> Option<WindowRef> {
    w.kit.fake.foreground_window().ok().flatten()
}

fn minimized(w: &World) -> bool {
    w.kit
        .fake
        .window_info(w.game)
        .map(|i| i.minimized)
        .unwrap_or(false)
}

fn scoped_ctx(w: &World, args: serde_json::Value) -> super::Ctx<'_> {
    let ctx = w.kit.ctx(args);
    ctx.set_scope(GAME).expect("scope the game");
    ctx
}

// ---------------------------------------------------------------------------
// geometry: the capture rectangle and the coordinate mapping, both ways
// ---------------------------------------------------------------------------

#[test]
fn the_capture_rect_is_the_client_area_not_the_frame() {
    let w = world();
    let ctx = scoped_ctx(&w, json!({"shot_long": 160}));
    assert_eq!(ctx.display_rect(), CLIENT); // not the frame (23, 20, 347, 232)
    assert_eq!(ctx.scale().shot_size(), (160, 90)); // the whole long edge goes to the window
}

#[test]
fn a_window_without_a_client_area_falls_back_to_the_visible_frame() {
    let w = world();
    w.kit.fake.set_client_rect(w.game, Rect::new(0, 0, 0, 0));
    let ctx = scoped_ctx(&w, json!({}));
    assert_eq!(ctx.display_rect(), FRAME);
}

#[test]
fn norm1000_maps_both_ways_inside_the_window() {
    let w = world();
    let ctx = scoped_ctx(&w, json!({"coords": "norm1000"}));
    let sc = ctx.scale();
    assert_eq!(sc.to_screen(0.0, 0.0), Ok((25, 50)));
    // the window's last pixel, never outside
    assert_eq!(sc.to_screen(1000.0, 1000.0), Ok((25 + 319, 50 + 179)));
    assert_eq!(sc.to_screen(500.0, 500.0), Ok((25 + 159, 50 + 89)));
    assert_eq!(sc.from_screen(25, 50), [0, 0]);
    assert_eq!(sc.from_screen(25 + 319, 50 + 179), [1000, 1000]);
    assert_eq!(sc.from_screen(25 + 159, 50 + 89), [498, 497]); // 159 * 1000 / 319, 89 * 1000 / 179
    for (x, y) in [
        (0.0, 0.0),
        (250.0, 750.0),
        (333.0, 667.0),
        (1000.0, 1000.0),
        (999.0, 1.0),
    ] {
        let (sx, sy) = sc.to_screen(x, y).unwrap();
        let back = sc.from_screen(sx, sy);
        // one window pixel is about 3 units of 0..1000 here (5 vertically), so the trip loses at most that
        assert!(
            (f64::from(back[0]) - x).abs() <= 4.0 && (f64::from(back[1]) - y).abs() <= 6.0,
            "{x},{y} came back as {back:?}"
        );
    }
    let outside = sc.to_screen(1001.0, 5.0).unwrap_err().to_string();
    assert!(outside.contains("outside 0..1000"), "{outside}");
}

#[test]
fn pixel_coordinates_map_both_ways_at_the_scaled_size() {
    let w = world();
    let ctx = scoped_ctx(&w, json!({"coords": "pixels", "shot_long": 160}));
    let sc = ctx.scale();
    assert_eq!(sc.to_screen(0.0, 0.0), Ok((25, 50)));
    assert_eq!(sc.to_screen(80.0, 45.0), Ok((25 + 160, 50 + 90))); // a 160x90 image, scale 0.5
    assert_eq!(sc.from_screen(185, 140), [80, 45]);
    assert_eq!(sc.from_screen(344, 229), [159, 89]); // clamped inside the picture
    assert_eq!(sc.from_screen(5000, 5000), [159, 89]);
    assert_eq!(sc.from_screen(-9, -9), [0, 0]);
    let outside = sc.to_screen(160.0, 10.0).unwrap_err().to_string();
    assert!(
        outside.contains("outside the 160x90 screenshot"),
        "{outside}"
    );
}

#[test]
fn shot_long_sets_the_scoped_image_size() {
    let w = world();
    let ctx = scoped_ctx(&w, json!({"coords": "pixels", "shot_long": 128}));
    let sc = ctx.scale();
    assert_eq!(sc.shot_size(), (128, 72));
    assert_eq!(sc.to_screen(64.0, 36.0), Ok((25 + 160, 50 + 90)));
}

#[test]
fn a_window_on_a_second_monitor_with_a_negative_origin() {
    // monitor 2 lies left of the primary one, so its coordinates are negative
    for (coords, far_in, far_out) in [
        ("norm1000", (1000.0, 1000.0), (-640 + 319, 179)),
        ("pixels", (159.0, 89.0), (-640 + 318, 178)), // int(159 / 0.5), int(89 / 0.5)
    ] {
        let w = world();
        set_monitors(&w.kit, &[DESKTOP, Rect::new(-640, 0, 0, 360)]);
        w.kit
            .fake
            .update_window(w.game, |i| i.rect = Rect::new(-648, -40, -312, 188));
        w.kit
            .fake
            .set_client_rect(w.game, Rect::new(-640, 0, -320, 180));
        let ctx = scoped_ctx(&w, json!({"coords": coords, "shot_long": 160}));
        let sc = ctx.scale();
        assert_eq!(
            ctx.display_rect(),
            Rect::new(-640, 0, -320, 180),
            "{coords}"
        );
        assert_eq!(sc.to_screen(0.0, 0.0), Ok((-640, 0)), "{coords}");
        assert_eq!(sc.to_screen(far_in.0, far_in.1), Ok(far_out), "{coords}");
        assert_eq!(sc.from_screen(-640, 0), [0, 0], "{coords}");
        let back = sc.from_screen(far_out.0, far_out.1);
        assert!(
            (f64::from(back[0]) - far_in.0).abs() <= 1.0
                && (f64::from(back[1]) - far_in.1).abs() <= 1.0,
            "{coords}: {back:?}"
        );
        if coords == "norm1000" {
            assert_eq!(sc.to_screen(500.0, 500.0), Ok((-640 + 159, 89)));
            assert_eq!(sc.from_screen(-640 + 159, 89), [498, 497]);
        }
    }
}

#[test]
fn a_window_on_a_monitor_to_the_right_with_a_positive_offset() {
    let w = world();
    set_monitors(&w.kit, &[DESKTOP, Rect::new(640, 0, 1280, 360)]);
    w.kit
        .fake
        .set_client_rect(w.game, Rect::new(660, 30, 980, 210));
    let ctx = scoped_ctx(&w, json!({"coords": "norm1000"}));
    let sc = ctx.scale();
    assert_eq!(sc.to_screen(500.0, 500.0), Ok((660 + 159, 30 + 89)));
    assert_eq!(sc.from_screen(660 + 159, 30 + 89), [498, 497]);
}

#[test]
fn a_window_hanging_off_the_virtual_screen_is_clipped_to_it() {
    let w = world();
    set_monitors(&w.kit, &[DESKTOP, Rect::new(-640, 0, 0, 360)]);
    // 120 px past the left edge of the desktop
    w.kit
        .fake
        .set_client_rect(w.game, Rect::new(-760, 40, -440, 220));
    let ctx = scoped_ctx(&w, json!({}));
    assert_eq!(ctx.display_rect(), Rect::new(-640, 40, -440, 220));
}

#[test]
fn the_rect_follows_a_window_that_moves() {
    let w = world();
    let ctx = scoped_ctx(&w, json!({}));
    w.kit
        .fake
        .set_client_rect(w.game, Rect::new(300, 80, 620, 260));
    assert_eq!(ctx.display_rect(), Rect::new(300, 80, 620, 260));
}

#[test]
fn an_unmeasurable_window_keeps_its_last_good_rect() {
    let w = world();
    let ctx = scoped_ctx(&w, json!({}));
    w.kit.fake.set_client_rect(w.game, Rect::new(0, 0, 0, 0));
    // what a minimized window reports
    w.kit.fake.update_window(w.game, |i| {
        i.rect = Rect::new(-32000, -32000, -31840, -31970)
    });
    assert_eq!(ctx.display_rect(), CLIENT);
}

#[test]
fn a_window_that_closed_keeps_its_last_good_rect() {
    let w = world();
    let ctx = scoped_ctx(&w, json!({}));
    w.kit.fake.close_window(w.game);
    assert_eq!(ctx.display_rect(), CLIENT);
}

// ---------------------------------------------------------------------------
// what the model is told, and what is captured
// ---------------------------------------------------------------------------

#[test]
fn screenshot_says_what_it_shows_and_crops_exactly_the_window() {
    let w = world();
    let out = run(
        &w,
        json!({"action": "screenshot", "scope": GAME, "coords": "norm1000"}),
    );
    assert!(
        out.text.starts_with(&format!(
            "screenshot of window \"{GAME}\" (320x180); give coordinates in 0-1000 of THIS image"
        )),
        "{}",
        out.text
    );
    assert_eq!(w.kit.fake.captured_rects().last(), Some(&CLIENT));
    assert_eq!(out.images.len(), 1);
    assert_eq!(png_size(&out.images[0]), (320, 180));
}

#[test]
fn a_pixel_model_is_told_the_same_way() {
    let w = world();
    let out = run(
        &w,
        json!({"action": "screenshot", "scope": GAME, "coords": "pixels", "shot_long": 160}),
    );
    assert!(
        out.text.contains(&format!(
            "screenshot of window \"{GAME}\" (160x90); give coordinates in pixels of THIS image"
        )),
        "{}",
        out.text
    );
    assert_eq!(png_size(&out.images[0]), (160, 90));
}

#[test]
fn an_unscoped_screenshot_text_is_unchanged() {
    let w = world();
    let out = run(&w, json!({"action": "screenshot", "coords": "norm1000"}));
    assert!(
        out.text
            .starts_with("screenshot; give coordinates in 0-1000 on both axes")
            && !out.text.contains("window"),
        "{}",
        out.text
    );
}

#[test]
fn zoom_regions_are_relative_to_the_window() {
    let w = world();
    let out = run(
        &w,
        json!({"action": "zoom", "region": [100, 100, 500, 400], "scope": GAME, "coords": "norm1000"}),
    );
    // norm corners: (100,100) -> (25 + int(.1 * 319), 50 + int(.1 * 179)), (500,400) -> (25 + 159, 50 + 71)
    assert_eq!(w.kit.fake.captured_rects()[0], Rect::new(56, 67, 184, 121));
    assert_eq!(out.images.len(), 1);
    assert!(
        out.text
            .contains(&format!("the window \"{GAME}\" screenshot's space")),
        "{}",
        out.text
    );
}

#[test]
fn the_trailing_screenshot_of_an_acting_call_names_the_window() {
    let w = world();
    let out = run(
        &w,
        json!({"action": "left_click", "coordinate": [500, 500], "scope": GAME, "coords": "norm1000"}),
    );
    assert_eq!(clicks(&w), [Point::new(25 + 159, 50 + 89)]);
    assert!(
        out.text.contains(&format!(
            "[image 1: window \"{GAME}\" (320x180); coordinates in 0-1000 of THIS image]"
        )),
        "{}",
        out.text
    );
    assert_eq!(w.kit.fake.captured_rects().last(), Some(&CLIENT));
}

#[test]
fn without_scope_the_trailing_line_still_says_full_screen() {
    let w = world();
    let out = run(
        &w,
        json!({"action": "left_click", "coordinate": [500, 500], "coords": "norm1000"}),
    );
    assert!(out.text.contains("[image 1: full screen]"), "{}", out.text);
}

#[test]
fn a_long_title_is_cut_in_the_note() {
    let w = world();
    let long = "T".repeat(100);
    w.kit.fake.update_window(w.game, |i| i.title = long.clone());
    let out = run(&w, json!({"action": "screenshot", "scope": "w0"}));
    assert!(
        out.text
            .contains(&format!("window \"{}...\"", "T".repeat(57))),
        "{}",
        out.text
    );
}

// ---------------------------------------------------------------------------
// find_text / click_text points
// ---------------------------------------------------------------------------

fn word(text: &str, rect: Rect) -> OcrWord {
    OcrWord {
        text: text.into(),
        rect,
        confidence: 0.9,
    }
}

fn ocr_row(words: Vec<OcrWord>) -> OcrResult {
    OcrResult {
        lines: vec![OcrLine {
            text: words
                .iter()
                .map(|w| w.text.clone())
                .collect::<Vec<_>>()
                .join(" "),
            rect: Rect::default(),
            words,
            confidence: 0.9,
        }],
        engine: "fake".into(),
    }
}

#[test]
fn find_text_points_are_relative_to_the_window() {
    let w = world();
    // one OCR line in the grabbed image (window pixels): x 160..208, y 100..120
    w.kit.fake.set_ocr(ocr_row(vec![
        word("End", Rect::new(160, 100, 180, 120)),
        word("Turn", Rect::new(184, 100, 208, 120)),
    ]));
    let ctx = scoped_ctx(&w, json!({"coords": "norm1000"}));
    let (note, _) = ctx.find_text("End Turn").unwrap();
    // centre of the line is window pixel (184, 110): 184 * 1000 / 319 = 577, 110 * 1000 / 179 = 615
    assert!(
        note.contains("End Turn @[577, 615]") && note.contains("(ocr)"),
        "{note}"
    );
}

#[test]
fn find_text_ignores_elements_outside_the_window() {
    let w = world();
    let root = Element::new("root", Role::Window, GAME)
        .with_child(Element::new("bar", Role::Button, "Close").at(Rect::new(300, 25, 340, 40)));
    w.kit.fake.set_tree(w.game, root);
    let ctx = scoped_ctx(&w, json!({"coords": "norm1000"}));
    let (note, _) = ctx.find_text("Close").unwrap();
    assert!(!note.contains("(uia)"), "{note}"); // the title bar is not in the picture
}

#[test]
fn click_text_candidates_are_screen_points_inside_the_window_only() {
    let w = world();
    let root = Element::new("root", Role::Window, GAME)
        .with_child(Element::new("bar", Role::Button, "Play").at(Rect::new(200, 25, 300, 40)))
        .with_child(Element::new("play", Role::Button, "Play").at(Rect::new(200, 100, 300, 140)));
    w.kit.fake.set_tree(w.game, root);
    let ctx = scoped_ctx(&w, json!({}));
    let found = ctx.locate_text("play").unwrap();
    // the first sits in the title bar, above the client area
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!((found[0].x, found[0].y, found[0].source), (250, 120, "uia"));
}

#[test]
fn ocr_click_candidates_add_the_window_origin() {
    let w = world();
    w.kit
        .fake
        .set_ocr(ocr_row(vec![word("Play", Rect::new(10, 20, 50, 40))]));
    let ctx = scoped_ctx(&w, json!({}));
    let found = ctx.locate_text("play").unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        (found[0].x, found[0].y, found[0].source),
        (25 + 30, 50 + 30, "ocr")
    );
}

// ---------------------------------------------------------------------------
// batches and recipes
// ---------------------------------------------------------------------------

#[test]
fn scope_applies_to_every_step_of_a_batch() {
    let w = world();
    let out = run(
        &w,
        json!({"scope": GAME, "coords": "norm1000", "actions": [
            {"action": "left_click", "coordinate": [0, 0]},
            {"action": "screenshot"},
            {"action": "left_click", "coordinate": [1000, 1000]}]}),
    );
    assert_eq!(
        clicks(&w),
        [Point::new(25, 50), Point::new(25 + 319, 50 + 179)]
    );
    assert!(
        out.text.matches(&format!("window \"{GAME}\"")).count() >= 2
            && !out.text.contains("full screen"),
        "{}",
        out.text
    );
    assert_eq!(out.images.len(), 2);
    assert!(out.images.iter().all(|i| png_size(i) == (320, 180)));
}

#[test]
fn scope_does_not_leak_into_the_next_call() {
    let w = world();
    run(&w, json!({"action": "screenshot", "scope": GAME}));
    let out = run(&w, json!({"action": "screenshot", "coords": "norm1000"}));
    assert!(out.text.starts_with("screenshot; give"), "{}", out.text);
    let last = *w.kit.fake.captured_rects().last().unwrap();
    assert_eq!(last.width(), 640);
}

#[test]
fn scope_wins_over_display() {
    let w = world();
    set_monitors(&w.kit, &[DESKTOP, Rect::new(640, 0, 1280, 360)]);
    let out = run(
        &w,
        json!({"action": "screenshot", "scope": GAME, "display": 2}),
    );
    assert_eq!(w.kit.fake.captured_rects().last(), Some(&CLIENT));
    assert!(
        out.text.contains(&format!("window \"{GAME}\"")),
        "{}",
        out.text
    );
}

#[test]
fn a_step_may_set_its_own_scope_for_that_step_only() {
    let w = world();
    let pad = w
        .kit
        .window("Notepad", "notepad.exe", 8, Rect::new(330, 20, 630, 300));
    w.kit
        .fake
        .set_client_rect(pad, Rect::new(334, 40, 626, 296));
    let out = run(
        &w,
        json!({"coords": "norm1000", "actions": [
            {"action": "screenshot", "scope": "Notepad"},
            {"action": "screenshot", "scope": GAME},
            {"action": "screenshot"}]}),
    );
    let lines: Vec<&str> = out.text.lines().collect();
    assert!(
        lines[0].starts_with("screenshot of window \"Notepad\" (292x256)"),
        "{}",
        out.text
    );
    assert!(
        lines[1].starts_with(&format!("screenshot of window \"{GAME}\" (320x180)")),
        "{}",
        out.text
    );
    assert!(
        lines[2].starts_with("screenshot; give coordinates in 0-1000 on both axes"),
        "{}",
        out.text
    );
}

#[test]
fn a_recipe_step_may_carry_scope_and_it_is_not_reserved() {
    let w = world();
    // `scope` is not one of the call options a stored recipe may not carry
    w.kit.put_user(&json!({
        "name": "shoot",
        "description": "a picture of one window",
        "params": {"target": {"required": true, "description": "the window"}},
        "steps": [{"action": "screenshot", "scope": "{target}"}]
    }));
    let out = run(
        &w,
        json!({"action": "recipe", "name": "shoot", "params": {"target": GAME}}),
    );
    assert!(
        out.text
            .contains(&format!("screenshot of window \"{GAME}\" (320x180)")),
        "{}",
        out.text
    );
    // and the bundled recipes do not set one
    let steps = crate::recipes::expand(
        &w.kit.computer.recipes,
        &[json!({"action": "recipe", "name": "capture_page",
                 "params": {"url": "https://x.org", "file": "C:/t/p.png"}})],
        &BTreeSet::new(),
    )
    .unwrap();
    assert!(!steps.is_empty() && steps.iter().all(|s| s.step.get("scope").is_none()));
}

#[test]
fn a_failed_scope_inside_a_batch_names_the_step() {
    let w = world();
    let out = run(
        &w,
        json!({"actions": [{"action": "screenshot", "scope": "no such window"}]}),
    );
    assert!(
        out.text.contains("step 1 (screenshot) failed") && out.text.contains("no window matches"),
        "{}",
        out.text
    );
    assert!(out.images.is_empty());
}

#[test]
fn a_call_made_only_of_screenless_actions_ignores_scope() {
    let w = world();
    for action in ["list_windows", "recipe_list"] {
        // it must still be able to list what is open
        let out = run(&w, json!({"action": action, "scope": "no such window"}));
        assert!(!out.text.is_empty(), "{action}");
    }
    // a batch is scoped
    let err = err_of(
        w.kit
            .run(json!({"actions": [{"action": "list_windows"}], "scope": "no such window"})),
    );
    assert!(err.message().contains("no window matches"), "{err}");
}

// ---------------------------------------------------------------------------
// front, minimized, covered
// ---------------------------------------------------------------------------

#[test]
fn a_minimized_window_is_restored_after_the_human_pause() {
    let w = world();
    w.kit.fake.update_window(w.game, |i| i.minimized = true);
    let ctx = scoped_ctx(&w, json!({}));
    assert!(w.kit.fake.human_idle_reads() > 0, "the pause was consulted");
    assert!(!minimized(&w));
    assert_eq!(ctx.display_rect(), CLIENT);
}

#[test]
fn a_person_at_the_mouse_stops_a_scope_that_must_restore_the_window() {
    let w = world();
    w.kit.fake.update_window(w.game, |i| i.minimized = true);
    w.kit.fake.set_human_idle(Duration::from_millis(50));
    let ctx = w.kit.ctx(json!({}));
    let err = ctx.set_scope(GAME).unwrap_err();
    assert!(err.message().contains("a person is using"), "{err}");
    assert!(minimized(&w), "left alone");
    assert!(!ctx.scoped());
}

#[test]
fn a_minimized_window_that_cannot_be_restored_is_a_clear_error() {
    let w = world();
    w.kit.fake.update_window(w.game, |i| i.minimized = true);
    w.kit.fake.refuse_restore(true);
    let err = err_of(w.kit.run(json!({"action": "screenshot", "scope": GAME})));
    assert!(
        err.message()
            .contains("minimized and could not be restored"),
        "{err}"
    );
    assert!(w.kit.fake.captured_rects().is_empty());
}

fn cover(w: &World, rect: Rect) -> WindowRef {
    let cover = w.kit.window("Some editor", "editor.exe", 9, rect);
    w.kit.fake.set_foreground(Some(cover));
    cover
}

#[test]
fn a_covered_window_is_brought_to_the_front_after_the_human_pause() {
    let w = world();
    cover(&w, DESKTOP);
    let _ctx = scoped_ctx(&w, json!({}));
    assert_eq!(foreground(&w), Some(w.game));
    assert!(w.kit.fake.human_idle_reads() > 0);
    assert_eq!(w.kit.computer.mem().target, Some(w.game));
}

#[test]
fn an_uncovered_window_is_left_alone() {
    let w = world();
    let other = w
        .kit
        .window("Other", "other.exe", 9, Rect::new(400, 300, 500, 350));
    w.kit.fake.set_foreground(Some(other));
    let _ctx = scoped_ctx(&w, json!({}));
    assert_eq!(foreground(&w), Some(other));
    assert_eq!(w.kit.fake.human_idle_reads(), 0, "no pause needed");
}

#[test]
fn a_cover_over_one_corner_is_noticed() {
    let w = world();
    // the lower right inset corner is (25 + 288, 50 + 162) = (313, 212)
    cover(&w, Rect::new(300, 200, 340, 225));
    let _ctx = scoped_ctx(&w, json!({}));
    assert_eq!(foreground(&w), Some(w.game));
}

#[test]
fn the_human_input_pause_still_stops_a_scope_that_must_raise_the_window() {
    let w = world();
    let cover = cover(&w, DESKTOP);
    w.kit.fake.set_human_idle(Duration::from_millis(50));
    let ctx = w.kit.ctx(json!({}));
    let err = ctx.set_scope(GAME).unwrap_err();
    assert!(err.message().contains("a person is using"), "{err}");
    assert_eq!(foreground(&w), Some(cover));
    assert!(!ctx.scoped());
}

#[test]
fn a_window_off_every_monitor_is_a_clear_error() {
    let w = world();
    w.kit
        .fake
        .set_client_rect(w.game, Rect::new(5000, 5000, 5320, 5180));
    let err = w.kit.ctx(json!({})).set_scope(GAME).unwrap_err();
    assert!(err.message().contains("no visible area"), "{err}");
}

// ---------------------------------------------------------------------------
// the existing safety checks are unchanged
// ---------------------------------------------------------------------------

#[test]
fn the_callers_own_window_cannot_be_scoped() {
    let w = world();
    w.kit
        .window("Hermes", "hermes.exe", 12, Rect::new(0, 0, 200, 100));
    let err = err_of(
        w.kit
            .run(json!({"action": "screenshot", "scope": "Hermes"})),
    );
    assert!(err.message().contains("refused"), "{err}");
    assert!(w.kit.fake.captured_rects().is_empty());
}

#[test]
fn only_apps_applies_to_the_scoped_window() {
    let w = world();
    let err = err_of(
        w.kit
            .run(json!({"action": "screenshot", "scope": GAME, "only_apps": ["notepad.exe"]})),
    );
    assert!(err.message().contains("may not touch"), "{err}");
    let out = run(
        &w,
        json!({"action": "screenshot", "scope": GAME, "only_apps": ["slaythespire2.exe"]}),
    );
    assert!(
        !out.images.is_empty() && out.text.contains(&format!("window \"{GAME}\"")),
        "{}",
        out.text
    );
}

#[test]
fn a_protected_process_cannot_be_scoped() {
    let w = world();
    let file = w.kit.computer.cfg.paths().protected();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "slaythespire2.exe\n").unwrap();
    let err = w.kit.ctx(json!({})).set_scope(GAME).unwrap_err();
    assert!(
        matches!(err, StepError::Permission(_)) && err.message().contains("is protected"),
        "{err:?}"
    );
}

#[test]
fn ambiguous_and_missing_scopes_use_the_window_resolver() {
    let w = world();
    w.kit.window(
        "Slay the Spire 2 launcher",
        "launcher.exe",
        8,
        Rect::new(0, 0, 100, 100),
    );
    let ctx = w.kit.ctx(json!({}));
    // the exact title beats the longer one
    let label = ctx.set_scope(GAME).unwrap();
    assert!(label.contains(&format!("\"{GAME}\"")), "{label}");
    let err = ctx.set_scope("Spire").unwrap_err();
    assert!(err.message().contains("ambiguous"), "{err}");
    let err = ctx.set_scope("Nothing Here").unwrap_err();
    assert!(err.message().contains("no window matches"), "{err}");
    // by exe name
    assert!(ctx.set_scope("slaythespire2.exe").unwrap().contains(GAME));
    // the window in front (no ref in the list)
    assert!(ctx.set_scope("front").unwrap().starts_with("hwnd:"));
}

#[test]
fn acting_steps_still_pause_for_a_human() {
    let w = world();
    w.kit.fake.set_human_idle(Duration::from_millis(50));
    let out = run(
        &w,
        json!({"action": "left_click", "coordinate": [500, 500], "scope": GAME, "coords": "norm1000"}),
    );
    assert!(
        out.text.contains("a person is using the mouse or keyboard"),
        "{}",
        out.text
    );
    assert!(clicks(&w).is_empty());
}

#[test]
fn clicks_still_check_the_window_under_the_pointer() {
    let w = world();
    let host = w
        .kit
        .window("Hermes", "hermes.exe", 12, Rect::new(500, 0, 600, 100));
    // whatever the pointer is over is the agent's own window, wherever it is
    w.kit.fake.set_window_at_point(Some(host));
    let out = run(
        &w,
        json!({"action": "left_click", "coordinate": [500, 500], "scope": GAME, "coords": "norm1000"}),
    );
    assert!(
        out.text.contains("step 1 (left_click) failed") && out.text.contains("refused"),
        "{}",
        out.text
    );
    assert!(clicks(&w).is_empty());
}

#[test]
fn save_to_under_scope_saves_the_window_and_says_so() {
    let w = world();
    let file = w.kit.tmp.path().join("game.png");
    let out = run(
        &w,
        json!({"action": "screenshot", "save_to": file.to_string_lossy(), "scope": GAME}),
    );
    let bytes = std::fs::read(&file).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(
        (
            u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
            u32::from_be_bytes(bytes[20..24].try_into().unwrap())
        ),
        (320, 180)
    );
    assert!(
        out.text.contains(&format!("saved window \"{GAME}\"")) && out.text.contains("320x180px"),
        "{}",
        out.text
    );
}

// ---------------------------------------------------------------------------
// test_20261008c_display: the agent's own app in front does not pick the monitor
// ---------------------------------------------------------------------------

const MONITOR_1: Rect = Rect::new(0, 0, 640, 360);
const MONITOR_2: Rect = Rect::new(640, 0, 1280, 360);

/// The game on monitor 1, then the window in front on monitor 2.
fn two_monitors(front_app: &str) -> Kit {
    let kit = Kit::new();
    set_monitors(&kit, &[MONITOR_1, MONITOR_2]);
    kit.window(
        "Slay the Spire 2",
        "slaythespire2.exe",
        7,
        Rect::new(100, 40, 500, 300),
    );
    let front = kit.window("Front", front_app, 12, Rect::new(700, 20, 1000, 200));
    kit.fake.set_foreground(Some(front));
    kit
}

#[test]
fn the_agents_own_app_in_front_does_not_pick_the_monitor() {
    let kit = two_monitors("hermes.exe");
    let ctx = kit.ctx(json!({}));
    assert_eq!(ctx.display_rect(), MONITOR_1); // the game's monitor, not Hermes'
}

#[test]
fn an_ordinary_window_in_front_still_picks_its_monitor() {
    let kit = two_monitors("brave.exe");
    let ctx = kit.ctx(json!({}));
    assert_eq!(ctx.display_rect(), MONITOR_2);
}

#[test]
fn a_minimized_or_hidden_window_is_not_the_one_behind_the_agent() {
    let kit = Kit::new();
    set_monitors(&kit, &[MONITOR_1, MONITOR_2]);
    let mut hidden = crate::backend::WindowInfo::basic(
        "Overview ghost",
        "ghost.exe",
        5,
        Rect::new(100, 40, 500, 300),
    );
    hidden.hidden = true;
    kit.fake.add_window(hidden);
    let gone = kit.window("Parked", "parked.exe", 6, Rect::new(100, 40, 500, 300));
    kit.fake.update_window(gone, |i| i.minimized = true);
    let hermes = kit.window("Front", "hermes.exe", 12, Rect::new(700, 20, 1000, 200));
    kit.fake.set_foreground(Some(hermes));
    let ctx = kit.ctx(json!({}));
    // nothing else is on screen: the agent's own window decides, as before
    assert_eq!(ctx.display_rect(), MONITOR_2);
}

#[test]
fn an_allowlist_pinning_one_window_scopes_the_call_to_it() {
    let w = world();
    let out = run(
        &w,
        json!({"action": "left_click", "coordinate": [500, 500], "coords": "norm1000",
               "only_apps": ["slaythespire2.exe:Spire"]}),
    );
    assert_eq!(clicks(&w), [Point::new(25 + 159, 50 + 89)]);
    assert!(
        out.text.contains(&format!("[image 1: window \"{GAME}\"")),
        "{}",
        out.text
    );
}

#[test]
fn a_pinned_window_still_gets_the_full_screen_when_asked_or_unpinned() {
    let w = world();
    let full = run(
        &w,
        json!({"action": "screenshot", "scope": false, "only_apps": ["slaythespire2.exe:Spire"]}),
    );
    assert!(!full.text.contains("window \""), "{}", full.text);
    let app_only = run(
        &w,
        json!({"action": "screenshot", "only_apps": ["slaythespire2.exe"]}),
    );
    assert!(!app_only.text.contains("window \""), "{}", app_only.text);
    let missing = run(
        &w,
        json!({"action": "screenshot", "only_apps": ["slaythespire2.exe:Not open"]}),
    );
    assert!(!missing.text.contains("window \""), "{}", missing.text);
}

#[test]
fn a_remembered_pin_is_dropped_when_the_window_changes_title() {
    let w = world();
    let pin = json!({"action": "screenshot", "only_apps": ["slaythespire2.exe:Spire"]});
    assert!(run(&w, pin.clone()).text.contains("window \""));
    assert!(
        run(&w, pin.clone()).text.contains("window \""),
        "the second call reuses it"
    );
    w.kit
        .fake
        .update_window(w.game, |i| i.title = "Main menu".into());
    assert!(!run(&w, pin).text.contains("window \""));
}

#[test]
fn a_title_scoped_entry_may_name_a_mac_app_by_its_short_name() {
    use super::guard::entry_names_app;
    assert!(entry_names_app("com.apple.textedit", "com.apple.textedit"));
    assert!(entry_names_app("slaythespire2.exe", "slaythespire2.exe"));
    assert!(!entry_names_app("apple", "com.apple.textedit"));
    assert!(!entry_names_app("edit", "com.apple.textedit"));
    assert_eq!(
        entry_names_app("textedit", "com.apple.textedit"),
        !cfg!(windows)
    );
}
