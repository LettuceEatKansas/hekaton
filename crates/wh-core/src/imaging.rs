//! The image pipeline. Port of the picture half of `computer.py`: `screenshot_b64` (resize to the
//! call's size, encode), `_encode` / `mime` (JPEG quality 82, or PNG), and the pixel comparison
//! `settle` runs (grayscale 160x90 thumbnails, "changed" means a pixel moved by more than 14, and a
//! screen is calm when fewer than 2% changed).
//!
//! Python used Pillow. Resampling follows what Pillow did:
//!
//! * an exact integer shrink uses a box filter (`Image.reduce`), about 6x faster than a convolution;
//! * any other shrink is bilinear and any enlargement is nearest;
//! * the settle thumbnail is bicubic (Pillow's default for `resize`), i.e. Catmull-Rom.
//!
//! The pixels are not bit-identical to Pillow's (rounding differs by a level), which nothing relies on.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use fast_image_resize::images::Image as FirImage;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

use crate::backend::Image;

/// JPEG quality of a screenshot (Python `img.save(buf, "JPEG", quality=82)`).
pub const JPEG_QUALITY: u8 = 82;

/// Settle thumbnails are this size (Python `.resize((160, 90))`).
pub const THUMB_SIZE: (u32, u32) = (160, 90);

/// A thumbnail pixel counts as "changed" when its gray level moved by more than this.
pub const CHANGE_THRESHOLD: i16 = 14;

/// The screen is calm when fewer than this share of thumbnail pixels changed. An animated
/// background keeps the average difference up forever, so the share is what is measured; a UI
/// transition moves far more than 2% of the screen.
pub const CALM_RATIO: f64 = 0.02;

/// A thumbnail whose standard deviation is below this is blank (black or white): steady but not
/// ready, a loading screen.
pub const BLANK_STD: f64 = 4.0;

/// How a screenshot is encoded (config `img`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageFormat {
    /// JPEG at [`JPEG_QUALITY`], about 5x smaller than PNG here.
    #[default]
    Jpeg,
    /// PNG, default compression.
    Png,
}

impl ImageFormat {
    /// `"png"` is PNG, anything else JPEG (Python: `IMG_FORMAT == "png"`).
    pub fn from_name(name: &str) -> Self {
        if name.trim().eq_ignore_ascii_case("png") {
            ImageFormat::Png
        } else {
            ImageFormat::Jpeg
        }
    }

    /// The MIME type of the encoded bytes. Python `mime()`.
    pub fn mime(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Png => "image/png",
        }
    }
}

/// Something went wrong resizing or encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageError(pub String);

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ImageError {}

fn err<E: fmt::Display>(what: &str) -> impl FnOnce(E) -> ImageError + '_ {
    move |e| ImageError(format!("{what}: {e}"))
}

// ------------------------------------------------------------------------------------ resizing

fn resize_raw(
    pixels: Vec<u8>,
    (w, h): (u32, u32),
    (ow, oh): (u32, u32),
    kind: PixelType,
    alg: ResizeAlg,
) -> Result<Vec<u8>, ImageError> {
    if w == 0 || h == 0 || ow == 0 || oh == 0 {
        return Err(ImageError("cannot resize to or from an empty image".into()));
    }
    let src = FirImage::from_vec_u8(w, h, pixels, kind).map_err(err("resize source"))?;
    let mut dst = FirImage::new(ow, oh, kind);
    Resizer::new()
        .resize(&src, &mut dst, &ResizeOptions::new().resize_alg(alg))
        .map_err(err("resize"))?;
    Ok(dst.into_vec())
}

/// Resize to exactly `(out_w, out_h)` the way `screenshot_b64` did. An image already that size is
/// returned as is.
pub fn resize(img: &Image, out_w: u32, out_h: u32) -> Result<Image, ImageError> {
    if (img.width(), img.height()) == (out_w, out_h) {
        return Ok(img.clone());
    }
    if out_w == 0 || out_h == 0 {
        return Err(ImageError("cannot resize to an empty image".into()));
    }
    let f = f64::from(img.width()) / f64::from(out_w);
    let exact = out_w < img.width()
        && (f - f.round()).abs() < 1e-6
        && (f64::from(img.height()) / f64::from(out_h) - f).abs() < 1e-6;
    let alg = if exact {
        ResizeAlg::Convolution(FilterType::Box)
    } else if out_w < img.width() {
        ResizeAlg::Convolution(FilterType::Bilinear)
    } else {
        ResizeAlg::Nearest
    };
    let bytes = resize_raw(
        img.rgba().to_vec(),
        (img.width(), img.height()),
        (out_w, out_h),
        PixelType::U8x4,
        alg,
    )?;
    let mut out = Image::new(out_w, out_h, bytes).map_err(|e| ImageError(e.to_string()))?;
    out = out.with_origin(img.origin());
    // The resized image covers the same screen area, so pixels per input unit scale with it.
    let k = f64::from(out_w) / f64::from(img.width());
    Ok(out.with_scale(img.scale() * k))
}

/// Resize to exactly `(out_w, out_h)` with a bicubic filter, whichever way: what an OCR input is
/// enlarged with (Python `img.resize((w * k, h * k), Image.BICUBIC)`). Unlike [`resize`], which
/// enlarges by nearest neighbour as a screenshot does, this keeps glyph edges smooth for the engine.
pub fn resize_bicubic(img: &Image, out_w: u32, out_h: u32) -> Result<Image, ImageError> {
    if (img.width(), img.height()) == (out_w, out_h) {
        return Ok(img.clone());
    }
    let bytes = resize_raw(
        img.rgba().to_vec(),
        (img.width(), img.height()),
        (out_w, out_h),
        PixelType::U8x4,
        ResizeAlg::Convolution(FilterType::CatmullRom),
    )?;
    let out = Image::new(out_w, out_h, bytes).map_err(|e| ImageError(e.to_string()))?;
    let k = f64::from(out_w) / f64::from(img.width());
    Ok(out.with_origin(img.origin()).with_scale(img.scale() * k))
}

// ------------------------------------------------------------------------------------ encoding

fn rgb_bytes(img: &Image) -> Vec<u8> {
    let mut rgb = Vec::with_capacity((img.width() as usize) * (img.height() as usize) * 3);
    for px in img.rgba().as_chunks::<4>().0 {
        rgb.extend_from_slice(&px[..3]);
    }
    rgb
}

/// JPEG bytes at quality 82. Alpha is dropped (Python grabbed RGB).
pub fn encode_jpeg(img: &Image) -> Result<Vec<u8>, ImageError> {
    encode_jpeg_quality(img, JPEG_QUALITY)
}

/// JPEG bytes at the given quality (1 to 100). A screenshot saved to a file (`save_to`) uses 90.
pub fn encode_jpeg_quality(img: &Image, quality: u8) -> Result<Vec<u8>, ImageError> {
    let (w, h) = (
        u16::try_from(img.width()).map_err(|_| ImageError("image too wide for JPEG".into()))?,
        u16::try_from(img.height()).map_err(|_| ImageError("image too tall for JPEG".into()))?,
    );
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, quality)
        .encode(&rgb_bytes(img), w, h, jpeg_encoder::ColorType::Rgb)
        .map_err(err("jpeg encode"))?;
    Ok(out)
}

/// PNG bytes, 8-bit RGB, default compression.
pub fn encode_png(img: &Image) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.width(), img.height());
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().map_err(err("png header"))?;
        writer
            .write_image_data(&rgb_bytes(img))
            .map_err(err("png data"))?;
    }
    Ok(out)
}

/// Encode in the configured format. Python `_encode`, before the base64 step.
pub fn encode(img: &Image, format: ImageFormat) -> Result<Vec<u8>, ImageError> {
    match format {
        ImageFormat::Jpeg => encode_jpeg(img),
        ImageFormat::Png => encode_png(img),
    }
}

/// Standard base64 of `bytes`.
pub fn base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Resize to `(out_w, out_h)`, encode, and base64: the wire form of a screenshot.
/// Python `screenshot_b64` after the grab (the click marker is drawn by the caller).
pub fn screenshot_b64(
    img: &Image,
    out_w: u32,
    out_h: u32,
    format: ImageFormat,
) -> Result<String, ImageError> {
    Ok(base64(&encode(&resize(img, out_w, out_h)?, format)?))
}

// ------------------------------------------------------------------------------------ settle

/// A grayscale thumbnail for comparing two looks at the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumb {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Thumb {
    /// Build from gray bytes (`width * height` of them).
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Option<Self> {
        ((width as usize) * (height as usize) == pixels.len()).then_some(Self {
            width,
            height,
            pixels,
        })
    }

    /// Width.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The gray bytes.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Population standard deviation of the gray levels (`numpy.std`). A thumbnail below
    /// [`BLANK_STD`] is a blank screen.
    pub fn std_dev(&self) -> f64 {
        std_dev(&self.pixels)
    }

    /// Is the screen one flat colour, i.e. loading?
    pub fn is_blank(&self) -> bool {
        self.std_dev() < BLANK_STD
    }
}

/// Pillow's `convert("L")` for one pixel: `(19595 R + 38470 G + 7471 B + 0x8000) >> 16`.
fn gray(r: u8, g: u8, b: u8) -> u8 {
    ((19595 * u32::from(r) + 38470 * u32::from(g) + 7471 * u32::from(b) + 0x8000) >> 16) as u8
}

/// `convert("L")` then a bicubic `resize((160, 90))`: what `settle` compares.
pub fn thumbnail(img: &Image) -> Result<Thumb, ImageError> {
    thumbnail_sized(img, THUMB_SIZE.0, THUMB_SIZE.1)
}

/// [`thumbnail`] at an explicit size.
pub fn thumbnail_sized(img: &Image, w: u32, h: u32) -> Result<Thumb, ImageError> {
    let gray_px: Vec<u8> = img
        .rgba()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| gray(p[0], p[1], p[2]))
        .collect();
    let pixels = if (img.width(), img.height()) == (w, h) {
        gray_px
    } else {
        resize_raw(
            gray_px,
            (img.width(), img.height()),
            (w, h),
            PixelType::U8,
            ResizeAlg::Convolution(FilterType::CatmullRom),
        )?
    };
    Thumb::new(w, h, pixels).ok_or_else(|| ImageError("thumbnail size mismatch".into()))
}

/// The share of pixels whose gray level differs by more than [`CHANGE_THRESHOLD`]:
/// `(np.abs(cur - prev) > 14).mean()`. Thumbnails of different sizes count as entirely changed.
pub fn change_ratio(prev: &Thumb, cur: &Thumb) -> f64 {
    if (prev.width, prev.height) != (cur.width, cur.height) || prev.pixels.is_empty() {
        return 1.0;
    }
    let changed = prev
        .pixels
        .iter()
        .zip(&cur.pixels)
        .filter(|(a, b)| (i16::from(**a) - i16::from(**b)).abs() > CHANGE_THRESHOLD)
        .count();
    changed as f64 / prev.pixels.len() as f64
}

/// Is the screen calm between two looks? True when fewer than [`CALM_RATIO`] of the pixels moved.
pub fn is_calm(prev: &Thumb, cur: &Thumb) -> bool {
    change_ratio(prev, cur) < CALM_RATIO
}

/// Population standard deviation of bytes (numpy's default, ddof 0). 0 for an empty slice.
pub fn std_dev(values: &[u8]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let n = values.len() as f64;
    let mean = values.iter().map(|v| f64::from(*v)).sum::<f64>() / n;
    let var = values
        .iter()
        .map(|v| (f64::from(*v) - mean).powi(2))
        .sum::<f64>()
        / n;
    var.sqrt()
}

// ------------------------------------------------------------------------------------ marker

/// Radius of the click marker ring, and its stroke (Python `d.ellipse((px-7, py-7, px+7, py+7),
/// outline=(255, 40, 40), width=2)`).
pub const MARKER_RADIUS: i32 = 7;
/// The marker colour.
pub const MARKER_COLOR: [u8; 4] = [255, 40, 40, 255];

/// Draw the last-click marker: a small red ring at `(cx, cy)` in image pixels, like a person's eye
/// following the pointer. Shows the model where its last click landed.
pub fn draw_marker(img: &mut Image, cx: i32, cy: i32) {
    let outer = f64::from(MARKER_RADIUS) + 0.5;
    let inner = f64::from(MARKER_RADIUS) - 1.5;
    for dy in -MARKER_RADIUS..=MARKER_RADIUS {
        for dx in -MARKER_RADIUS..=MARKER_RADIUS {
            let d = f64::from(dx * dx + dy * dy).sqrt();
            if d <= outer
                && d >= inner
                && let (Ok(x), Ok(y)) = (u32::try_from(cx + dx), u32::try_from(cy + dy))
            {
                img.put_pixel(x, y, MARKER_COLOR);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(w: u32, h: u32) -> Image {
        let mut bytes = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let v = ((u64::from(y) * u64::from(w) + u64::from(x)) * 7 % 256) as u8;
                bytes.extend_from_slice(&[v, v.wrapping_add(40), v.wrapping_mul(3), 255]);
            }
        }
        Image::new(w, h, bytes).unwrap()
    }

    /// Dimensions from a JPEG's SOF0 marker, no decoder needed.
    fn jpeg_size(bytes: &[u8]) -> (u32, u32) {
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "SOI");
        let mut i = 2;
        while i + 9 < bytes.len() {
            assert_eq!(bytes[i], 0xFF, "marker at {i}");
            let marker = bytes[i + 1];
            let len = (usize::from(bytes[i + 2]) << 8) | usize::from(bytes[i + 3]);
            if marker == 0xC0 || marker == 0xC2 {
                let h = (u32::from(bytes[i + 5]) << 8) | u32::from(bytes[i + 6]);
                let w = (u32::from(bytes[i + 7]) << 8) | u32::from(bytes[i + 8]);
                return (w, h);
            }
            i += 2 + len;
        }
        panic!("no SOF marker");
    }

    /// The first value of each quantisation table in the file, in order.
    fn jpeg_first_quant_values(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 2;
        while i + 4 < bytes.len() {
            let marker = bytes[i + 1];
            let len = (usize::from(bytes[i + 2]) << 8) | usize::from(bytes[i + 3]);
            if marker == 0xDB {
                let mut p = i + 4;
                while p < i + 2 + len {
                    let precision = bytes[p] >> 4;
                    out.push(bytes[p + 1]);
                    p += 1 + if precision == 0 { 64 } else { 128 };
                }
            }
            if marker == 0xDA {
                break;
            }
            i += 2 + len;
        }
        out
    }

    #[test]
    fn quality_82_is_what_python_used() {
        assert_eq!(JPEG_QUALITY, 82);
        let bytes = encode_jpeg(&pattern(64, 48)).unwrap();
        // IJG quality 82: scale 200 - 2*82 = 36, so the luminance DC entry 16 becomes
        // (16 * 36 + 50) / 100 = 6, and the chrominance entry 17 becomes (17 * 36 + 50) / 100 = 6.
        assert_eq!(jpeg_first_quant_values(&bytes), [6, 6]);
        assert_eq!(jpeg_size(&bytes), (64, 48));
    }

    #[test]
    fn png_round_trips_the_pixels() {
        let img = pattern(13, 9);
        let bytes = encode_png(&img).unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (13, 9));
        assert_eq!(info.color_type, png::ColorType::Rgb);
        for (i, px) in img.rgba().as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(&buf[i * 3..i * 3 + 3], &px[..3], "pixel {i}");
        }
    }

    #[test]
    fn the_format_comes_from_the_img_setting() {
        assert_eq!(ImageFormat::from_name("png"), ImageFormat::Png);
        assert_eq!(ImageFormat::from_name("PNG"), ImageFormat::Png);
        assert_eq!(ImageFormat::from_name("jpeg"), ImageFormat::Jpeg);
        assert_eq!(ImageFormat::from_name("webp"), ImageFormat::Jpeg);
        assert_eq!(ImageFormat::Jpeg.mime(), "image/jpeg");
        assert_eq!(ImageFormat::Png.mime(), "image/png");
    }

    #[test]
    fn an_exact_halving_is_a_box_average() {
        // 4x2 -> 2x1: each output pixel is the mean of a 2x2 block.
        let mut bytes = Vec::new();
        for row in 0..2u8 {
            for col in 0..4u8 {
                let v = if col < 2 {
                    10 + row * 10 + col * 10
                } else {
                    200
                };
                bytes.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let img = Image::new(4, 2, bytes).unwrap();
        let out = resize(&img, 2, 1).unwrap();
        assert_eq!((out.width(), out.height()), (2, 1));
        let left = out.pixel(0, 0).unwrap()[0];
        // block {10, 20, 20, 30} averages to 20
        assert!((i32::from(left) - 20).abs() <= 1, "{left}");
        assert_eq!(out.pixel(1, 0).unwrap()[0], 200);
        assert_eq!(out.scale(), 0.5);
    }

    #[test]
    fn shrinking_and_growing_hit_the_requested_size() {
        let img = pattern(100, 60);
        let small = resize(&img, 40, 24).unwrap();
        assert_eq!((small.width(), small.height()), (40, 24));
        let big = resize(&small, 80, 48).unwrap();
        assert_eq!((big.width(), big.height()), (80, 48));
        assert_eq!(resize(&img, 100, 60).unwrap(), img, "same size is a no-op");
        assert!(resize(&img, 0, 10).is_err());
    }

    #[test]
    fn nearest_enlargement_repeats_pixels() {
        let img = Image::new(2, 1, vec![10, 10, 10, 255, 200, 200, 200, 255]).unwrap();
        let big = resize(&img, 4, 2).unwrap();
        assert_eq!(big.pixel(0, 0).unwrap()[0], 10);
        assert_eq!(big.pixel(1, 1).unwrap()[0], 10);
        assert_eq!(big.pixel(2, 0).unwrap()[0], 200);
        assert_eq!(big.pixel(3, 1).unwrap()[0], 200);
    }

    #[test]
    fn a_screenshot_is_resized_encoded_and_base64d() {
        let img = pattern(320, 180);
        let b64 = screenshot_b64(&img, 160, 90, ImageFormat::Jpeg).unwrap();
        let bytes = STANDARD.decode(b64).unwrap();
        assert_eq!(jpeg_size(&bytes), (160, 90));
        let png_b64 = screenshot_b64(&img, 80, 45, ImageFormat::Png).unwrap();
        let bytes = STANDARD.decode(png_b64).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn a_zoom_region_comes_out_at_the_computed_size() {
        // Ported from the W3 zoom tests: the encoded image has the size the scale rule asked for.
        use crate::backend::Rect;
        use crate::scale::{Coords, Scale};
        let scale = Scale::new(Rect::from_xywh(0, 0, 1280, 720), Coords::Pixels, 1280);
        let (rect, (w, h)) = scale.zoom_box([0.0, 0.0, 400.0, 300.0]).unwrap();
        assert_eq!((rect.width(), rect.height()), (400, 300));
        let region = pattern(400, 300);
        let png = screenshot_b64(&region, w, h, ImageFormat::Png).unwrap();
        let bytes = STANDARD.decode(png).unwrap();
        let reader = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        assert_eq!((reader.info().width, reader.info().height), (800, 600));
    }

    // ---- the settle comparison ----

    #[test]
    fn gray_matches_pillows_formula() {
        assert_eq!(gray(0, 0, 0), 0);
        assert_eq!(gray(255, 255, 255), 255);
        assert_eq!(gray(255, 0, 0), 76);
        assert_eq!(gray(0, 255, 0), 150);
        assert_eq!(gray(0, 0, 255), 29);
    }

    #[test]
    fn a_still_screen_has_no_change() {
        let a = thumbnail(&pattern(320, 180)).unwrap();
        let b = thumbnail(&pattern(320, 180)).unwrap();
        assert_eq!((a.width(), a.height()), THUMB_SIZE);
        assert_eq!(change_ratio(&a, &b), 0.0);
        assert!(is_calm(&a, &b));
        assert!(!a.is_blank(), "std {}", a.std_dev());
    }

    #[test]
    fn a_ui_transition_is_not_calm_but_a_few_moving_pixels_are() {
        let still = thumbnail(&Image::solid(160, 90, [0, 0, 0, 255])).unwrap();
        let all = thumbnail(&Image::solid(160, 90, [255, 255, 255, 255])).unwrap();
        assert_eq!(change_ratio(&still, &all), 1.0);
        assert!(!is_calm(&still, &all));
        // 1% of the pixels flip: still calm (an animated background).
        let mut flicker = Image::solid(160, 90, [0, 0, 0, 255]);
        for x in 0..144 {
            flicker.put_pixel(x, 0, [255, 255, 255, 255]);
        }
        let t = thumbnail(&flicker).unwrap();
        assert!((change_ratio(&still, &t) - 0.01).abs() < 1e-9);
        assert!(is_calm(&still, &t));
        // 3% flip: busy.
        for x in 0..144 {
            flicker.put_pixel(x, 1, [255, 255, 255, 255]);
            flicker.put_pixel(x, 2, [255, 255, 255, 255]);
        }
        assert!(!is_calm(&still, &thumbnail(&flicker).unwrap()));
    }

    #[test]
    fn the_threshold_is_strictly_greater_than_14() {
        let base = Thumb::new(2, 1, vec![100, 100]).unwrap();
        let at = Thumb::new(2, 1, vec![114, 86]).unwrap();
        let over = Thumb::new(2, 1, vec![115, 85]).unwrap();
        assert_eq!(change_ratio(&base, &at), 0.0, "exactly 14 is not a change");
        assert_eq!(change_ratio(&base, &over), 1.0);
    }

    #[test]
    fn mismatched_thumbnails_count_as_changed() {
        let a = Thumb::new(2, 1, vec![0, 0]).unwrap();
        let b = Thumb::new(1, 1, vec![0]).unwrap();
        assert_eq!(change_ratio(&a, &b), 1.0);
        assert!(Thumb::new(2, 2, vec![0]).is_none());
    }

    #[test]
    fn a_flat_screen_is_blank_and_noise_is_not() {
        let black = thumbnail(&Image::solid(160, 90, [0, 0, 0, 255])).unwrap();
        let white = thumbnail(&Image::solid(160, 90, [255, 255, 255, 255])).unwrap();
        assert!(black.is_blank() && white.is_blank());
        assert_eq!(std_dev(&[]), 0.0);
        assert!((std_dev(&[0, 10]) - 5.0).abs() < 1e-12);
        assert!(!thumbnail(&pattern(160, 90)).unwrap().is_blank());
    }

    #[test]
    fn the_marker_is_a_red_ring_and_clips_at_the_edges() {
        let mut img = Image::solid(40, 40, [255, 255, 255, 255]);
        draw_marker(&mut img, 20, 20);
        assert_eq!(img.pixel(27, 20), Some(MARKER_COLOR), "ring at radius 7");
        assert_eq!(img.pixel(20, 13), Some(MARKER_COLOR));
        assert_eq!(
            img.pixel(20, 20),
            Some([255, 255, 255, 255]),
            "hollow centre"
        );
        assert_eq!(
            img.pixel(20, 5),
            Some([255, 255, 255, 255]),
            "nothing outside the ring"
        );
        let mut corner = Image::solid(10, 10, [0, 0, 0, 255]);
        draw_marker(&mut corner, 0, 0);
        draw_marker(&mut corner, -50, 99);
        assert_eq!(corner.pixel(7, 0), Some(MARKER_COLOR));
    }
}
