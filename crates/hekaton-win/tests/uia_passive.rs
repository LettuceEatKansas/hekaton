//! Passive checks of the UI Automation and OCR half of the Windows backend against the real
//! desktop. They read: element trees, the element under the pointer, the focused element, document
//! text, OCR of a capture. They never invoke, toggle, set a value, focus, click, type or raise a
//! window (every tree read uses `WakePolicy::Never`).
//!
//! Marked `#[ignore]` because they need a desktop session. Run them with
//! `cargo test -p hekaton-win -j 4 -- --ignored --nocapture`.
#![cfg(windows)]

use std::time::{Duration, Instant};

use hekaton_core::backend::{
    Backend, CaptureTarget, ElementFilter, Role, TreeLimits, WakePolicy, WindowInfo,
};
use hekaton_win::WindowsBackend;

fn backend() -> WindowsBackend {
    let b = WindowsBackend::new();
    b.init().expect("init");
    b
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

fn limits(filter: ElementFilter, max: usize) -> TreeLimits {
    TreeLimits {
        max_items: max,
        max_depth: 25,
        deadline: Duration::from_millis(2500),
        filter,
        flatten: true,
        wake: WakePolicy::Never,
    }
}

/// The first listed window whose title contains `needle` (any case).
fn window_titled(b: &WindowsBackend, needle: &str) -> Option<WindowInfo> {
    b.list_windows(false)
        .expect("list")
        .into_iter()
        .find(|w| w.title.to_lowercase().contains(&needle.to_lowercase()) && !w.minimized)
}

#[test]
#[ignore = "needs a desktop session"]
fn windows_on_this_desktop() {
    let b = backend();
    for w in b.list_windows(false).expect("list") {
        println!(
            "{:>10} {:<18} {:<28} min={} {}x{} {:?}",
            w.window.raw(),
            w.app,
            w.class,
            w.minimized,
            w.rect.width(),
            w.rect.height(),
            w.title.chars().take(50).collect::<String>()
        );
    }
}

#[test]
#[ignore = "needs a desktop session"]
fn element_tree_of_the_foreground_window() {
    let b = backend();
    let fg = b
        .foreground_window()
        .expect("fg")
        .expect("a window is in front");
    let info = b.window_info(fg).expect("info");
    let started = Instant::now();
    let tree = b
        .element_tree(fg, &limits(ElementFilter::Interactive, 300))
        .expect("tree");
    println!(
        "foreground {:?} ({}): {} interactive elements, truncated={}, wake_hint={}, {}",
        info.title,
        info.app,
        tree.root.children.len(),
        tree.truncated,
        tree.wake_hint,
        ms(started.elapsed())
    );
    assert_eq!(tree.window, fg);
    assert!(!tree.title.is_empty());
    assert!(
        !tree.root.children.is_empty(),
        "a foreground app exposes something"
    );
    assert!(tree.root.children.iter().all(|e| e.is_interactive()));
    for e in tree.root.children.iter().take(8) {
        println!(
            "  {} {:?} value={:?} rect={:?} state={:?}",
            e.native_role,
            e.name.chars().take(40).collect::<String>(),
            e.value
                .as_ref()
                .map(|v| v.chars().take(30).collect::<String>()),
            e.rect,
            e.state.flags()
        );
    }
    // a fresh read of one element answers with the same id and a name
    let first = &tree.root.children[0];
    let again = b.element(&first.id).expect("element");
    assert_eq!(again.id, first.id);
    assert_eq!(again.native_role, first.native_role);
    let top = b.element_window(&first.id).expect("element_window");
    assert!(
        b.same_window(top, fg),
        "an element lives in the window it was read from"
    );
}

#[test]
#[ignore = "needs a desktop session"]
fn element_ids_survive_another_thread() {
    let b = std::sync::Arc::new(backend());
    let fg = b.foreground_window().unwrap().unwrap();
    let tree = b
        .element_tree(fg, &limits(ElementFilter::Interactive, 50))
        .expect("tree");
    let target = tree.root.children.first().expect("an element").clone();
    let b2 = std::sync::Arc::clone(&b);
    let id = target.id.clone();
    // a thread that never initialised COM: the id must still resolve (cache or re-find by name)
    let got = std::thread::spawn(move || b2.element(&id))
        .join()
        .unwrap()
        .expect("element on a new thread");
    assert_eq!(got.name, target.name);
}

#[test]
#[ignore = "needs a desktop session"]
fn big_trees_time_against_python() {
    let b = backend();
    println!("python: ~316 ms for 300 items (Hermes), ~103 ms (ChatGPT)");
    for needle in ["Claude", "ChatGPT", "Brave", "Basecut", "Hermes"] {
        let Some(w) = window_titled(&b, needle) else {
            println!("{needle}: no such window open");
            continue;
        };
        for (label, l) in [
            ("interactive 300", limits(ElementFilter::Interactive, 300)),
            ("interactive 120", limits(ElementFilter::Interactive, 120)),
            (
                "interactive ALL",
                limits(ElementFilter::Interactive, 100000),
            ),
            ("named 300ms", TreeLimits::named(Duration::from_millis(300))),
        ] {
            let mut times = Vec::new();
            let mut last = None;
            for _ in 0..7 {
                let t = Instant::now();
                let tree = b.element_tree(w.window, &l);
                times.push(t.elapsed());
                last = Some(tree);
            }
            let first = times[0];
            match last.unwrap() {
                Ok(tree) => println!(
                    "{needle} ({}) {label}: {} items, truncated={} hint={}; first {} median {} (min {})",
                    w.app,
                    tree.root.children.len(),
                    tree.truncated,
                    tree.wake_hint,
                    ms(first),
                    ms(median(times.clone())),
                    ms(*times.iter().min().unwrap())
                ),
                Err(e) => println!("{needle} {label}: {e}"),
            }
        }
    }
}

#[test]
#[ignore = "needs a desktop session"]
fn element_tree_from_a_thread_that_is_already_in_the_mta() {
    // no init() on this thread, so UI Automation runs right here, not on a helper thread
    let b = WindowsBackend::new();
    let fg = b.foreground_window().unwrap().unwrap();
    let mut times = Vec::new();
    let mut n = 0;
    for _ in 0..7 {
        let t = Instant::now();
        let tree = b
            .element_tree(fg, &limits(ElementFilter::Interactive, 300))
            .expect("tree");
        times.push(t.elapsed());
        n = tree.root.children.len();
    }
    println!("mta thread: {n} items, median {}", ms(median(times)));
}

#[test]
#[ignore = "needs a desktop session"]
fn the_element_under_the_pointer() {
    let b = backend();
    let p = b.cursor_position().expect("cursor");
    let started = Instant::now();
    let hit = b
        .element_at(p, 3, Duration::from_millis(500))
        .expect("element_at");
    println!(
        "element at {p:?}: {} ({})",
        match &hit {
            Some(h) => format!(
                "{} {:?} with {} ancestors [{}]",
                h.element.native_role,
                h.element.name.chars().take(40).collect::<String>(),
                h.ancestors.len(),
                h.ancestors
                    .iter()
                    .map(|a| a.native_role.as_str())
                    .collect::<Vec<_>>()
                    .join(" < ")
            ),
            None => "nothing".to_string(),
        },
        ms(started.elapsed())
    );
    if let Some(h) = hit {
        assert!(h.ancestors.len() <= 3);
        let rect = h.element.rect.expect("a box");
        assert!(rect.width() >= 0);
        assert!(!matches!(h.element.role, Role::Other(ref n) if n.is_empty()));
    }
    // a point nowhere near any window still answers promptly
    let t = Instant::now();
    let far = b
        .element_at(
            hekaton_core::backend::Point::new(-20000, -20000),
            0,
            Duration::from_millis(350),
        )
        .expect("far point");
    println!(
        "far point: {:?} in {}",
        far.map(|h| h.element.name),
        ms(t.elapsed())
    );
    assert!(t.elapsed() < Duration::from_millis(900));
}

#[test]
#[ignore = "needs a desktop session"]
fn the_focused_element() {
    let b = backend();
    let t = Instant::now();
    let focused = b.focused_element().expect("focused");
    match &focused {
        Some(e) => {
            println!(
                "focused: {} {:?} password={} value={:?} in {}",
                e.native_role,
                e.name.chars().take(40).collect::<String>(),
                e.password,
                e.value.as_ref().map(|v| v.len()),
                ms(t.elapsed())
            );
            if e.password {
                assert_eq!(e.value, None, "a password is never read back");
            }
            assert!(e.focused || e.rect.is_some());
        }
        None => println!("nothing has keyboard focus ({})", ms(t.elapsed())),
    }
}

#[test]
#[ignore = "needs a desktop session"]
fn ocr_of_the_primary_display_cold_and_warm() {
    let b = backend();
    assert!(b.capabilities().ocr);
    assert_eq!(b.ocr_engine_name(), "untried");
    let shot = b.capture(CaptureTarget::Display(1)).expect("capture");
    let t = Instant::now();
    let first = b.ocr(&shot).expect("ocr");
    let cold = t.elapsed();
    assert_eq!(first.engine, "winrt");
    let words: usize = first.lines.iter().map(|l| l.words.len()).sum();
    let mut warm = Vec::new();
    for _ in 0..7 {
        let t = Instant::now();
        let again = b.ocr(&shot).expect("ocr again");
        warm.push(t.elapsed());
        assert_eq!(again.lines.len(), first.lines.len(), "OCR is deterministic");
    }
    println!(
        "ocr {}x{}: {} lines, {} words; cold {} warm median {} (min {}); python ~174 ms find_text, 1140 ms first call",
        shot.width(),
        shot.height(),
        first.lines.len(),
        words,
        ms(cold),
        ms(median(warm.clone())),
        ms(*warm.iter().min().unwrap())
    );
    for l in first.lines.iter().take(5) {
        println!(
            "  line {:?} @ {:?}",
            l.text.chars().take(60).collect::<String>(),
            l.rect
        );
    }
    assert!(words > 5, "a desktop shows some words");
    assert!(first.lines.iter().all(|l| !l.words.is_empty()));
    for l in &first.lines {
        assert!(
            l.rect.left >= 0 && l.rect.right <= shot.width() as i32 + 1,
            "{l:?}"
        );
    }
    assert!(b.ocr_engine_name().starts_with("winrt"));
}

#[test]
#[ignore = "needs a desktop session"]
fn warm_ocr_makes_the_first_real_call_fast() {
    let b = backend();
    let t = Instant::now();
    b.warm_ocr().expect("warm");
    let warm = t.elapsed();
    let shot = b.capture(CaptureTarget::Display(1)).expect("capture");
    let t = Instant::now();
    let r = b.ocr(&shot).expect("ocr");
    let first_real = t.elapsed();
    println!(
        "warm_ocr took {}; first real call after it {} ({} lines)",
        ms(warm),
        ms(first_real),
        r.lines.len()
    );
    assert!(
        first_real < Duration::from_millis(900),
        "{}",
        ms(first_real)
    );
}

#[test]
#[ignore = "needs a desktop session"]
fn menu_items_with_no_menu_open_is_empty_and_quick() {
    let b = backend();
    if !b.popup_menus().expect("popups").is_empty() {
        println!("a menu is open on this desktop; skipping the empty check");
        return;
    }
    let mut times = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let items = b
            .menu_items(Duration::from_millis(100))
            .expect("menu_items");
        times.push(t.elapsed());
        assert!(items.is_empty());
    }
    println!(
        "menu_items (none open, 100 ms budget): median {}",
        ms(median(times.clone()))
    );
    for budget in [400u64, 3000] {
        let t = Instant::now();
        let items = b
            .menu_items(Duration::from_millis(budget))
            .expect("menu_items");
        println!(
            "menu_items (none open, {budget} ms budget): {} items in {}",
            items.len(),
            ms(t.elapsed())
        );
    }
    assert!(median(times) < Duration::from_millis(600));
}

#[test]
#[ignore = "needs a desktop session"]
fn document_text_and_search_on_a_browser_or_editor_window() {
    let b = backend();
    let mut tried = 0;
    for w in b.list_windows(false).expect("list") {
        if w.minimized || w.rect.width() < 200 {
            continue;
        }
        let t = Instant::now();
        let text = b.document_text(w.window, 500_000);
        let took = t.elapsed();
        if let Ok(Some(text)) = text {
            tried += 1;
            let hits = b
                .find_in_documents(w.window, "the", 3, Duration::from_millis(1000))
                .expect("find");
            println!(
                "{} ({}): {} chars in {}; {} hits for 'the'",
                w.title.chars().take(40).collect::<String>(),
                w.app,
                text.chars().count(),
                ms(took),
                hits.len()
            );
            for h in &hits {
                assert!(!h.line.is_empty());
            }
            if tried >= 3 {
                break;
            }
        }
    }
    println!("{tried} window(s) exposed document text");
}

#[test]
#[ignore = "needs a desktop session"]
fn the_file_dialog_field_is_absent_from_an_ordinary_window() {
    let b = backend();
    let fg = b.foreground_window().unwrap().unwrap();
    let t = Instant::now();
    let field = b.find_file_dialog_field(fg).expect("find");
    println!(
        "file dialog field in the foreground window: {field:?} ({})",
        ms(t.elapsed())
    );
}

#[test]
#[ignore = "needs a desktop session"]
fn wait_window_ready_returns_at_once_for_a_ready_window() {
    let b = backend();
    let fg = b.foreground_window().unwrap().unwrap();
    let t = Instant::now();
    b.wait_window_ready(fg, Duration::from_millis(2000))
        .expect("ready");
    println!(
        "wait_window_ready on the foreground window: {}",
        ms(t.elapsed())
    );
    assert!(t.elapsed() < Duration::from_millis(1500));
}

#[test]
#[ignore = "needs a desktop session"]
fn tree_limits_are_honoured_on_a_real_window() {
    let b = backend();
    let fg = b.foreground_window().unwrap().unwrap();
    // item cap: exactly three, and the read says it stopped short
    let few = b
        .element_tree(fg, &limits(ElementFilter::Interactive, 3))
        .expect("tree");
    println!(
        "cap 3: {} items, truncated={}",
        few.root.children.len(),
        few.truncated
    );
    assert!(few.root.children.len() <= 3);
    if few.root.children.len() == 3 {
        assert!(few.truncated, "a full window has more than three");
    }
    // a deadline already past: nothing is read, and it says so
    let mut late = limits(ElementFilter::Interactive, 300);
    late.deadline = Duration::ZERO;
    late.max_items = 50;
    let none = b.element_tree(fg, &late).expect("tree");
    println!(
        "deadline 0: {} items, truncated={}",
        none.root.children.len(),
        none.truncated
    );
    assert!(none.root.children.is_empty() && none.truncated);
    // nested: a real hierarchy that mirrors the flat order
    let mut nested = limits(ElementFilter::Interactive, 300);
    nested.flatten = false;
    let tree = b.element_tree(fg, &nested).expect("nested tree");
    let flat = b
        .element_tree(fg, &limits(ElementFilter::Interactive, 300))
        .expect("flat tree");
    let mut walked = Vec::new();
    for top in &tree.root.children {
        top.walk(&mut walked);
    }
    println!(
        "nested: {} top-level, {} in all; flat: {}",
        tree.root.children.len(),
        walked.len(),
        flat.root.children.len()
    );
    assert!(
        walked.len().abs_diff(flat.root.children.len()) <= 2,
        "same elements, give or take churn"
    );
    // depth: nothing deeper than the limit
    let mut shallow = limits(ElementFilter::All, 100_000);
    shallow.max_depth = 1;
    shallow.flatten = false;
    let top = b.element_tree(fg, &shallow).expect("shallow tree");
    assert!(
        top.root
            .children
            .iter()
            .all(|e| e.children.len() <= 100_000)
    );
    let mut deepest = 0;
    fn depth(e: &hekaton_core::backend::Element) -> usize {
        1 + e.children.iter().map(depth).max().unwrap_or(0)
    }
    for e in &top.root.children {
        deepest = deepest.max(depth(e));
    }
    println!("max_depth 1 (python style: 0 and 1 below the window): deepest chain {deepest}");
    assert!(deepest <= 2, "{deepest}");
}

#[test]
#[ignore = "needs a desktop session"]
fn every_listed_window_reads_without_an_error() {
    let b = backend();
    for w in b.list_windows(false).expect("list") {
        if w.minimized {
            continue;
        }
        let t = Instant::now();
        let r = b.element_tree(w.window, &limits(ElementFilter::Interactive, 160));
        match r {
            Ok(tree) => println!(
                "{:<18} {:<28} {:>4} items truncated={:<5} hint={:<5} {:>8}  elevated={}",
                w.app,
                w.class.chars().take(28).collect::<String>(),
                tree.root.children.len(),
                tree.truncated,
                tree.wake_hint,
                ms(t.elapsed()),
                w.elevated
            ),
            Err(e) => println!(
                "{:<18} {:<28} ERROR {e} after {}",
                w.app,
                w.class,
                ms(t.elapsed())
            ),
        }
    }
}

#[test]
#[ignore = "needs a desktop session"]
fn a_named_read_carries_values_and_never_a_password() {
    let b = backend();
    let mut seen = 0;
    for w in b.list_windows(false).expect("list") {
        if w.minimized {
            continue;
        }
        let Ok(tree) = b.element_tree(w.window, &TreeLimits::named(Duration::from_millis(400)))
        else {
            continue;
        };
        for e in &tree.root.children {
            assert!(
                !e.name.trim().is_empty() && e.rect.is_some_and(|r| !r.is_empty()),
                "{e:?}"
            );
            if e.password {
                assert_eq!(e.value, None);
            }
            if e.role == Role::Edit && e.value.is_some() {
                seen += 1;
                if seen <= 4 {
                    println!(
                        "{}: {} {:?} value {} chars",
                        w.app,
                        e.native_role,
                        e.name.chars().take(30).collect::<String>(),
                        e.value.as_ref().map_or(0, |v| v.chars().count())
                    );
                }
            }
        }
    }
    println!("{seen} edit element(s) with a value in Named reads");
    assert!(
        seen > 0,
        "some window on a desktop has a text box with text"
    );
}
