//! Screen capture through ScreenCaptureKit (`SCScreenshotManager`, macOS 14+;
//! `CGWindowListCreateImage` is obsoleted in macOS 15) and CGImage to RGBA conversion.
//!
//! ScreenCaptureKit is callback based. Each call here starts the request and blocks the calling
//! thread on a channel until the completion handler (which runs on a framework queue) answers or
//! a deadline passes.

use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImage,
    CGImageAlphaInfo,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
    SCWindow,
};
use wh_core::backend::{BackendError, BackendResult, Image, Point, Rect};

use crate::cf::Shared;
use crate::displays::DisplayInfo;
use crate::geom::{CapturePlan, to_pixels};
use crate::keymap::PANE_SCREEN_RECORDING;
use crate::perm;

/// How long one ScreenCaptureKit request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// How long the display list is reused.
const DISPLAY_TTL: Duration = Duration::from_millis(1500);
/// `kCVPixelFormatType_32BGRA`.
const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241;

type DisplayCache = Option<(Instant, Vec<(u32, Shared<Retained<SCDisplay>>)>)>;

/// The capture engine. Holds a short-lived cache of `SCDisplay` objects.
#[derive(Default)]
pub(crate) struct Capturer {
    displays: Mutex<DisplayCache>,
}

fn os(msg: impl Into<String>) -> BackendError {
    BackendError::Os(msg.into())
}

/// Turn the `NSError` a ScreenCaptureKit handler delivers into a backend error.
fn describe_error(err: *mut NSError) -> BackendError {
    // SAFETY: the pointer is the handler's argument: null or a live NSError for the call.
    let Some(e) = (unsafe { err.as_ref() }) else {
        return os("ScreenCaptureKit returned neither an image nor an error");
    };
    let text = e.localizedDescription().to_string();
    // SCStreamErrorUserDeclined = -3801
    if e.code() == -3801 || text.to_lowercase().contains("declined") {
        return BackendError::Denied(format!(
            "macOS has not granted winhands Screen Recording access. Open {PANE_SCREEN_RECORDING}, \
             switch winhands on, then restart winhands and try again"
        ));
    }
    os(format!("ScreenCaptureKit: {text} (code {})", e.code()))
}

/// Read the shareable content (displays and windows) and hand `pick` the result inside the
/// handler's lifetime.
fn shareable<T: Send + 'static>(
    pick: impl Fn(&SCShareableContent) -> T + 'static,
) -> BackendResult<T> {
    let (tx, rx) = mpsc::channel::<Result<T, BackendError>>();
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        // SAFETY: the pointer is the handler's argument: null or a live object for the call.
        let res = match unsafe { content.as_ref() } {
            Some(c) => Ok(pick(c)),
            None => Err(describe_error(err)),
        };
        let _ = tx.send(res);
    });
    // SAFETY: the block matches the (content, error) completion handler signature.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    rx.recv_timeout(REQUEST_TIMEOUT)
        .map_err(|_| BackendError::Timeout)?
}

/// Capture through a filter and configuration, blocking for the CGImage.
fn capture_image(
    filter: &SCContentFilter,
    config: &SCStreamConfiguration,
) -> BackendResult<CFRetained<CGImage>> {
    let (tx, rx) = mpsc::channel::<Result<CFRetained<CGImage>, BackendError>>();
    let block = RcBlock::new(move |image: *mut CGImage, err: *mut NSError| {
        let res = match NonNull::new(image) {
            // SAFETY: the image is valid for the handler call; retain it to keep it afterwards.
            Some(p) => Ok(unsafe { CFRetained::retain(p) }),
            None => Err(describe_error(err)),
        };
        let _ = tx.send(res);
    });
    // SAFETY: filter and configuration are live objects; the block matches the handler type.
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            filter,
            config,
            Some(&block),
        );
    }
    rx.recv_timeout(REQUEST_TIMEOUT)
        .map_err(|_| BackendError::Timeout)?
}

fn new_config(width: u32, height: u32) -> Retained<SCStreamConfiguration> {
    // SAFETY: plain property setters on a freshly created configuration object.
    unsafe {
        let cfg = SCStreamConfiguration::new();
        cfg.setWidth(width as usize);
        cfg.setHeight(height as usize);
        cfg.setShowsCursor(false);
        cfg.setScalesToFit(true);
        cfg.setPixelFormat(PIXEL_FORMAT_BGRA);
        cfg
    }
}

/// CGImage to tightly packed RGBA bytes.
pub(crate) fn cgimage_to_rgba(img: &CGImage) -> BackendResult<(u32, u32, Vec<u8>)> {
    let w = CGImage::width(Some(img));
    let h = CGImage::height(Some(img));
    if w == 0 || h == 0 {
        return Err(os("the capture came back empty"));
    }
    let space = srgb().ok_or_else(|| os("no RGB colour space"))?;
    let mut buf = vec![0u8; w * h * 4];
    // SAFETY: `buf` is w*h*4 bytes, matching 8 bits x 4 components and bytes_per_row w*4; it
    // outlives the context, which is dropped before the buffer is read.
    let ctx = unsafe {
        CGBitmapContextCreate(
            buf.as_mut_ptr().cast(),
            w,
            h,
            8,
            w * 4,
            Some(&space),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }
    .ok_or_else(|| os("could not create a bitmap context for the capture"))?;
    let rect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: w as f64,
            height: h as f64,
        },
    };
    CGContext::draw_image(Some(&ctx), rect, Some(img));
    drop(ctx);
    // A screen is opaque: make sure no undrawn corner reads as transparent.
    for alpha in buf.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }
    Ok((w as u32, h as u32, buf))
}

/// The sRGB colour space, else device RGB.
fn srgb() -> Option<CFRetained<CGColorSpace>> {
    // SAFETY: reading an immutable CoreGraphics constant.
    let name = unsafe { objc2_core_graphics::kCGColorSpaceSRGB };
    CGColorSpace::with_name(Some(name)).or_else(CGColorSpace::new_device_rgb)
}

/// RGBA bytes to a CGImage (for Vision). `keep` must outlive the image's use.
pub(crate) fn rgba_to_cgimage(
    image: &Image,
    keep: &mut Vec<u8>,
) -> BackendResult<CFRetained<CGImage>> {
    let (w, h) = (image.width() as usize, image.height() as usize);
    if w == 0 || h == 0 {
        return Err(os("cannot read text from an empty image"));
    }
    keep.clear();
    keep.extend_from_slice(image.rgba());
    let space = srgb().ok_or_else(|| os("no RGB colour space"))?;
    // SAFETY: `keep` is w*h*4 bytes and stays alive while the caller uses the image.
    let ctx = unsafe {
        CGBitmapContextCreate(
            keep.as_mut_ptr().cast(),
            w,
            h,
            8,
            w * 4,
            Some(&space),
            CGImageAlphaInfo::NoneSkipLast.0,
        )
    }
    .ok_or_else(|| os("could not create a bitmap context"))?;
    CGBitmapContextCreateImage(Some(&ctx)).ok_or_else(|| os("could not make an image"))
}

/// A pixel buffer the per-display captures are copied into.
struct Canvas {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Canvas {
    /// An opaque black canvas.
    fn new(width: u32, height: u32) -> Self {
        let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
        for alpha in rgba.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }
        Self {
            width,
            height,
            rgba,
        }
    }

    /// Copy `src` (w x h RGBA) in at `(dx, dy)`, clipped to the canvas.
    fn blit(&mut self, src: &[u8], w: u32, h: u32, dx: u32, dy: u32) {
        let copy_w = w.min(self.width.saturating_sub(dx)) as usize;
        if copy_w == 0 {
            return;
        }
        for row in 0..h {
            let y = dy + row;
            if y >= self.height {
                break;
            }
            let from = (row as usize) * (w as usize) * 4;
            let to = ((y as usize) * (self.width as usize) + dx as usize) * 4;
            self.rgba[to..to + copy_w * 4].copy_from_slice(&src[from..from + copy_w * 4]);
        }
    }
}

impl Capturer {
    fn display_objects(
        &self,
        force: bool,
    ) -> BackendResult<Vec<(u32, Shared<Retained<SCDisplay>>)>> {
        let mut cache = self.displays.lock().unwrap_or_else(|e| e.into_inner());
        if !force
            && let Some((at, list)) = cache.as_ref()
            && at.elapsed() < DISPLAY_TTL
        {
            return Ok(list
                .iter()
                .map(|(id, d)| (*id, Shared(d.0.clone())))
                .collect());
        }
        // SAFETY: SCDisplay objects are immutable snapshots the framework hands to any thread.
        let fresh: Vec<(u32, Shared<Retained<SCDisplay>>)> = shareable(|c| {
            // SAFETY: reading properties of live SCDisplay objects.
            unsafe { c.displays() }
                .iter()
                .map(|d| (unsafe { d.displayID() }, Shared(d)))
                .collect::<Vec<_>>()
        })?;
        *cache = Some((
            Instant::now(),
            fresh
                .iter()
                .map(|(id, d)| (*id, Shared(d.0.clone())))
                .collect(),
        ));
        Ok(fresh)
    }

    /// Capture the region a [`CapturePlan`] describes, one ScreenCaptureKit call per display.
    pub(crate) fn capture_plan(
        &self,
        plan: &CapturePlan,
        displays: &[DisplayInfo],
    ) -> BackendResult<Image> {
        perm::require_screen_capture("take screenshots")?;
        let mut canvas = Canvas::new(plan.width, plan.height);
        for part in &plan.parts {
            let cg_id = displays[part.display].cg_id;
            let img = self.capture_display_rect(cg_id, part.relative, part.width, part.height)?;
            let (w, h, rgba) = cgimage_to_rgba(&img)?;
            canvas.blit(&rgba, w, h, part.dest_x, part.dest_y);
        }
        Ok(Image::new(plan.width, plan.height, canvas.rgba)?
            .with_scale(plan.scale)
            .with_origin(plan.origin))
    }

    fn capture_display_rect(
        &self,
        cg_id: u32,
        relative: Rect,
        width: u32,
        height: u32,
    ) -> BackendResult<CFRetained<CGImage>> {
        let mut retried = false;
        loop {
            let list = self.display_objects(retried)?;
            let Some((_, display)) = list.iter().find(|(id, _)| *id == cg_id) else {
                if retried {
                    return Err(os("ScreenCaptureKit does not list that display"));
                }
                retried = true;
                continue;
            };
            // SAFETY: init with a live SCDisplay and an empty exclusion list.
            let filter = unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &display.0,
                    &NSArray::<SCWindow>::new(),
                )
            };
            let cfg = new_config(width, height);
            let src = CGRect {
                origin: CGPoint {
                    x: f64::from(relative.left),
                    y: f64::from(relative.top),
                },
                size: CGSize {
                    width: f64::from(relative.width()),
                    height: f64::from(relative.height()),
                },
            };
            // SAFETY: plain property setter.
            unsafe { cfg.setSourceRect(src) };
            match capture_image(&filter, &cfg) {
                Ok(img) => return Ok(img),
                // A stale display object after a hot-plug: refresh the list once and retry.
                Err(BackendError::Os(_)) if !retried => retried = true,
                Err(e) => return Err(e),
            }
        }
    }

    /// One display's `relative` rect scaled to `width` x `height` by ScreenCaptureKit itself: for
    /// change detection, so origin and scale are left at their defaults.
    pub(crate) fn capture_small(
        &self,
        cg_id: u32,
        relative: Rect,
        width: u32,
        height: u32,
    ) -> BackendResult<Image> {
        perm::require_screen_capture("take screenshots")?;
        let img = self.capture_display_rect(cg_id, relative, width, height)?;
        let (w, h, rgba) = cgimage_to_rgba(&img)?;
        Ok(Image::new(w, h, rgba)?)
    }

    /// Capture one window's own pixels (even when covered), at `scale` pixels per point.
    pub(crate) fn capture_window(
        &self,
        window_id: u32,
        rect: Rect,
        scale: f64,
    ) -> BackendResult<Image> {
        perm::require_screen_capture("take screenshots")?;
        let width = to_pixels(rect.width(), scale).max(1);
        let height = to_pixels(rect.height(), scale).max(1);
        let found = shareable(move |c| {
            // SAFETY: reading properties of live SCWindow objects.
            unsafe { c.windows() }
                .iter()
                .find(|w| unsafe { w.windowID() } == window_id)
                .map(Shared)
        })?;
        let Some(win) = found else {
            return Err(BackendError::WindowGone);
        };
        // SAFETY: init with a live SCWindow.
        let filter = unsafe {
            SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &win.0)
        };
        let cfg = new_config(width, height);
        // SAFETY: plain property setter; drops the drop shadow so the image is the window frame.
        unsafe { cfg.setIgnoreShadowsSingleWindow(true) };
        let img = capture_image(&filter, &cfg)?;
        let (w, h, rgba) = cgimage_to_rgba(&img)?;
        Ok(Image::new(w, h, rgba)?
            .with_scale(scale)
            .with_origin(Point {
                x: rect.left,
                y: rect.top,
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blit_places_and_clips() {
        let mut canvas = Canvas::new(4, 4);
        canvas.blit(&[9u8; 3 * 3 * 4], 3, 3, 2, 2);
        let px = |x: usize, y: usize| canvas.rgba[(y * 4 + x) * 4];
        assert_eq!(px(1, 1), 0);
        assert_eq!(px(2, 2), 9);
        assert_eq!(px(3, 3), 9);
        assert_eq!(canvas.rgba.len(), 64);
    }

    #[test]
    fn blit_outside_the_canvas_does_nothing() {
        let mut canvas = Canvas::new(2, 2);
        canvas.blit(&[7u8; 16], 2, 2, 5, 5);
        assert!(
            canvas
                .rgba
                .chunks(4)
                .all(|p| p[..3] == [0, 0, 0] && p[3] == 255)
        );
    }

    #[test]
    fn a_new_canvas_is_opaque() {
        let canvas = Canvas::new(2, 1);
        assert_eq!(canvas.rgba, [0, 0, 0, 255, 0, 0, 0, 255]);
    }
}
