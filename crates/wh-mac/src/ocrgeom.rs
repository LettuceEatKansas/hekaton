//! Vision OCR geometry. Vision reports boxes normalised to the image with the origin at the
//! bottom left; the core wants pixel rects with the origin at the top left. Also the UTF-16 word
//! spans needed to ask Vision for per-word boxes (`NSRange` counts UTF-16 units).

use wh_core::backend::Rect;

/// A normalised, bottom-left-origin box to a pixel rect in an `img_w` by `img_h` image. Edges
/// round outward so the box never clips a glyph, and are clamped to the image.
pub(crate) fn flip_normalized(x: f64, y: f64, w: f64, h: f64, img_w: u32, img_h: u32) -> Rect {
    let (iw, ih) = (f64::from(img_w), f64::from(img_h));
    // The tiny epsilon keeps products that are whole numbers up to float noise (0.25 * 200) from
    // rounding outward by a whole pixel.
    let eps = 1e-6;
    let left = ((x * iw) + eps).floor();
    let right = (((x + w) * iw) - eps).ceil();
    let top = (((1.0 - y - h) * ih) + eps).floor();
    let bottom = (((1.0 - y) * ih) - eps).ceil();
    let clamp = |v: f64, max: u32| v.clamp(0.0, f64::from(max)) as i32;
    Rect {
        left: clamp(left, img_w),
        top: clamp(top, img_h),
        right: clamp(right, img_w),
        bottom: clamp(bottom, img_h),
    }
}

/// One whitespace-separated word of a recognised line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WordSpan {
    /// The word.
    pub(crate) text: String,
    /// Offset of its first UTF-16 unit in the line.
    pub(crate) start: usize,
    /// Length in UTF-16 units.
    pub(crate) len: usize,
}

/// Split a line into words, recording each word's UTF-16 range.
pub(crate) fn split_words(line: &str) -> Vec<WordSpan> {
    let mut out = Vec::new();
    let mut units = 0usize;
    let mut cur: Option<(usize, String)> = None;
    for ch in line.chars() {
        if ch.is_whitespace() {
            if let Some((start, text)) = cur.take() {
                out.push(WordSpan {
                    text,
                    start,
                    len: units - start,
                });
            }
        } else {
            cur.get_or_insert_with(|| (units, String::new())).1.push(ch);
        }
        units += ch.len_utf16();
    }
    if let Some((start, text)) = cur {
        out.push(WordSpan {
            text,
            start,
            len: units - start,
        });
    }
    out
}

/// Collapse runs of whitespace to single spaces and trim.
pub(crate) fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sort rects into reading order: by top edge, then left edge.
pub(crate) fn reading_order<T>(items: &mut [T], rect_of: impl Fn(&T) -> Rect) {
    items.sort_by_key(|it| {
        let r = rect_of(it);
        (r.top, r.left)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flips_the_vertical_axis() {
        // A box in the top-left corner of a 200x100 image: Vision y is measured from the bottom.
        let r = flip_normalized(0.0, 0.8, 0.25, 0.2, 200, 100);
        assert_eq!(r, Rect::new(0, 0, 50, 20));
        // The bottom-right corner.
        let r = flip_normalized(0.75, 0.0, 0.25, 0.1, 200, 100);
        assert_eq!(r, Rect::new(150, 90, 200, 100));
    }

    #[test]
    fn centre_box_and_outward_rounding() {
        let r = flip_normalized(0.25, 0.25, 0.5, 0.5, 100, 100);
        assert_eq!(r, Rect::new(25, 25, 75, 75));
        // 0.333 of 100 is 33.3: left rounds down, right edge 66.6 rounds up.
        let r = flip_normalized(0.333, 0.333, 0.333, 0.333, 100, 100);
        assert_eq!(r, Rect::new(33, 33, 67, 67));
    }

    #[test]
    fn boxes_outside_the_image_are_clamped() {
        let r = flip_normalized(-0.1, -0.1, 1.3, 1.3, 100, 50);
        assert_eq!(r, Rect::new(0, 0, 100, 50));
    }

    #[test]
    fn words_carry_utf16_ranges() {
        let w = split_words("  Save  As… now ");
        let got: Vec<(&str, usize, usize)> = w
            .iter()
            .map(|s| (s.text.as_str(), s.start, s.len))
            .collect();
        assert_eq!(got, [("Save", 2, 4), ("As…", 8, 3), ("now", 12, 3)]);
    }

    #[test]
    fn astral_characters_count_as_two_units() {
        let w = split_words("a 😀b c");
        assert_eq!(w[1].text, "😀b");
        assert_eq!((w[1].start, w[1].len), (2, 3));
        assert_eq!((w[2].start, w[2].len), (6, 1));
    }

    #[test]
    fn empty_and_blank_lines_have_no_words() {
        assert!(split_words("").is_empty());
        assert!(split_words(" \t ").is_empty());
    }

    #[test]
    fn whitespace_collapses() {
        assert_eq!(collapse_ws("  a \n b\t c  "), "a b c");
    }

    #[test]
    fn reading_order_is_top_then_left() {
        let mut v = vec![
            Rect::new(50, 10, 60, 20),
            Rect::new(0, 30, 10, 40),
            Rect::new(0, 10, 10, 20),
        ];
        reading_order(&mut v, |r| *r);
        assert_eq!(v[0], Rect::new(0, 10, 10, 20));
        assert_eq!(v[1], Rect::new(50, 10, 60, 20));
        assert_eq!(v[2], Rect::new(0, 30, 10, 40));
    }
}
