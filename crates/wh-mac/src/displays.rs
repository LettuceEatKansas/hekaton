//! The active displays: bounds in global points, backing scale, primary first.

use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode, CGGetActiveDisplayList,
    CGMainDisplayID,
};
use wh_core::backend::{BackendError, BackendResult, Display, Rect};

use crate::geom::{DisplaySpec, rect_from_f64};

/// One display with its `CGDirectDisplayID`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DisplayInfo {
    /// The CoreGraphics display id.
    pub(crate) cg_id: u32,
    /// Bounds in global points.
    pub(crate) rect: Rect,
    /// Backing pixels per point.
    pub(crate) scale: f64,
    /// The main display.
    pub(crate) primary: bool,
}

impl DisplayInfo {
    pub(crate) fn spec(&self) -> DisplaySpec {
        DisplaySpec {
            rect: self.rect,
            scale: self.scale,
        }
    }
}

/// Order displays primary first, then by left edge, then top.
pub(crate) fn sort_displays(list: &mut [DisplayInfo]) {
    list.sort_by_key(|d| (!d.primary, d.rect.left, d.rect.top));
}

/// Read the active displays.
pub(crate) fn list() -> BackendResult<Vec<DisplayInfo>> {
    let mut ids = [0u32; 16];
    let mut count: u32 = 0;
    // SAFETY: `ids` holds 16 entries, matching max_displays; `count` is a valid out pointer.
    let err = unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut count) };
    if err.0 != 0 {
        return Err(BackendError::Os(format!(
            "CGGetActiveDisplayList failed with code {}",
            err.0
        )));
    }
    let main = CGMainDisplayID();
    let mut out: Vec<DisplayInfo> = ids
        .iter()
        .take(count as usize)
        .map(|&id| {
            let b = CGDisplayBounds(id);
            let rect = rect_from_f64(b.origin.x, b.origin.y, b.size.width, b.size.height);
            DisplayInfo {
                cg_id: id,
                rect,
                scale: backing_scale(id),
                primary: id == main,
            }
        })
        .collect();
    if out.is_empty() {
        return Err(BackendError::Os(
            "macOS reports no active display (a headless Mac, or the display is asleep)".into(),
        ));
    }
    sort_displays(&mut out);
    Ok(out)
}

/// Pixels per point from the display mode (2.0 on Retina, 1.0 otherwise).
fn backing_scale(id: u32) -> f64 {
    let Some(mode) = CGDisplayCopyDisplayMode(id) else {
        return 1.0;
    };
    let points = CGDisplayMode::width(Some(&mode));
    let pixels = CGDisplayMode::pixel_width(Some(&mode));
    if points == 0 || pixels == 0 {
        1.0
    } else {
        pixels as f64 / points as f64
    }
}

/// The neutral records, ids 1-based in list order.
pub(crate) fn to_neutral(list: &[DisplayInfo]) -> Vec<Display> {
    list.iter()
        .enumerate()
        .map(|(i, d)| Display {
            id: i as u32 + 1,
            name: if d.primary {
                "Main display".to_string()
            } else {
                format!("Display {}", i + 1)
            },
            rect: d.rect,
            scale: d.scale,
            primary: d.primary,
        })
        .collect()
}

/// The bounding rect of all displays.
pub(crate) fn union(list: &[DisplayInfo]) -> Rect {
    let mut it = list.iter().map(|d| d.rect);
    let Some(first) = it.next() else {
        return Rect::default();
    };
    it.fold(first, |a, r| Rect {
        left: a.left.min(r.left),
        top: a.top.min(r.top),
        right: a.right.max(r.right),
        bottom: a.bottom.max(r.bottom),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: u32, x: i32, primary: bool) -> DisplayInfo {
        DisplayInfo {
            cg_id: id,
            rect: Rect::from_xywh(x, 0, 1000, 800),
            scale: 2.0,
            primary,
        }
    }

    #[test]
    fn primary_first_then_by_left_edge() {
        let mut v = vec![d(3, 2000, false), d(2, -1000, false), d(1, 0, true)];
        sort_displays(&mut v);
        assert_eq!(v.iter().map(|x| x.cg_id).collect::<Vec<_>>(), [1, 2, 3]);
    }

    #[test]
    fn union_covers_every_display() {
        let v = vec![d(1, 0, true), d(2, -1000, false)];
        assert_eq!(union(&v), Rect::new(-1000, 0, 1000, 800));
        assert_eq!(union(&[]), Rect::default());
    }

    #[test]
    fn neutral_records_are_one_based() {
        let n = to_neutral(&[d(7, 0, true), d(9, 1000, false)]);
        assert_eq!((n[0].id, n[1].id), (1, 2));
        assert!(n[0].primary && !n[1].primary);
        assert_eq!(n[0].scale, 2.0);
    }
}
