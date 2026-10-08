//! The task shortcuts over a fake desktop: the `window` and `folder` actions, window-relative `at`
//! points, `screenshot save_to` and `open_app` on a URL. Ported from
//! `tests/test_20261008b_shortcuts.py`, whose `Desk` fixture stood in for Win32, the Shell COM object
//! and the screen; here the same job is done by [`FakeBackend`](crate::backend::fake::FakeBackend).

use std::cell::Cell;
use std::path::Path;

use serde_json::{Value, json};

use super::testkit::Kit;
use super::{CallOpts, Ctx, StepError};
use crate::backend::fake::InputEvent;
use crate::backend::{
    AppTarget, Backend, LaunchSpec, Point, Rect, WindowInfo, WindowKind, WindowOp, WindowRef,
};
use crate::caller::Caller;
use crate::destructive::Policy;

// Used only by the Explorer folder tests at the end, which are compiled off macOS only.
#[cfg(not(target_os = "macos"))]
use std::collections::VecDeque;
#[cfg(not(target_os = "macos"))]
use std::sync::{Arc, Mutex};
#[cfg(not(target_os = "macos"))]
use std::time::{Duration, UNIX_EPOCH};

#[cfg(not(target_os = "macos"))]
use crate::backend::fake::ShellMode;
#[cfg(not(target_os = "macos"))]
use crate::backend::{Element, Role};
#[cfg(not(target_os = "macos"))]
use crate::env::MapEnv;
#[cfg(not(target_os = "macos"))]
use crate::keys::Key;

// ---------------------------------------------------------------------------- a fake desktop

/// Windows by title, app and class, in a kit with one small display.
struct Desk {
    kit: Kit,
    pid: Cell<u32>,
}

fn kind_of(class: &str) -> WindowKind {
    match class {
        "CabinetWClass" => WindowKind::FileManager,
        "Progman" | "WorkerW" => WindowKind::Desktop,
        "Shell_TrayWnd" => WindowKind::Taskbar,
        _ => WindowKind::Normal,
    }
}

impl Desk {
    fn new() -> Self {
        Self {
            kit: Kit::new(),
            pid: Cell::new(0),
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn with_env(env: MapEnv) -> Self {
        Self {
            kit: Kit::with_env(env),
            pid: Cell::new(0),
        }
    }

    /// A window (the most recently added is on top), 200 x 120 inside the display.
    fn add(&self, title: &str, app: &str, class: &str) -> WindowRef {
        self.add_at(title, app, class, Rect::from_xywh(10, 10, 200, 120))
    }

    fn add_at(&self, title: &str, app: &str, class: &str, rect: Rect) -> WindowRef {
        self.pid.set(self.pid.get() + 1);
        let w = self.kit.window(title, app, self.pid.get(), rect);
        let class = class.to_string();
        self.kit.fake.update_window(w, move |i| {
            i.kind = kind_of(&class);
            i.class = class;
        });
        w
    }

    /// The `wN` ref of a window, as `list_windows` would print it.
    fn wref(&self, w: WindowRef) -> String {
        let lines = self.kit.computer.window_refs().list(false).unwrap();
        let line = lines
            .iter()
            .find(|l| l.window == w)
            .expect("window is listed");
        format!("w{}", line.reference)
    }

    fn resolve(&self, spec: &str) -> Result<Vec<WindowRef>, StepError> {
        self.resolve_note(spec, false).map(|(w, _)| w)
    }

    fn resolve_note(&self, spec: &str, all: bool) -> Result<(Vec<WindowRef>, String), StepError> {
        let ctx = self.kit.ctx(json!({}));
        ctx.resolve_window(spec, all)
            .map(|(t, note)| (t.into_iter().map(|t| t.window).collect(), note))
    }

    fn window(&self, args: Value) -> Result<String, StepError> {
        let mut a = args;
        a["action"] = json!("window");
        self.kit.step(a).map(|(note, img)| {
            assert!(img.is_none());
            note
        })
    }

    fn window_only(&self, only: &[&str], args: Value) -> Result<String, StepError> {
        let mut a = args;
        a["action"] = json!("window");
        self.kit.step_only(only, a).map(|(note, _)| note)
    }

    fn ops(&self) -> Vec<(WindowRef, WindowOp)> {
        self.kit.fake.window_ops()
    }
}

fn err(r: Result<String, StepError>) -> StepError {
    r.expect_err("the call should fail")
}

// ---------------------------------------------------------------------------- window: finding the target

// Ported from test_window_by_ref_title_and_exe_name.
#[test]
fn window_by_ref_title_and_exe_name() {
    let d = Desk::new();
    let notepad = d.add("Untitled - Notepad", "notepad.exe", "Notepad");
    let explorer = d.add("Downloads", "explorer.exe", "CabinetWClass");
    d.add(
        "Calculator",
        "applicationframehost.exe",
        "ApplicationFrameHost",
    );
    assert_eq!(d.resolve(&d.wref(explorer)).unwrap(), [explorer]);
    assert_eq!(d.resolve("Downloads").unwrap(), [explorer], "exact title");
    assert_eq!(d.resolve("untitled").unwrap(), [notepad], "part of a title");
    assert_eq!(
        d.resolve("notepad").unwrap(),
        [notepad],
        "app name without .exe"
    );
    assert_eq!(d.resolve("notepad.exe").unwrap(), [notepad]);
    assert_eq!(
        d.resolve("EXPLORER.EXE").unwrap(),
        [explorer],
        "case does not matter"
    );
}

// Ported from test_a_store_app_is_found_by_its_real_exe_not_applicationframehost.
#[test]
fn a_store_app_is_found_by_its_real_exe_not_applicationframehost() {
    let d = Desk::new();
    let calc = d.add(
        "Calculator",
        "ApplicationFrameHost.exe",
        "ApplicationFrameHost",
    );
    d.kit.fake.set_hosted_app(calc, "calculatorapp.exe");
    assert_eq!(d.resolve("calculatorapp.exe").unwrap(), [calc]);
}

// Ported from test_an_ambiguous_title_lists_the_candidates_and_does_not_guess.
#[test]
fn an_ambiguous_title_lists_the_candidates_and_does_not_guess() {
    let d = Desk::new();
    d.add("report.txt - Notepad", "notepad.exe", "Notepad");
    d.add("notes.txt - Notepad", "notepad.exe", "Notepad");
    d.add("Mail", "olk.exe", "x");
    let e = d.resolve("txt").unwrap_err();
    assert!(matches!(e, StepError::Target(_)));
    let msg = e.message();
    assert!(
        msg.contains("2 windows match")
            && msg.contains("report.txt")
            && msg.contains("notes.txt")
            && msg.contains("wN ref"),
        "{msg}"
    );
}

// Ported from test_an_exe_with_several_windows_takes_the_first_and_says_so_and_all_takes_every_one.
// (The top of the stack is the window added last.)
#[test]
fn an_exe_with_several_windows_takes_the_first_and_says_so_and_all_takes_every_one() {
    let d = Desk::new();
    let a = d.add("a - Notepad", "notepad.exe", "Notepad");
    let b = d.add("b - Notepad", "notepad.exe", "Notepad");
    let (wins, note) = d.resolve_note("notepad.exe", false).unwrap();
    assert_eq!(wins, [b]);
    assert!(note.contains("first of 2"), "{note}");
    let (wins, note) = d.resolve_note("notepad.exe", true).unwrap();
    assert_eq!(wins, [b, a]);
    assert!(note.contains("all 2"), "{note}");
}

// Ported from test_no_match_and_missing_spec_are_clear_errors.
#[test]
fn no_match_and_missing_spec_are_clear_errors() {
    let d = Desk::new();
    d.add("a - Notepad", "notepad.exe", "Notepad");
    assert!(
        d.resolve("paint.exe")
            .unwrap_err()
            .message()
            .contains("no window matches")
    );
    assert!(
        d.resolve("")
            .unwrap_err()
            .message()
            .contains("window needs window")
    );
    assert!(
        d.resolve("w99")
            .unwrap_err()
            .message()
            .contains("no window ref")
    );
}

// Ported from test_the_desktop_and_taskbar_are_never_matched_by_name.
#[test]
fn the_desktop_and_taskbar_are_never_matched_by_name() {
    let d = Desk::new();
    let desktop = d.add("Program Manager", "explorer.exe", "Progman");
    let downloads = d.add("Downloads", "explorer.exe", "CabinetWClass");
    let (wins, _) = d.resolve_note("explorer.exe", true).unwrap();
    assert_eq!(wins, [downloads]);
    // by explicit ref it is refused, not skipped
    let e = err(d.window(json!({"op": "close", "window": d.wref(desktop)})));
    assert!(e.message().contains("desktop or the taskbar"), "{e}");
    assert!(d.ops().is_empty());
}

// Ported from test_front_means_the_window_in_front.
#[test]
fn front_means_the_window_in_front() {
    let d = Desk::new();
    d.add("a - Notepad", "notepad.exe", "Notepad");
    let docs = d.add("Docs", "explorer.exe", "CabinetWClass");
    d.kit.fake.set_foreground(Some(docs));
    let (wins, note) = d.resolve_note("front", false).unwrap();
    assert_eq!(wins, [docs]);
    assert_eq!(note, "the window in front");
    d.kit.fake.set_foreground(None);
    assert!(
        d.resolve("front")
            .unwrap_err()
            .message()
            .contains("no window is in front")
    );
}

// Ported from test_a_whole_title_segment_is_not_ambiguous_with_substrings (the resolution half):
// 2026-10-08 live run, window=c matched 5 windows though only "c - File Explorer" is named c.
#[test]
fn a_whole_title_segment_beats_a_bare_substring() {
    let d = Desk::new();
    let c = d.add("c - File Explorer", "explorer.exe", "CabinetWClass");
    for t in ["Claude", "Parsec", "ChatGPT - Brave", "Docs | Chrome"] {
        d.add(t, "x.exe", "x");
    }
    assert_eq!(d.resolve("c").unwrap(), [c]);
    // a substring that is no whole segment still works when it is unambiguous
    assert_eq!(d.resolve("parse").unwrap().len(), 1);
}

// ---------------------------------------------------------------------------- window: the operations

// Ported from test_close_is_a_polite_wm_close_and_reports_it.
#[test]
fn close_is_a_polite_close_and_reports_it() {
    let d = Desk::new();
    let w = d.add("Downloads", "explorer.exe", "CabinetWClass");
    let r = d.wref(w);
    assert_eq!(
        d.window(json!({"op": "close", "window": "Downloads"}))
            .unwrap(),
        format!("closed {r} \"Downloads\"")
    );
    assert_eq!(d.ops(), [(w, WindowOp::Close)]);
    assert!(!d.kit.fake.window_exists(w));
}

// Ported from test_close_that_is_held_open_by_a_save_prompt_says_so_and_answers_nothing.
#[test]
fn close_held_open_by_a_save_prompt_says_so_and_answers_nothing() {
    let d = Desk::new();
    let w = d.add("*notes - Notepad", "notepad.exe", "Notepad");
    d.kit.fake.hold_open_on_close(w);
    let note = d
        .window(json!({"op": "close", "window": "notepad.exe"}))
        .unwrap();
    assert!(
        note.contains("still open") && note.contains("answer it yourself"),
        "{note}"
    );
    assert!(d.kit.fake.window_exists(w));
    assert_eq!(
        d.ops(),
        [(w, WindowOp::Close)],
        "only the one polite close was sent"
    );
}

// Ported from test_minimize_maximize_restore_report_the_state_they_reached.
#[test]
fn minimize_maximize_restore_report_the_state_they_reached() {
    let d = Desk::new();
    let w = d.add("a - Notepad", "notepad.exe", "Notepad");
    let r = d.wref(w);
    let say = |op: &str| d.window(json!({"op": op, "window": r})).unwrap();
    assert_eq!(say("minimize"), format!("minimized {r} \"a - Notepad\""));
    assert_eq!(say("maximize"), format!("maximized {r} \"a - Notepad\""));
    assert_eq!(say("restore"), format!("restored {r} \"a - Notepad\""));
    let ops: Vec<WindowOp> = d.ops().into_iter().map(|(_, o)| o).collect();
    assert_eq!(
        ops,
        [WindowOp::Minimize, WindowOp::Maximize, WindowOp::Restore]
    );
}

// Ported from test_a_window_minimized_on_purpose_is_not_pulled_back_as_the_remembered_target.
#[test]
fn a_window_minimized_on_purpose_is_not_pulled_back_as_the_remembered_target() {
    let d = Desk::new();
    let a = d.add("a - Notepad", "notepad.exe", "Notepad");
    let b = d.add("b - Notepad", "notepad.exe", "Notepad");
    d.kit.computer.mem().target = Some(a);
    d.window(json!({"op": "minimize", "window": d.wref(a)}))
        .unwrap();
    assert_eq!(d.kit.computer.mem().target, None);
    d.kit.computer.mem().target = Some(b);
    // some other window: the target is left alone
    d.window(json!({"op": "close", "window": d.wref(a)}))
        .unwrap();
    assert_eq!(d.kit.computer.mem().target, Some(b));
}

// Ported from test_an_op_the_app_does_not_honour_is_reported_not_claimed.
#[test]
fn an_op_the_app_does_not_honour_is_reported_not_claimed() {
    let d = Desk::new();
    let w = d.add("game", "game.exe", "x");
    d.kit.fake.ignore_window_ops(true);
    let note = d
        .window(json!({"op": "maximize", "window": d.wref(w)}))
        .unwrap();
    assert!(note.contains("but it is normal"), "{note}");
}

// Ported from test_op_aliases_and_a_bad_op.
#[test]
fn op_aliases_and_a_bad_op() {
    let d = Desk::new();
    let w = d.add("a - Notepad", "notepad.exe", "Notepad");
    let r = d.wref(w);
    assert!(
        d.window(json!({"op": "MIN", "window": r}))
            .unwrap()
            .starts_with("minimized")
    );
    let e = err(d.window(json!({"op": "explode", "window": r})));
    assert!(e.message().contains("one of close, minimize"), "{e}");
    let e = err(d.window(json!({"window": r})));
    assert!(e.message().contains("window needs op"), "{e}");
    // the op may come in `text`, as a model that forgot the key sends it
    assert!(
        d.window(json!({"text": "restore", "window": r}))
            .unwrap()
            .starts_with("restored")
    );
}

// Ported from test_focus_restores_and_raises.
#[test]
fn focus_restores_and_raises() {
    let d = Desk::new();
    let a = d.add("a - Notepad", "notepad.exe", "Notepad");
    d.kit.fake.update_window(a, |i| i.minimized = true);
    let b = d.add("b", "b.exe", "x");
    d.kit.fake.set_foreground(Some(b));
    let r = d.wref(a);
    assert_eq!(
        d.window(json!({"op": "focus", "window": r})).unwrap(),
        format!("focused {r} \"a - Notepad\"")
    );
    assert_eq!(d.kit.fake.foreground_window().unwrap(), Some(a));
    assert!(!d.kit.fake.window_info(a).unwrap().minimized);
}

// Ported from test_move_sets_the_rect_and_restores_a_maximized_window_first.
#[test]
fn move_sets_the_rect_and_restores_a_maximized_window_first() {
    let d = Desk::new();
    let w = d.add_at(
        "a - Notepad",
        "notepad.exe",
        "Notepad",
        Rect::from_xywh(0, 0, 1920, 1080),
    );
    d.kit.fake.set_maximized(w, true);
    let r = d.wref(w);
    let note = d
        .window(json!({"op": "move", "window": r, "rect": [10, 20, 800, 600]}))
        .unwrap();
    assert_eq!(
        note,
        format!("moved {r} \"a - Notepad\" to [10, 20, 800, 600]")
    );
    assert_eq!(
        d.ops(),
        [
            (w, WindowOp::Restore),
            (w, WindowOp::Move(Rect::from_xywh(10, 20, 800, 600)))
        ]
    );
}

// Ported from test_move_says_when_the_app_limited_the_size.
#[test]
fn move_says_when_the_app_limited_the_size() {
    let d = Desk::new();
    let w = d.add("a - Notepad", "notepad.exe", "Notepad");
    d.kit.fake.limit_window_width(300);
    let note = d
        .window(json!({"op": "move", "window": d.wref(w), "rect": [0, 0, 400, 100]}))
        .unwrap();
    assert!(
        note.contains("to [0, 0, 300, 100]") && note.contains("asked [0, 0, 400, 100]"),
        "{note}"
    );
}

// Ported from test_move_refuses_a_bad_or_off_screen_rect_before_touching_anything. The fake desktop
// is 1920 x 1080 for the screen rect check (the kit's capture area is smaller).
#[test]
fn move_refuses_a_bad_or_off_screen_rect_before_touching_anything() {
    let d = Desk::new();
    let w = d.add("a - Notepad", "notepad.exe", "Notepad");
    d.kit.fake.set_displays(vec![crate::backend::Display {
        id: 1,
        name: "big".into(),
        rect: Rect::from_xywh(0, 0, 1920, 1080),
        scale: 1.0,
        primary: true,
    }]);
    let r = d.wref(w);
    for rect in [
        Value::Null,
        json!([1, 2, 3]),
        json!([0, 0, 5, 500]),
        json!([0, 0, 500, 99999]),
        json!(["a", 0, 400, 400]),
        json!([true, 0, 400, 400]),
        json!([5000, 0, 400, 400]),
        json!([0, -2000, 400, 400]),
    ] {
        let e = err(d.window(json!({"op": "move", "window": r, "rect": rect})));
        assert!(matches!(e, StepError::Value(_)), "{rect}: {e}");
    }
    assert!(d.ops().is_empty());
    // a rect that is only partly off screen is fine
    assert!(
        d.window(json!({"op": "move", "window": r, "rect": [1800, 900, 400, 400]}))
            .is_ok()
    );
}

// Ported from test_all_closes_every_window_of_an_exe.
#[test]
fn all_closes_every_window_of_an_exe() {
    let d = Desk::new();
    let a = d.add("a - Notepad", "notepad.exe", "Notepad");
    let b = d.add("b - Notepad", "notepad.exe", "Notepad");
    let mail = d.add("Mail", "olk.exe", "x");
    let note = d
        .window(json!({"op": "close", "window": "notepad.exe", "all": true}))
        .unwrap();
    assert_eq!(note.matches("closed").count(), 2, "{note}");
    assert!(!d.kit.fake.window_exists(a) && !d.kit.fake.window_exists(b));
    assert!(d.kit.fake.window_exists(mail));
}

// ---------------------------------------------------------------------------- window: the same refusals every input action has

// Ported from test_a_person_using_the_pc_is_waited_for_before_the_window_is_touched.
#[test]
fn a_person_using_the_pc_is_waited_for_before_the_window_is_touched() {
    let d = Desk::new();
    let w = d.add("a - Notepad", "notepad.exe", "Notepad");
    d.window(json!({"op": "close", "window": d.wref(w)}))
        .unwrap();
    assert!(
        d.kit.fake.human_idle_reads() > 0,
        "the human-input gate was consulted"
    );
}

// Ported from test_a_bad_target_is_refused_without_waiting_for_a_person.
#[test]
fn a_bad_target_is_refused_without_waiting_for_a_person() {
    let d = Desk::new();
    let e = err(d.window(json!({"op": "close", "window": "nothing"})));
    assert!(matches!(e, StepError::Target(_)));
    assert_eq!(d.kit.fake.human_idle_reads(), 0);
}

// Ported from test_the_callers_own_window_is_refused.
#[test]
fn the_callers_own_window_is_refused() {
    let mut d = Desk::new();
    let w = d.kit.window(
        "Claude",
        "claude.exe",
        100,
        Rect::from_xywh(10, 10, 200, 120),
    );
    d.kit
        .fake
        .update_window(w, |i| i.class = "Chrome_WidgetWin_1".into());
    d.kit.who = Caller::new(vec![(100, "claude.exe".into())], false);
    let e = err(d.window(json!({"op": "close", "window": "Claude"})));
    assert!(matches!(e, StepError::Target(_)));
    assert!(
        e.message()
            .starts_with("refused: that window belongs to claude.exe (pid 100)"),
        "{e}"
    );
    assert!(d.ops().is_empty());
}

// Ported from test_a_protected_process_is_refused.
#[test]
fn a_protected_process_is_refused() {
    let d = Desk::new();
    let w = d.add("vault", "vault.exe", "x");
    let paths = d.kit.computer.cfg.paths();
    std::fs::create_dir_all(paths.dir()).unwrap();
    std::fs::write(paths.protected(), "vault.exe\n").unwrap();
    let e = err(d.window(json!({"op": "minimize", "window": d.wref(w)})));
    assert!(matches!(e, StepError::Permission(_)), "{e}");
    assert!(e.message().contains("protected"), "{e}");
    assert!(d.ops().is_empty());
}

// Ported from test_the_only_apps_allowlist_applies.
#[test]
fn the_only_apps_allowlist_applies() {
    let d = Desk::new();
    let a = d.add("a - Notepad", "notepad.exe", "Notepad");
    let mail = d.add("Mail", "olk.exe", "x");
    assert!(
        d.window_only(
            &["notepad.exe"],
            json!({"op": "minimize", "window": d.wref(a)})
        )
        .unwrap()
        .starts_with("minimized")
    );
    let e = err(d.window_only(
        &["notepad.exe"],
        json!({"op": "close", "window": d.wref(mail)}),
    ));
    assert!(e.message().contains("may not touch"), "{e}");
    assert!(d.kit.fake.window_exists(mail));
}

// Ported from test_all_skips_the_refused_windows_and_says_so_but_a_single_target_raises.
#[test]
fn all_skips_the_refused_windows_and_says_so_but_a_single_target_raises() {
    let d = Desk::new();
    let mine = d.add("mine - Terminal", "terminal.exe", "t");
    let other = d.add("other - Terminal", "terminal.exe", "t");
    let only = ["terminal.exe:other"];
    let (r_mine, r_other) = (d.wref(mine), d.wref(other));
    let note = d
        .window_only(
            &only,
            json!({"op": "close", "window": "terminal.exe", "all": true}),
        )
        .unwrap();
    assert!(
        note.contains(&format!("closed {r_other} \"other - Terminal\"")),
        "{note}"
    );
    assert!(
        note.contains(&format!("skipped {r_mine} \"mine - Terminal\": refused:")),
        "{note}"
    );
    assert!(d.kit.fake.window_exists(mine));
    assert!(!d.kit.fake.window_exists(other));
    // one target named outright: the refusal is raised, not skipped
    let e = err(d.window_only(&only, json!({"op": "close", "window": d.wref(mine)})));
    assert!(e.message().starts_with("refused:"), "{e}");
}

// Ported from test_a_consent_window_is_refused.
#[test]
fn a_consent_window_is_refused() {
    let d = Desk::new();
    let w = d.add("User Account Control", "consent.exe", "x");
    let e = err(d.window(json!({"op": "close", "window": d.wref(w)})));
    assert!(matches!(e, StepError::Permission(_)), "{e}");
    assert!(e.message().contains("elevation"), "{e}");
    assert!(d.ops().is_empty());
}

// ---------------------------------------------------------------------------- at: window-relative points

fn at(d: &Desk, args: Value) -> Result<Option<Point>, StepError> {
    let ctx = d.kit.ctx(json!({}));
    ctx.at_point(&Kit::args(args), "at")
}

// Ported from test_at_is_a_fraction_of_the_client_area.
#[test]
fn at_is_a_fraction_of_the_client_area() {
    let d = Desk::new();
    let w = d.add_at("game", "game.exe", "x", Rect::new(100, 100, 1100, 700));
    d.kit
        .fake
        .set_client_rect(w, Rect::new(108, 140, 1092, 692)); // 984 x 552
    let r = d.wref(w);
    let p = |fx: f64, fy: f64| {
        at(&d, json!({"at": [fx, fy], "window": r}))
            .unwrap()
            .unwrap()
    };
    assert_eq!(
        p(0.5, 0.5),
        Point {
            x: 108 + 492,
            y: 140 + 276
        }
    );
    assert_eq!(p(0.0, 0.0), Point { x: 108, y: 140 });
    assert_eq!(
        p(1.0, 1.0),
        Point {
            x: 108 + 983,
            y: 140 + 551
        },
        "1,1 is the last pixel"
    );
}

// Ported from test_at_can_use_the_whole_window_frame_instead.
#[test]
fn at_can_use_the_whole_window_frame_instead() {
    let d = Desk::new();
    let w = d.add_at("game", "game.exe", "x", Rect::new(100, 100, 1100, 700));
    d.kit
        .fake
        .set_client_rect(w, Rect::new(108, 140, 1092, 692));
    let r = d.wref(w);
    let p = |a: Value| at(&d, a).unwrap().unwrap();
    assert_eq!(
        p(json!({"at": [0, 0], "window": r, "at_rect": "window"})),
        Point { x: 100, y: 100 }
    );
    assert_eq!(
        p(json!({"at": [0.5, 0.25], "window": r, "at_rect": "window"})),
        Point { x: 600, y: 250 }
    );
}

// Ported from test_at_follows_the_window_when_it_moves.
#[test]
fn at_follows_the_window_when_it_moves() {
    let d = Desk::new();
    let w = d.add_at("game", "game.exe", "x", Rect::new(0, 0, 800, 600));
    d.kit.fake.set_client_rect(w, Rect::new(0, 0, 800, 600));
    let r = d.wref(w);
    let first = at(&d, json!({"at": [0.5, 0.5], "window": r}))
        .unwrap()
        .unwrap();
    d.kit
        .fake
        .update_window(w, |i| i.rect = Rect::new(400, 300, 1200, 900));
    d.kit
        .fake
        .set_client_rect(w, Rect::new(400, 300, 1200, 900));
    let second = at(&d, json!({"at": [0.5, 0.5], "window": r}))
        .unwrap()
        .unwrap();
    assert_eq!((second.x - first.x, second.y - first.y), (400, 300));
}

// Ported from test_at_without_window_uses_the_window_in_front.
#[test]
fn at_without_window_uses_the_window_in_front() {
    let d = Desk::new();
    d.add_at("a", "a.exe", "x", Rect::new(0, 0, 400, 400));
    let b = d.add_at("b", "b.exe", "x", Rect::new(1000, 0, 1400, 400));
    d.kit.fake.set_foreground(Some(b));
    assert_eq!(
        at(&d, json!({"at": [0.5, 0.5]})).unwrap().unwrap(),
        Point { x: 1200, y: 200 }
    );
    d.kit.fake.set_foreground(None);
    let e = at(&d, json!({"at": [0.5, 0.5]})).unwrap_err();
    assert!(e.message().contains("at needs a window"), "{e}");
}

// Ported from test_no_at_means_no_point_and_bad_fractions_are_refused.
#[test]
fn no_at_means_no_point_and_bad_fractions_are_refused() {
    let d = Desk::new();
    let w = d.add("a", "a.exe", "x");
    let r = d.wref(w);
    assert_eq!(at(&d, json!({"coordinate": [1, 2]})).unwrap(), None);
    for bad in [
        json!([1.2, 0.5]),
        json!([-0.1, 0]),
        json!([0.5]),
        json!("0.5,0.5"),
        json!([0.5, true]),
    ] {
        let e = at(&d, json!({"at": bad, "window": r})).unwrap_err();
        assert!(
            matches!(e, StepError::Value(_)) && e.message().contains("at "),
            "{bad}: {e}"
        );
    }
}

// Ported from test_at_refuses_a_minimized_window_and_the_callers_window.
#[test]
fn at_refuses_a_minimized_window_and_the_callers_window() {
    let mut d = Desk::new();
    let w = d.add("a", "a.exe", "x");
    let r = d.wref(w);
    d.kit.fake.update_window(w, |i| i.minimized = true);
    let e = at(&d, json!({"at": [0.5, 0.5], "window": r})).unwrap_err();
    assert!(e.message().contains("minimized"), "{e}");
    d.kit.fake.update_window(w, |i| i.minimized = false);
    d.kit.who = Caller::new(vec![(1, "a.exe".into())], false);
    let e = at(&d, json!({"at": [0.5, 0.5], "window": r})).unwrap_err();
    assert!(e.message().contains("refused"), "{e}");
}

// Ported from test_a_click_with_at_lands_on_the_computed_pixel_through_the_normal_checks.
#[test]
fn a_click_with_at_lands_on_the_computed_pixel_through_the_normal_checks() {
    let d = Desk::new();
    let w = d.add_at("game", "game.exe", "x", Rect::new(0, 0, 300, 160));
    d.kit.fake.set_client_rect(w, Rect::new(0, 0, 300, 160));
    let (note, _) = d
        .kit
        .step(json!({"action": "left_click", "at": [0.25, 0.5], "window": d.wref(w)}))
        .unwrap();
    let clicks: Vec<Point> = d
        .kit
        .fake
        .events()
        .into_iter()
        .filter_map(|e| match e {
            InputEvent::Click { at, .. } => Some(at),
            _ => None,
        })
        .collect();
    assert_eq!(clicks, [Point { x: 75, y: 80 }]);
    let shown = d.kit.ctx(json!({})).pt(75, 80);
    assert!(note.contains(&shown), "{note} should name {shown}");
    // the same refusals as any click: a window that is not allowed stops it before any input
    d.kit.fake.clear_events();
    let e = d
        .kit
        .step_only(
            &["notepad.exe"],
            json!({"action": "left_click", "at": [0.5, 0.5], "window": d.wref(w)}),
        )
        .unwrap_err();
    assert!(e.message().contains("may not touch"), "{e}");
    assert!(d.kit.fake.events().is_empty());
}

// Ported from test_at_works_for_scroll_and_drag_ends_too.
#[test]
fn at_works_for_scroll_and_drag_ends_too() {
    let d = Desk::new();
    let w = d.add_at("doc", "doc.exe", "x", Rect::new(0, 0, 300, 160));
    d.kit.fake.set_client_rect(w, Rect::new(0, 0, 300, 160));
    let r = d.wref(w);
    d.kit
        .step(json!({"action": "scroll", "at": [0.5, 0.5], "window": r, "scroll_amount": 2}))
        .unwrap();
    let (note, _) = d
        .kit
        .step(json!({"action": "left_click_drag", "start_at": [0.1, 0.1], "at": [0.9, 0.9], "window": r}))
        .unwrap();
    let events = d.kit.fake.events();
    assert!(events.iter().any(|e| matches!(e, InputEvent::Scroll { at, notches: 2, .. } if *at == Point { x: 150, y: 80 })), "{events:?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, InputEvent::Drag { from, to }
            if *from == (Point { x: 30, y: 16 }) && *to == (Point { x: 270, y: 144 }))),
        "{events:?}"
    );
    // the note names where the drag went, not "None"
    assert!(!note.contains("None"), "{note}");
}

// ---------------------------------------------------------------------------- screenshot save_to

fn tmp_png(d: &Desk, name: &str) -> String {
    d.kit.tmp.path().join(name).to_string_lossy().into_owned()
}

// Ported from test_save_to_writes_a_png_of_the_display_at_full_size.
#[test]
fn save_to_writes_a_png_of_the_display_at_full_size() {
    let d = Desk::new();
    let out = tmp_png(&d, "page.png");
    let (note, img) = d
        .kit
        .step(json!({"action": "screenshot", "save_to": out}))
        .unwrap();
    assert_eq!(&std::fs::read(&out).unwrap()[..8], b"\x89PNG\r\n\x1a\n");
    assert!(
        note.contains("saved the display") && note.contains("320x180px"),
        "{note}"
    );
    assert!(img.is_some());
    assert!(!note.contains("replaced"), "{note}");
}

// Ported from test_save_to_a_jpeg_of_one_window_and_shot_false_returns_no_picture.
#[test]
fn save_to_a_jpeg_of_one_window_and_shot_false_returns_no_picture() {
    let d = Desk::new();
    let w = d.add_at(
        "Brave",
        "brave.exe",
        "Chrome_WidgetWin_1",
        Rect::new(20, 30, 220, 130),
    );
    let out = d.kit.tmp.path().join("sub").join("page.JPG");
    let (note, img) = d
        .kit
        .step(json!({"action": "screenshot", "save_to": out.to_string_lossy(), "window": d.wref(w), "shot": false}))
        .unwrap();
    assert_eq!(&std::fs::read(&out).unwrap()[..3], b"\xff\xd8\xff");
    assert!(img.is_none());
    assert!(
        note.contains(&format!("{} \"Brave\"", d.wref(w))) && note.contains("200x100px"),
        "{note}"
    );
}

// Ported from test_save_to_replaces_an_old_file_and_says_so.
#[test]
fn save_to_replaces_an_old_file_and_says_so() {
    let d = Desk::new();
    let out = tmp_png(&d, "a.png");
    std::fs::write(&out, b"old").unwrap();
    let (note, _) = d
        .kit
        .step(json!({"action": "screenshot", "save_to": out, "shot": false}))
        .unwrap();
    assert!(note.contains("replaced the old file"), "{note}");
    assert_eq!(&std::fs::read(&out).unwrap()[..4], b"\x89PNG");
    assert!(!Path::new(&format!("{out}.part")).exists());
}

// Ported from test_save_to_refuses_a_minimized_window.
#[test]
fn save_to_refuses_a_minimized_window() {
    let d = Desk::new();
    let w = d.add("Brave", "brave.exe", "x");
    d.kit.fake.update_window(w, |i| i.minimized = true);
    let out = tmp_png(&d, "a.png");
    let e = d
        .kit
        .step(json!({"action": "screenshot", "save_to": out, "window": d.wref(w)}))
        .unwrap_err();
    assert!(e.message().contains("minimized"), "{e}");
    assert!(!Path::new(&out).exists());
}

// Ported from test_save_to_needs_a_full_image_path.
#[test]
fn save_to_needs_a_full_image_path() {
    let d = Desk::new();
    let before = d.kit.fake.capture_count();
    for (bad, why) in [
        ("rel/a.png", "full path"),
        ("C:\\Users\\x\\a.txt", ".png, .jpg"),
        ("C:\\Users\\x\\a", ".png, .jpg"),
    ] {
        let e = d
            .kit
            .step(json!({"action": "screenshot", "save_to": bad}))
            .unwrap_err();
        assert!(
            matches!(e, StepError::Value(_)) && e.message().contains(why),
            "{bad}: {e}"
        );
    }
    assert_eq!(d.kit.fake.capture_count(), before, "nothing was captured");
}

// Ported from test_an_empty_save_to_is_an_error_when_asked_directly.
#[test]
fn an_empty_save_to_is_an_error_when_asked_directly() {
    let d = Desk::new();
    let ctx = d.kit.ctx(json!({}));
    assert!(
        ctx.save_path("")
            .unwrap_err()
            .message()
            .contains("needs a file path")
    );
    assert!(ctx.save_path("  \"\"  ").is_err());
}

// Ported from test_save_to_refuses_what_the_owners_hard_line_forbids.
#[test]
fn save_to_refuses_what_the_owners_hard_line_forbids() {
    let d = Desk::new();
    let before = d.kit.fake.capture_count();
    for path in [
        "C:\\Windows\\a.png",
        "C:\\Windows\\System32\\a.png",
        "D:\\a\\system32\\a.jpg",
        "C:\\Windows\\System32\\config\\SAM.png",
    ] {
        let e = d
            .kit
            .step(json!({"action": "screenshot", "save_to": path}))
            .unwrap_err();
        assert!(
            matches!(e, StepError::Permission(_)) && e.message().contains("allow_destructive"),
            "{path}: {e}"
        );
    }
    assert_eq!(
        d.kit.fake.capture_count(),
        before,
        "refused before the screen was even read"
    );
}

// Ported from test_the_hard_line_can_be_lifted_for_one_call_only.
#[test]
fn the_hard_line_can_be_lifted_for_one_call_only() {
    let d = Desk::new();
    let lifted =
        CallOpts::from_args(&d.kit.computer, &Kit::args(json!({})), Policy::allow()).unwrap();
    let ctx = Ctx {
        c: &d.kit.computer,
        o: lifted,
        who: &d.kit.who,
    };
    assert_eq!(
        ctx.save_path("C:\\Windows\\a.png").unwrap(),
        "C:\\Windows\\a.png"
    );
    assert!(
        d.kit
            .ctx(json!({}))
            .save_path("C:\\Windows\\a.png")
            .is_err()
    );
}

// Ported from test_under_an_allowlist_save_to_may_only_write_under_temp.
#[test]
fn under_an_allowlist_save_to_may_only_write_under_temp() {
    let d = Desk::new();
    let ok = tmp_png(&d, "ok.png");
    d.kit
        .step_only(
            &["notepad.exe"],
            json!({"action": "screenshot", "save_to": ok, "shot": false}),
        )
        .unwrap();
    assert!(Path::new(&ok).exists());
    let elsewhere = std::env::temp_dir().join(format!("wh-elsewhere-{}.png", std::process::id()));
    let e = d
        .kit
        .step_only(
            &["notepad.exe"],
            json!({"action": "screenshot", "save_to": elsewhere.to_string_lossy()}),
        )
        .unwrap_err();
    assert!(e.message().contains("save_to may only write"), "{e}");
    assert!(!elsewhere.exists());
}

// Ported from test_a_plain_screenshot_is_unchanged.
#[test]
fn a_plain_screenshot_is_unchanged() {
    let d = Desk::new();
    let (note, img) = d.kit.step(json!({"action": "screenshot"})).unwrap();
    assert!(note.starts_with("screenshot; give coordinates"), "{note}");
    assert!(img.is_some());
}

// ---------------------------------------------------------------------------- open_app on a URL

fn browser_hook(d: &Desk, title: &'static str) {
    d.kit.fake.set_launch_hook(move |fake, _| {
        let w = fake.add_window(WindowInfo::basic(
            title,
            "brave.exe",
            50,
            Rect::from_xywh(0, 0, 200, 100),
        ));
        fake.update_window(w, |i| i.class = "Chrome_WidgetWin_1".into());
        fake.set_foreground(Some(w));
    });
}

// Ported from test_open_app_routes_urls_to_open_url_and_everything_else_as_before.
#[test]
fn open_app_routes_urls_to_the_browser_and_starts_it_de_elevated() {
    let d = Desk::new();
    d.add("terminal", "terminal.exe", "t");
    browser_hook(&d, "Example Domain - Brave");
    let (note, _) = d
        .kit
        .step(json!({"action": "open_app", "text": "https://example.com"}))
        .unwrap();
    assert!(
        note.starts_with("opened https://example.com in w"),
        "{note}"
    );
    assert!(
        note.contains("\"Example Domain - Brave\" (brave.exe)"),
        "{note}"
    );
    // the default browser must not run with the daemon's admin rights
    assert_eq!(
        d.kit.fake.launches(),
        [LaunchSpec {
            target: AppTarget::Other("https://example.com".into()),
            args: String::new(),
            deelevate: true
        }]
    );
}

// Ported from test_a_url_is_refused_under_an_allowlist_before_any_browser_starts.
#[test]
fn a_url_is_refused_under_an_allowlist_before_any_browser_starts() {
    let d = Desk::new();
    let e = d
        .kit
        .step_only(
            &["notepad.exe"],
            json!({"action": "open_app", "text": "https://example.com"}),
        )
        .unwrap_err();
    assert!(e.message().contains("not an app this task may open"), "{e}");
    assert!(d.kit.fake.launches().is_empty());
}

// Ported from test_open_url_waits_for_a_browser_window_to_change_and_reports_it.
#[test]
fn open_url_waits_for_a_browser_window_and_reports_it() {
    let d = Desk::new();
    let t = d.add("terminal", "terminal.exe", "t");
    d.kit.fake.set_foreground(Some(t));
    browser_hook(&d, "Example Domain - Brave");
    let note = d
        .kit
        .ctx(json!({}))
        .open_url("https://example.com", 8.0)
        .unwrap();
    assert!(
        note.contains("\"Example Domain - Brave\"") && note.contains("brave.exe"),
        "{note}"
    );
}

// Ported from test_open_url_in_an_already_front_browser_waits_for_the_title_to_change.
#[test]
fn open_url_in_an_already_front_browser_waits_for_the_title_to_change() {
    let d = Desk::new();
    let b = d.add("New Tab - Brave", "brave.exe", "Chrome_WidgetWin_1");
    d.kit.fake.set_foreground(Some(b));
    d.kit.fake.set_launch_hook(move |fake, _| {
        fake.update_window(b, |i| i.title = "Example Domain - Brave".into());
    });
    let note = d
        .kit
        .ctx(json!({}))
        .open_url("https://example.com", 8.0)
        .unwrap();
    assert!(note.contains("Example Domain - Brave"), "{note}");
}

// A browser that adds a tab and changes nothing the core can see is not an error: say it may be loading.
#[test]
fn open_url_in_a_front_browser_that_shows_no_change_says_it_may_still_be_loading() {
    let d = Desk::new();
    let b = d.add("New Tab - Brave", "brave.exe", "Chrome_WidgetWin_1");
    d.kit.fake.set_foreground(Some(b));
    let note = d
        .kit
        .ctx(json!({}))
        .open_url("https://example.com", 2.0)
        .unwrap();
    assert!(
        note.starts_with("sent https://example.com to the browser in front"),
        "{note}"
    );
}

// Ported from test_open_url_that_never_shows_a_browser_is_an_error.
#[test]
fn open_url_that_never_shows_a_browser_is_an_error() {
    let d = Desk::new();
    let t = d.add("terminal", "terminal.exe", "t");
    d.kit.fake.set_foreground(Some(t));
    let e = d
        .kit
        .ctx(json!({}))
        .open_url("https://example.com", 8.0)
        .unwrap_err();
    assert!(matches!(e, StepError::Target(_)));
    assert!(
        e.message()
            .contains("no browser window came to the front within 8s"),
        "{e}"
    );
}

// Ported from test_open_url_runs_the_hard_line_on_the_address.
#[test]
fn open_url_runs_the_hard_line_on_the_address() {
    let d = Desk::new();
    let e = d
        .kit
        .ctx(json!({}))
        .open_url("https://example.com/ntds.dit", 8.0)
        .unwrap_err();
    assert!(matches!(e, StepError::Permission(_)), "{e}");
    assert!(d.kit.fake.launches().is_empty());
}

// The folder tests below drive the Explorer path: a launched explorer.exe opens a window, the Shell or the
// header click sets the view and sort. On macOS the folder action opens Finder instead and cannot set
// either, so these tests are Windows-path tests and are skipped there.
#[cfg(not(target_os = "macos"))]
mod explorer {
    use super::*;

    // ---------------------------------------------------------------------------- folder: the action

    fn set_mtime(path: &Path, secs: u64) {
        let when = UNIX_EPOCH + Duration::from_secs(secs);
        let mut opts = std::fs::OpenOptions::new();
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            opts.write(true).custom_flags(0x0200_0000); // FILE_FLAG_BACKUP_SEMANTICS: a folder can be opened
        }
        #[cfg(not(windows))]
        {
            opts.read(true);
        }
        opts.open(path).unwrap().set_modified(when).unwrap();
    }

    /// Five entries with known times and sizes. Newest first: new.txt, b10.txt, sub, b2.txt, old.txt.
    fn make_folder(root: &Path) -> String {
        let dir = root.join("docs");
        std::fs::create_dir_all(&dir).unwrap();
        let t0 = 1_700_000_000;
        for (name, size, mtime) in [
            ("old.txt", 5, t0),
            ("b2.txt", 300, t0 + 100),
            ("b10.txt", 20, t0 + 200),
            ("new.txt", 1000, t0 + 300),
        ] {
            std::fs::write(dir.join(name), vec![b'x'; size]).unwrap();
            set_mtime(&dir.join(name), mtime);
        }
        std::fs::create_dir(dir.join("sub")).unwrap();
        set_mtime(&dir.join("sub"), t0 + 150);
        dir.to_string_lossy().into_owned()
    }

    /// A kit whose launching Explorer opens a window titled with the folder's name.
    fn folder_desk() -> (Desk, String) {
        let d = Desk::new();
        let folder = make_folder(d.kit.tmp.path());
        d.kit.fake.set_launch_hook(|fake, spec| {
            if let AppTarget::Executable(exe) = &spec.target
                && exe.eq_ignore_ascii_case("explorer.exe")
            {
                let leaf = spec
                    .args
                    .trim_matches('"')
                    .rsplit(['\\', '/'])
                    .next()
                    .unwrap_or("")
                    .to_string();
                let w = fake.add_window(WindowInfo::basic(
                    &leaf,
                    "explorer.exe",
                    900,
                    Rect::from_xywh(5, 5, 300, 170),
                ));
                fake.update_window(w, |i| {
                    i.class = "CabinetWClass".into();
                    i.kind = WindowKind::FileManager;
                });
            }
        });
        (d, folder)
    }

    fn folder_call(d: &Desk, args: Value) -> Result<String, StepError> {
        let mut a = args;
        a["action"] = json!("folder");
        d.kit.step(a).map(|(note, img)| {
            assert!(img.is_none());
            note
        })
    }

    fn name_of(line: &str) -> String {
        line.split("  ").nth(2).unwrap_or("").to_string()
    }

    fn pressed(d: &Desk) -> Vec<Key> {
        d.kit
            .fake
            .events()
            .into_iter()
            .filter_map(|e| match e {
                InputEvent::KeyDown(k) => Some(k),
                _ => None,
            })
            .collect()
    }

    fn explorer_ref(d: &Desk, leaf: &str) -> String {
        let lines = d.kit.computer.window_refs().list(false).unwrap();
        let l = lines
            .iter()
            .find(|l| l.title == leaf)
            .expect("the new explorer window");
        format!("w{}", l.reference)
    }

    fn leaf_of(folder: &str) -> String {
        Path::new(folder)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    // Ported from test_the_benchmark_task_in_one_call.
    #[test]
    fn the_benchmark_task_in_one_call() {
        let (d, folder) = folder_desk();
        let out = folder_call(
            &d,
            json!({"path": folder, "view": "details", "sort": "date_modified desc", "list": 3}),
        )
        .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        let leaf = leaf_of(&folder);
        let r = explorer_ref(&d, &leaf);
        assert!(
        lines[0].ends_with(&format!(
            "opened a new window, {r} \"{leaf}\"; view details (shell); sort date_modified desc (shell)"
        )),
        "{}",
        lines[0]
    );
        assert!(
            lines[1].ends_with("first 3 of 5 entries by date_modified desc"),
            "{}",
            lines[1]
        );
        assert_eq!(name_of(lines[2]), "new.txt");
        assert_eq!(lines.len(), 5);
        assert_eq!(
            d.kit.fake.shell_state(),
            ("details".to_string(), "date_modified desc".to_string())
        );
        assert!(pressed(&d).is_empty(), "the Shell did it, no keystrokes");
    }

    // A folder call ends in text, so a batch whose last step is one carries no picture.
    #[test]
    fn a_folder_call_in_a_batch_ends_without_a_screenshot() {
        let (d, folder) = folder_desk();
        let out = d
            .kit
            .run(json!({"action": "folder", "path": folder, "list": 2}))
            .unwrap();
        assert!(out.images.is_empty());
    }

    // Ported from test_folder_waits_for_a_person_and_remembers_the_window_as_the_target.
    #[test]
    fn folder_waits_for_a_person_and_remembers_the_window_as_the_target() {
        let (d, folder) = folder_desk();
        folder_call(&d, json!({"path": folder})).unwrap();
        assert!(d.kit.fake.human_idle_reads() > 0);
        assert!(d.kit.computer.mem().target.is_some());
    }

    // Ported from test_folder_with_only_a_path_just_opens_it.
    #[test]
    fn folder_with_only_a_path_just_opens_it() {
        let (d, folder) = folder_desk();
        let out = folder_call(&d, json!({"path": folder})).unwrap();
        assert!(
            !out.contains("view") && !out.contains("sort") && !out.contains("entries"),
            "{out}"
        );
        assert!(out.contains("opened a new window"), "{out}");
        assert_eq!(d.kit.fake.shell_calls(), 0);
    }

    // Ported from test_list_without_sort_means_newest_first.
    #[test]
    fn list_without_sort_means_newest_first() {
        let (d, folder) = folder_desk();
        // a string works, recipes send one
        let out = folder_call(&d, json!({"path": folder, "list": "2"})).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].ends_with("by date_modified desc"), "{}", lines[1]);
        assert_eq!(name_of(lines[2]), "new.txt");
    }

    // Ported from test_list_follows_the_requested_sort.
    #[test]
    fn list_follows_the_requested_sort() {
        let (d, folder) = folder_desk();
        let out = folder_call(&d, json!({"path": folder, "sort": "size asc", "list": 2})).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // the Shell was asked to sort too, so the head line says so
        assert!(lines.iter().any(|l| l.ends_with("by size asc")), "{out}");
        let list_at = lines
            .iter()
            .position(|l| l.ends_with("by size asc"))
            .unwrap();
        assert!(
            ["sub", "old.txt"].contains(&name_of(lines[list_at + 1]).as_str()),
            "{out}"
        );
    }

    // Ported from test_text_is_accepted_for_the_path_like_list_files.
    #[test]
    fn text_is_accepted_for_the_path_like_list_files() {
        let (d, folder) = folder_desk();
        assert!(
            folder_call(&d, json!({"text": folder}))
                .unwrap()
                .contains("opened a new window")
        );
    }

    // Ported from test_arguments_are_checked_before_any_window_opens.
    #[test]
    fn arguments_are_checked_before_any_window_opens() {
        let (d, folder) = folder_desk();
        let nope = d.kit.tmp.path().join("nope").to_string_lossy().into_owned();
        for bad in [
            json!({"path": ""}),
            json!({"path": nope}),
            json!({"path": folder, "view": "gallery"}),
            json!({"path": folder, "sort": "colour"}),
            json!({"path": folder, "list": "many"}),
            json!({"path": folder, "list": -1}),
        ] {
            let e = folder_call(&d, bad.clone()).unwrap_err();
            assert!(
                matches!(e, StepError::Value(_) | StepError::Target(_)),
                "{bad}: {e}"
            );
        }
        assert!(d.kit.fake.launches().is_empty());
    }

    // Ported from test_a_credential_folder_is_refused_by_the_hard_line.
    #[test]
    fn a_credential_folder_is_refused_by_the_hard_line() {
        let (d, _) = folder_desk();
        let cred = d.kit.tmp.path().join("config").join("SAM");
        std::fs::create_dir_all(&cred).unwrap();
        let e = folder_call(&d, json!({"path": cred.to_string_lossy()})).unwrap_err();
        assert!(
            matches!(e, StepError::Permission(_)) && e.message().contains("credential"),
            "{e}"
        );
        assert!(d.kit.fake.launches().is_empty());
    }

    // Ported from test_under_an_allowlist_explorer_must_be_allowed_and_listing_stays_in_temp.
    #[test]
    fn under_an_allowlist_explorer_must_be_allowed_and_listing_stays_in_temp() {
        let (d, folder) = folder_desk();
        let e = d
            .kit
            .step_only(
                &["notepad.exe"],
                json!({"action": "folder", "path": folder}),
            )
            .unwrap_err();
        assert!(e.message().contains("may not open explorer.exe"), "{e}");
        assert!(d.kit.fake.launches().is_empty());
        // explorer allowed, but %TEMP% is somewhere else: the listing is refused
        let elsewhere = tempdir_elsewhere();
        let d2 = Desk::with_env(MapEnv::new().with("TEMP", &elsewhere));
        let folder2 = make_folder(d2.kit.tmp.path());
        let e = d2
            .kit
            .step_only(
                &["explorer.exe"],
                json!({"action": "folder", "path": folder2, "list": 3}),
            )
            .unwrap_err();
        assert!(e.message().contains("folder list may only read"), "{e}");
        assert!(d2.kit.fake.launches().is_empty());
    }

    fn tempdir_elsewhere() -> String {
        std::env::temp_dir()
            .join(format!("wh-not-the-kit-{}", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }

    // Ported from test_the_callers_own_explorer_like_window_is_refused_after_opening: a window the guard
    // refuses is never arranged.
    #[test]
    fn a_refused_folder_window_is_never_arranged() {
        let (mut d, folder) = folder_desk();
        d.kit.who = Caller::new(vec![(900, "explorer.exe".into())], false);
        let e = folder_call(&d, json!({"path": folder, "view": "details"})).unwrap_err();
        assert!(e.message().contains("refused"), "{e}");
        assert!(pressed(&d).is_empty());
        assert_eq!(d.kit.fake.shell_calls(), 0, "nothing was changed");
    }

    // Ported from test_when_the_shell_is_unreachable_the_view_falls_back_to_ctrl_shift_6.
    #[test]
    fn when_the_shell_is_unreachable_the_view_falls_back_to_ctrl_shift_6() {
        let (d, folder) = folder_desk();
        d.kit.fake.set_shell_mode(ShellMode::Unavailable);
        let out = folder_call(&d, json!({"path": folder, "view": "details"})).unwrap();
        let keys = pressed(&d);
        assert_eq!(keys, [Key::Ctrl, Key::Shift, Key::Char('6')], "{keys:?}");
        assert!(
            out.contains("view details (keyboard, not read back)"),
            "{out}"
        );
        assert!(out.contains("shell api unavailable ("), "{out}");
        assert!(out.contains("sort not set"), "{out}");
    }

    // Ported from test_each_view_has_its_own_fallback_key.
    #[test]
    fn each_view_has_its_own_fallback_key() {
        let (d, folder) = folder_desk();
        d.kit.fake.set_shell_mode(ShellMode::Absent);
        for (view, key) in [
            ("list", '5'),
            ("tiles", '7'),
            ("icons", '3'),
            ("large", '2'),
        ] {
            folder_call(&d, json!({"path": folder, "view": view})).unwrap();
            assert_eq!(pressed(&d).last(), Some(&Key::Char(key)), "{view}");
        }
        assert!(
            folder_call(&d, json!({"path": folder, "view": "list"}))
                .unwrap()
                .contains("did not list the window")
        );
    }

    // ---------------------------------------------------------------------------- folder: the header click fallback

    /// The folder window's tree: a "Date modified" column header and one list row.
    fn explorer_tree(first_row: &str, with_header: bool) -> Element {
        let mut root = Element::new("root", Role::Window, "docs");
        if with_header {
            root = root.with_child(
                Element::new("h", Role::Other("HeaderItem".into()), "Date modified")
                    .at(Rect::new(100, 10, 160, 25)),
            );
        }
        root.with_child(Element::new("r", Role::ListItem, first_row).at(Rect::new(10, 40, 90, 55)))
    }

    /// A desk where every header click moves the first visible row through `rows`, one per click.
    fn header_desk(rows: &[&str], with_header: bool) -> (Desk, String, Arc<Mutex<u32>>) {
        let (d, folder) = folder_desk();
        d.kit.fake.set_shell_mode(ShellMode::Absent);
        let leaf = leaf_of(&folder);
        let queue: Arc<Mutex<VecDeque<String>>> =
            Arc::new(Mutex::new(rows.iter().map(|r| (*r).to_string()).collect()));
        let clicks = Arc::new(Mutex::new(0u32));
        let launch_leaf = leaf.clone();
        let click_leaf = leaf;
        let tree_header = with_header;
        d.kit.fake.set_click_hook({
            let (queue, clicks) = (queue.clone(), clicks.clone());
            move |fake, _| {
                *clicks.lock().unwrap() += 1;
                let next = queue.lock().unwrap().pop_front();
                if let (Some(name), Some(w)) = (
                    next,
                    fake.list_windows(true)
                        .unwrap()
                        .into_iter()
                        .find(|w| w.title == click_leaf)
                        .map(|w| w.window),
                ) {
                    fake.set_tree(w, explorer_tree(&name, tree_header));
                }
            }
        });
        let prior = d.kit.fake.launches().len();
        assert_eq!(prior, 0);
        // the tree exists as soon as the window does
        d.kit.fake.set_launch_hook({
            let tree_header = with_header;
            move |fake, spec| {
                if let AppTarget::Executable(_) = &spec.target {
                    let w = fake.add_window(WindowInfo::basic(
                        &launch_leaf,
                        "explorer.exe",
                        900,
                        Rect::from_xywh(5, 5, 300, 170),
                    ));
                    fake.update_window(w, |i| {
                        i.class = "CabinetWClass".into();
                        i.kind = WindowKind::FileManager;
                    });
                    fake.set_tree(w, explorer_tree("start", tree_header));
                }
            }
        });
        (d, folder, clicks)
    }

    // Ported from test_header_click_that_lands_the_right_way_first_time_is_verified.
    #[test]
    fn header_click_that_lands_the_right_way_first_time_is_verified() {
        let (d, folder, clicks) = header_desk(&["new"], true); // Explorer hides the .txt
        let out = folder_call(&d, json!({"path": folder, "sort": "date_modified desc"})).unwrap();
        assert_eq!(*clicks.lock().unwrap(), 1);
        assert!(
            out.contains("date_modified desc (header click, verified)"),
            "{out}"
        );
    }

    // Ported from test_header_click_toggles_once_when_the_first_click_went_the_other_way.
    #[test]
    fn header_click_toggles_once_when_the_first_click_went_the_other_way() {
        let (d, folder, clicks) = header_desk(&["old.txt", "new.txt"], true);
        let out = folder_call(&d, json!({"path": folder, "sort": "date_modified desc"})).unwrap();
        assert_eq!(*clicks.lock().unwrap(), 2);
        assert!(out.contains("(header click, verified)"), "{out}");
    }

    // Ported from test_header_click_gives_up_after_two_and_says_not_verified.
    #[test]
    fn header_click_gives_up_after_two_and_says_not_verified() {
        let (d, folder, clicks) = header_desk(&["old.txt", "b2.txt"], true);
        let out = folder_call(&d, json!({"path": folder, "sort": "date_modified desc"})).unwrap();
        assert_eq!(*clicks.lock().unwrap(), 2);
        assert!(
            out.contains("not verified") && out.contains("'b2.txt'"),
            "{out}"
        );
    }

    // Ported from test_header_click_accepts_a_folder_first_listing.
    #[test]
    fn header_click_accepts_a_folder_first_listing() {
        let (d, folder, _clicks) = header_desk(&["sub"], true); // folders grouped on top is also fine
        let out = folder_call(&d, json!({"path": folder, "sort": "date_modified asc"})).unwrap();
        assert!(out.contains("(header click, verified)"), "{out}");
    }

    // Ported from test_no_column_header_means_the_sort_is_reported_not_applied.
    #[test]
    fn no_column_header_means_the_sort_is_reported_not_applied() {
        let (d, folder, clicks) = header_desk(&["x"], false);
        let out = folder_call(&d, json!({"path": folder, "sort": "size desc"})).unwrap();
        assert!(
            out.contains("sort not applied (no 'Size' column header")
                && out.contains("view=details"),
            "{out}"
        );
        assert_eq!(*clicks.lock().unwrap(), 0);
    }

    // Ported from test_a_sort_the_shell_applied_is_never_clicked.
    #[test]
    fn a_sort_the_shell_applied_is_never_clicked() {
        let (d, folder, clicks) = header_desk(&["x"], true);
        d.kit.fake.set_shell_mode(ShellMode::Works);
        folder_call(&d, json!({"path": folder, "sort": "name"})).unwrap();
        assert_eq!(*clicks.lock().unwrap(), 0);
        assert_eq!(d.kit.fake.shell_state().1, "name asc");
    }

    // Ported from test_the_shell_sets_the_view_and_a_failing_sort_alone_falls_back_to_the_header.
    #[test]
    fn the_shell_sets_the_view_and_a_failing_sort_alone_falls_back_to_the_header() {
        let (d, folder, clicks) = header_desk(&["new"], true);
        d.kit.fake.set_shell_mode(ShellMode::Works);
        d.kit.fake.fail_shell_sort(true);
        let out = folder_call(
            &d,
            json!({"path": folder, "view": "details", "sort": "date_modified desc"}),
        )
        .unwrap();
        assert!(
            out.contains("view details (shell)") && out.contains("(header click, verified)"),
            "{out}"
        );
        assert!(out.contains("shell sort: E_FAIL"), "{out}");
        assert_eq!(*clicks.lock().unwrap(), 1);
        assert!(pressed(&d).is_empty());
    }

    // ---------------------------------------------------------------------------- the bundled recipes that use them

    #[test]
    fn the_bundled_close_app_recipe_closes_every_window_of_the_app() {
        let d = Desk::new();
        let a = d.add("a - Notepad", "notepad.exe", "Notepad");
        let b = d.add("b - Notepad", "notepad.exe", "Notepad");
        let mail = d.add("Mail", "olk.exe", "x");
        let out = d
        .kit
        .run(json!({"action": "recipe", "name": "close_app", "params": {"app": "notepad.exe"}, "shot": false}))
        .unwrap();
        assert!(
            out.text.starts_with("recipe close_app(app): 1 steps"),
            "{}",
            out.text
        );
        assert!(!d.kit.fake.window_exists(a) && !d.kit.fake.window_exists(b));
        assert!(d.kit.fake.window_exists(mail));
    }

    #[test]
    fn the_bundled_explorer_newest_recipe_answers_in_text() {
        let (d, folder) = folder_desk();
        let out = d
        .kit
        .run(json!({"action": "recipe", "name": "explorer_newest", "params": {"path": folder, "n": 2}}))
        .unwrap();
        let lines: Vec<&str> = out.text.lines().collect();
        assert_eq!(lines[0], "recipe explorer_newest(path, n=3): 1 steps");
        assert!(
            lines[1].contains("view details (shell); sort date_modified desc (shell)"),
            "{}",
            out.text
        );
        assert_eq!(name_of(lines[3]), "new.txt");
        assert!(out.images.is_empty(), "a text answer needs no picture");
    }

    #[test]
    fn the_bundled_open_url_recipe_waits_for_the_browser() {
        let d = Desk::new();
        let t = d.add("terminal", "terminal.exe", "t");
        d.kit.fake.set_foreground(Some(t));
        browser_hook(&d, "Example Domain - Brave");
        let out = d
        .kit
        .run(json!({"action": "recipe", "name": "open_url", "params": {"url": "https://example.com"}, "shot": false}))
        .unwrap();
        assert!(
            out.text.contains("opened https://example.com in w"),
            "{}",
            out.text
        );
    }

    // ---------------------------------------------------------------------------- folder: opening the window

    // Ported from test_a_new_window_is_found_from_do_opens_reply.
    #[test]
    fn a_new_window_is_found_from_do_opens_reply() {
        let (d, folder) = folder_desk();
        d.add("Downloads", "explorer.exe", "CabinetWClass");
        let ctx = d.kit.ctx(json!({}));
        let (w, how) = ctx.open_explorer(&folder).unwrap();
        assert_eq!(how, "opened a new window");
        assert_eq!(d.kit.fake.window_info(w).unwrap().title, leaf_of(&folder));
        let launches = d.kit.fake.launches();
        assert_eq!(launches.len(), 1);
        assert!(launches[0].args.contains(&folder), "{:?}", launches[0]);
    }

    // Ported from test_an_explorer_window_already_on_the_folder_is_reused_when_the_shell_can_say_so.
    #[test]
    fn an_explorer_window_already_on_the_folder_is_reused_when_the_shell_can_say_so() {
        let (d, folder) = folder_desk();
        let existing = d.add("Downloads", "explorer.exe", "CabinetWClass");
        d.kit.fake.set_folder_window(&folder, existing);
        let ctx = d.kit.ctx(json!({}));
        let (w, how) = ctx.open_explorer(&folder).unwrap();
        assert_eq!(
            (w, how.as_str()),
            (existing, "reused the window already open on it")
        );
        assert!(
            d.kit.fake.launches().is_empty(),
            "must not start another window"
        );
        assert_eq!(d.kit.fake.foreground_window().unwrap(), Some(existing));
    }

    // Ported from test_when_explorer_raises_the_existing_window_instead_of_making_one.
    #[test]
    fn when_explorer_raises_the_existing_window_instead_of_making_one() {
        let d = Desk::new();
        let folder = make_folder(d.kit.tmp.path());
        let existing = d.add(&leaf_of(&folder), "explorer.exe", "CabinetWClass");
        // the launch makes no new window at all
        let ctx = d.kit.ctx(json!({}));
        let (w, how) = ctx.open_explorer(&folder).unwrap();
        assert_eq!(
            (w, how.as_str()),
            (existing, "found the window already open on it")
        );
        // a window with some other title does not count
        d.kit
            .fake
            .update_window(existing, |i| i.title = "Something else".into());
        let e = ctx.open_explorer(&folder).unwrap_err();
        assert!(matches!(e, StepError::Target(_)), "{e}");
    }
}
