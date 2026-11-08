//! Captures the primary display through DXGI and through GDI back to back and reports how far the
//! pixels differ (read-only, no input). Run on a still screen.
//! Windows only: on other systems it compiles to a stub so `cargo test` builds everywhere.

#[cfg(windows)]
mod imp {
    use hekaton_core::backend::{Backend, CaptureTarget};

    pub fn main() {
        let b = hekaton_win::WindowsBackend::new();
        b.init().expect("init");
        let region = b.displays().expect("displays")[0].rect;
        for round in 0..3 {
            let dxgi = b.capture(CaptureTarget::Region(region)).unwrap();
            let gdi = b.capture_gdi(region).unwrap();
            let (a, g) = (dxgi.rgba(), gdi.rgba());
            let differing = a
                .as_chunks::<4>()
                .0
                .iter()
                .zip(g.as_chunks::<4>().0)
                .filter(|(p, q)| p[..3] != q[..3])
                .count();
            let max = a
                .iter()
                .zip(g)
                .map(|(x, y)| x.abs_diff(*y))
                .max()
                .unwrap_or(0);
            println!(
                "round {round}: dxgi {}x{} gdi {}x{} origin {:?}/{:?} | pixels differing {differing} of {} | max channel diff {max}",
                dxgi.width(),
                dxgi.height(),
                gdi.width(),
                gdi.height(),
                dxgi.origin(),
                gdi.origin(),
                a.len() / 4
            );
        }
    }
}

#[cfg(windows)]
fn main() {
    imp::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("compare_capture runs on Windows only");
}
