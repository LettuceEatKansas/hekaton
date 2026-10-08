//! Passive checks against the real machine: they read the screen, the window list, the process
//! table and the clipboard sequence, and nothing else. No input is sent, no window is touched, no
//! program is started, no hotkey is registered, nothing is written to the clipboard.
//!
//! Marked `#[ignore]` because they need a desktop session. Run them with
//! `cargo test -p wh-win -j 4 -- --ignored --nocapture`.
#![cfg(windows)]

use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use wh_core::backend::{AppTarget, Backend, CaptureTarget, Rect, WindowKind};
use wh_core::imaging;
use wh_win::WindowsBackend;

fn backend() -> WindowsBackend {
    let b = WindowsBackend::new();
    b.init().expect("init");
    b
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[test]
#[ignore = "needs a desktop session"]
fn init_is_idempotent_and_makes_coordinates_physical_pixels() {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
    let b = backend();
    b.init().expect("second init");
    let primary = b.displays().expect("displays")[0].clone();
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `mode` is a live DEVMODEW with dmSize set; a null device name means the primary display.
    let ok = unsafe {
        EnumDisplaySettingsW(
            windows::core::PCWSTR::null(),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    };
    assert!(ok.as_bool());
    // With per-monitor-v2 awareness the monitor rect is the real pixel size of the mode.
    assert_eq!(
        (primary.rect.width(), primary.rect.height()),
        (mode.dmPelsWidth as i32, mode.dmPelsHeight as i32),
        "the display rect should match the display mode in physical pixels"
    );
}

#[test]
#[ignore = "needs a desktop session"]
fn the_primary_display_captures_at_its_own_size() {
    let b = backend();
    let displays = b.displays().expect("displays");
    let primary = &displays[0];
    assert!(primary.primary);
    assert_eq!(primary.rect.left, 0);
    assert_eq!(primary.rect.top, 0);
    let (pw, ph) = primary.pixel_size();
    let shot = b.capture(CaptureTarget::Display(1)).expect("capture");
    assert_eq!((shot.width(), shot.height()), (pw, ph));
    assert_eq!(shot.scale(), 1.0);
    assert_eq!((shot.origin().x, shot.origin().y), (0, 0));
    assert_eq!(shot.rgba().len(), (pw as usize) * (ph as usize) * 4);
    for (x, y) in [(0, 0), (pw - 1, 0), (0, ph - 1), (pw - 1, ph - 1)] {
        assert_eq!(
            shot.pixel(x, y).map(|p| p[3]),
            Some(255),
            "opaque at {x},{y}"
        );
    }
    let distinct: std::collections::HashSet<[u8; 4]> = shot
        .rgba()
        .as_chunks::<4>()
        .0
        .iter()
        .step_by(997)
        .copied()
        .collect();
    println!(
        "primary display {pw}x{ph}, {} distinct sampled colours",
        distinct.len()
    );
    assert!(
        distinct.len() > 1,
        "a live desktop is never one flat colour"
    );
    assert!(b.capture(CaptureTarget::Display(99)).is_err());
}

#[test]
#[ignore = "needs a desktop session"]
fn displays_sum_to_the_virtual_screen() {
    let b = backend();
    let displays = b.displays().expect("displays");
    let vs = b.virtual_screen().expect("virtual screen");
    assert!(!displays.is_empty());
    assert_eq!(displays.iter().filter(|d| d.primary).count(), 1);
    assert!(displays[0].primary, "primary comes first");
    let ids: Vec<u32> = displays.iter().map(|d| d.id).collect();
    assert_eq!(ids, (1..=displays.len() as u32).collect::<Vec<_>>());
    let union = displays.iter().fold(displays[0].rect, |acc, d| {
        Rect::new(
            acc.left.min(d.rect.left),
            acc.top.min(d.rect.top),
            acc.right.max(d.rect.right),
            acc.bottom.max(d.rect.bottom),
        )
    });
    assert_eq!(
        union, vs,
        "the bounding box of the displays is the virtual screen"
    );
    let desktop = b.capture(CaptureTarget::VirtualDesktop).expect("capture");
    assert_eq!(
        (desktop.width() as i32, desktop.height() as i32),
        (vs.width(), vs.height())
    );
    assert_eq!((desktop.origin().x, desktop.origin().y), (vs.left, vs.top));
    println!("{} display(s), virtual screen {vs:?}", displays.len());
}

#[test]
#[ignore = "needs a desktop session"]
fn a_region_capture_is_the_size_asked_for() {
    let b = backend();
    let r = Rect::from_xywh(10, 20, 301, 77);
    let shot = b.capture(CaptureTarget::Region(r)).expect("capture");
    assert_eq!((shot.width(), shot.height()), (301, 77));
    assert_eq!((shot.origin().x, shot.origin().y), (10, 20));
    assert!(
        b.capture(CaptureTarget::Region(Rect::new(5, 5, 5, 9)))
            .is_err()
    );
}

#[test]
#[ignore = "needs a desktop session"]
fn list_windows_is_not_empty_and_each_row_is_sane() {
    let b = backend();
    let shown = b.list_windows(false).expect("list");
    let all = b.list_windows(true).expect("list all");
    assert!(!shown.is_empty(), "a desktop session has windows");
    assert!(all.len() >= shown.len());
    assert!(
        shown.iter().all(|w| !w.hidden),
        "hidden windows are listed only on request"
    );
    assert!(shown.iter().filter(|w| w.foreground).count() <= 1);
    let mut refs: Vec<u64> = all.iter().map(|w| w.window.raw()).collect();
    refs.sort_unstable();
    refs.dedup();
    assert_eq!(refs.len(), all.len(), "one ref per window");
    for w in &shown {
        assert!(!w.title.trim().is_empty());
        assert!(w.pid != 0, "{w:?}");
        assert!(!w.app.is_empty());
        let live = b
            .window_info(w.window)
            .expect("window_info of a listed window");
        assert_eq!((live.pid, &live.class), (w.pid, &w.class));
        assert!(b.window_exists(w.window));
    }
    println!(
        "{} windows ({} incl. hidden); first: {:?} [{}]",
        shown.len(),
        all.len(),
        shown[0].title,
        shown[0].app
    );
    // Dead handles are reported as gone, not as a crash.
    let gone = wh_core::backend::WindowRef::from_raw(0x7FFF_FFF0);
    assert!(!b.window_exists(gone));
    assert!(b.window_info(gone).is_err());
}

#[test]
#[ignore = "needs a desktop session"]
fn the_foreground_window_can_be_read_from_every_side() {
    let b = backend();
    let Some(fg) = b.foreground_window().expect("foreground") else {
        println!("no foreground window (locked screen?)");
        return;
    };
    let info = b.window_info(fg).expect("info");
    assert!(info.foreground);
    let root = b.root_window(fg).expect("root");
    assert!(b.same_window(fg, root));
    b.owner_chain(fg).expect("owner chain");
    b.hosted_app(fg).expect("hosted app");
    b.window_text(fg).expect("window text");
    b.has_caret(Some(fg)).expect("has caret");
    b.has_caret(None)
        .expect("has caret of the foreground thread");
    b.caret().expect("caret");
    b.focus_token().expect("focus token");
    println!(
        "foreground: {:?} [{}] kind {:?} class {}",
        info.title, info.app, info.kind, info.class
    );
}

#[test]
#[ignore = "needs a desktop session"]
fn the_desktop_is_found_at_the_screen_centre_and_menus_can_be_listed() {
    let b = backend();
    let c = b.displays().expect("displays")[0].rect.center();
    let hit = b
        .window_at_point(c)
        .expect("hit test")
        .expect("something is at the centre");
    let info = b.window_info(hit).expect("info");
    println!(
        "at {c:?}: {:?} [{}] {:?}",
        info.title, info.class, info.kind
    );
    assert!(b.point_on_screen(c).expect("on screen"));
    assert!(
        !b.point_on_screen(wh_core::backend::Point::new(-100_000, 0))
            .expect("off screen")
    );
    let top = b.visible_fraction(hit, info.rect, 3).expect("fraction");
    assert!((0.0..=1.0).contains(&top));
    let menus = b.popup_menus().expect("popup menus");
    for m in menus {
        assert_eq!(
            b.window_info(m).map(|i| i.kind).ok(),
            Some(WindowKind::PopupMenu)
        );
    }
}

#[test]
#[ignore = "needs a desktop session"]
fn the_process_table_contains_our_own_pid() {
    let b = backend();
    let me = std::process::id();
    let table = b.process_table().expect("table");
    let mine = table
        .iter()
        .find(|p| p.pid == me)
        .expect("our pid is in the table");
    let exe = std::env::current_exe().unwrap();
    let exe_name = exe.file_name().unwrap().to_string_lossy().to_lowercase();
    assert_eq!(mine.exe_name.to_lowercase(), exe_name);

    let info = b.process_info(me).expect("info").expect("some");
    assert_eq!(info.exe_name.to_lowercase(), exe_name);
    assert_eq!(
        info.parent_pid, mine.parent_pid,
        "the kernel and the snapshot agree on the parent"
    );
    assert!(info.exe_path.is_some());
    assert!(info.started_at.is_some_and(|t| t > 0));
    assert_eq!(b.own_process().expect("own").map(|p| p.pid), Some(me));
    assert!(b.process_info(0x7FFF_FFF0).expect("lookup").is_none());

    // The chain the caller guard walks: us first, then upward, never an infinite loop.
    let chain = wh_core::caller::process_chain(&b, me);
    assert_eq!(chain.first().map(|c| c.0), Some(me));
    println!("process chain: {chain:?}");
    println!("elevated: {}", info.elevated);
}

#[test]
#[ignore = "needs a desktop session"]
fn peer_pid_names_the_owner_of_a_loopback_client() {
    let b = backend();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let server_port = listener.local_addr().unwrap().port();
    let client = TcpStream::connect(("127.0.0.1", server_port)).expect("connect");
    let (_accepted, _) = listener.accept().expect("accept");
    let client_port = client.local_addr().unwrap().port();
    assert_eq!(
        b.peer_pid(client_port, server_port).expect("peer pid"),
        Some(std::process::id())
    );
    // A port nobody uses has no owner; port 0 never matches.
    assert_eq!(b.peer_pid(1, server_port).expect("peer pid"), None);
    assert_eq!(b.peer_pid(0, server_port).expect("peer pid"), None);
}

#[test]
#[ignore = "needs a desktop session"]
fn the_clipboard_can_be_read_and_has_a_sequence_number() {
    let b = backend();
    let seq = b.clipboard_sequence().expect("sequence");
    assert!(seq.is_some_and(|s| s > 0));
    // Reading only: the content is never printed.
    let kind = match b.clipboard().expect("read") {
        wh_core::backend::ClipboardContent::Empty => "empty",
        wh_core::backend::ClipboardContent::Text(_) => "text",
        wh_core::backend::ClipboardContent::Files(_) => "files",
        wh_core::backend::ClipboardContent::Other => "other",
    };
    println!("clipboard holds: {kind}");
    assert_eq!(
        b.clipboard_sequence().expect("sequence"),
        seq,
        "a read does not change it"
    );
}

#[test]
#[ignore = "needs a desktop session"]
fn idle_time_cursor_and_time_zone_are_plausible() {
    let b = backend();
    let idle = b.human_input_idle().expect("idle");
    assert!(
        idle < Duration::from_secs(60 * 60 * 24 * 49),
        "a 32-bit tick count wraps at 49 days"
    );
    let p = b.cursor_position().expect("cursor");
    let vs = b.virtual_screen().unwrap();
    assert!(
        vs.contains(p),
        "the pointer {p:?} is inside the virtual screen {vs:?}"
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let offset = b.local_utc_offset(now).expect("offset");
    assert!(
        offset.abs() <= 14 * 3600 && offset % 900 == 0,
        "offset {offset}"
    );
    let winter = b.local_utc_offset(1_768_521_600).expect("jan 2026");
    let summer = b.local_utc_offset(1_784_160_000).expect("jul 2026");
    println!("idle {idle:?}, cursor {p:?}, utc offset now {offset} s (jan {winter}, jul {summer})");
    assert!(b.local_utc_offset(-20_000_000_000).is_err());
    assert_eq!(b.input_events(), 0, "no passive check sends input");
}

#[test]
#[ignore = "needs a desktop session"]
fn app_names_resolve_without_launching_anything() {
    let b = backend();
    for name in ["notepad", "calc", "paint", "explorer", "cmd"] {
        let t = b.resolve_app(name).expect("resolve");
        println!("{name:>10} -> {t:?}");
    }
    let cmd = b
        .resolve_app("cmd.exe")
        .expect("resolve")
        .expect("cmd.exe is on PATH");
    assert!(matches!(cmd, AppTarget::Executable(p) if p.to_lowercase().ends_with("cmd.exe")));
    assert_eq!(
        b.resolve_app("definitely-not-an-app-xyzzy")
            .expect("resolve"),
        None
    );
    assert_eq!(b.resolve_app("").expect("resolve"), None);
    let calc = b
        .resolve_app(r"C:\Windows\System32\notepad.exe")
        .expect("resolve");
    assert!(matches!(calc, Some(AppTarget::Executable(_))));
}

#[test]
#[ignore = "needs a desktop session"]
fn capture_latency() {
    let b = backend();
    let primary = b.displays().unwrap()[0].clone();
    // Warm up: first call loads GDI state.
    let warm = b.capture(CaptureTarget::Display(primary.id)).unwrap();
    let mut grab = Vec::new();
    let mut shrink_and_encode = Vec::new();
    let mut bytes = 0;
    for _ in 0..20 {
        let t = Instant::now();
        let shot = b.capture(CaptureTarget::Display(primary.id)).unwrap();
        grab.push(t.elapsed());
        let long = shot.width().max(shot.height());
        let k = 1024.0 / f64::from(long);
        let (w, h) = (
            (f64::from(shot.width()) * k).round().max(1.0) as u32,
            (f64::from(shot.height()) * k).round().max(1.0) as u32,
        );
        let t = Instant::now();
        let small = imaging::resize(&shot, w, h).unwrap();
        let jpeg = imaging::encode_jpeg(&small).unwrap();
        shrink_and_encode.push(t.elapsed());
        bytes = jpeg.len();
    }
    let total: Vec<Duration> = grab
        .iter()
        .zip(&shrink_and_encode)
        .map(|(a, b)| *a + *b)
        .collect();
    println!(
        "capture {}x{}: grab median {:?}, resize+JPEG to 1024 long edge median {:?} ({} bytes), grab+resize+JPEG median {:?}",
        warm.width(),
        warm.height(),
        median(grab.clone()),
        median(shrink_and_encode),
        bytes,
        median(total)
    );
    assert!(median(grab) < Duration::from_secs(2));
}
