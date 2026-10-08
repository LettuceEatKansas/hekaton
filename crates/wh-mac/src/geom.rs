//! Points versus pixels, and how a screen region splits across displays. Input coordinates on a
//! Mac are points (global, origin at the main display's top left, y down); screenshots are pixels,
//! `scale` pixels per point per display. Pure arithmetic, tested on every OS.

use wh_core::backend::{Point, Rect};

/// One display as the planner needs it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DisplaySpec {
    /// Bounds in global points.
    pub(crate) rect: Rect,
    /// Backing pixels per point.
    pub(crate) scale: f64,
}

/// One display's share of a capture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CapturePart {
    /// Index into the display slice given to [`plan_capture`].
    pub(crate) display: usize,
    /// The part in global points.
    pub(crate) region: Rect,
    /// The part in points relative to its display's top left (ScreenCaptureKit `sourceRect`).
    pub(crate) relative: Rect,
    /// Where the part lands on the canvas, in pixels.
    pub(crate) dest_x: u32,
    /// Where the part lands on the canvas, in pixels.
    pub(crate) dest_y: u32,
    /// Output size in pixels at the canvas scale.
    pub(crate) width: u32,
    /// Output size in pixels at the canvas scale.
    pub(crate) height: u32,
}

/// How to build one image of a region from per-display captures.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CapturePlan {
    /// Canvas pixels per point: the largest scale among the displays involved.
    pub(crate) scale: f64,
    /// Canvas size in pixels.
    pub(crate) width: u32,
    /// Canvas size in pixels.
    pub(crate) height: u32,
    /// The region's top left in points: the image's origin.
    pub(crate) origin: Point,
    /// One entry per display the region touches.
    pub(crate) parts: Vec<CapturePart>,
}

/// Points to pixels.
pub(crate) fn to_pixels(points: i32, scale: f64) -> u32 {
    (f64::from(points) * scale).round().max(0.0) as u32
}

/// Split `region` (global points) over the displays it touches. `None` when it touches none.
pub(crate) fn plan_capture(displays: &[DisplaySpec], region: Rect) -> Option<CapturePlan> {
    if region.is_empty() {
        return None;
    }
    let mut hit: Vec<(usize, Rect)> = Vec::new();
    for (i, d) in displays.iter().enumerate() {
        if let Some(part) = d.rect.intersect(&region) {
            hit.push((i, part));
        }
    }
    if hit.is_empty() {
        return None;
    }
    let scale = hit
        .iter()
        .map(|(i, _)| displays[*i].scale)
        .fold(1.0_f64, f64::max);
    let width = to_pixels(region.width(), scale).max(1);
    let height = to_pixels(region.height(), scale).max(1);
    let parts = hit
        .into_iter()
        .map(|(i, part)| {
            let d = &displays[i];
            let dest_x = to_pixels(part.left - region.left, scale);
            let dest_y = to_pixels(part.top - region.top, scale);
            CapturePart {
                display: i,
                region: part,
                relative: part.translate(-d.rect.left, -d.rect.top),
                dest_x,
                dest_y,
                width: to_pixels(part.width(), scale)
                    .max(1)
                    .min(width.saturating_sub(dest_x).max(1)),
                height: to_pixels(part.height(), scale)
                    .max(1)
                    .min(height.saturating_sub(dest_y).max(1)),
            }
        })
        .collect();
    Some(CapturePlan {
        scale,
        width,
        height,
        origin: Point {
            x: region.left,
            y: region.top,
        },
        parts,
    })
}

/// Which display (by index) holds the point, else the nearest by centre distance.
pub(crate) fn display_for_point(displays: &[DisplaySpec], p: Point) -> Option<usize> {
    if let Some(i) = displays.iter().position(|d| d.rect.contains(p)) {
        return Some(i);
    }
    displays
        .iter()
        .enumerate()
        .min_by_key(|(_, d)| {
            let c = d.rect.center();
            let (dx, dy) = (i64::from(c.x - p.x), i64::from(c.y - p.y));
            dx * dx + dy * dy
        })
        .map(|(i, _)| i)
}

/// Round a floating-point global rect to the integer point grid, growing it outward so nothing
/// the element covers is lost.
pub(crate) fn rect_from_f64(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect {
        left: x.floor() as i32,
        top: y.floor() as i32,
        right: (x + w).ceil() as i32,
        bottom: (y + h).ceil() as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retina(x: i32, y: i32, w: i32, h: i32) -> DisplaySpec {
        DisplaySpec {
            rect: Rect::from_xywh(x, y, w, h),
            scale: 2.0,
        }
    }

    fn plain(x: i32, y: i32, w: i32, h: i32) -> DisplaySpec {
        DisplaySpec {
            rect: Rect::from_xywh(x, y, w, h),
            scale: 1.0,
        }
    }

    #[test]
    fn point_to_pixel_conversion_rounds_and_clamps() {
        assert_eq!(to_pixels(100, 2.0), 200);
        assert_eq!(to_pixels(101, 1.5), 152);
        assert_eq!(to_pixels(-5, 2.0), 0);
    }

    #[test]
    fn one_retina_display_region() {
        let d = [retina(0, 0, 1440, 900)];
        let plan = plan_capture(&d, Rect::from_xywh(100, 50, 300, 200)).unwrap();
        assert_eq!((plan.scale, plan.width, plan.height), (2.0, 600, 400));
        assert_eq!(plan.origin, Point::new(100, 50));
        assert_eq!(plan.parts.len(), 1);
        let p = plan.parts[0];
        assert_eq!(p.relative, Rect::from_xywh(100, 50, 300, 200));
        assert_eq!((p.dest_x, p.dest_y, p.width, p.height), (0, 0, 600, 400));
    }

    #[test]
    fn region_spanning_two_displays_uses_the_larger_scale() {
        // A 1x external monitor to the right of a 2x laptop panel.
        let d = [retina(0, 0, 1440, 900), plain(1440, 0, 1920, 1080)];
        let plan = plan_capture(&d, Rect::new(1400, 100, 1500, 200)).unwrap();
        assert_eq!(plan.scale, 2.0);
        assert_eq!((plan.width, plan.height), (200, 200));
        assert_eq!(plan.parts.len(), 2);
        let left = plan.parts[0];
        assert_eq!(left.region, Rect::new(1400, 100, 1440, 200));
        assert_eq!(left.relative, Rect::new(1400, 100, 1440, 200));
        assert_eq!((left.dest_x, left.width), (0, 80));
        let right = plan.parts[1];
        assert_eq!(right.relative, Rect::new(0, 100, 60, 200));
        assert_eq!((right.dest_x, right.dest_y, right.width), (80, 0, 120));
        assert_eq!(left.width + right.width, plan.width);
    }

    #[test]
    fn display_left_of_the_primary_has_negative_origin() {
        let d = [retina(0, 0, 1440, 900), plain(-1920, -100, 1920, 1080)];
        let plan = plan_capture(&d, Rect::new(-100, 0, 100, 50)).unwrap();
        assert_eq!(plan.origin, Point::new(-100, 0));
        let ext = plan
            .parts
            .iter()
            .find(|p| p.display == 1)
            .expect("external part");
        assert_eq!(ext.relative, Rect::new(1820, 100, 1920, 150));
        assert_eq!(ext.dest_x, 0);
    }

    #[test]
    fn region_off_every_display_has_no_plan() {
        let d = [retina(0, 0, 100, 100)];
        assert!(plan_capture(&d, Rect::new(500, 500, 600, 600)).is_none());
        assert!(plan_capture(&d, Rect::new(10, 10, 10, 50)).is_none());
        assert!(plan_capture(&[], Rect::new(0, 0, 10, 10)).is_none());
    }

    #[test]
    fn region_hanging_off_the_edge_keeps_its_size() {
        let d = [plain(0, 0, 100, 100)];
        let plan = plan_capture(&d, Rect::new(50, 50, 150, 150)).unwrap();
        assert_eq!((plan.width, plan.height), (100, 100));
        assert_eq!(plan.parts[0].width, 50);
        assert_eq!(plan.parts[0].height, 50);
    }

    #[test]
    fn picks_the_display_for_a_point() {
        let d = [retina(0, 0, 1440, 900), plain(1440, 0, 1920, 1080)];
        assert_eq!(display_for_point(&d, Point::new(10, 10)), Some(0));
        assert_eq!(display_for_point(&d, Point::new(1500, 10)), Some(1));
        assert_eq!(display_for_point(&d, Point::new(-500, 400)), Some(0));
        assert_eq!(display_for_point(&[], Point::new(0, 0)), None);
    }

    #[test]
    fn float_rects_grow_outward() {
        assert_eq!(
            rect_from_f64(10.4, 20.6, 100.2, 50.0),
            Rect::new(10, 20, 111, 71)
        );
        assert_eq!(
            rect_from_f64(-3.5, -1.0, 2.0, 2.0),
            Rect::new(-4, -1, -1, 1)
        );
    }
}
