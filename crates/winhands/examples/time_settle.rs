//! Times the pieces of one settle look on the live machine (read-only, no input): a full-display
//! capture, the grayscale thumbnail, and the frame comparison.
use std::time::Instant;

use wh_core::backend::CaptureTarget;

fn main() {
    let b = winhands_lib::platform::backend().expect("backend");
    let displays = b.displays().expect("displays");
    let region = displays[0].rect;
    println!("display {:?}", region);
    let mut prev = None;
    for round in 0..8 {
        let t = Instant::now();
        let img = b.capture(CaptureTarget::Region(region)).unwrap();
        let t_cap = t.elapsed();
        let t = Instant::now();
        let th = wh_core::imaging::thumbnail(&img).unwrap();
        let t_th = t.elapsed();
        let t = Instant::now();
        let calm = prev.as_ref().map(|p| wh_core::imaging::is_calm(p, &th));
        let t_cmp = t.elapsed();
        let t = Instant::now();
        let pv = b.capture_preview(region, 320, 180).unwrap();
        let t_pv = t.elapsed();
        let t = Instant::now();
        let pth = wh_core::imaging::thumbnail(&pv).unwrap();
        let t_pth = t.elapsed();
        let agree = wh_core::imaging::change_ratio(&th, &pth);
        println!(
            "round {round}: capture {t_cap:?} | thumbnail {t_th:?} | compare {t_cmp:?} calm={calm:?} | preview {t_pv:?} {}x{} | thumb {t_pth:?} | full-vs-preview diff {agree:.4}",
            pv.width(),
            pv.height()
        );
        prev = Some(th);
    }
}
