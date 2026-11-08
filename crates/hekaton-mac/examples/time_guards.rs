//! Times the window lookups the typing and pointer guards call (read-only, no input).
#[cfg(target_os = "macos")]
use hekaton_core::backend::Backend;
#[cfg(target_os = "macos")]
use std::time::Instant;

#[cfg(target_os = "macos")]
fn time<T>(label: &str, n: u32, f: impl Fn() -> T) {
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(f());
    }
    println!(
        "{label:<28} {:7.2} ms",
        t.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
    );
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    let b = hekaton_mac::MacBackend::new();
    b.init().unwrap();
    let w = b.foreground_window().unwrap().unwrap();
    time("foreground_window", 50, || b.foreground_window());
    time("window_pid", 50, || b.window_pid(w));
    time("window_info", 50, || b.window_info(w));
    time("list_windows", 50, || b.list_windows(false));
    time("window_at_point", 50, || {
        b.window_at_point(hekaton_core::backend::Point { x: 700, y: 500 })
    });
    let screen = b.virtual_screen().unwrap();
    time("capture_preview cold", 1, || {
        b.capture_preview(screen, 320, 180)
    });
    std::thread::sleep(std::time::Duration::from_millis(600));
    time("capture_preview", 20, || {
        b.capture_preview(screen, 320, 180)
    });
    time("capture_full", 10, || {
        b.capture(hekaton_core::backend::CaptureTarget::Display(1))
    });
    time("focus_token", 20, || b.focus_token());
}
