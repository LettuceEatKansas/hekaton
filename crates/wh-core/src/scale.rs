//! Coordinate scaling between what the model sees and the screen. Port of the "display and scale"
//! half of `winhands/computer.py`: `display_rect`, `_scale`, `shot_size`, `to_screen`,
//! `from_screen`, `_min_zoom`, the zoom size rule of `screenshot_b64`, and the per-call resets of
//! `run_many`.
//!
//! Two conventions, chosen per call:
//!
//! * **pixels** (default): coordinates are pixels of the screenshot the caller was shown, whose long
//!   edge is `shot_long` (1024 by default, 128..=4096 per call). The screenshot is the display scaled
//!   down by `min(1, shot_long / long edge)`.
//! * **norm1000**: coordinates run 0..=1000 on both axes, the convention Gemini, GLM-V and Seed
//!   models answer in (measured 2026-09-20: they click 0-1000 even when asked for pixels).
//!
//! Multi-display: the model sees one display at a time. Its rect carries the display's offset in the
//! virtual desktop, so a point on a second monitor maps to its real (possibly negative) coordinates.

use std::fmt;

use serde_json::Value;

use crate::backend::{Display, Rect};
use crate::config::{Config, to_f64, to_i64};

/// The smallest and largest `shot_long` a call may ask for.
pub const SHOT_LONG_MIN: i64 = 128;
/// See [`SHOT_LONG_MIN`].
pub const SHOT_LONG_MAX: i64 = 4096;

/// A zoom is never enlarged beyond this multiple of the region's own pixels (computer.py
/// `ZOOM_MAX_UPSCALE`): more costs image tokens and adds no detail.
pub const ZOOM_MAX_UPSCALE: f64 = 2.0;

/// Which coordinate convention a call uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Coords {
    /// Pixels of the screenshot.
    #[default]
    Pixels,
    /// 0 to 1000 on both axes.
    Norm1000,
}

impl Coords {
    /// `"norm1000"` is Norm1000, anything else Pixels (Python: `COORDS == "norm1000"`).
    pub fn from_name(name: &str) -> Self {
        if name.trim().eq_ignore_ascii_case("norm1000") {
            Coords::Norm1000
        } else {
            Coords::Pixels
        }
    }
}

/// A coordinate could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScaleError {
    /// The message, worded as the Python `ValueError`.
    Invalid(String),
}

impl fmt::Display for ScaleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ScaleError::Invalid(msg) = self;
        f.write_str(msg)
    }
}

impl std::error::Error for ScaleError {}

fn invalid<T>(msg: String) -> Result<T, ScaleError> {
    Err(ScaleError::Invalid(msg))
}

/// How a number reads in a Python list repr: `1200` for a whole number, `1200.5` otherwise.
fn py_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn py_pair(x: f64, y: f64) -> String {
    format!("[{}, {}]", py_num(x), py_num(y))
}

/// Round half to even, as Python's `round()` does.
fn py_round(v: f64) -> i64 {
    v.round_ties_even() as i64
}

/// The screen a call looks at, the coordinate convention, and the screenshot size.
/// One value per call; nothing here is global.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scale {
    /// The display the model sees, in input coordinates.
    pub display: Rect,
    /// The convention in force.
    pub coords: Coords,
    /// The screenshot's long edge.
    pub long_edge: u32,
}

impl Scale {
    /// A scale for `display` with an explicit convention and screenshot size.
    pub fn new(display: Rect, coords: Coords, long_edge: u32) -> Self {
        Self {
            display,
            coords,
            long_edge,
        }
    }

    /// Python `_scale(w, h)`: `min(1, long_edge / max(w, h))`.
    pub fn factor(&self) -> f64 {
        let (w, h) = (
            f64::from(self.display.width()),
            f64::from(self.display.height()),
        );
        (f64::from(self.long_edge) / w.max(h)).min(1.0)
    }

    /// The size of the screenshot of the whole display. Python `shot_size()`.
    pub fn shot_size(&self) -> (i32, i32) {
        let s = self.factor();
        (
            (f64::from(self.display.width()) * s) as i32,
            (f64::from(self.display.height()) * s) as i32,
        )
    }

    /// A model coordinate to an absolute screen point. Python `to_screen(coord)`.
    pub fn to_screen(&self, x: f64, y: f64) -> Result<(i32, i32), ScaleError> {
        let (left, top) = (self.display.left, self.display.top);
        let (w, h) = (self.display.width(), self.display.height());
        match self.coords {
            Coords::Norm1000 => {
                if !((0.0..=1000.0).contains(&x) && (0.0..=1000.0).contains(&y)) {
                    return invalid(format!("coordinate {} is outside 0..1000", py_pair(x, y)));
                }
                // The origin is added after the truncation: `as i32` of a negative sum rounds toward
                // zero, one pixel off on a monitor or window whose origin is negative (left of or
                // above the primary one). Python `to_screen`, 2026-10-08.
                Ok((
                    left + (x / 1000.0 * f64::from(w - 1)) as i32,
                    top + (y / 1000.0 * f64::from(h - 1)) as i32,
                ))
            }
            Coords::Pixels => {
                let (sw, sh) = self.shot_size();
                if !(0.0 <= x && x < f64::from(sw) && 0.0 <= y && y < f64::from(sh)) {
                    return invalid(format!(
                        "coordinate {} is outside the {sw}x{sh} screenshot",
                        py_pair(x, y)
                    ));
                }
                let s = self.factor();
                Ok((left + (x / s) as i32, top + (y / s) as i32))
            }
        }
    }

    /// A model coordinate given as JSON, which must be `[x, y]` of numbers (or numeric strings, as
    /// Python's `float()` accepts). Python `to_screen(coord)` including its type check.
    pub fn to_screen_value(&self, coord: &Value) -> Result<(i32, i32), ScaleError> {
        let pair = match coord {
            Value::Array(items) if items.len() == 2 => items,
            _ => return invalid("coordinate must be [x, y]".into()),
        };
        let num = |v: &Value| {
            to_f64(v).ok_or_else(|| ScaleError::Invalid("coordinate must be [x, y]".into()))
        };
        self.to_screen(num(&pair[0])?, num(&pair[1])?)
    }

    /// An absolute screen point to a model coordinate, clamped inside the screenshot.
    ///
    /// 2026-09-21 h-clockdate: on a 2560x1440 screen the bottom-right pixel rounded to [1280, 720],
    /// one past the 1280x720 shot, so find_text on the taskbar clock raised "outside the
    /// screenshot" and the model never got the date it had found. Python `from_screen(x, y)`.
    pub fn from_screen(&self, x: i32, y: i32) -> [i32; 2] {
        let (left, top) = (f64::from(self.display.left), f64::from(self.display.top));
        let (w, h) = (
            f64::from(self.display.width()),
            f64::from(self.display.height()),
        );
        let (dx, dy) = (f64::from(x) - left, f64::from(y) - top);
        match self.coords {
            Coords::Norm1000 => [
                py_round(dx * 1000.0 / (w - 1.0)).clamp(0, 1000) as i32,
                py_round(dy * 1000.0 / (h - 1.0)).clamp(0, 1000) as i32,
            ],
            Coords::Pixels => {
                let s = self.factor();
                let (sw, sh) = self.shot_size();
                [
                    py_round(dx * s).clamp(0, i64::from(sw - 1)) as i32,
                    py_round(dy * s).clamp(0, i64::from(sh - 1)) as i32,
                ]
            }
        }
    }

    /// Grow a tiny zoom box around its centre to at least 16% x 6% of the screen.
    ///
    /// 2026-09-21 eval: Qwen zoomed 11 times on boxes as small as 52 x 11 (0-1000 scale) trying to
    /// read "Mega Crit"; each missed the word by a few pixels and came back empty. A person squints
    /// at an area, not a slit. A box drawn to the screen's very edge is pulled inside the picture.
    /// Python `_min_zoom(region)` with `region = [x0, y0, x1, y1]` in model coordinates.
    pub fn min_zoom(&self, region: [f64; 4]) -> [i32; 4] {
        let (w, h) = match self.coords {
            Coords::Norm1000 => (1000.0, 1000.0),
            Coords::Pixels => {
                let (sw, sh) = self.shot_size();
                (f64::from(sw), f64::from(sh))
            }
        };
        let clamp = |v: f64, max: f64| v.max(0.0).min(max - 1.0);
        let (mut x0, mut x1) = (clamp(region[0], w), clamp(region[2], w));
        let (mut y0, mut y1) = (clamp(region[1], h), clamp(region[3], h));
        let (mw, mh) = (0.16 * w, 0.06 * h);
        if x1 - x0 < mw {
            let cx = (x0 + x1) / 2.0;
            x0 = (cx - mw / 2.0).max(0.0);
            x1 = (cx + mw / 2.0).min(w - 1.0);
        }
        if y1 - y0 < mh {
            let cy = (y0 + y1) / 2.0;
            y0 = (cy - mh / 2.0).max(0.0);
            y1 = (cy + mh / 2.0).min(h - 1.0);
        }
        [x0 as i32, y0 as i32, x1 as i32, y1 as i32]
    }

    /// The screen box of a zoom region and the size its image is rendered at.
    ///
    /// 2026-10-08 (W3): never upscale a zoom past 2x the region's native pixels, and never past the
    /// call's shot_long long edge. A 400x300 box used to be blown up to 1280x960 (3.2x), which costs
    /// image tokens and adds no detail the 2x version lacks. Python `screenshot_b64(region)`.
    pub fn zoom_box(&self, region: [f64; 4]) -> Result<(Rect, (u32, u32)), ScaleError> {
        let (x0, y0) = self.to_screen(region[0], region[1])?;
        let (x1, y1) = self.to_screen(region[2], region[3])?;
        if x1 <= x0 || y1 <= y0 {
            return invalid("region must have x1 > x0 and y1 > y0".into());
        }
        let (bw, bh) = (x1 - x0, y1 - y0);
        let s = (f64::from(self.long_edge) / f64::from(bw.max(bh))).min(ZOOM_MAX_UPSCALE);
        let out = (
            ((f64::from(bw) * s) as u32).max(1),
            ((f64::from(bh) * s) as u32).max(1),
        );
        Ok((Rect::new(x0, y0, x1, y1), out))
    }

    /// The same, from a JSON `region` that must be `[x0, y0, x1, y1]`.
    pub fn zoom_box_value(&self, region: &Value) -> Result<(Rect, (u32, u32)), ScaleError> {
        match region {
            Value::Array(items) if items.len() == 4 => {
                let mut r = [0.0; 4];
                for (slot, v) in r.iter_mut().zip(items) {
                    *slot = to_f64(v).ok_or_else(|| {
                        ScaleError::Invalid("region must be [x0, y0, x1, y1]".into())
                    })?;
                }
                self.zoom_box(r)
            }
            _ => invalid("region must be [x0, y0, x1, y1]".into()),
        }
    }
}

// ------------------------------------------------------------------------------------ per call

/// The per-call options `run_many` resets and layers (D1.1, 2026-09-21: a caller's earlier choice of
/// norm1000, a smaller screenshot or a second monitor used to leak into a later call from a
/// different caller). Every call starts from the configured defaults; its own args are applied on
/// top; nothing carries over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallScale {
    /// The convention for this call.
    pub coords: Coords,
    /// The screenshot long edge for this call, 128..=4096.
    pub long_edge: u32,
    /// A 1-based display chosen by the call, if any.
    pub display_index: Option<usize>,
}

impl CallScale {
    /// The defaults from config, with this call's `shot_long`, `coords` and `display` applied.
    /// A present, truthy `shot_long` is clamped to 128..=4096; `coords` counts only when it is
    /// exactly "pixels" or "norm1000". Python `run_many` preamble.
    pub fn from_args(cfg: &Config, args: &Value) -> Result<Self, ScaleError> {
        let mut coords = Coords::from_name(&cfg.coords());
        let mut long_edge = clamp_shot_long(cfg.shot_long());
        if let Some(raw) = args.get("shot_long")
            && crate::config::truthy(raw)
        {
            let n = to_i64(raw).ok_or_else(|| {
                ScaleError::Invalid(format!("shot_long must be a number, got {raw}"))
            })?;
            long_edge = clamp_shot_long(n);
        }
        match args.get("coords").and_then(Value::as_str) {
            Some("pixels") => coords = Coords::Pixels,
            Some("norm1000") => coords = Coords::Norm1000,
            _ => {}
        }
        let display_index = match args.get("display") {
            Some(v) if crate::config::truthy(v) => {
                let n = to_i64(v).ok_or_else(|| {
                    ScaleError::Invalid(format!("display must be a number, got {v}"))
                })?;
                Some(usize::try_from(n).unwrap_or(0))
            }
            _ => None,
        };
        Ok(Self {
            coords,
            long_edge,
            display_index,
        })
    }
}

fn clamp_shot_long(n: i64) -> u32 {
    n.clamp(SHOT_LONG_MIN, SHOT_LONG_MAX) as u32
}

// ------------------------------------------------------------------------------------ displays

/// Monitors as `(left, top, width, height)` rects, primary (the one at 0,0) first, then by left
/// edge. Python `monitors()` sort: `mons.sort(key=lambda r: (r[0] != 0 or r[1] != 0, r[0]))`.
pub fn sort_displays(displays: &mut [Display]) {
    displays.sort_by_key(|d| (d.rect.left != 0 || d.rect.top != 0, d.rect.left));
}

/// Per-call monitor choice, like Claude's `switch_display`: 1 = primary, 2 = next, ... Python
/// `set_display(n)`.
pub fn pick_display(displays: &[Display], n: usize) -> Result<Rect, ScaleError> {
    n.checked_sub(1)
        .and_then(|i| displays.get(i))
        .map(|d| d.rect)
        .ok_or_else(|| {
            ScaleError::Invalid(format!(
                "display {n} does not exist; this PC has {} monitor(s)",
                displays.len()
            ))
        })
}

/// The display containing a point, e.g. the centre of the foreground window.
/// Python `_fg_monitor()`.
pub fn display_containing(displays: &[Display], x: i32, y: i32) -> Option<Rect> {
    displays
        .iter()
        .find(|d| d.rect.contains(crate::backend::Point { x, y }))
        .map(|d| d.rect)
}

/// Parse a `display` config value `"x,y,w,h"` (spaces allowed). Python `display_rect()`:
/// `x, y, w, h = (int(v) for v in spec.split(","))`.
pub fn parse_display_spec(spec: &str) -> Result<Rect, ScaleError> {
    let parts: Vec<&str> = spec.split(',').collect();
    let nums: Result<Vec<i32>, _> = parts.iter().map(|p| p.trim().parse::<i32>()).collect();
    match nums {
        Ok(n) if n.len() == 4 => Ok(Rect::from_xywh(n[0], n[1], n[2], n[3])),
        _ => invalid(format!("display must be x,y,w,h integers, got {spec:?}")),
    }
}

/// The screen the model sees, in Python's order: a per-call choice, else the configured
/// `x,y,w,h`, else the monitor holding the foreground window, else the primary monitor.
/// Python `display_rect()`.
pub fn choose_display(
    per_call: Option<Rect>,
    spec: &str,
    foreground_monitor: Option<Rect>,
    primary: Rect,
) -> Result<Rect, ScaleError> {
    if let Some(r) = per_call {
        return Ok(r);
    }
    if !spec.trim().is_empty() {
        return parse_display_spec(spec);
    }
    Ok(foreground_monitor.unwrap_or(primary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Display;
    use crate::env::MapEnv;
    use crate::paths::Paths;
    use crate::testutil::TempDir;
    use serde_json::json;

    fn screen_2560() -> Rect {
        Rect::from_xywh(0, 0, 2560, 1440)
    }

    fn pixels(long_edge: u32) -> Scale {
        Scale::new(screen_2560(), Coords::Pixels, long_edge)
    }

    fn norm() -> Scale {
        Scale::new(screen_2560(), Coords::Norm1000, 1024)
    }

    // ---- ported from tests/test_computer.py ----

    #[test]
    fn pixel_coords_round_trip() {
        let s = pixels(1280);
        assert_eq!(s.shot_size(), (1280, 720));
        assert_eq!(s.to_screen(640.0, 360.0), Ok((1280, 720)));
        assert_eq!(s.from_screen(1280, 720), [640, 360]);
    }

    #[test]
    fn norm1000_coords() {
        let s = norm();
        assert_eq!(s.to_screen(500.0, 500.0), Ok((1279, 719)));
        let err = s.to_screen(1200.0, 10.0).unwrap_err().to_string();
        assert!(err.contains("0..1000"), "{err}");
        assert_eq!(err, "coordinate [1200, 10] is outside 0..1000");
    }

    #[test]
    fn out_of_screenshot_coordinate_refused() {
        let err = pixels(1280)
            .to_screen(1300.0, 10.0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside the 1280x720"), "{err}");
        assert_eq!(
            err,
            "coordinate [1300, 10] is outside the 1280x720 screenshot"
        );
    }

    #[test]
    fn tiny_zoom_box_grows() {
        let s = norm();
        assert_eq!(
            s.min_zoom([308.0, 314.0, 360.0, 325.0]),
            [254, 289, 414, 349]
        );
        assert_eq!(s.min_zoom([0.0, 0.0, 500.0, 300.0]), [0, 0, 500, 300]);
    }

    // ---- ported from tests/test_20260921_live_next.py ----

    #[test]
    fn from_screen_clamps_bottom_right_pixel() {
        let s = pixels(1280);
        assert_eq!(s.from_screen(2559, 1439), [1279, 719]);
        // must round-trip without "outside the screenshot"
        let back = s.from_screen(2559, 1439);
        s.to_screen(f64::from(back[0]), f64::from(back[1])).unwrap();
        assert_eq!(norm().from_screen(2600, -5), [1000, 0]);
    }

    // ---- ported from tests/test_20261008_speed_fixes.py (W2: shot_long) ----

    #[test]
    fn default_shot_long_is_1024() {
        let dir = TempDir::new("scale-default");
        let cfg = Config::new(Paths::in_dir(dir.path()), MapEnv::new());
        assert_eq!(cfg.shot_long(), 1024);
        let s = Scale::new(
            screen_2560(),
            Coords::from_name(&cfg.coords()),
            cfg.shot_long() as u32,
        );
        assert_eq!(s.shot_size(), (1024, 576));
        assert_eq!(s.to_screen(512.0, 288.0), Ok((1280, 720)));
    }

    #[test]
    fn run_many_resets_scale_to_the_new_default() {
        let dir = TempDir::new("scale-reset");
        let cfg = Config::new(Paths::in_dir(dir.path()), MapEnv::new());
        let call = CallScale::from_args(&cfg, &json!({"action": "mouse_move"})).unwrap();
        assert_eq!(call.long_edge, 1024);
        assert_eq!(call.coords, Coords::Pixels);
    }

    #[test]
    fn coordinates_use_the_calls_own_shot_long() {
        // The same [x, y] means a different screen point under a different shot_long, and a call
        // that sends none is read in the default scale again (never the previous call's).
        let dir = TempDir::new("scale-own");
        let cfg = Config::new(Paths::in_dir(dir.path()), MapEnv::new());
        let mut seen = Vec::new();
        for args in [
            json!({"action": "mouse_move", "shot_long": 640}),
            json!({"action": "mouse_move"}),
            json!({"action": "mouse_move", "shot_long": 1280}),
        ] {
            let call = CallScale::from_args(&cfg, &args).unwrap();
            let s = Scale::new(screen_2560(), call.coords, call.long_edge);
            seen.push(s.to_screen(100.0, 100.0).unwrap());
        }
        assert_eq!(seen, [(400, 400), (250, 250), (200, 200)]);
    }

    #[test]
    fn shot_long_is_clamped() {
        let dir = TempDir::new("scale-clamp");
        let cfg = Config::new(Paths::in_dir(dir.path()), MapEnv::new());
        let low = CallScale::from_args(&cfg, &json!({"shot_long": 3})).unwrap();
        assert_eq!(low.long_edge, 128);
        let high = CallScale::from_args(&cfg, &json!({"shot_long": 99999})).unwrap();
        assert_eq!(high.long_edge, 4096);
        let text = CallScale::from_args(&cfg, &json!({"shot_long": "800"})).unwrap();
        assert_eq!(text.long_edge, 800);
        let zero = CallScale::from_args(&cfg, &json!({"shot_long": 0})).unwrap();
        assert_eq!(zero.long_edge, 1024, "a falsy shot_long means 'not sent'");
        assert!(CallScale::from_args(&cfg, &json!({"shot_long": "wide"})).is_err());
    }

    /// Ported helper `_zoomed_size`: the region corners are given in the shot's coordinates, picked
    /// so the SCREEN box is box_w x box_h.
    fn zoomed_size(box_w: i32, box_h: i32, long_edge: u32) -> (u32, u32) {
        let s = Scale::new(Rect::from_xywh(0, 0, 1280, 720), Coords::Pixels, long_edge);
        let f = s.factor();
        let region = [
            0.0,
            0.0,
            f64::from((f64::from(box_w) * f) as i32),
            f64::from((f64::from(box_h) * f) as i32),
        ];
        s.zoom_box(region).unwrap().1
    }

    #[test]
    fn zoom_never_upscales_more_than_2x() {
        // 400x300 used to become 1280x960 (3.2x); now 2x the region's own pixels
        assert_eq!(zoomed_size(400, 300, 1280), (800, 600));
    }

    #[test]
    fn zoom_never_exceeds_shot_long() {
        let (w, h) = zoomed_size(400, 300, 500);
        // 1.25x, capped by shot_long (not 3.2x or even 2x)
        assert!(
            w.max(h) == 500 && (i64::from(h) - 375).abs() <= 2,
            "{w}x{h}"
        );
    }

    #[test]
    fn zoom_still_downscales_a_big_region() {
        let (w, h) = zoomed_size(1200, 600, 640);
        assert_eq!((w, h), (640, 320));
    }

    // ---- extra coverage ----

    #[test]
    fn a_second_monitor_keeps_its_offset() {
        // A display to the left of the primary has negative coordinates.
        let left = Rect::from_xywh(-1920, 0, 1920, 1080);
        let s = Scale::new(left, Coords::Pixels, 960);
        assert_eq!(s.shot_size(), (960, 540));
        assert_eq!(s.to_screen(0.0, 0.0), Ok((-1920, 0)));
        assert_eq!(s.to_screen(480.0, 270.0), Ok((-960, 540)));
        assert_eq!(s.from_screen(-960, 540), [480, 270]);
        let above = Scale::new(
            Rect::from_xywh(0, -1080, 1920, 1080),
            Coords::Norm1000,
            1024,
        );
        assert_eq!(above.to_screen(1000.0, 0.0), Ok((1919, -1080)));
        assert_eq!(above.from_screen(1919, -1080), [1000, 0]);
    }

    // ---- ported from test_20261008d_origin.py: the origin is added after truncating, so a monitor
    // whose origin is negative is not a pixel off.

    fn left_monitor(coords: Coords) -> Scale {
        // a native size shot, so pixels are screen pixels
        Scale::new(Rect::from_xywh(-1920, 0, 1920, 1080), coords, 1920)
    }

    #[test]
    fn norm1000_on_a_monitor_with_a_negative_origin() {
        let s = left_monitor(Coords::Norm1000);
        assert_eq!(s.to_screen(0.0, 0.0), Ok((-1920, 0)));
        // 959.5 and 539.5 truncate, then the origin is added
        assert_eq!(s.to_screen(500.0, 500.0), Ok((-1920 + 959, 539)));
        assert_eq!(s.to_screen(1000.0, 1000.0), Ok((-1, 1079)));
    }

    #[test]
    fn pixels_on_a_monitor_with_a_negative_origin() {
        let s = left_monitor(Coords::Pixels);
        assert_eq!(s.to_screen(0.0, 0.0), Ok((-1920, 0)));
        assert_eq!(s.to_screen(960.0, 540.0), Ok((-960, 540)));
        assert_eq!(s.to_screen(1919.0, 1079.0), Ok((-1, 1079)));
    }

    #[test]
    fn a_fractional_pixel_point_truncates_before_the_negative_origin() {
        // 1920 -> 1280 shot, scale 2/3: shot pixel 1 is screen pixel 1.5. Truncated first, that is
        // origin + 1; added first, -1918.5 would round toward zero to -1918, a pixel off.
        let s = Scale::new(
            Rect::from_xywh(-1920, -1080, 1920, 1080),
            Coords::Pixels,
            1280,
        );
        assert_eq!(s.to_screen(1.0, 1.0), Ok((-1920 + 1, -1080 + 1)));
        assert_eq!(s.to_screen(3.0, 3.0), Ok((-1920 + 4, -1080 + 4)));
    }

    #[test]
    fn a_positive_origin_is_unchanged() {
        let s = Scale::new(
            Rect::from_xywh(2560, 60, 1920, 1080),
            Coords::Norm1000,
            1920,
        );
        assert_eq!(s.to_screen(500.0, 500.0), Ok((2560 + 959, 60 + 539)));
    }

    #[test]
    fn a_small_screen_is_never_enlarged() {
        let s = Scale::new(Rect::from_xywh(0, 0, 800, 600), Coords::Pixels, 1024);
        assert_eq!(s.factor(), 1.0);
        assert_eq!(s.shot_size(), (800, 600));
        assert_eq!(s.to_screen(799.0, 599.0), Ok((799, 599)));
        assert!(s.to_screen(800.0, 0.0).is_err());
        assert!(s.to_screen(-1.0, 0.0).is_err());
    }

    #[test]
    fn from_screen_rounds_half_to_even_like_python() {
        // round(0.5) = 0 and round(1.5) = 2 in Python 3.
        let s = Scale::new(Rect::from_xywh(0, 0, 2000, 1000), Coords::Pixels, 1000);
        assert_eq!(s.factor(), 0.5);
        assert_eq!(s.from_screen(1, 0)[0], 0);
        assert_eq!(s.from_screen(3, 0)[0], 2);
    }

    #[test]
    fn to_screen_value_checks_the_shape_and_accepts_numeric_strings() {
        let s = pixels(1280);
        assert_eq!(s.to_screen_value(&json!([640, 360])), Ok((1280, 720)));
        assert_eq!(s.to_screen_value(&json!(["640", 360.0])), Ok((1280, 720)));
        for bad in [
            json!([1]),
            json!([1, 2, 3]),
            json!("x"),
            json!(null),
            json!(["a", 1]),
        ] {
            assert_eq!(
                s.to_screen_value(&bad),
                Err(ScaleError::Invalid("coordinate must be [x, y]".into())),
                "{bad}"
            );
        }
    }

    #[test]
    fn min_zoom_pulls_an_edge_box_inside_the_picture() {
        let s = pixels(1280);
        assert_eq!(
            s.min_zoom([1200.0, 700.0, 1280.0, 720.0]),
            [1137, 687, 1279, 719]
        );
        let n = norm();
        let grown = n.min_zoom([995.0, 995.0, 1000.0, 1000.0]);
        assert_eq!(grown, [917, 967, 999, 999]);
    }

    #[test]
    fn zoom_box_validates_the_region() {
        let s = pixels(1280);
        assert_eq!(
            s.zoom_box([10.0, 10.0, 10.0, 50.0])
                .unwrap_err()
                .to_string(),
            "region must have x1 > x0 and y1 > y0"
        );
        assert!(s.zoom_box([10.0, 10.0, 5000.0, 50.0]).is_err());
        let (rect, size) = s.zoom_box([0.0, 0.0, 640.0, 360.0]).unwrap();
        assert_eq!(rect, Rect::new(0, 0, 1280, 720));
        assert_eq!(size, (1280, 720), "capped by shot_long, below 2x");
        assert_eq!(
            s.zoom_box_value(&json!([1, 2, 3])).unwrap_err().to_string(),
            "region must be [x0, y0, x1, y1]"
        );
        assert!(s.zoom_box_value(&json!([0, 0, 100, 50])).is_ok());
    }

    #[test]
    fn per_call_options_pick_coords_and_display() {
        let dir = TempDir::new("scale-call");
        let cfg = Config::new(
            Paths::in_dir(dir.path()),
            MapEnv::new().with("WINHANDS_COORDS", "norm1000"),
        );
        let default = CallScale::from_args(&cfg, &json!({})).unwrap();
        assert_eq!(default.coords, Coords::Norm1000, "the configured default");
        let call = CallScale::from_args(&cfg, &json!({"coords": "pixels", "display": 2})).unwrap();
        assert_eq!((call.coords, call.display_index), (Coords::Pixels, Some(2)));
        let junk = CallScale::from_args(&cfg, &json!({"coords": "feet"})).unwrap();
        assert_eq!(junk.coords, Coords::Norm1000, "an unknown name is ignored");
        let next = CallScale::from_args(&cfg, &json!({})).unwrap();
        assert_eq!(
            next.display_index, None,
            "nothing carries over between calls"
        );
    }

    fn display(id: u32, rect: Rect, primary: bool) -> Display {
        Display {
            id,
            name: String::new(),
            rect,
            scale: 1.0,
            primary,
        }
    }

    #[test]
    fn displays_sort_primary_first_then_by_left_edge() {
        let mut d = vec![
            display(3, Rect::from_xywh(1920, 0, 1920, 1080), false),
            display(2, Rect::from_xywh(-1920, 0, 1920, 1080), false),
            display(1, Rect::from_xywh(0, 0, 1920, 1080), true),
        ];
        sort_displays(&mut d);
        assert_eq!(d.iter().map(|x| x.id).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(pick_display(&d, 1), Ok(d[0].rect));
        assert_eq!(pick_display(&d, 3), Ok(d[2].rect));
        assert_eq!(
            pick_display(&d, 4).unwrap_err().to_string(),
            "display 4 does not exist; this PC has 3 monitor(s)"
        );
        assert!(pick_display(&d, 0).is_err());
        assert_eq!(display_containing(&d, -1, 5), Some(d[1].rect));
        assert_eq!(display_containing(&d, 5000, 5), None);
    }

    #[test]
    fn the_display_is_chosen_call_then_config_then_foreground_then_primary() {
        let primary = Rect::from_xywh(0, 0, 1920, 1080);
        let second = Rect::from_xywh(1920, 0, 1920, 1080);
        let pick = Rect::from_xywh(5, 5, 50, 50);
        assert_eq!(
            choose_display(Some(pick), "1,2,3,4", Some(second), primary),
            Ok(pick)
        );
        assert_eq!(
            choose_display(None, "10, 20, 300, 200", Some(second), primary),
            Ok(Rect::from_xywh(10, 20, 300, 200))
        );
        assert_eq!(choose_display(None, "", Some(second), primary), Ok(second));
        assert_eq!(choose_display(None, "   ", None, primary), Ok(primary));
        assert!(choose_display(None, "1,2,3", None, primary).is_err());
        assert!(parse_display_spec("a,b,c,d").is_err());
    }

    #[test]
    fn python_style_numbers_in_messages() {
        assert_eq!(py_num(1200.0), "1200");
        assert_eq!(py_num(12.5), "12.5");
        assert_eq!(py_pair(-1.0, 0.0), "[-1, 0]");
    }
}
