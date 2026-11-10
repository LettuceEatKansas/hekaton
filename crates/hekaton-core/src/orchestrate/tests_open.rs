//! Starting and focusing apps and filling the file dialog. Ported from `tests/test_20260919_fixes.py`
//! (the `open` tests), `tests/test_20260920_game_fixes.py` (a restored Notepad session) and
//! `tests/test_computer.py` (`_file_dialog`, `_repair_escapes`, open_app). What a name resolves to is
//! the backend's business, so the fake is scripted with what `resolve_app` should find.

use std::sync::{Arc, Mutex};

use serde_json::json;

use super::StepError;
use super::testkit::{Kit, cmd_key};
use crate::backend::Backend;
use crate::backend::fake::FakeBackend;
use crate::backend::{
    AppTarget, ClipboardContent, Element, ElementId, LaunchSpec, Rect, Role, WindowInfo, WindowRef,
};
use crate::clock::Clock;
use crate::keys::Key;

fn app_window(kit: &Kit, title: &str, app: &str, pid: u32) -> WindowRef {
    kit.fake.add_window(WindowInfo::basic(
        title,
        app,
        pid,
        Rect::from_xywh(0, 0, 50, 50),
    ))
}

fn do_open(kit: &Kit, app: &str, wait: f64, new: bool, reuse: bool) -> Result<String, StepError> {
    kit.ctx(json!({})).do_open(app, wait, new, reuse)
}

fn record_chords(kit: &Kit) -> Arc<Mutex<Vec<Vec<Key>>>> {
    let seen: Arc<Mutex<Vec<Vec<Key>>>> = Arc::default();
    let sink = seen.clone();
    kit.fake.set_chord_hook(move |fake, keys| {
        sink.lock().unwrap().push(keys.to_vec());
        if keys == [cmd_key(), Key::Char('n')] {
            retitle_notepad(fake);
        }
    });
    seen
}

/// ctrl+n in a tabbed editor opens a blank tab, and the window shows it in its title.
fn retitle_notepad(fake: &FakeBackend) {
    for w in crate::backend::Backend::list_windows(fake, true).unwrap() {
        if w.app.to_lowercase() == "notepad.exe" {
            fake.update_window(w.window, |i| i.title = "Untitled - Notepad".into());
        }
    }
}

// ---------------------------------------------------------------------------- do_open

// Ported from test_open_focuses_running_app_instead_of_launching.
#[test]
fn open_focuses_a_running_app_instead_of_launching() {
    let kit = Kit::new();
    let notepad = app_window(&kit, "job.log - Notepad", "Notepad.exe", 30);
    app_window(&kit, "Claude", "claude.exe", 40);
    let out = do_open(&kit, "notepad", 8.0, false, true).unwrap();
    assert!(
        out.contains("job.log - Notepad") && !out.contains("new blank tab"),
        "{out}"
    );
    assert_eq!(kit.fake.foreground_window().unwrap(), Some(notepad));
    assert!(kit.fake.launches().is_empty());
}

// Ported from test_open_new_on_running_editor_makes_blank_tab (2026-09-20): open('Notepad', new=True)
// reused a Notepad showing job.log and the typed text landed there.
#[test]
fn open_new_on_a_running_editor_makes_a_blank_tab() {
    let kit = Kit::new();
    let w = app_window(&kit, "job.log - Notepad", "Notepad.exe", 30);
    let chords = record_chords(&kit);
    let out = do_open(&kit, "notepad", 8.0, true, true).unwrap();
    assert_eq!(
        chords.lock().unwrap().clone(),
        vec![vec![cmd_key(), Key::Char('n')]]
    );
    assert_eq!(
        out,
        "ok new blank tab in w0 \"Untitled - Notepad\" (was showing \"job.log - Notepad\")"
    );
    assert_eq!(kit.fake.foreground_window().unwrap(), Some(w));
}

#[test]
fn a_new_tab_that_never_appears_is_reported_not_hidden() {
    let kit = Kit::new();
    app_window(&kit, "job.log - Notepad", "Notepad.exe", 30);
    let out = do_open(&kit, "notepad", 8.0, true, true).unwrap();
    assert_eq!(
        out,
        "ok focused w0 \"job.log - Notepad\" but no new tab appeared; the text you type next goes into \
         that document. Press ctrl+n yourself or pass reuse=false"
    );
}

// Ported from test_open_reuse_false_skips_focus_shortcut. The notepad -> notepad.exe alias is Windows only.
#[cfg(windows)]
#[test]
fn open_with_reuse_false_launches_even_when_it_is_running() {
    let kit = Kit::new();
    app_window(&kit, "job.log - Notepad", "Notepad.exe", 30);
    let err = do_open(&kit, "notepad", 0.3, true, false).unwrap_err();
    assert!(matches!(err, StepError::Target(_)));
    assert!(
        err.message().starts_with(
            "'notepad' was started but showed no window within 0s. It is probably still loading"
        ),
        "{err}"
    );
    assert_eq!(
        kit.fake.launches(),
        vec![LaunchSpec {
            target: AppTarget::Executable("notepad.exe".into()),
            args: String::new(),
            deelevate: true
        }]
    );
}

// Ported from test_open_reuses_store_app_by_title.
#[test]
fn open_reuses_a_store_app_by_its_title() {
    let kit = Kit::new();
    let calc = app_window(&kit, "Calculator", "ApplicationFrameHost.exe", 30);
    let out = do_open(&kit, "calc", 8.0, true, true).unwrap();
    assert!(out.contains("already running"), "{out}");
    assert_eq!(kit.fake.foreground_window().unwrap(), Some(calc));
    assert!(kit.fake.launches().is_empty());
}

#[cfg(not(windows))]
#[test]
fn open_reuses_a_running_mac_app_by_its_bundle_id() {
    let kit = Kit::new();
    let safari = app_window(&kit, "DuckDuckGo search", "com.apple.Safari", 30);
    let out = do_open(&kit, "Safari", 8.0, true, true).unwrap();
    assert!(out.contains("already running"), "{out}");
    assert_eq!(kit.fake.foreground_window().unwrap(), Some(safari));
    assert!(kit.fake.launches().is_empty());
}

#[test]
fn a_ref_or_an_exact_title_focuses_without_launching() {
    let kit = Kit::new();
    app_window(&kit, "Budget", "excel.exe", 1);
    let notes = app_window(&kit, "Notes", "notepad.exe", 2);
    // refs are assigned as windows are first listed: the topmost, Notes, is w0
    assert_eq!(
        do_open(&kit, "w0", 8.0, true, true).unwrap(),
        "ok focused w0 \"Notes\""
    );
    assert_eq!(kit.fake.foreground_window().unwrap(), Some(notes));
    assert_eq!(
        do_open(&kit, "  budget  ", 8.0, true, true).unwrap(),
        "ok focused w1 \"Budget\""
    );
    let err = do_open(&kit, "w9", 8.0, true, true).unwrap_err();
    assert_eq!(err.message(), "no window ref 'w9'");
    // new=false also takes a substring of a title
    assert_eq!(
        do_open(&kit, "udge", 8.0, false, true).unwrap(),
        "ok focused w1 \"Budget\""
    );
    assert!(kit.fake.launches().is_empty());
}

#[test]
fn a_protected_window_is_not_focused_by_open() {
    let kit = Kit::new();
    app_window(&kit, "Vault", "keepass.exe", 1);
    std::fs::create_dir_all(kit.computer.config().paths().dir()).unwrap();
    std::fs::write(kit.computer.config().paths().protected(), "keepass.exe\n").unwrap();
    let err = do_open(&kit, "Vault", 8.0, true, true).unwrap_err();
    assert!(matches!(err, StepError::Permission(_)), "{err:?}");
}

// Ported from test_open_a_folder_path_goes_to_explorer_not_createprocess.
#[cfg(windows)]
#[test]
fn open_a_folder_path_goes_to_explorer_not_to_createprocess() {
    let kit = Kit::new();
    let folder = kit.tmp.path().join("Reports");
    std::fs::create_dir(&folder).unwrap();
    let path = folder.to_string_lossy().to_string();
    let err = do_open(&kit, &path, 0.2, true, true).unwrap_err();
    assert!(
        matches!(err, StepError::Target(_)),
        "no window appears in the fake"
    );
    let launched = kit.fake.launches();
    assert_eq!(
        launched[0].target,
        AppTarget::Executable("explorer.exe".into())
    );
    assert_eq!(launched[0].args, format!("\"{path}\""));
    // an Explorer window already showing the folder is focused instead
    let kit = Kit::new();
    let folder = kit.tmp.path().join("Reports");
    std::fs::create_dir(&folder).unwrap();
    let shown = app_window(&kit, "Reports - File Explorer", "explorer.exe", 5);
    let out = do_open(&kit, &folder.to_string_lossy(), 0.2, true, true).unwrap();
    assert_eq!(
        out,
        "ok focused w0 \"Reports - File Explorer\" (folder already open)"
    );
    assert_eq!(kit.fake.foreground_window().unwrap(), Some(shown));
    assert!(kit.fake.launches().is_empty());
}

// Ported from test_fresh_notepad_launch_on_restored_tab_gets_blank_tab (bug 7): a fresh Notepad
// restores its session onto an old unsaved tab.
#[test]
fn a_fresh_notepad_on_a_restored_tab_gets_a_blank_tab() {
    let kit = Kit::new();
    app_window(&kit, "Claude", "claude.exe", 10);
    let chords = record_chords(&kit);
    kit.fake.set_launch_hook(|fake, _| {
        fake.add_window(WindowInfo::basic(
            "*debug_notes.txt - Notepad",
            "Notepad.exe",
            30,
            Rect::from_xywh(0, 0, 50, 50),
        ));
    });
    let out = do_open(&kit, "notepad", 3.0, true, false).unwrap();
    assert_eq!(
        chords.lock().unwrap().clone(),
        vec![vec![cmd_key(), Key::Char('n')]]
    );
    assert!(out.contains("Untitled - Notepad"), "{out}");
    assert!(out.starts_with("ok new blank tab in w1"), "{out}");
}

#[test]
fn a_fresh_blank_notepad_is_left_alone_and_a_new_window_is_reported() {
    let kit = Kit::new();
    app_window(&kit, "Claude", "claude.exe", 10);
    let chords = record_chords(&kit);
    kit.fake.set_launch_hook(|fake, _| {
        fake.add_window(WindowInfo::basic(
            "Untitled - Notepad",
            "Notepad.exe",
            30,
            Rect::from_xywh(0, 0, 50, 50),
        ));
    });
    let out = do_open(&kit, "notepad", 3.0, true, false).unwrap();
    assert_eq!(out, "ok focused w1 \"Untitled - Notepad\" pid:30");
    assert!(chords.lock().unwrap().is_empty());
    // an app that is not an editor is reported the same way
    let kit = Kit::new();
    kit.fake.set_launch_hook(|fake, _| {
        fake.add_window(WindowInfo::basic(
            "Hello - Viewer",
            "viewer.exe",
            77,
            Rect::from_xywh(0, 0, 50, 50),
        ));
    });
    assert_eq!(
        do_open(&kit, "viewer.exe --fast", 3.0, true, true).unwrap(),
        "ok focused w0 \"Hello - Viewer\" pid:77"
    );
    assert_eq!(kit.fake.launches()[0].args, "--fast");
}

// Ported from test_open_prefers_start_menu_app_over_path_tool (2026-09-20).
#[test]
fn a_bare_name_launches_what_the_backend_resolves_it_to() {
    let kit = Kit::new();
    kit.fake.set_app(
        "hermes",
        AppTarget::Shortcut("C:\\Start Menu\\Hermes.lnk".into()),
    );
    let err = do_open(&kit, "Hermes", 0.2, true, true).unwrap_err();
    assert!(
        matches!(err, StepError::Target(_)),
        "no window appears in the fake; the launch is what matters"
    );
    assert_eq!(
        kit.fake.launches()[0],
        LaunchSpec {
            target: AppTarget::Shortcut("C:\\Start Menu\\Hermes.lnk".into()),
            args: String::new(),
            deelevate: true
        }
    );
    // a Store app goes through the shell
    kit.fake.set_app(
        "snipping tool",
        AppTarget::Packaged("Microsoft.ScreenSketch_8wekyb3d8bbwe!App".into()),
    );
    do_open(&kit, "Snipping Tool", 0.2, true, true).unwrap_err();
    assert_eq!(
        kit.fake.launches()[1].target,
        AppTarget::Packaged("Microsoft.ScreenSketch_8wekyb3d8bbwe!App".into())
    );
    // a program on the path is launched by the path the backend found
    kit.fake.set_app(
        "brave",
        AppTarget::Executable("C:\\Brave\\brave.exe".into()),
    );
    do_open(&kit, "brave --new-window http://x", 0.2, true, true).unwrap_err();
    assert_eq!(
        kit.fake.launches()[2],
        LaunchSpec {
            target: AppTarget::Executable("C:\\Brave\\brave.exe".into()),
            args: "--new-window http://x".into(),
            deelevate: true
        }
    );
    // anything else goes to the shell as it is (a URI, a document)
    do_open(&kit, "ms-settings:", 0.2, true, true).unwrap_err();
    assert_eq!(
        kit.fake.launches()[3].target,
        AppTarget::Other("ms-settings:".into())
    );
}

// Python split "brave --new-window url" because its resolver fell through to the shortcut's target,
// so a name that only resolves to a Start Menu shortcut must split too.
#[test]
fn a_name_that_resolves_to_a_shortcut_still_splits_off_its_arguments() {
    let kit = Kit::new();
    kit.fake.set_app(
        "brave",
        AppTarget::Shortcut("C:\\Start Menu\\Brave.lnk".into()),
    );
    do_open(&kit, "brave --new-window http://x", 0.2, true, true).unwrap_err();
    assert_eq!(
        kit.fake.launches()[0],
        LaunchSpec {
            target: AppTarget::Shortcut("C:\\Start Menu\\Brave.lnk".into()),
            args: "--new-window http://x".into(),
            deelevate: true
        }
    );
    // a packaged app is not a program: nothing is split off it
    kit.fake.set_app(
        "sample",
        AppTarget::Packaged("Sample.App_8wekyb3d8bbwe!App".into()),
    );
    do_open(&kit, "sample --flag", 0.2, true, true).unwrap_err();
    assert_eq!(kit.fake.launches()[1].args, "");
}

#[test]
fn only_programs_and_shortcuts_count_as_launchable_heads() {
    use super::open::is_launchable;
    assert!(is_launchable(Some(&AppTarget::Executable("a.exe".into()))));
    assert!(is_launchable(Some(&AppTarget::Shortcut("a.lnk".into()))));
    assert!(!is_launchable(Some(&AppTarget::Packaged("pkg!App".into()))));
    assert!(!is_launchable(Some(&AppTarget::Other(
        "ms-settings:".into()
    ))));
    assert!(!is_launchable(None));
}

#[test]
fn a_destructive_launch_string_is_refused_before_anything_starts() {
    let kit = Kit::new();
    let err = do_open(&kit, "cmd /c format D: /q", 0.2, true, true).unwrap_err();
    assert!(matches!(err, StepError::Permission(_)), "{err:?}");
    assert!(err.message().contains("the launch string"));
    assert!(kit.fake.launches().is_empty());
    // the one-call opt-in lets it through
    let ctx = {
        use crate::destructive::Policy;
        let o = super::CallOpts::from_args(&kit.computer, &Kit::args(json!({})), Policy::allow())
            .unwrap();
        super::Ctx {
            c: &kit.computer,
            o,
            who: &kit.who,
        }
    };
    ctx.do_open("cmd /c format D: /q", 0.2, true, true)
        .unwrap_err();
    assert_eq!(kit.fake.launches().len(), 1);
}

// ---------------------------------------------------------------------------- open_app

// Ported from test_open_app_name_focuses_existing_window (the open_app side).
#[test]
fn open_app_with_a_window_title_focuses_that_window() {
    let kit = Kit::new();
    let brave = app_window(&kit, "Overview - Brave", "brave.exe", 1);
    app_window(&kit, "Terminal", "wt.exe", 2);
    let (note, _) = kit
        .step(json!({"action": "open_app", "text": "Overview"}))
        .unwrap();
    assert_eq!(note, "focused existing window 'Overview - Brave'");
    assert_eq!(kit.fake.foreground_window().unwrap(), Some(brave));
    assert!(kit.fake.launches().is_empty());
    assert_eq!(
        kit.step(json!({"action": "open_app"}))
            .unwrap_err()
            .message(),
        "open_app needs text: the app name, e.g. Notepad"
    );
}

#[test]
fn open_app_under_an_allowlist_only_starts_or_focuses_what_is_allowed() {
    let kit = Kit::new();
    app_window(&kit, "Overview - Brave", "brave.exe", 1);
    let err = kit
        .step_only(
            &["notepad.exe"],
            json!({"action": "open_app", "text": "Overview"}),
        )
        .unwrap_err();
    assert!(
        err.message()
            .starts_with("refused: 'Overview' is not an app this task may open"),
        "{err}"
    );
    let err = kit
        .step_only(
            &["notepad.exe"],
            json!({"action": "open_app", "text": "calc"}),
        )
        .unwrap_err();
    assert!(
        err.message().contains("may not open calculatorapp.exe"),
        "{err}"
    );
    assert!(kit.fake.launches().is_empty());
}

#[test]
fn open_app_repairs_a_path_whose_backslashes_became_control_characters() {
    let kit = Kit::new();
    kit.fake.set_launch_hook(|fake, _| {
        fake.add_window(WindowInfo::basic(
            "Tool",
            "tool.exe",
            9,
            Rect::from_xywh(0, 0, 9, 9),
        ));
    });
    // "C:\tools\bin\tool.exe" sent with single backslashes arrives as C:<TAB>ools<BS>in<TAB>ool.exe
    let (note, _) = kit
        .step(json!({"action": "open_app", "text": "C:\tools\u{8}in\\tool.exe"}))
        .unwrap();
    assert!(note.starts_with("ok focused w0 \"Tool\" pid:9"), "{note}");
    assert_eq!(
        kit.fake.launches()[0].target,
        AppTarget::Executable("C:\\tools\\bin\\tool.exe".into())
    );
}

// ---------------------------------------------------------------------------- file_dialog

const FIELD: &str = "filename";

/// A Save As dialog in front with its "File name" box holding an old name. Pasting into the box
/// changes its value; Enter closes the dialog.
fn dialog_kit(
    on_enter: impl Fn(&FakeBackend, WindowRef) + Send + Sync + 'static,
) -> (Kit, WindowRef) {
    let kit = Kit::new();
    let dlg = kit.front("Save As", "notepad.exe", 7);
    kit.fake.update_window(dlg, |i| i.class = "#32770".into());
    kit.fake.set_tree(
        dlg,
        Element::new("root", Role::Window, "Save As").with_child(
            Element::new(FIELD, Role::Edit, "File name:")
                .at(Rect::new(10, 10, 100, 30))
                .valued("old.txt"),
        ),
    );
    kit.fake.set_file_dialog_field(dlg, ElementId(FIELD.into()));
    kit.fake.set_chord_hook(move |fake, keys| {
        if keys == [cmd_key(), Key::Char('v')] {
            let text = crate::backend::Backend::clipboard_text(fake).unwrap();
            fake.set_element_value(&ElementId(FIELD.into()), &text);
        }
        if keys == [Key::Enter] {
            on_enter(fake, dlg);
        }
    });
    (kit, dlg)
}

// Ported from the happy path of _file_dialog (2026-09-21 overnight evals).
#[test]
fn file_dialog_fills_the_box_and_confirms() {
    let (kit, dlg) = dialog_kit(|fake, dlg| fake.close_window(dlg));
    kit.fake
        .set_clipboard(ClipboardContent::Text("mine".into()));
    let (note, _) = kit
        .step(json!({"action": "file_dialog", "text": "C:/x/y.txt"}))
        .unwrap();
    // the dialog rejects "C:/x/y.txt" as an invalid name, so the slashes are turned round
    assert_eq!(note, "file dialog: entered C:\\x\\y.txt and confirmed");
    assert!(
        kit.fake.performed().iter().any(|(id, _)| id.0 == FIELD),
        "the box was focused"
    );
    use crate::backend::Backend;
    assert_eq!(
        kit.fake.clipboard_text().unwrap(),
        "mine",
        "the owner's clipboard comes back"
    );
    assert!(!kit.fake.window_exists(dlg));
}

#[test]
fn file_dialog_reports_a_question_the_dialog_asks_and_never_answers_it() {
    let (kit, _dlg) = dialog_kit(|fake, _| {
        let popup = fake.add_window(WindowInfo {
            class: "#32770".into(),
            ..WindowInfo::basic(
                "Confirm Save As",
                "notepad.exe",
                7,
                Rect::from_xywh(0, 0, 40, 40),
            )
        });
        fake.set_window_text(
            popup,
            "Confirm Save As / y.txt already exists. Do you want to replace it?",
        );
        fake.set_foreground(Some(popup));
    });
    let (note, _) = kit
        .step(json!({"action": "file_dialog", "text": "C:\\x\\y.txt"}))
        .unwrap();
    assert_eq!(
        note,
        "file dialog: entered C:\\x\\y.txt, but a popup asks: 'Confirm Save As / y.txt already exists. Do you want to replace it?'. Nothing was answered; decide and click it yourself"
    );
}

#[test]
fn file_dialog_says_when_the_dialog_is_still_open() {
    let (kit, _dlg) = dialog_kit(|_, _| {});
    let (note, _) = kit
        .step(json!({"action": "file_dialog", "text": "C:\\x\\y.txt"}))
        .unwrap();
    assert_eq!(
        note,
        "file dialog: entered C:\\x\\y.txt and pressed Enter, but a dialog is still open (maybe 'already exists, replace?' or an invalid name): look at the screenshot"
    );
}

#[test]
fn file_dialog_does_not_press_enter_on_a_box_that_did_not_take_the_path() {
    let kit = Kit::new();
    let dlg = kit.front("Save As", "notepad.exe", 7);
    kit.fake.set_tree(
        dlg,
        Element::new("root", Role::Window, "Save As").with_child(
            Element::new(FIELD, Role::Edit, "File name:")
                .at(Rect::new(10, 10, 100, 30))
                .valued("old.txt"),
        ),
    );
    kit.fake.set_file_dialog_field(dlg, ElementId(FIELD.into()));
    let (note, _) = kit
        .step(json!({"action": "file_dialog", "text": "C:\\x\\y.txt"}))
        .unwrap();
    assert_eq!(
        note,
        "file dialog: could not get 'C:\\\\x\\\\y.txt' into the File name box (it shows 'old.txt'); nothing was confirmed"
    );
    assert!(
        !kit.fake
            .events()
            .contains(&crate::backend::fake::InputEvent::KeyDown(Key::Enter)),
        "Enter would have asked to overwrite the OLD file"
    );
    // three tries: two pastes, then real keystrokes
    assert_eq!(kit.fake.typed_text(), "C:\\x\\y.txt");
}

// ---- the macOS Open panel has no File name box: file_dialog goes through Go to the folder

#[cfg(not(windows))]
const GOTO: &str = "goto";
#[cfg(not(windows))]
const PANEL_PATH: &str = "/Users/you/Documents/hekaton-test/open-me.txt";

#[cfg(not(windows))]
fn panel_root(with_sheet: bool) -> Element {
    let root = Element::new("root", Role::Window, "Open");
    if with_sheet {
        root.with_child(
            Element::new(GOTO, Role::Edit, "Go to the folder:")
                .at(Rect::new(10, 10, 200, 30))
                .valued(""),
        )
    } else {
        root
    }
}

/// An Open panel in front with no name box. cmd+shift+g opens the Go to the folder sheet; the first
/// Return closes the sheet (when `sheet_closes`), the second runs `on_open`.
#[cfg(not(windows))]
fn open_panel_kit(
    sheet_closes: bool,
    on_open: impl Fn(&FakeBackend, WindowRef) + Send + Sync + 'static,
) -> (
    Kit,
    WindowRef,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let kit = Kit::new();
    let dlg = kit.front("Open", "TextEdit", 7);
    kit.fake.set_tree(dlg, panel_root(false));
    let enters = std::sync::Arc::new(AtomicUsize::new(0));
    let count = enters.clone();
    kit.fake.set_chord_hook(move |fake, keys| {
        if keys.last() == Some(&Key::Char('g')) {
            fake.set_tree(dlg, panel_root(true));
            fake.set_file_dialog_field(dlg, ElementId(GOTO.into()));
        }
        if keys == [cmd_key(), Key::Char('v')] {
            let text = crate::backend::Backend::clipboard_text(fake).unwrap();
            fake.set_element_value(&ElementId(GOTO.into()), &text);
        }
        if keys == [Key::Enter] {
            let n = count.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 && sheet_closes {
                fake.set_tree(dlg, panel_root(false));
            } else if n == 2 {
                on_open(fake, dlg);
            }
        }
    });
    (kit, dlg, enters)
}

#[cfg(not(windows))]
#[test]
fn file_dialog_on_the_mac_open_panel_goes_to_the_folder_and_opens_the_file() {
    use std::sync::atomic::Ordering;
    let (kit, dlg, enters) = open_panel_kit(true, |fake, dlg| fake.close_window(dlg));
    kit.fake
        .set_clipboard(ClipboardContent::Text("mine".into()));
    let (note, _) = kit
        .step(json!({"action": "file_dialog", "text": PANEL_PATH}))
        .unwrap();
    assert_eq!(
        note,
        format!("file dialog: entered {PANEL_PATH} and confirmed")
    );
    assert_eq!(enters.load(Ordering::SeqCst), 2, "go there, then Open");
    assert!(!kit.fake.window_exists(dlg));
    use crate::backend::Backend;
    assert_eq!(kit.fake.clipboard_text().unwrap(), "mine");
    assert!(
        kit.clock.now() < std::time::Duration::from_secs(3),
        "it did not wait 3 s for a name box that never comes"
    );
}

#[cfg(not(windows))]
#[test]
fn file_dialog_on_the_mac_open_panel_says_when_go_to_the_folder_refuses_the_path() {
    use std::sync::atomic::Ordering;
    let (kit, dlg, enters) = open_panel_kit(false, |fake, dlg| fake.close_window(dlg));
    let (note, _) = kit
        .step(json!({"action": "file_dialog", "text": PANEL_PATH}))
        .unwrap();
    assert!(note.contains("did not accept"), "{note}");
    assert_eq!(enters.load(Ordering::SeqCst), 1, "no second Return");
    assert!(kit.fake.window_exists(dlg));
}

// If the first Return already opened the file, a second one would land in the document just opened.
#[cfg(not(windows))]
#[test]
fn file_dialog_on_the_mac_open_panel_does_not_press_return_into_the_app_behind() {
    use std::sync::atomic::Ordering;
    let (kit, dlg, enters) = open_panel_kit(true, |_, _| {});
    // the panel is gone as soon as the sheet closes
    let count = enters.clone();
    kit.fake.set_chord_hook(move |fake, keys| {
        if keys.last() == Some(&Key::Char('g')) {
            fake.set_tree(dlg, panel_root(true));
            fake.set_file_dialog_field(dlg, ElementId(GOTO.into()));
        }
        if keys == [cmd_key(), Key::Char('v')] {
            let text = crate::backend::Backend::clipboard_text(fake).unwrap();
            fake.set_element_value(&ElementId(GOTO.into()), &text);
        }
        if keys == [Key::Enter] {
            count.fetch_add(1, Ordering::SeqCst);
            fake.set_tree(dlg, panel_root(false));
            fake.close_window(dlg);
        }
    });
    let (note, _) = kit
        .step(json!({"action": "file_dialog", "text": PANEL_PATH}))
        .unwrap();
    assert_eq!(
        note,
        format!("file dialog: entered {PANEL_PATH} and confirmed")
    );
    assert_eq!(enters.load(Ordering::SeqCst), 1);
}

#[cfg(not(windows))]
#[test]
fn file_dialog_on_the_mac_open_panel_needs_a_full_path_and_honours_the_allowlist() {
    let (kit, _dlg, enters) = open_panel_kit(true, |_, _| {});
    let err = kit
        .step(json!({"action": "file_dialog", "text": "open-me.txt"}))
        .unwrap_err();
    assert!(err.message().contains("needs the full path"), "{err}");
    let err = kit
        .step_only(
            &["calc"],
            json!({"action": "file_dialog", "text": PANEL_PATH}),
        )
        .unwrap_err();
    assert!(
        err.message().starts_with("refused: that window belongs to"),
        "{err}"
    );
    assert_eq!(enters.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn file_dialog_needs_a_dialog_a_path_and_a_safe_place() {
    let kit = Kit::new();
    kit.front("Notepad", "notepad.exe", 7);
    let err = kit
        .step(json!({"action": "file_dialog", "text": "C:\\x\\y.txt"}))
        .unwrap_err();
    // the hint names the keys of the OS it runs on
    let keys = if cfg!(windows) {
        "ctrl+shift+s or ctrl+o"
    } else {
        "cmd+shift+s or cmd+o"
    };
    assert_eq!(
        err.message(),
        format!(
            "no Save As / Open dialog with a 'File name' box is in front; open it first (e.g. {keys})"
        )
    );
    assert!(matches!(err, StepError::Target(_)));
    assert!(
        kit.clock.now() >= std::time::Duration::from_secs(3),
        "it looked for 3 s"
    );
    assert_eq!(
        kit.step(json!({"action": "file_dialog"}))
            .unwrap_err()
            .message(),
        "file_dialog needs text: the full file path"
    );
    // the owner's hard line: no saving into the Windows folder, with or without a dialog
    let err = kit
        .step(json!({"action": "file_dialog", "text": "C:\\Windows\\System32\\config\\SAM"}))
        .unwrap_err();
    assert!(matches!(err, StepError::Permission(_)));
    assert!(err.message().contains("the file dialog path"));
    assert!(kit.clock.now() < std::time::Duration::from_secs(4));
}

#[test]
fn file_dialog_checks_the_allowlist_against_the_dialog_itself() {
    // 2026-09-21: under only_apps, the dialog itself was never checked
    let (kit, _dlg) = dialog_kit(|fake, dlg| fake.close_window(dlg));
    let err = kit
        .step_only(
            &["calc.exe"],
            json!({"action": "file_dialog", "text": "C:\\x\\y.txt"}),
        )
        .unwrap_err();
    assert!(
        err.message()
            .starts_with("refused: that window belongs to notepad.exe"),
        "{err}"
    );
    assert!(kit.fake.events().is_empty());
}

// ---------------------------------------------------------------------------- a blank document, verified

/// A running Notepad showing `title` and `text`, whose new-document shortcut opens a blank tab: it
/// retitles the window and empties the document.
fn notepad_with_new_tab(kit: &Kit, title: &str, text: &str) -> WindowRef {
    let w = app_window(kit, title, "notepad.exe", 30);
    kit.fake.set_document(w, text);
    kit.fake.set_chord_hook(move |fake, keys| {
        if keys == [cmd_key(), Key::Char('n')] {
            fake.update_window(w, |i| i.title = "Untitled - Notepad".into());
            fake.set_document(w, "");
        }
    });
    w
}

fn new_doc_presses(kit: &Kit) -> usize {
    let n = Key::Char('n');
    kit.fake
        .events()
        .iter()
        .filter(|e| matches!(e, crate::backend::fake::InputEvent::KeyDown(k) if *k == n))
        .count()
}

#[test]
fn only_a_new_unsaved_document_with_nothing_in_it_is_blank() {
    let kit = Kit::new();
    let cases = [
        ("Untitled - Notepad", Some(""), true),
        ("Untitled - Notepad", None, true),
        ("Untitled - Notepad", Some("  \n"), true),
        ("New 1 - Notepad++", Some(""), true),
        ("Document - WordPad", Some(""), true),
        // edited since it was new: the star
        ("*Untitled - Notepad", Some(""), false),
        // an empty file that already has a name: typing would change that file
        ("todo.txt - Notepad", Some(""), false),
        ("Untitled - Notepad", Some("hello"), false),
    ];
    for (title, text, blank) in cases {
        let w = app_window(&kit, title, "notepad.exe", 30);
        if let Some(t) = text {
            kit.fake.set_document(w, t);
        }
        assert_eq!(
            kit.ctx(json!({})).is_blank_document(w),
            blank,
            "{title} {text:?}"
        );
        kit.fake.close_window(w);
    }
}

// The `note` recipe's first step used to press Ctrl+N and look for "Untitled" in the title. Now open_app
// checks the document itself.
#[test]
fn blank_confirms_a_document_that_is_already_empty_and_unsaved() {
    let kit = Kit::new();
    let w = notepad_with_new_tab(&kit, "Untitled - Notepad", "");
    let (note, _) = kit
        .step(json!({"action": "open_app", "text": "notepad", "blank": true}))
        .unwrap();
    assert!(note.ends_with("; blank document confirmed"), "{note}");
    assert_eq!(kit.fake.foreground_window().unwrap(), Some(w));
    assert!(kit.fake.launches().is_empty());
}

#[test]
fn blank_after_a_new_tab_that_really_is_blank_is_verified() {
    let kit = Kit::new();
    notepad_with_new_tab(&kit, "*todo - Notepad", "secret plans");
    let (note, _) = kit
        .step(json!({"action": "open_app", "text": "notepad", "blank": true}))
        .unwrap();
    assert!(
        note.contains("ok new blank tab") && note.ends_with("; blank document confirmed"),
        "{note}"
    );
    assert_eq!(
        new_doc_presses(&kit),
        1,
        "the one new-document shortcut open_app already sent"
    );
}

#[test]
fn blank_makes_its_own_new_document_when_the_front_one_has_content() {
    let kit = Kit::new();
    let w = notepad_with_new_tab(&kit, "report.txt - Notepad", "old text");
    kit.fake.set_foreground(Some(w));
    let note = kit.ctx(json!({})).ensure_blank_document().unwrap();
    assert_eq!(note, "opened a new blank document (verified)");
    assert_eq!(new_doc_presses(&kit), 1);
    // the second call finds it blank and presses nothing more
    assert_eq!(
        kit.ctx(json!({})).ensure_blank_document().unwrap(),
        "blank document confirmed"
    );
    assert_eq!(new_doc_presses(&kit), 1);
}
#[test]
fn blank_refuses_when_no_blank_document_can_be_made() {
    let kit = Kit::new();
    let w = app_window(&kit, "*todo - Notepad", "notepad.exe", 30);
    kit.fake.set_document(w, "secret plans"); // Ctrl+N does nothing in this one
    let err = kit
        .step(json!({"action": "open_app", "text": "notepad", "blank": true}))
        .unwrap_err();
    assert!(matches!(err, StepError::Target(_)), "{err}");
    assert!(
        err.message()
            .starts_with("refused: no blank document could be made"),
        "{err}"
    );
    assert!(err.message().contains("nothing was typed"), "{err}");
    assert_eq!(kit.fake.typed_text(), "");
}

#[test]
fn blank_does_not_count_an_empty_named_file_as_blank() {
    let kit = Kit::new();
    notepad_with_new_tab(&kit, "todo.txt - Notepad", "");
    let (note, _) = kit
        .step(json!({"action": "open_app", "text": "notepad", "blank": true}))
        .unwrap();
    // the empty named file was left alone; a new document was made
    assert!(
        note.contains("ok new blank tab in") && note.contains("\"Untitled - Notepad\""),
        "{note}"
    );
    assert!(new_doc_presses(&kit) >= 1);
}

#[test]
fn blank_is_checked_against_the_allowlist_and_the_callers_own_window() {
    let mut kit = Kit::new();
    let claude = app_window(&kit, "Claude", "claude.exe", 100);
    kit.fake.set_foreground(Some(claude));
    kit.who = crate::caller::Caller::new(vec![(100, "claude.exe".into())], false);
    let err = kit.ctx(json!({})).ensure_blank_document().unwrap_err();
    assert!(err.message().starts_with("refused:"), "{err}");
    assert!(kit.fake.events().is_empty());
    let kit = Kit::new();
    let mail = app_window(&kit, "Mail", "olk.exe", 8);
    kit.fake.set_foreground(Some(mail));
    let err = kit
        .ctx(json!({"only_apps": ["notepad.exe"]}))
        .ensure_blank_document()
        .unwrap_err();
    assert!(err.message().contains("may not touch"), "{err}");
    let err = Kit::new()
        .ctx(json!({}))
        .ensure_blank_document()
        .unwrap_err();
    assert!(err.message().contains("no window is in front"), "{err}");
}

// `new_window: true` (or `reuse: false`) launches another instance instead of focusing the running one.
#[test]
fn new_window_launches_another_instance_instead_of_focusing_the_running_one() {
    for flag in [
        json!({"new_window": true}),
        json!({"reuse": false}),
        json!({"reuse": "no"}),
    ] {
        let kit = Kit::new();
        app_window(&kit, "job.log - Notepad", "notepad.exe", 30);
        kit.fake.set_launch_hook(|fake, _| {
            fake.add_window(WindowInfo::basic(
                "Untitled - Notepad",
                "notepad.exe",
                31,
                Rect::from_xywh(0, 0, 50, 50),
            ));
        });
        let mut call = json!({"action": "open_app", "text": "notepad"});
        for (k, v) in flag.as_object().unwrap() {
            call[k] = v.clone();
        }
        let (note, _) = kit.step(call).unwrap();
        assert!(
            note.starts_with("ok focused") && note.contains("Untitled - Notepad"),
            "{flag}: {note}"
        );
        assert_eq!(kit.fake.launches().len(), 1, "{flag}");
    }
    // and by default the running one is focused
    let kit = Kit::new();
    app_window(&kit, "job.log - Notepad", "notepad.exe", 30);
    kit.step(json!({"action": "open_app", "text": "notepad"}))
        .unwrap();
    assert!(kit.fake.launches().is_empty());
}

// The `note` recipe end to end: Notepad has restored old tabs; the text goes into a fresh blank one.
#[test]
fn the_note_recipe_types_into_a_fresh_blank_document_never_an_existing_one() {
    let kit = Kit::new();
    notepad_with_new_tab(&kit, "*todo - Notepad", "secret plans");
    let out = kit
        .run(json!({"action": "recipe", "name": "note", "params": {"text": "hi"}}))
        .unwrap();
    assert!(!out.text.contains("failed:"), "{}", out.text);
    assert!(out.text.contains("expect: PASS"), "{}", out.text);
    assert_eq!(kit.fake.typed_text(), "hi", "{}", out.text);
    // the new-document shortcut came before the first typed character
    let events = kit.fake.events();
    let new_tab = events
        .iter()
        .position(|e| matches!(e, crate::backend::fake::InputEvent::KeyDown(Key::Char('n'))))
        .expect("a new document was made");
    let first_char = events
        .iter()
        .position(|e| matches!(e, crate::backend::fake::InputEvent::Type(_)))
        .expect("text was typed");
    assert!(new_tab < first_char);
}

#[test]
fn the_note_recipe_stops_before_typing_when_no_blank_document_appears() {
    let kit = Kit::new();
    let w = app_window(&kit, "*todo - Notepad", "notepad.exe", 30);
    kit.fake.set_document(w, "secret plans");
    let out = kit
        .run(json!({"action": "recipe", "name": "note", "params": {"text": "oops"}}))
        .unwrap();
    assert!(
        out.text
            .contains("step 1 (open_app) failed: refused: no blank document could be made"),
        "{}",
        out.text
    );
    assert_eq!(
        kit.fake.typed_text(),
        "",
        "never typed into an existing document"
    );
}

// 2026-10-28: once Save As has taken focus, file_dialog waits for its own name box, so the opener
// skips the screen settle. Without a focus change it still settles.
#[test]
fn an_opener_before_file_dialog_skips_the_settle_once_focus_moved() {
    let kit = Kit::with_env(crate::env::MapEnv::new().with("HEKATON_AUTO_SHOT", "0"));
    let editor = kit.front("Untitled - Notepad", "notepad.exe", 7);
    let dlg = kit.window("Save As", "notepad.exe", 7, Rect::new(0, 0, 200, 100));
    kit.fake.update_window(dlg, |i| i.class = "#32770".into());
    kit.fake.set_tree(
        dlg,
        Element::new("root", Role::Window, "Save As").with_child(
            Element::new(FIELD, Role::Edit, "File name:")
                .at(Rect::new(10, 10, 100, 30))
                .valued("old.txt"),
        ),
    );
    kit.fake.set_file_dialog_field(dlg, ElementId(FIELD.into()));
    kit.fake.set_foreground(Some(editor));
    kit.fake.set_chord_hook(move |fake, keys| {
        if keys.last() == Some(&Key::Char('s')) {
            fake.set_foreground(Some(dlg));
        }
        if keys == [cmd_key(), Key::Char('v')] {
            let text = crate::backend::Backend::clipboard_text(fake).unwrap();
            fake.set_element_value(&ElementId(FIELD.into()), &text);
        }
        if keys == [Key::Enter] {
            fake.close_window(dlg);
        }
    });
    let out = kit
        .run(json!({"actions": [
            {"action": "key", "text": "ctrl+shift+s"},
            {"action": "file_dialog", "text": "C:/x/y.txt"}
        ], "only_apps": ["notepad.exe"]}))
        .unwrap();
    assert!(
        out.text.contains(r"entered C:\x\y.txt and confirmed"),
        "{}",
        out.text
    );
    assert_eq!(
        kit.fake.capture_count(),
        0,
        "no settle between the chord and the dialog"
    );
}

// 2026-10-28: on macOS a folder opens through /usr/bin/open (Finder), never explorer.exe.
#[cfg(not(windows))]
#[test]
fn open_a_folder_path_goes_to_finder_on_macos() {
    let kit = Kit::new();
    let folder = kit.tmp.path().join("Reports");
    std::fs::create_dir(&folder).unwrap();
    let path = folder.to_string_lossy().to_string();
    let _ = do_open(&kit, &path, 0.2, true, true);
    let launched = kit.fake.launches();
    assert_eq!(
        launched[0].target,
        AppTarget::Executable("/usr/bin/open".into())
    );
    assert_eq!(launched[0].args, format!("\"{path}\""));
}

// 2026-10-28: an app name with a space is one app, not a program plus an argument.
#[test]
fn split_launch_keeps_a_spaced_app_name_whole() {
    let knows = |n: &str| matches!(n, "Microsoft Excel" | "Microsoft" | "notepad");
    let no = |_: &str| false;
    assert_eq!(
        super::open::split_launch("Microsoft Excel", &knows, &no, &no),
        ("Microsoft Excel".to_string(), String::new())
    );
    // a real program plus argument still splits
    assert_eq!(
        super::open::split_launch("notepad notes.txt", &knows, &no, &no),
        ("notepad".to_string(), "notes.txt".to_string())
    );
}

// ---- 2026-11-09: a web address through open_app lands as a tab, with no new window or process.

fn brave_with_title(kit: &Kit, title: &str) -> WindowRef {
    let w = app_window(kit, title, "brave.exe", 77);
    kit.fake
        .update_window(w, |i| i.class = "Chrome_WidgetWin_1".into());
    kit.fake.set_foreground(Some(w));
    w
}

#[test]
fn a_page_launch_that_retitles_a_browser_window_is_a_success() {
    for target in ["brave https://example.com", "brave file:///tmp/a.html"] {
        let kit = Kit::new();
        let w = brave_with_title(&kit, "New Tab - Brave");
        kit.fake.set_app(
            "brave",
            AppTarget::Executable("C:\\Brave\\brave.exe".into()),
        );
        kit.fake.set_launch_hook(move |fake, _| {
            fake.update_window(w, |i| i.title = "Example - Brave".into());
        });
        let out = do_open(&kit, target, 2.0, true, true).unwrap();
        assert!(
            out.contains("opened the page") && out.contains("Example - Brave"),
            "{out}"
        );
    }
}

#[test]
fn a_page_launch_that_changes_nothing_is_still_a_failure() {
    let kit = Kit::new();
    brave_with_title(&kit, "New Tab - Brave");
    kit.fake.set_app(
        "brave",
        AppTarget::Executable("C:\\Brave\\brave.exe".into()),
    );
    let e = do_open(&kit, "brave https://example.com", 0.5, true, true).unwrap_err();
    assert!(e.message().contains("showed no window"), "{e}");
}

#[cfg(not(windows))]
#[test]
fn a_display_name_of_two_words_finds_the_running_bundle() {
    let kit = Kit::new();
    let w = app_window(&kit, "Page", "com.brave.browser", 78);
    kit.fake.set_app(
        "brave browser",
        AppTarget::Shortcut("/Applications/Brave Browser.app".into()),
    );
    let out = do_open(&kit, "Brave Browser", 0.5, true, true).unwrap();
    assert!(out.contains("already running"), "{out}");
    assert_eq!(kit.fake.foreground_window().unwrap(), Some(w));
    assert!(kit.fake.launches().is_empty());
}
