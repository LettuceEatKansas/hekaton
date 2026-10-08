//! Windows' own OCR (`Windows.Media.Ocr`). 2026-09-20 on the Python build: 66 ms on a 1920 x 1032
//! window against 5 to 6 s for rapidocr, and cleaner text. The engine is built once (it takes about a
//! second the first time) and kept; `warm_ocr` builds it and runs one recognition so the first real
//! call is warm too.
//!
//! This returns the engine's lines with word boxes. Grouping words into rows (a menu bar reads as one
//! line but each word is its own button) stays in the core, as the trait says.
//!
//! rapidocr has no Rust equivalent: when no language pack gives an engine, `ocr` says so with
//! `Unsupported` and the reason, and the core treats it as "no text".
//!
//! Python reference: `core.py:_ocr_winrt`, `ocr_engine_name`.

use std::sync::Mutex;

use wh_core::backend::{BackendError, BackendResult, Image, OcrLine, OcrResult, OcrWord, Rect};
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::core::HSTRING;

use crate::uia::{self, Shared};
use crate::util::os_err;

/// The name `OcrResult::engine` carries and `/health` prints.
const ENGINE: &str = "winrt";

enum Slot {
    Untried,
    Ready(Shared<OcrEngine>),
    Unavailable(String),
}

/// The OCR engine, built on first use and shared by every thread (it is free-threaded).
pub(crate) struct Ocr {
    slot: Mutex<Slot>,
    /// The last thing that went wrong, for `/health` (Python `_OCR_NOTE[-1]`).
    note: Mutex<Option<String>>,
}

impl Default for Ocr {
    fn default() -> Self {
        Self {
            slot: Mutex::new(Slot::Untried),
            note: Mutex::new(None),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The engine for the user's languages, else US English as Python tried (`try_create_from_language`).
fn create_engine() -> Result<OcrEngine, String> {
    if let Ok(engine) = OcrEngine::TryCreateFromUserProfileLanguages() {
        return Ok(engine);
    }
    let en = Language::CreateLanguage(&HSTRING::from("en-US"))
        .map_err(|e| format!("no OCR language pack: {e}"))?;
    OcrEngine::TryCreateFromLanguage(&en).map_err(|_| {
        "no OCR language pack is installed (Settings, Time & language, Language & region)"
            .to_string()
    })
}

impl Ocr {
    /// The engine, creating it the first time. Must run in the MTA.
    fn engine(&self) -> BackendResult<OcrEngine> {
        let mut slot = lock(&self.slot);
        if matches!(*slot, Slot::Untried) {
            *slot = match create_engine() {
                Ok(e) => Slot::Ready(Shared(e)),
                Err(why) => {
                    *lock(&self.note) = Some(format!("winrt OCR unavailable ({why})"));
                    Slot::Unavailable(why)
                }
            };
        }
        match &*slot {
            Slot::Ready(e) => Ok(e.0.clone()),
            Slot::Unavailable(why) => Err(BackendError::Unsupported(format!(
                "Windows OCR is unavailable: {why}"
            ))),
            Slot::Untried => Err(BackendError::Os("OCR engine state lost".into())),
        }
    }

    /// Can this backend OCR? True until an attempt to build the engine has failed.
    pub(crate) fn available(&self) -> bool {
        !matches!(*lock(&self.slot), Slot::Unavailable(_))
    }

    /// Recognise text in `image`.
    pub(crate) fn recognize(&self, image: &Image) -> BackendResult<OcrResult> {
        let result = uia::on_mta(|| -> BackendResult<OcrResult> {
            let engine = self.engine()?;
            recognize_with(&engine, image)
        })?;
        if let Err(e) = &result {
            *lock(&self.note) = Some(format!("winrt OCR failed once ({e})"));
        }
        result
    }

    /// Build the engine and run one recognition on a synthetic image, so the first real call is warm.
    pub(crate) fn warm(&self) -> BackendResult<()> {
        let image = warm_image();
        self.recognize(&image).map(|_| ())
    }

    /// Which engine answers, plus the last fallback reason (Python `ocr_engine_name`).
    pub(crate) fn name(&self) -> String {
        let base = match &*lock(&self.slot) {
            Slot::Ready(_) => ENGINE,
            Slot::Unavailable(_) => "unavailable",
            Slot::Untried => "untried",
        };
        match &*lock(&self.note) {
            Some(note) => format!("{base} ({note})"),
            None => base.to_string(),
        }
    }
}

// ------------------------------------------------------------------------------------ recognition

fn recognize_with(engine: &OcrEngine, image: &Image) -> BackendResult<OcrResult> {
    let (w, h) = (image.width(), image.height());
    if w == 0 || h == 0 {
        return Ok(OcrResult {
            lines: Vec::new(),
            engine: ENGINE.to_string(),
        });
    }
    let max = OcrEngine::MaxImageDimension().unwrap_or(10_000);
    if w > max || h > max {
        return Err(BackendError::Os(format!(
            "a {w}x{h} picture is larger than Windows OCR reads ({max} pixels on a side); capture a smaller region"
        )));
    }
    let bgra = rgba_to_bgra(image.rgba());
    let writer = DataWriter::new().map_err(|e| os_err("OCR buffer", &e))?;
    writer
        .WriteBytes(&bgra)
        .map_err(|e| os_err("OCR buffer", &e))?;
    let buffer = writer
        .DetachBuffer()
        .map_err(|e| os_err("OCR buffer", &e))?;
    let bitmap =
        SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, w as i32, h as i32)
            .map_err(|e| os_err("OCR bitmap", &e))?;
    let found = engine
        .RecognizeAsync(&bitmap)
        .and_then(|op| op.join())
        .map_err(|e| os_err("Windows OCR failed", &e))?;
    let lines_in = found
        .Lines()
        .map_err(|e| os_err("Windows OCR result", &e))?;
    let mut lines = Vec::new();
    for i in 0..lines_in.Size().unwrap_or(0) {
        let Ok(line) = lines_in.GetAt(i) else {
            continue;
        };
        let Ok(words_in) = line.Words() else {
            continue;
        };
        let mut words = Vec::new();
        for j in 0..words_in.Size().unwrap_or(0) {
            let Ok(word) = words_in.GetAt(j) else {
                continue;
            };
            let (Ok(text), Ok(b)) = (word.Text(), word.BoundingRect()) else {
                continue;
            };
            words.push(OcrWord {
                text: text.to_string(),
                rect: rect_of(b.X, b.Y, b.Width, b.Height),
                confidence: 1.0,
            });
        }
        if let Some(line) = line_of(words) {
            lines.push(line);
        }
    }
    Ok(OcrResult {
        lines,
        engine: ENGINE.to_string(),
    })
}

// ------------------------------------------------------------------------------------ pure pieces

/// RGBA bytes to the BGRA order a `SoftwareBitmap` wants.
pub(crate) fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.as_chunks::<4>().0 {
        out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    out
}

/// A float box (x, y, width, height) as whole pixels that cover it.
pub(crate) fn rect_of(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect::new(
        x.floor() as i32,
        y.floor() as i32,
        (x + w).ceil() as i32,
        (y + h).ceil() as i32,
    )
}

/// One recognised line from its words: the words joined by single spaces and the box around them.
/// A line with no words is dropped (`if not words: continue`).
pub(crate) fn line_of(words: Vec<OcrWord>) -> Option<OcrLine> {
    let first = words.first()?;
    let mut rect = first.rect;
    for w in &words[1..] {
        rect = Rect::new(
            rect.left.min(w.rect.left),
            rect.top.min(w.rect.top),
            rect.right.max(w.rect.right),
            rect.bottom.max(w.rect.bottom),
        );
    }
    let text = words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Some(OcrLine {
        text,
        rect,
        words,
        confidence: 1.0,
    })
}

/// A white picture with dark text-sized blocks in rows: enough for the engine to load everything it
/// loads for real text.
pub(crate) fn warm_image() -> Image {
    let (w, h) = (360u32, 120u32);
    let mut img = Image::solid(w, h, [255, 255, 255, 255]);
    for row in 0..3u32 {
        let y0 = 18 + row * 32;
        let mut x = 16u32;
        while x + 14 < w - 16 {
            // a "letter": two strokes and a bar
            for dy in 0..18 {
                img.put_pixel(x, y0 + dy, [0, 0, 0, 255]);
                img.put_pixel(x + 1, y0 + dy, [0, 0, 0, 255]);
                img.put_pixel(x + 9, y0 + dy, [0, 0, 0, 255]);
                img.put_pixel(x + 10, y0 + dy, [0, 0, 0, 255]);
            }
            for dx in 0..11 {
                img.put_pixel(x + dx, y0 + 8, [0, 0, 0, 255]);
            }
            x += 18 + (row * 3);
        }
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, l: i32, t: i32, r: i32, b: i32) -> OcrWord {
        OcrWord {
            text: text.into(),
            rect: Rect::new(l, t, r, b),
            confidence: 1.0,
        }
    }

    #[test]
    fn rgba_becomes_bgra_pixel_by_pixel() {
        assert_eq!(
            rgba_to_bgra(&[1, 2, 3, 4, 10, 20, 30, 255]),
            [3, 2, 1, 4, 30, 20, 10, 255]
        );
        assert!(rgba_to_bgra(&[]).is_empty());
    }

    #[test]
    fn float_boxes_become_covering_pixel_boxes() {
        assert_eq!(rect_of(10.2, 20.7, 30.0, 8.0), Rect::new(10, 20, 41, 29));
        assert_eq!(rect_of(0.0, 0.0, 5.0, 5.0), Rect::new(0, 0, 5, 5));
        assert_eq!(rect_of(-1.5, 3.0, 2.0, 1.0), Rect::new(-2, 3, 1, 4));
    }

    #[test]
    fn a_line_joins_its_words_and_boxes_them() {
        let line = line_of(vec![
            word("STORE", 10, 20, 60, 34),
            word("LIBRARY", 90, 18, 150, 36),
            word("COMMUNITY", 180, 20, 260, 34),
        ])
        .unwrap();
        assert_eq!(line.text, "STORE LIBRARY COMMUNITY");
        assert_eq!(line.rect, Rect::new(10, 18, 260, 36));
        assert_eq!(line.words.len(), 3);
        assert_eq!(line.confidence, 1.0);
    }

    #[test]
    fn a_line_without_words_is_dropped() {
        assert_eq!(line_of(Vec::new()), None);
    }

    #[test]
    fn the_warm_up_picture_is_white_with_dark_ink() {
        let img = warm_image();
        assert_eq!((img.width(), img.height()), (360, 120));
        let dark = img
            .rgba()
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] == 0)
            .count();
        assert!(dark > 200, "{dark} dark pixels");
        assert_eq!(img.pixel(0, 0), Some([255, 255, 255, 255]));
    }

    #[test]
    fn the_engine_name_says_untried_before_any_call() {
        let ocr = Ocr::default();
        assert_eq!(ocr.name(), "untried");
        assert!(ocr.available());
    }
}
