//! OCR through Vision's `VNRecognizeTextRequest` (accurate level, language correction off: UI
//! text is mostly short labels and file names, which correction mangles).

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSDictionary, NSRange};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRecognizedText, VNRecognizedTextObservation,
    VNRequest, VNRequestTextRecognitionLevel,
};
use wh_core::backend::{BackendError, BackendResult, Image, OcrLine, OcrResult, OcrWord, Rect};

use crate::capture::rgba_to_cgimage;
use crate::ocrgeom::{collapse_ws, flip_normalized, reading_order, split_words};

/// The engine name reported in `/health` and in [`OcrResult::engine`].
pub(crate) const ENGINE: &str = "vision";

fn os(msg: impl Into<String>) -> BackendError {
    BackendError::Os(msg.into())
}

/// Recognise the text in an RGBA image. Boxes are in pixels of `image`, origin top left.
pub(crate) fn recognize(image: &Image) -> BackendResult<OcrResult> {
    let (iw, ih) = (image.width(), image.height());
    let mut keep = Vec::new();
    let cg = rgba_to_cgimage(image, &mut keep)?;
    // SAFETY: init with a live CGImage and an empty options dictionary.
    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            &cg,
            &NSDictionary::new(),
        )
    };
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(false);
    let as_request: &VNRequest = &request;
    let requests = NSArray::from_slice(&[as_request]);
    handler.performRequests_error(&requests).map_err(|e| {
        os(format!(
            "Vision text recognition failed: {}",
            e.localizedDescription()
        ))
    })?;
    let mut lines = Vec::new();
    if let Some(results) = request.results() {
        for obs in results.iter() {
            if let Some(line) = read_line(&obs, iw, ih) {
                lines.push(line);
            }
        }
    }
    reading_order(&mut lines, |l: &OcrLine| l.rect);
    drop(keep);
    Ok(OcrResult {
        lines,
        engine: ENGINE.to_string(),
    })
}

fn read_line(obs: &VNRecognizedTextObservation, iw: u32, ih: u32) -> Option<OcrLine> {
    let top: Retained<VNRecognizedText> = obs.topCandidates(1).iter().next()?;
    let raw = top.string().to_string();
    let text = collapse_ws(&raw);
    if text.is_empty() {
        return None;
    }
    let confidence = top.confidence();
    // SAFETY: reading the bounding box of a live observation.
    let b = unsafe { obs.boundingBox() };
    let rect = flip_normalized(b.origin.x, b.origin.y, b.size.width, b.size.height, iw, ih);
    let total16: usize = raw.chars().map(char::len_utf16).sum::<usize>().max(1);
    let words = split_words(&raw)
        .into_iter()
        .map(|w| {
            // SAFETY: the range lies inside the recognised string, measured in UTF-16 units.
            let boxed = unsafe { top.boundingBoxForRange_error(NSRange::new(w.start, w.len)) };
            let wrect = match boxed {
                Ok(r) => {
                    // SAFETY: reading the bounding box of a live observation.
                    let wb = unsafe { r.boundingBox() };
                    flip_normalized(
                        wb.origin.x,
                        wb.origin.y,
                        wb.size.width,
                        wb.size.height,
                        iw,
                        ih,
                    )
                }
                // Fall back to slicing the line box in proportion to the character offsets.
                Err(_) => proportional(rect, w.start, w.len, total16),
            };
            OcrWord {
                text: w.text,
                rect: wrect,
                confidence,
            }
        })
        .collect();
    Some(OcrLine {
        text,
        rect,
        words,
        confidence,
    })
}

/// A word's box as a share of the line box, by UTF-16 offset.
pub(crate) fn proportional(line: Rect, start: usize, len: usize, total: usize) -> Rect {
    let w = f64::from(line.width());
    let left = line.left + (w * start as f64 / total as f64).floor() as i32;
    let right = line.left + (w * (start + len) as f64 / total as f64).ceil() as i32;
    Rect {
        left,
        top: line.top,
        right: right.min(line.right).max(left),
        bottom: line.bottom,
    }
}

/// Load the models now so the first real call is fast.
pub(crate) fn warm() -> BackendResult<()> {
    let probe = Image::solid(96, 32, [255, 255, 255, 255]);
    recognize(&probe).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proportional_boxes_split_the_line() {
        let line = Rect::new(100, 10, 200, 30);
        assert_eq!(proportional(line, 0, 5, 10), Rect::new(100, 10, 150, 30));
        assert_eq!(proportional(line, 5, 5, 10), Rect::new(150, 10, 200, 30));
        assert_eq!(proportional(line, 8, 9, 10).right, 200);
    }
}
