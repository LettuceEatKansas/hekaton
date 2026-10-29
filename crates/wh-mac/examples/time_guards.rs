//! Times the window lookups the typing and pointer guards call (read-only, no input).
use std::time::Instant;
use wh_core::backend::Backend;

fn time<T>(label: &str, n: u32, f: impl Fn() -> T) {
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(f());
    }
    println!("{label:<28} {:7.2} ms", t.elapsed().as_secs_f64() * 1000.0 / f64::from(n));
}

fn main() {
    let b = wh_mac::MacBackend::new();
    b.init().unwrap();
    let w = b.foreground_window().unwrap().unwrap();
    time("foreground_window", 50, || b.foreground_window());
    time("window_pid", 50, || b.window_pid(w));
    time("window_info", 50, || b.window_info(w));
    time("list_windows", 50, || b.list_windows(false));
    time("window_at_point", 50, || b.window_at_point(wh_core::backend::Point { x: 700, y: 500 }));
}
