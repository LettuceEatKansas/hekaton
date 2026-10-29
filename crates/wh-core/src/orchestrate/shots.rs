//! Pictures and waiting. Port of `computer.screenshot_b64` (display grab, resize, click marker,
//! encode), `computer.settle`, `computer._settle_floor_s`, and the OCR plumbing around
//! `core._ocr_winrt` (the word-to-row grouping the backend leaves to the core).

use super::error::{StepError, StepResult};
use super::{Ctx, secs};
use crate::backend::{Image, OcrResult, Rect};
use crate::imaging::{self, ImageFormat};
use crate::settle::{SettleParams, settle_floor, settle_with_frame};

/// One recognised run of text in the pixels of the image that was OCR'd.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OcrRow {
    /// Its box.
    pub rect: Rect,
    /// Its text.
    pub text: String,
}

/// Group a backend's OCR output into rows the way `core._ocr_winrt` did.
///
/// With `words_only`, one row per word, for aiming at a single word (the double-click snap). Otherwise
/// a recognised line is one row, except that a menu bar reads as one line ("STORE LIBRARY COMMUNITY")
/// while each word is its own button: the line splits where the gap between words is wider than the
/// text is tall (1.2 times); a normal sentence has gaps of a fraction of that and stays one row.
pub(crate) fn ocr_rows(result: &OcrResult, words_only: bool) -> Vec<OcrRow> {
    let mut rows = Vec::new();
    for line in &result.lines {
        if line.words.is_empty() {
            // an engine that reports lines without words: the line is the row
            if !words_only && !line.text.trim().is_empty() {
                rows.push(OcrRow {
                    rect: line.rect,
                    text: line.text.clone(),
                });
            }
            continue;
        }
        if words_only {
            for w in &line.words {
                rows.push(OcrRow {
                    rect: w.rect,
                    text: w.text.clone(),
                });
            }
            continue;
        }
        let height = line
            .words
            .iter()
            .map(|w| w.rect.height())
            .max()
            .filter(|h| *h != 0)
            .unwrap_or(1);
        let mut groups: Vec<Vec<&crate::backend::OcrWord>> = vec![vec![&line.words[0]]];
        for w in &line.words[1..] {
            let prev = groups
                .last()
                .and_then(|g| g.last())
                .map_or(0, |p| p.rect.right);
            if f64::from(w.rect.left - prev) > f64::from(height) * 1.2 {
                groups.push(vec![w]);
            } else if let Some(g) = groups.last_mut() {
                g.push(w);
            }
        }
        for g in groups {
            let rect = Rect::new(
                g.iter().map(|w| w.rect.left).min().unwrap_or(0),
                g.iter().map(|w| w.rect.top).min().unwrap_or(0),
                g.iter().map(|w| w.rect.right).max().unwrap_or(0),
                g.iter().map(|w| w.rect.bottom).max().unwrap_or(0),
            );
            rows.push(OcrRow {
                rect,
                text: g
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            });
        }
    }
    rows
}

impl super::Computer {
    /// The whole display the model would see, scaled like a `computer` screenshot and encoded as a PNG
    /// whatever the `img` setting says. No click marker. For the daemon's legacy `screenshot` route,
    /// which always answered with PNG bytes.
    pub fn display_png(&self) -> StepResult<Vec<u8>> {
        let who = crate::caller::Caller::unknown();
        let ctx = self.read_ctx(&who);
        let scale = ctx.scale();
        let (w, h) = scale.shot_size();
        let img = ctx.grab(scale.display)?;
        let img = imaging::resize(&img, w.max(1) as u32, h.max(1) as u32)?;
        Ok(imaging::encode_png(&img)?)
    }
}

impl Ctx<'_> {
    pub(crate) fn image_format(&self) -> ImageFormat {
        ImageFormat::from_name(&self.cfg().img_format())
    }

    /// The MIME type of the pictures this call returns (Python `mime()`).
    pub(crate) fn mime(&self) -> &'static str {
        self.image_format().mime()
    }

    /// Capture a rectangle of the screen (Python `_grab`).
    pub(crate) fn grab(&self, region: Rect) -> StepResult<Image> {
        Ok(self
            .be()
            .capture(crate::backend::CaptureTarget::Region(region))?)
    }

    /// Recognise text in `img`. `None` when no engine answers (the caller treats that as no text).
    pub(crate) fn ocr_rows_of(&self, img: &Image, words_only: bool) -> Option<Vec<OcrRow>> {
        self.be().ocr(img).ok().map(|r| ocr_rows(&r, words_only))
    }

    /// The whole display at `shot_size()`, or a zoomed sub-rectangle given in model coordinates,
    /// base64 encoded. A zoom is never enlarged past 2x its own pixels or past the call's `shot_long`
    /// (2026-10-08, W3). The last click is ringed on a whole-screen picture. Python `screenshot_b64`.
    pub(crate) fn screenshot_b64(&self, region: Option<[f64; 4]>) -> StepResult<String> {
        let scale = self.scale();
        let display = scale.display;
        let (rect, (out_w, out_h)) = match region {
            None => {
                let (w, h) = scale.shot_size();
                (display, (w.max(1) as u32, h.max(1) as u32))
            }
            Some(r) => scale.zoom_box(r)?,
        };
        let img = self.grab(rect)?;
        self.encode_frame(&img, out_w, out_h, region.is_none())
    }

    /// Encode the final settled pixels instead of capturing the same screen a second time.
    pub(crate) fn encode_full_frame(&self, img: &Image) -> StepResult<String> {
        let (w, h) = self.scale().shot_size();
        self.encode_frame(img, w.max(1) as u32, h.max(1) as u32, true)
    }

    fn encode_frame(&self, img: &Image, out_w: u32, out_h: u32, whole: bool) -> StepResult<String> {
        self.check_cancelled()?;
        if !self.cfg().mark() && img.width() == out_w && img.height() == out_h {
            return Ok(imaging::base64(&imaging::encode(img, self.image_format())?));
        }
        let display = self.scale().display;
        let mut img = imaging::resize(img, out_w, out_h)?;
        if whole
            && self.cfg().mark()
            && let Some(p) = self.c.mem().last_point
        {
            let px = (f64::from(p.x - display.left) * f64::from(out_w) / f64::from(display.width()))
                as i32;
            let py = (f64::from(p.y - display.top) * f64::from(out_h) / f64::from(display.height()))
                as i32;
            imaging::draw_marker(&mut img, px, py);
        }
        Ok(imaging::base64(&imaging::encode(
            &img,
            self.image_format(),
        )?))
    }

    /// Wait until the screen stops changing, instead of a fixed sleep. A static app is ready in about
    /// 60 ms; an animation gets up to `max_s` (default `settle_ms`). `calm_needed` is how many calm
    /// reads in a row end the wait: 2 for the trailing settle, 1 for the mid-batch step settle where
    /// the next step is about to act anyway (2026-10-08, speed fix W4). Returns seconds watched.
    /// Python `settle`.
    pub(crate) fn settle(
        &self,
        max_s: Option<f64>,
        min_s: f64,
        calm_needed: u32,
    ) -> StepResult<f64> {
        self.settle_looks(max_s, min_s, calm_needed)
            .map(|s| s.elapsed.as_secs_f64())
    }

    /// [`Self::settle`] returning the looks; [`crate::settle::SettledFrame::full`] or a fresh grab
    /// gives the response its full-size pixels.
    pub(crate) fn settle_looks(
        &self,
        max_s: Option<f64>,
        min_s: f64,
        calm_needed: u32,
    ) -> StepResult<crate::settle::SettledFrame> {
        self.check_cancelled()?;
        let mut params = SettleParams::from_config(self.cfg())
            .with_min(secs(min_s))
            .with_calm_needed(calm_needed);
        if let Some(m) = max_s {
            params = params.with_max(secs(m));
        }
        settle_with_frame(
            self.be(),
            self.clock(),
            self.display_rect(),
            &params,
            &|| {
                self.check_cancelled()
                    .map_err(|e| crate::backend::BackendError::Denied(e.message().to_string()))
            },
        )
        .map_err(StepError::from)
    }

    /// Minimum watch time of the trailing settle after an acting call: `settle_floor_ms` (130 by
    /// default) for ordinary windows, 450 ms where late fades happen (a fullscreen window or a window
    /// that exposes no UIA elements: a game engine draws everything itself). Python `_settle_floor_s`.
    pub(crate) fn settle_floor_s(&self) -> f64 {
        settle_floor(
            self.cfg(),
            self.be(),
            self.clock(),
            &self.c.probe,
            self.display_rect(),
        )
        .as_secs_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{OcrLine, OcrWord};

    fn word(text: &str, l: i32, t: i32, r: i32, b: i32) -> OcrWord {
        OcrWord {
            text: text.into(),
            rect: Rect::new(l, t, r, b),
            confidence: 1.0,
        }
    }

    fn line(words: Vec<OcrWord>) -> OcrLine {
        OcrLine {
            text: words
                .iter()
                .map(|w| w.text.clone())
                .collect::<Vec<_>>()
                .join(" "),
            rect: Rect::default(),
            words,
            confidence: 1.0,
        }
    }

    #[test]
    fn a_sentence_stays_one_row_and_a_menu_bar_splits() {
        let sentence = line(vec![
            word("status:", 0, 0, 60, 20),
            word("draft", 66, 0, 110, 20),
        ]);
        let menu = line(vec![
            word("STORE", 0, 40, 50, 60),
            word("LIBRARY", 200, 40, 270, 60),
            word("COMMUNITY", 420, 40, 520, 60),
        ]);
        let result = OcrResult {
            lines: vec![sentence, menu],
            engine: "fake".into(),
        };
        let rows = ocr_rows(&result, false);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].text, "status: draft");
        assert_eq!(rows[0].rect, Rect::new(0, 0, 110, 20));
        assert_eq!(rows[1].text, "STORE");
        assert_eq!(rows[3].rect, Rect::new(420, 40, 520, 60));
    }

    #[test]
    fn words_only_gives_one_row_per_word() {
        let result = OcrResult {
            lines: vec![line(vec![
                word("a", 0, 0, 10, 10),
                word("b", 12, 0, 20, 10),
            ])],
            engine: "fake".into(),
        };
        let rows = ocr_rows(&result, true);
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn a_line_without_words_is_a_row_of_its_own() {
        let result = OcrResult {
            lines: vec![OcrLine {
                text: "whole line".into(),
                rect: Rect::new(1, 2, 3, 4),
                words: vec![],
                confidence: 1.0,
            }],
            engine: "fake".into(),
        };
        let rows = ocr_rows(&result, false);
        assert_eq!(
            rows,
            vec![OcrRow {
                rect: Rect::new(1, 2, 3, 4),
                text: "whole line".into()
            }]
        );
        assert!(ocr_rows(&result, true).is_empty());
    }
}
