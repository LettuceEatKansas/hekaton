//! End-to-end checks against real apps (Calculator, TextEdit). They launch the apps, click, type,
//! read the accessibility tree and screenshot them, so they are `#[ignore]`d: run them on purpose
//! with `cargo test -p wh-mac --test macos_e2e -- --ignored --nocapture --test-threads=1`.
//!
//! They are informational. Each step prints an `E2E` line with what the machine did and keeps going
//! when a step fails, so one CI run shows how far the whole chain works on a real Mac.
#![cfg(target_os = "macos")]

use std::fmt::{Debug, Display};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use wh_core::backend::{
    AppTarget, Backend, CaptureTarget, Element, ElementAction, ElementFilter, LaunchSpec,
    MouseButton, Point, Role, ScrollDirection, TreeLimits, WindowInfo, WindowRef, no_guard,
};
use wh_core::keys::Key;
use wh_mac::MacBackend;

fn show<T: Debug, E: Display>(label: &str, r: Result<T, E>) -> Option<T> {
    match r {
        Ok(v) => {
            let text = format!("{v:?}");
            let text: String = text.chars().take(360).collect();
            eprintln!("E2E {label:<26} ok   {text}");
            Some(v)
        }
        Err(e) => {
            eprintln!("E2E {label:<26} FAIL {e}");
            None
        }
    }
}

fn launch(b: &MacBackend, name: &str) -> bool {
    let Some(Some(target)) = show("resolve_app", b.resolve_app(name)) else {
        return false;
    };
    show(
        "launch",
        b.launch(&LaunchSpec {
            target,
            args: String::new(),
            deelevate: false,
        }),
    )
    .is_some()
}

fn wait_window(b: &MacBackend, app: &str, secs: u64) -> Option<WindowInfo> {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if let Ok(list) = b.list_windows(false)
            && let Some(w) = list.into_iter().find(|w| w.app.eq_ignore_ascii_case(app))
        {
            return Some(w);
        }
        sleep(Duration::from_millis(400));
    }
    eprintln!("E2E wait_window({app}) gave up after {secs}s");
    None
}

fn find<'a>(root: &'a Element, pred: &dyn Fn(&Element) -> bool) -> Option<&'a Element> {
    if pred(root) {
        return Some(root);
    }
    root.children.iter().find_map(|c| find(c, pred))
}

fn texts(root: &Element) -> Vec<String> {
    fn go(e: &Element, out: &mut Vec<String>) {
        if e.role == Role::Text && !e.name.is_empty() {
            out.push(e.name.clone());
        }
        for c in &e.children {
            go(c, out);
        }
    }
    let mut all = Vec::new();
    go(root, &mut all);
    all
}

fn quit(name: &str) {
    let _ = Command::new("killall").arg(name).status();
    sleep(Duration::from_millis(500));
}

fn report_tree(b: &MacBackend, w: WindowRef, label: &str) -> Option<Element> {
    let snap = show(
        &format!("{label} tree(interactive)"),
        b.element_tree(w, &TreeLimits::snapshot(300)).map(|s| {
            let n = s.root.children.len();
            (n, s.truncated, s.wake_hint, s.title.clone())
        }),
    );
    let _ = snap;
    let full = b
        .element_tree(
            w,
            &TreeLimits {
                filter: ElementFilter::All,
                flatten: false,
                ..TreeLimits::snapshot(2000)
            },
        )
        .ok()?;
    eprintln!("E2E {label} full tree: {} elements", full.root.count());
    let mut flat = Vec::new();
    full.root.walk(&mut flat);
    for e in flat.iter().take(40) {
        eprintln!(
            "E2E   {:<12} {:<22} {:?} value={:?} rect={:?}",
            e.role.short_name(),
            e.native_role,
            e.name,
            e.value,
            e.rect
        );
    }
    Some(full.root)
}

#[test]
#[ignore = "launches Calculator and clicks in it"]
fn calculator_end_to_end() {
    let b = MacBackend::new();
    b.init().unwrap();
    eprintln!("E2E permissions: {:?}", b.permissions().unwrap());
    quit("Calculator");
    if !launch(&b, "Calculator") {
        return;
    }
    let Some(win) = wait_window(&b, "com.apple.calculator", 25) else {
        eprintln!(
            "E2E windows now: {:?}",
            b.list_windows(true).map(|l| l.len())
        );
        return;
    };
    eprintln!("E2E window: {win:?}");
    show("window_info", b.window_info(win.window));
    show("bring_to_foreground", b.bring_to_foreground(win.window));
    show("foreground_window", b.foreground_window());
    show("root_window", b.root_window(win.window));
    let center = win.rect.center();
    for line in b.debug_windows_at(center) {
        eprintln!("E2E   at center: {line}");
    }
    let hit = show("window_at_point(center)", b.window_at_point(center));
    eprintln!(
        "E2E hit test lands on the Calculator window: {}",
        hit == Some(Some(win.window))
    );
    show(
        "visible_fraction",
        b.visible_fraction(win.window, win.rect, 8),
    );
    show("window_text", b.window_text(win.window));
    show(
        "wait_window_ready",
        b.wait_window_ready(win.window, Duration::from_secs(2)),
    );

    let Some(root) = report_tree(&b, win.window, "calc") else {
        return;
    };
    eprintln!("E2E display texts before: {:?}", texts(&root));

    // Press 7 through the accessibility action, then read the display back.
    let seven = find(&root, &|e| e.role == Role::Button && e.name == "7");
    if let Some(seven) = seven {
        show(
            "element(7)",
            b.element(&seven.id).map(|e| (e.name, e.rect, e.enabled)),
        );
        show("element_window(7)", b.element_window(&seven.id));
        show(
            "perform(Press 7)",
            b.perform(&seven.id, ElementAction::Press),
        );
        sleep(Duration::from_millis(400));
        if let Ok(snap) = b.element_tree(win.window, &TreeLimits::named(Duration::from_secs(2))) {
            eprintln!("E2E display after AX press of 7: {:?}", texts(&snap.root));
        }
    } else {
        eprintln!("E2E no button named 7 in the tree");
    }

    // Click 5 and 3 with real mouse events at their on-screen centres.
    for digit in ["5", "3"] {
        if let Some(btn) = find(&root, &|e| e.role == Role::Button && e.name == digit)
            && let Some(r) = btn.rect
        {
            show(
                &format!("mouse_click({digit})"),
                b.mouse_click(r.center(), MouseButton::Left, 1, &no_guard),
            );
            sleep(Duration::from_millis(300));
            show(
                &format!("element_at({digit})"),
                b.element_at(r.center(), 2, Duration::from_millis(800))
                    .map(|h| h.map(|h| (h.element.role, h.element.name, h.ancestors.len()))),
            );
        } else {
            eprintln!("E2E no clickable button {digit}");
        }
    }
    if let Ok(snap) = b.element_tree(win.window, &TreeLimits::named(Duration::from_secs(2))) {
        eprintln!("E2E display after clicks: {:?}", texts(&snap.root));
    }

    // Typed keys.
    show(
        "key_chord(clear)",
        b.key_chord(&[Key::Escape], 1, &no_guard),
    );
    show("type_text(8*9=)", b.type_text("8*9=", &no_guard));
    sleep(Duration::from_millis(400));
    if let Ok(snap) = b.element_tree(win.window, &TreeLimits::named(Duration::from_secs(2))) {
        eprintln!("E2E display after typing: {:?}", texts(&snap.root));
    }
    show("key_chord(esc)", b.key_chord(&[Key::Escape], 1, &no_guard));
    show(
        "focused_element",
        b.focused_element().map(|e| e.map(|e| (e.role, e.name))),
    );
    eprintln!("E2E focus: {}", b.debug_focus());
    show("focus_token", b.focus_token());

    // Pixels.
    let shot = show(
        "capture(Window)",
        b.capture(CaptureTarget::Window(win.window))
            .map(|i| (i.width(), i.height(), i.scale(), i.origin())),
    );
    let _ = shot;
    show(
        "capture(Region)",
        b.capture(CaptureTarget::Region(win.rect))
            .map(|i| (i.width(), i.height(), i.scale(), i.origin())),
    );
    show(
        "ocr_region(window)",
        b.ocr_region(win.rect).map(|(r, origin, scale)| {
            (
                r.lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>(),
                origin,
                scale,
            )
        }),
    );
    show("menu_items", b.menu_items(Duration::from_millis(500)));
    show("popup_menus", b.popup_menus());
    show("clipboard", b.clipboard());
    quit("Calculator");
}

#[test]
#[ignore = "launches TextEdit and types into it"]
fn textedit_end_to_end() {
    let b = MacBackend::new();
    b.init().unwrap();
    quit("TextEdit");
    if !launch(&b, "TextEdit") {
        return;
    }
    // TextEdit opens a file chooser first when it has nothing to restore.
    let first = wait_window(&b, "com.apple.TextEdit", 25);
    eprintln!("E2E first window: {first:?}");
    if let Some(f) = &first {
        show("dialog text", b.window_text(f.window));
        show("file dialog field", b.find_file_dialog_field(f.window));
    }
    sleep(Duration::from_millis(800));
    show(
        "cmd+N",
        b.key_chord(&[Key::Meta, Key::Char('n')], 1, &no_guard),
    );
    sleep(Duration::from_millis(1500));
    let wins = b.list_windows(false).unwrap_or_default();
    eprintln!(
        "E2E TextEdit windows: {:?}",
        wins.iter()
            .filter(|w| w.app == "com.apple.TextEdit")
            .map(|w| (&w.title, w.kind, w.foreground, w.rect))
            .collect::<Vec<_>>()
    );
    let Some(doc) = wins
        .into_iter()
        .find(|w| w.app == "com.apple.TextEdit" && w.title.starts_with("Untitled"))
    else {
        eprintln!("E2E no Untitled document window");
        quit("TextEdit");
        return;
    };
    show("bring_to_foreground", b.bring_to_foreground(doc.window));
    let Some(root) = report_tree(&b, doc.window, "textedit") else {
        quit("TextEdit");
        return;
    };
    let area = find(&root, &|e| e.native_role == "AXTextArea");
    eprintln!(
        "E2E text area: {:?}",
        area.map(|a| (&a.id, a.rect, a.focused))
    );

    show("has_caret", b.has_caret(Some(doc.window)));
    show("type_text(short)", b.type_text("hello winhands", &no_guard));
    show(
        "type_text(newline)",
        b.type_text("\nsecond line é😀", &no_guard),
    );
    sleep(Duration::from_millis(500));
    show("document_text", b.document_text(doc.window, 10_000));
    show(
        "find_in_documents(second)",
        b.find_in_documents(doc.window, "SECOND", 5, Duration::from_secs(1)),
    );
    show("caret", b.caret());
    if let Some(a) = area
        && let Some(r) = a.rect
    {
        let p = Point::new(r.left + 40, r.top + 12);
        show("document_line_at(top)", b.document_line_at(doc.window, p));
        show(
            "element_at(top)",
            b.element_at(p, 3, Duration::from_millis(800))
                .map(|h| h.map(|h| (h.element.role, h.ancestors.len()))),
        );
    }

    // Select all and replace through the clipboard path (text longer than the chunk limit).
    show(
        "cmd+A",
        b.key_chord(&[Key::Meta, Key::Char('a')], 1, &no_guard),
    );
    let long: String = "paste-path ".repeat(60);
    show("clipboard before", b.set_clipboard_text("owner clipboard"));
    show("type_text(long)", b.type_text(&long, &no_guard));
    sleep(Duration::from_millis(600));
    let got = b
        .document_text(doc.window, 100_000)
        .ok()
        .flatten()
        .unwrap_or_default();
    eprintln!(
        "E2E long text landed: {} of {} chars, clipboard now {:?}",
        got.chars().count(),
        long.chars().count(),
        b.clipboard_text()
    );

    // Set a value through Accessibility.
    if let Some(a) = area {
        show(
            "perform(SetValue)",
            b.perform(&a.id, ElementAction::SetValue("set by ax".into())),
        );
        sleep(Duration::from_millis(300));
        show("document_text after set", b.document_text(doc.window, 100));
    }

    // Selection and a drag, then pixels with the text visible.
    let r = doc.rect;
    show(
        "mouse_drag",
        b.mouse_drag(
            Point::new(r.left + 30, r.top + 80),
            Point::new(r.left + 160, r.top + 80),
            &no_guard,
        ),
    );
    show(
        "mouse_scroll",
        b.mouse_scroll(r.center(), ScrollDirection::Down, 2, &no_guard),
    );
    show(
        "ocr_region(doc)",
        b.ocr_region(r)
            .map(|(res, _, _)| res.lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>()),
    );
    show(
        "restore_window(noop)",
        b.restore_window(doc.window, Duration::from_secs(2)),
    );

    // Context menu.
    if let Some(a) = area
        && let Some(r) = a.rect
    {
        show(
            "right click",
            b.mouse_click(
                Point::new(r.left + 100, r.top + 60),
                MouseButton::Right,
                1,
                &no_guard,
            ),
        );
        sleep(Duration::from_millis(700));
        show("popup_menus", b.popup_menus());
        show(
            "menu_items",
            b.menu_items(Duration::from_secs(1))
                .map(|v| v.into_iter().map(|m| m.name).collect::<Vec<_>>()),
        );
        show("esc", b.key_chord(&[Key::Escape], 1, &no_guard));
        sleep(Duration::from_millis(300));
    }

    // Minimise with Cmd+M, find it still listed, bring it back.
    show(
        "cmd+M",
        b.key_chord(&[Key::Meta, Key::Char('m')], 1, &no_guard),
    );
    sleep(Duration::from_millis(1200));
    show(
        "minimised window_info",
        b.window_info(doc.window)
            .map(|i| (i.minimized, i.hidden, i.foreground)),
    );
    show(
        "listed while minimised",
        b.list_windows(false).map(|l| {
            l.iter()
                .filter(|w| w.window == doc.window)
                .map(|w| w.minimized)
                .collect::<Vec<_>>()
        }),
    );
    show(
        "restore_window",
        b.restore_window(doc.window, Duration::from_secs(3)),
    );
    show(
        "restored window_info",
        b.window_info(doc.window)
            .map(|i| (i.minimized, i.hidden, i.foreground)),
    );
    show("bring_to_foreground", b.bring_to_foreground(doc.window));
    if let Some(AppTarget::Shortcut(p)) = b.resolve_app("TextEdit").ok().flatten() {
        eprintln!("E2E TextEdit bundle: {p}");
    }
    quit("TextEdit");
}
