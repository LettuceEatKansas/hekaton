//! The allowlist and the other places input may not land. Ported from `tests/test_computer.py`
//! (`_check_allowed`, `_check_launch_allowed`, `_refuse_explorer_open`, `_window_titled`,
//! `_real_launch_target`).

use serde_json::json;

use super::StepError;
use super::guard::{basename, normalize_only_app, real_launch_target};
use super::testkit::Kit;
use crate::backend::{Rect, WindowInfo, WindowKind, WindowRef};

fn add(kit: &Kit, title: &str, app: &str, pid: u32, class: &str) -> WindowRef {
    let mut info = WindowInfo::basic(title, app, pid, Rect::from_xywh(0, 0, 50, 50));
    info.class = class.into();
    kit.fake.add_window(info)
}

fn refused(r: Result<(), StepError>) -> StepError {
    let e = r.expect_err("should be refused");
    assert!(matches!(e, StepError::Target(_)), "{e:?}");
    e
}

// Ported from test_only_apps_refuses_other_windows.
//
// 1 = an unrelated game. 2 = notepad.exe, directly allowed. 3/4 = two msedge.exe windows, only the
// InPrivate-titled one allowed. 5 = the taskbar: explorer.exe, but a system surface, always refused
// under an allowlist (S1). 6 and 7 = the owner's own Explorer window and the eval's folder window,
// BOTH windows of the SAME explorer.exe pid (1001) -- the old bug approved any window sharing a pid
// with an allowed one, which would have let 6 through too. 8 = an unnamed popup (a different process,
// no title match) owned by the allowed Notepad window (2): approved only by walking that real owner
// relationship (S2), never by pid.
#[test]
fn only_apps_refuses_other_windows() {
    let kit = Kit::new();
    let w1 = add(&kit, "", "slaythespire2.exe", 100, "Game");
    let w2 = add(&kit, "", "notepad.exe", 200, "Notepad");
    let w3 = add(
        &kit,
        "wiki - [InPrivate] - Microsoft Edge",
        "msedge.exe",
        300,
        "Chrome_WidgetWin_1",
    );
    let w4 = add(
        &kit,
        "Xbox - Personal - Microsoft Edge",
        "msedge.exe",
        301,
        "Chrome_WidgetWin_1",
    );
    let w5 = add(&kit, "", "explorer.exe", 1001, "Shell_TrayWnd");
    let w6 = add(
        &kit,
        "Stop - Personal - File Explorer",
        "explorer.exe",
        1001,
        "CabinetWClass",
    );
    let w7 = add(
        &kit,
        "folder-123 - File Explorer",
        "explorer.exe",
        1001,
        "CabinetWClass",
    );
    let w8 = add(&kit, "", "printdlghost.exe", 900, "Popup");
    kit.fake.update_window(w8, |i| i.owner = Some(w2));
    let ctx = kit
        .ctx(json!({"only_apps": ["notepad.exe", "msedge.exe:inprivate", "explorer.exe:folder-"]}));
    for ok in [w2, w3, w7, w8] {
        ctx.check_allowed(ok)
            .unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    for bad in [w1, w4, w5, w6] {
        refused(ctx.check_allowed(bad));
    }
    let msg = refused(ctx.check_allowed(w1)).to_string();
    assert_eq!(
        msg,
        "refused: that window belongs to slaythespire2.exe, which this task may not touch (allowed: explorer.exe:folder-, msedge.exe:inprivate, notepad.exe)"
    );
    // a window that no longer exists is refused, not waved through
    refused(ctx.check_allowed(WindowRef::from_raw(31337)));
}

// Ported from test_desktop_and_taskbar_refused_under_allowlist and test_run_dialog_refused_under_allowlist.
#[test]
fn the_desktop_the_taskbar_and_the_run_dialog_are_a_way_out_of_an_allowlist() {
    for class in [
        "Progman",
        "WorkerW",
        "Shell_TrayWnd",
        "Shell_SecondaryTrayWnd",
    ] {
        let kit = Kit::new();
        let w = add(&kit, "", "explorer.exe", 5, class);
        let err = refused(
            kit.ctx(json!({"only_apps": ["notepad.exe"]}))
                .check_allowed(w),
        );
        assert!(
            err.to_string().contains("desktop, taskbar"),
            "{class}: {err}"
        );
    }
    let kit = Kit::new();
    let run = add(&kit, "Run", "explorer.exe", 5, "#32770");
    let err = refused(
        kit.ctx(json!({"only_apps": ["notepad.exe"]}))
            .check_allowed(run),
    );
    assert!(
        err.to_string()
            .contains("desktop, taskbar or the Run dialog"),
        "{err}"
    );
    // the neutral kinds a backend may report instead of the Windows class names
    let kit = Kit::new();
    let dock = add(&kit, "", "dock", 6, "AXDock");
    kit.fake
        .update_window(dock, |i| i.kind = WindowKind::Taskbar);
    refused(
        kit.ctx(json!({"only_apps": ["notepad.exe"]}))
            .check_allowed(dock),
    );
    // with no allowlist nothing is refused
    kit.ctx(json!({})).check_allowed(dock).unwrap();
}

#[test]
fn shell_surfaces_pass_but_a_file_manager_window_needs_allowing() {
    let kit = Kit::new();
    let start = add(
        &kit,
        "Start",
        "shellexperiencehost.exe",
        7,
        "Windows.UI.Core.CoreWindow",
    );
    let folder = add(&kit, "Documents", "explorer.exe", 8, "CabinetWClass");
    let ctx = kit.ctx(json!({"only_apps": ["notepad.exe"]}));
    ctx.check_allowed(start).unwrap();
    refused(ctx.check_allowed(folder));
    kit.ctx(json!({"only_apps": ["explorer.exe"]}))
        .check_allowed(folder)
        .unwrap();
}

#[test]
fn a_store_app_is_judged_by_the_program_inside_its_frame() {
    let kit = Kit::new();
    let frame = add(
        &kit,
        "Calculator",
        "ApplicationFrameHost.exe",
        9,
        "ApplicationFrameWindow",
    );
    kit.fake.set_hosted_app(frame, "CalculatorApp.exe");
    kit.ctx(json!({"only_apps": ["calculatorapp.exe"]}))
        .check_allowed(frame)
        .unwrap();
    refused(
        kit.ctx(json!({"only_apps": ["notepad.exe"]}))
            .check_allowed(frame),
    );
}

#[test]
fn a_regex_scope_matches_the_lowercased_title_case_insensitively() {
    let kit = Kit::new();
    let wiki = add(&kit, "The WIKI page - Edge", "msedge.exe", 1, "C");
    let other = add(&kit, "Wikipedia - Edge", "msedge.exe", 2, "C");
    let ctx = kit.ctx(json!({"only_apps": ["MSEdge.exe:re:\\bWiki\\b"]}));
    ctx.check_allowed(wiki).unwrap();
    refused(ctx.check_allowed(other));
    // a pattern that does not compile matches nothing instead of failing the call
    refused(
        kit.ctx(json!({"only_apps": ["msedge.exe:re:(["]}))
            .check_allowed(wiki),
    );
    // a substring scope is NOT lowercased: a capital in it never matches the lowercased title
    refused(
        kit.ctx(json!({"only_apps": ["msedge.exe:WIKI"]}))
            .check_allowed(wiki),
    );
}

#[test]
fn only_app_entries_lowercase_the_program_and_keep_the_scope() {
    assert_eq!(normalize_only_app("Notepad.EXE"), "notepad.exe");
    assert_eq!(
        normalize_only_app("MSEdge.exe:re:\\bWiki\\b"),
        "msedge.exe:re:\\bWiki\\b"
    );
    assert_eq!(normalize_only_app("A:B:C"), "a:B:C");
}

// ---------------------------------------------------------------------------- launches

/// The list the launch tests share.
fn launch_kit() -> (Kit, String) {
    let kit = Kit::new();
    add(&kit, "Untitled - EvalPad", "pythonw.exe", 50, "TkTopLevel");
    let temp = kit.tmp.path().to_string_lossy().to_string();
    (kit, temp)
}

const ALLOWED: [&str; 4] = [
    "msedge.exe:inprivate",
    "calculatorapp.exe",
    "winver.exe",
    "pythonw.exe: - evalpad",
];

// Ported from test_launch_allowlist.
#[test]
fn the_launch_allowlist_names_what_may_start() {
    let (kit, temp) = launch_kit();
    let ctx = kit.ctx(json!({"only_apps": ALLOWED}));
    let edge = "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe";
    let udd = format!("{temp}/eval-edge-profile");
    for ok in [
        "Calculator".to_string(),
        "EvalPad".to_string(),
        "winver".to_string(),
        format!("{edge} --inprivate --user-data-dir={udd} --new-window x"),
    ] {
        ctx.check_launch_allowed(&ok)
            .unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    let other = kit
        .tmp
        .path()
        .parent()
        .unwrap()
        .join("other")
        .to_string_lossy()
        .to_string();
    let folder = kit.tmp.path().join("docs");
    std::fs::create_dir_all(&folder).unwrap();
    let missing = kit
        .tmp
        .path()
        .join("does-not-exist")
        .to_string_lossy()
        .to_string();
    for bad in [
        format!("{edge} --new-window x"), // no --inprivate at all
        format!("{edge} --inprivate --new-window x"), // no --user-data-dir
        format!("{edge} --inprivate --user-data-dir=\"{other}\" x"), // dir outside %TEMP%
        "firefox.exe --private-window".to_string(),
        "notepad".to_string(),
        "ms-paint:".to_string(),
        "Microsoft.WindowsPaint_8wekyb3d8bbwe!App".to_string(),
        "powershell -c start calculatorapp.exe".to_string(), // the REAL program is powershell, not calc
        "explorer ms-settings:".to_string(),
        "explorer ::{20d04fe0-3aea-1069-a2d8-08002b30309d}".to_string(),
        format!("explorer {missing}"),
        format!("explorer {}", folder.to_string_lossy()), // explorer not allowlisted at all (live hole 2026-09-21)
    ] {
        let err = refused(ctx.check_launch_allowed(&bad));
        assert!(err.to_string().starts_with("refused: "), "{bad}: {err}");
    }
    let err = refused(ctx.check_launch_allowed("firefox.exe --private-window"));
    assert_eq!(
        err.to_string(),
        "refused: this task may not open firefox.exe (allowed: calculatorapp.exe, msedge.exe, pythonw.exe, winver.exe)"
    );
    let err = refused(ctx.check_launch_allowed("ms-paint:"));
    assert_eq!(
        err.to_string(),
        "refused: 'ms-paint:' is not an app this task may open (allowed: calculatorapp.exe, msedge.exe, pythonw.exe, winver.exe)"
    );
    // no allowlist: anything goes
    kit.ctx(json!({}))
        .check_launch_allowed("ms-paint:")
        .unwrap();
}

// Ported from test_explorer_launch_scoped.
#[test]
fn explorer_may_only_open_a_folder_the_scope_allows() {
    let kit = Kit::new();
    let good = kit.tmp.path().join("folder-dates-1");
    let other = kit.tmp.path().join("Documents");
    std::fs::create_dir_all(&good).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let ctx = kit.ctx(json!({"only_apps": ["explorer.exe:folder-", "explorer.exe:rename-"]}));
    ctx.check_launch_allowed(&format!("explorer {}", good.to_string_lossy()))
        .unwrap();
    for bad in [
        format!("explorer {}", other.to_string_lossy()),
        "explorer".to_string(),
    ] {
        let err = refused(ctx.check_launch_allowed(&bad));
        assert!(
            err.to_string()
                .contains("only open File Explorer on a folder matching ['folder-', 'rename-']"),
            "{err}"
        );
    }
    let open = kit.ctx(json!({"only_apps": ["explorer.exe"]}));
    open.check_launch_allowed(&format!("explorer {}", other.to_string_lossy()))
        .unwrap();
    open.check_launch_allowed("explorer").unwrap();
}

#[test]
fn explorer_is_never_launched_with_a_shell_uri_or_a_missing_folder() {
    let kit = Kit::new();
    let ctx = kit.ctx(json!({"only_apps": ["explorer.exe"]}));
    for arg in [
        "ms-settings:",
        "shell:::{20d04fe0}",
        "https://example.com",
        "C:/definitely/not/a/folder",
    ] {
        let err = refused(ctx.check_launch_allowed(&format!("explorer {arg}")));
        assert!(
            err.to_string().starts_with("refused: this task may only open File Explorer with no argument or an existing folder path, not "),
            "{arg}: {err}"
        );
    }
}

#[test]
fn a_private_edge_needs_its_own_temp_profile() {
    let (kit, temp) = launch_kit();
    let ctx = kit.ctx(json!({"only_apps": ALLOWED}));
    let err = refused(ctx.check_launch_allowed("msedge.exe --new-window x"));
    assert_eq!(
        err.to_string(),
        "refused: this task may only open Edge as a private window with its own temp profile: pass --inprivate and --user-data-dir=<a path under %TEMP%>"
    );
    // --inprivate must be a token of its own
    refused(ctx.check_launch_allowed(&format!(
        "msedge.exe --inprivate-ish --user-data-dir={temp}/p"
    )));
    ctx.check_launch_allowed(&format!("msedge.exe --inprivate --user-data-dir={temp}/p"))
        .unwrap();
}

// Ported from test_open_app_name_focuses_existing_window.
#[test]
fn a_name_that_matches_a_window_title_is_found_when_the_allowlist_permits() {
    let kit = Kit::new();
    add(
        &kit,
        "Overview - Brave",
        "brave.exe",
        1,
        "Chrome_WidgetWin_1",
    );
    let mut ghost = WindowInfo::basic(
        "Overview ghost",
        "brave.exe",
        2,
        Rect::from_xywh(0, 0, 5, 5),
    );
    ghost.hidden = true;
    kit.fake.add_window(ghost);
    let ctx = kit.ctx(json!({}));
    assert_eq!(
        ctx.window_titled("Overview").map(|w| w.title),
        Some("Overview - Brave".to_string())
    );
    assert!(ctx.window_titled("Nope").is_none());
    assert!(ctx.window_titled("   ").is_none());
    assert!(
        ctx.window_titled("ghost").is_none(),
        "a cloaked window cannot be foregrounded"
    );
    let limited = kit.ctx(json!({"only_apps": ["notepad.exe"]}));
    assert!(limited.window_titled("Overview").is_none());
}

#[test]
fn the_real_program_of_a_launch_command_is_its_first_token() {
    let t = |s: &str| real_launch_target(&s.to_lowercase());
    assert_eq!(
        t("Calculator"),
        ("calculatorapp.exe".to_string(), String::new())
    );
    assert_eq!(
        t("notepad C:\\notes.txt"),
        ("notepad.exe".to_string(), "c:\\notes.txt".to_string())
    );
    assert_eq!(
        t("\"C:\\Program Files\\App\\app.exe\" -x"),
        ("app.exe".to_string(), "-x".to_string())
    );
    assert_eq!(
        t("C:/Program Files (x86)/Edge/msedge.exe --inprivate"),
        ("msedge.exe".to_string(), "--inprivate".to_string())
    );
    // the REAL program, not any .exe-looking word later in the string
    assert_eq!(
        t("powershell -c start calculatorapp.exe"),
        (String::new(), "-c start calculatorapp.exe".to_string())
    );
    assert_eq!(t("ms-paint:").0, "");
    assert_eq!(t("").0, "");
    assert_eq!(basename("C:/a/b\\c.exe"), "c.exe");
}

// ---------------------------------------------------------------------------- explorer opens files

// Ported from test_enter_on_explorer_file_list_refused_under_allowlist and
// test_enter_in_explorer_menu_still_refused_with_menu_reason.
#[test]
fn enter_on_explorer_file_list_is_refused_under_an_allowlist() {
    let kit = Kit::new();
    let folder = add(&kit, "Rename test", "explorer.exe", 4, "CabinetWClass");
    let ctx = kit.ctx(json!({"only_apps": ["explorer.exe:rename-"]}));
    let err = refused(ctx.refuse_explorer_open(Some(folder)));
    assert_eq!(
        err.to_string(),
        "refused: Enter or double-click here would open the selected file in another app. To rename use F2; to read a file use its task's editor"
    );
    // typing a new name: Return confirms it
    kit.fake.set_window_caret(folder, true);
    ctx.refuse_explorer_open(Some(folder)).unwrap();
    // no allowlist: a person's own use
    kit.fake.set_window_caret(folder, false);
    kit.ctx(json!({}))
        .refuse_explorer_open(Some(folder))
        .unwrap();
    // not a file manager window, or no window at all
    let notepad = add(&kit, "Notes", "notepad.exe", 5, "Notepad");
    ctx.refuse_explorer_open(Some(notepad)).unwrap();
    ctx.refuse_explorer_open(None).unwrap();
    // an open menu gives a clearer reason: the highlighted item may be "Open"
    kit.fake
        .set_popup_menu(vec![WindowRef::from_raw(99)], vec![]);
    let err = refused(ctx.refuse_explorer_open(Some(folder)));
    assert!(err.to_string().contains("Click the menu item"), "{err}");
}

// Ported from test_refuse_explorer_open_checks_the_target_windows_own_caret (S5): the caret must
// belong to the window that will actually receive the input, not whatever else is in front.
#[test]
fn the_caret_checked_is_the_target_windows_own() {
    let kit = Kit::new();
    let background = add(&kit, "Eval", "explorer.exe", 4, "CabinetWClass");
    let front = add(&kit, "Other", "notepad.exe", 5, "Notepad");
    kit.fake.set_foreground(Some(front));
    kit.fake.set_window_caret(background, false);
    kit.fake.set_window_caret(front, true);
    let ctx = kit.ctx(json!({"only_apps": ["explorer.exe:eval-"]}));
    refused(ctx.refuse_explorer_open(Some(background)));
}

#[test]
fn keys_into_an_explorer_file_list_are_refused_under_an_allowlist() {
    let kit = Kit::new();
    let folder = add(&kit, "folder-1", "explorer.exe", 4, "CabinetWClass");
    kit.fake.set_foreground(Some(folder));
    for combo in ["enter", "alt+enter", "Return"] {
        let err = kit
            .step_only(
                &["explorer.exe:folder-"],
                json!({"action": "key", "text": combo}),
            )
            .unwrap_err();
        assert!(
            err.message().contains("would open the selected file"),
            "{combo}: {err}"
        );
    }
    let err = kit
        .step_only(
            &["explorer.exe:folder-"],
            json!({"action": "double_click", "coordinate": [10, 10]}),
        )
        .unwrap_err();
    assert!(
        err.message().contains("would open the selected file"),
        "{err}"
    );
    // F2 is the rename route and stays allowed
    kit.step_only(
        &["explorer.exe:folder-"],
        json!({"action": "key", "text": "f2"}),
    )
    .unwrap();
}
