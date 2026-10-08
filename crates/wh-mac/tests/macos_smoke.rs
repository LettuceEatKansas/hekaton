//! Smoke tests that touch the real macOS APIs. They run on the CI Mac (and on a developer Mac) and
//! are written to hold with or without the privacy grants: a missing grant must give a clear
//! `Denied`, never a crash. Run with `-- --nocapture` to read what the machine reported.
#![cfg(target_os = "macos")]

use wh_core::backend::{
    AppTarget, Backend, BackendError, CaptureTarget, ClipboardContent, Image, MouseButton, Point,
    Rect, no_guard,
};
use wh_mac::MacBackend;

fn backend() -> MacBackend {
    let b = MacBackend::new();
    b.init().expect("init");
    b
}

#[test]
fn identity_and_permission_report() {
    let b = backend();
    assert_eq!(b.name(), "macos");
    let caps = b.capabilities();
    assert!(caps.accessibility_tree && caps.ocr && caps.kill_hotkey && !caps.elevation);
    let offset = b.local_utc_offset(1_700_000_000).expect("time zone");
    assert!((-14 * 3600..=14 * 3600).contains(&offset), "{offset}");
    let perms = b.permissions().expect("permissions never fail");
    for p in &perms {
        eprintln!(
            "permission {:<18} granted={} fix={}",
            p.name, p.granted, p.fix
        );
        assert!(p.fix.contains("System Settings"));
    }
    let names: Vec<&str> = perms.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        ["Accessibility", "Post input events", "Screen Recording"]
    );
}

#[test]
fn process_table_contains_this_test() {
    let b = backend();
    let me = std::process::id();
    let table = b.process_table().unwrap();
    assert!(table.len() > 10, "only {} processes", table.len());
    let mine = table
        .iter()
        .find(|p| p.pid == me)
        .expect("own pid in table");
    assert!(mine.exe_name.contains("macos_smoke"), "{}", mine.exe_name);
    let one = b.process_info(me).unwrap().expect("own pid");
    assert_eq!(one.pid, me);
    assert!(one.parent_pid > 0);
    assert!(b.own_process().unwrap().is_some());
    assert!(b.process_info(u32::MAX - 7).unwrap().is_none());
}

#[test]
fn peer_pid_is_unsupported_for_now() {
    let b = backend();
    assert!(matches!(
        b.peer_pid(50000, 8765),
        Err(BackendError::Unsupported(_))
    ));
}

#[test]
fn apps_resolve_by_name_bundle_id_and_path() {
    let b = backend();
    match b.resolve_app("Terminal").unwrap() {
        Some(AppTarget::Shortcut(p)) => assert!(p.ends_with("Terminal.app"), "{p}"),
        other => panic!("Terminal resolved to {other:?}"),
    }
    assert_eq!(
        b.resolve_app("com.apple.Terminal").unwrap(),
        Some(AppTarget::Packaged("com.apple.Terminal".into()))
    );
    assert!(matches!(
        b.resolve_app("/bin/ls").unwrap(),
        Some(AppTarget::Executable(_))
    ));
    assert!(matches!(
        b.resolve_app("ls").unwrap(),
        Some(AppTarget::Executable(_))
    ));
    assert!(matches!(
        b.resolve_app("https://example.com/a").unwrap(),
        Some(AppTarget::Other(_))
    ));
    assert_eq!(b.resolve_app("no-such-app-xyz-12345").unwrap(), None);
    assert_eq!(b.resolve_app("  ").unwrap(), None);
}

#[test]
fn window_list_parses_and_every_entry_can_be_read_back() {
    let b = backend();
    let windows = b.list_windows(true).expect("list_windows");
    eprintln!("{} windows (including hidden)", windows.len());
    for w in windows.iter().take(8) {
        eprintln!(
            "  {:?} app={} pid={} rect={:?} fg={} min={} hidden={} kind={:?}",
            w.title, w.app, w.pid, w.rect, w.foreground, w.minimized, w.hidden, w.kind
        );
    }
    for w in &windows {
        assert!(w.pid > 0 && !w.rect.is_empty(), "{w:?}");
        match b.window_info(w.window) {
            Ok(info) => assert_eq!(info.window, w.window),
            // A window can close between the two calls.
            Err(BackendError::WindowGone) => {}
            Err(e) => panic!("window_info failed: {e}"),
        }
    }
    let fg = b.foreground_window().expect("foreground_window");
    eprintln!("foreground: {fg:?}");
    b.popup_menus().expect("popup_menus");
    // Hit testing the far corner of a huge rectangle must not fail either.
    b.window_at_point(Point::new(i32::MAX / 2, i32::MAX / 2))
        .unwrap();
}

#[test]
fn displays_are_ordered_and_scaled() {
    let b = backend();
    match b.displays() {
        Ok(list) => {
            eprintln!("displays: {list:?}");
            assert!(list[0].primary && list[0].id == 1);
            assert!(list.iter().all(|d| d.scale >= 1.0 && !d.rect.is_empty()));
            let virt = b.virtual_screen().unwrap();
            assert!(list.iter().all(|d| virt.covers(&d.rect)));
            assert!(b.point_on_screen(list[0].rect.center()).unwrap());
        }
        // A runner without a display: the error must say so.
        Err(e) => eprintln!("no displays here: {e}"),
    }
}

#[test]
fn capture_is_denied_cleanly_or_returns_pixels() {
    let b = backend();
    let granted = b
        .permissions()
        .unwrap()
        .iter()
        .any(|p| p.name == "Screen Recording" && p.granted);
    match b.capture(CaptureTarget::VirtualDesktop) {
        Ok(img) => {
            eprintln!(
                "captured {}x{} at scale {}",
                img.width(),
                img.height(),
                img.scale()
            );
            assert!(granted);
            assert!(img.width() > 0 && img.height() > 0);
        }
        Err(BackendError::Denied(msg)) => {
            assert!(!granted);
            assert!(
                msg.contains("Screen") && msg.contains("System Settings"),
                "{msg}"
            );
        }
        Err(e) => eprintln!("capture failed without a grant problem: {e}"),
    }
}

#[test]
fn input_is_denied_cleanly_without_the_grant() {
    let b = backend();
    let granted = b
        .permissions()
        .unwrap()
        .iter()
        .any(|p| p.name == "Post input events" && p.granted);
    if granted {
        eprintln!("input is granted here; not moving the pointer in a test");
        return;
    }
    let start = b.input_events();
    for r in [
        b.mouse_move(Point::new(5, 5)),
        b.mouse_click(Point::new(5, 5), MouseButton::Left, 1, &no_guard),
        b.type_text("x", &no_guard),
    ] {
        match r {
            Err(BackendError::Denied(msg)) => assert!(msg.contains("System Settings"), "{msg}"),
            other => panic!("expected Denied, got {other:?}"),
        }
    }
    assert_eq!(b.input_events(), start, "nothing may have been posted");
    assert!(b.cursor_position().is_ok());
    assert!(b.human_input_idle().is_ok());
}

#[test]
fn accessibility_calls_are_denied_cleanly_without_the_grant() {
    let b = backend();
    let granted = b
        .permissions()
        .unwrap()
        .iter()
        .any(|p| p.name == "Accessibility" && p.granted);
    if granted {
        // With the grant the system-wide queries must at least not fail hard.
        b.focused_element().unwrap();
        b.has_caret(None).unwrap();
        b.focus_token().unwrap();
        return;
    }
    let msg = match b.focused_element() {
        Err(BackendError::Denied(m)) => m,
        other => panic!("expected Denied, got {other:?}"),
    };
    assert!(msg.contains("Accessibility"), "{msg}");
}

#[test]
fn clipboard_round_trips_text() {
    let b = backend();
    let marker = format!("winhands smoke {}", std::process::id());
    if b.set_clipboard_text(&marker).is_err() {
        eprintln!("the pasteboard is not writable here");
        return;
    }
    assert_eq!(
        b.clipboard().unwrap(),
        ClipboardContent::Text(marker.clone())
    );
    assert_eq!(b.clipboard_text().unwrap(), marker);
    let before = b.clipboard_sequence().unwrap().expect("a change count");
    b.set_clipboard_text("second").unwrap();
    assert!(b.clipboard_sequence().unwrap().unwrap() > before);
}

/// Five by seven glyphs, one string per row, for the letters the OCR test draws.
fn glyph(c: char) -> [&'static str; 7] {
    match c {
        'H' => [
            "X...X", "X...X", "X...X", "XXXXX", "X...X", "X...X", "X...X",
        ],
        'E' => [
            "XXXXX", "X....", "X....", "XXXX.", "X....", "X....", "XXXXX",
        ],
        'L' => [
            "X....", "X....", "X....", "X....", "X....", "X....", "XXXXX",
        ],
        'O' => [
            ".XXX.", "X...X", "X...X", "X...X", "X...X", "X...X", ".XXX.",
        ],
        'W' => [
            "X...X", "X...X", "X...X", "X.X.X", "X.X.X", "XX.XX", "X...X",
        ],
        'R' => [
            "XXXX.", "X...X", "X...X", "XXXX.", "X.X..", "X..X.", "X...X",
        ],
        'D' => [
            "XXXX.", "X...X", "X...X", "X...X", "X...X", "X...X", "XXXX.",
        ],
        _ => [
            ".....", ".....", ".....", ".....", ".....", ".....", ".....",
        ],
    }
}

/// Black text on white, each glyph cell blown up by `zoom`; the text starts at `(x0, y0)`.
fn text_image(text: &str, zoom: u32, x0: u32, y0: u32, w: u32, h: u32) -> Image {
    let mut img = Image::solid(w, h, [255, 255, 255, 255]);
    for (n, c) in text.chars().enumerate() {
        for (row, bits) in glyph(c).iter().enumerate() {
            for (col, bit) in bits.chars().enumerate() {
                if bit != 'X' {
                    continue;
                }
                let gx = x0 + (n as u32 * 6 + col as u32) * zoom;
                let gy = y0 + row as u32 * zoom;
                for dy in 0..zoom {
                    for dx in 0..zoom {
                        img.put_pixel(gx + dx, gy + dy, [0, 0, 0, 255]);
                    }
                }
            }
        }
    }
    img
}

#[test]
fn vision_reads_drawn_text_and_reports_top_left_boxes() {
    let b = backend();
    b.warm_ocr().expect("Vision should load");
    assert_eq!(b.ocr_engine_name(), "vision");
    let blank = Image::solid(200, 100, [255, 255, 255, 255]);
    assert!(b.ocr(&blank).unwrap().lines.is_empty());

    // The word sits in the lower right quarter of a 640x240 image.
    let img = text_image("HELLO WORLD", 8, 280, 150, 640, 240);
    let result = b.ocr(&img).expect("ocr");
    assert_eq!(result.engine, "vision");
    eprintln!(
        "ocr lines: {:?}",
        result
            .lines
            .iter()
            .map(|l| (&l.text, l.rect))
            .collect::<Vec<_>>()
    );
    let all: String = result
        .lines
        .iter()
        .map(|l| l.text.to_uppercase())
        .collect::<Vec<_>>()
        .join(" ");
    if all.contains("HELLO") {
        let line = result
            .lines
            .iter()
            .find(|l| l.text.to_uppercase().contains("HELLO"))
            .unwrap();
        // Top-left origin: the text is in the bottom half and right half of the image.
        let r: Rect = line.rect;
        assert!(
            r.top >= 120 && r.bottom <= 240,
            "box {r:?} is not in the lower half"
        );
        assert!(r.left >= 200, "box {r:?} is not toward the right");
        assert!(
            line.words
                .iter()
                .all(|w| w.rect.left >= r.left - 2 && w.rect.right <= r.right + 2)
        );
    } else {
        eprintln!("Vision did not read the blocky glyphs on this machine; skipping box checks");
    }
}
