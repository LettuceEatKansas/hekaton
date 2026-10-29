//! Says whether the front window gets the long settle floor, and why (read-only, no input).
use std::time::Instant;

use wh_core::backend::{ElementFilter, TreeLimits, WakePolicy};

fn main() {
    let b = winhands_lib::platform::backend().expect("backend");
    let clock = wh_core::clock::SystemClock::default();
    let front = b.foreground_window().unwrap().expect("a front window");
    let root = b.root_window(front).unwrap();
    let info = b.window_info(root).unwrap();
    println!("front {:?} {:?} rect {:?}", info.title, info.app, info.rect);
    let limits = TreeLimits {
        max_items: 4,
        max_depth: 25,
        deadline: std::time::Duration::from_millis(2500),
        filter: ElementFilter::Interactive,
        flatten: true,
        wake: WakePolicy::Never,
    };
    let t = Instant::now();
    let tree = b.element_tree(root, &limits);
    println!(
        "tree in {:?}: {:?}",
        t.elapsed(),
        tree.as_ref().map(|t| t.root.children.len())
    );
    let probe = wh_core::settle::UiaProbe::new();
    let mon = b.displays().unwrap()[0].rect;
    let t = Instant::now();
    println!(
        "slow_ui_window = {:?} in {:?}",
        wh_core::settle::slow_ui_window(b.as_ref(), &clock, &probe, mon),
        t.elapsed()
    );
}
