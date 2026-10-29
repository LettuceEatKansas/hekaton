//! A live ScreenCaptureKit stream per display, so the settle loop's looks and screenshots read the
//! newest frame (a few ms) instead of starting a one-shot capture each time (~50 ms on this Mac,
//! 2026-10-29).
//!
//! A stream starts on first use and stops after [`IDLE`] without one, so the menu bar's
//! screen-recording indicator only shows while an agent is working. Until a new stream delivers its
//! first frame, callers use the one-shot path. `WINHANDS_NO_STREAM=1` turns streaming off.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, Encoding, RefEncode, define_class, msg_send};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};
use wh_core::backend::{Image, Rect};

use crate::cf::Shared;

/// A stream nobody has read for this long is stopped.
const IDLE: Duration = Duration::from_secs(30);
/// How often the idle check runs.
const REAP_EVERY: Duration = Duration::from_secs(2);
/// Widest stream kept; a larger display streams scaled down and full-size captures of it use the
/// one-shot path.
const MAX_STREAM_WIDTH: u32 = 3840;
/// `kCVPixelFormatType_32BGRA`.
const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241;
/// `SCFrameStatusComplete`: a frame with new pixels.
const FRAME_COMPLETE: i64 = 0;

/// CoreMedia's `CMSampleBuffer`, only ever behind a pointer.
#[repr(C)]
pub(crate) struct OpaqueSample {
    _private: [u8; 0],
}

// SAFETY: matches the runtime's `^{opaqueCMSampleBuffer=}` for a CMSampleBufferRef.
unsafe impl RefEncode for OpaqueSample {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("opaqueCMSampleBuffer", &[]));
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetImageBuffer(sample: *mut OpaqueSample) -> *mut c_void;
    fn CMSampleBufferGetSampleAttachmentsArray(
        sample: *mut OpaqueSample,
        create_if_necessary: u8,
    ) -> *const c_void;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetWidth(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(buffer: *mut c_void) -> usize;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(number: *const c_void, kind: isize, out: *mut c_void) -> u8;
}

/// `kCVPixelBufferLock_ReadOnly`.
const LOCK_READ_ONLY: u64 = 1;
/// `kCFNumberSInt64Type`.
const CF_NUMBER_SINT64: isize = 4;

/// One retained `CVPixelBuffer`.
pub(crate) struct Frame(*mut c_void);

// SAFETY: CoreVideo pixel buffers are reference counted thread-safely, and this type only reads
// one after locking its base address read-only.
unsafe impl Send for Frame {}
// SAFETY: as above.
unsafe impl Sync for Frame {}

impl Clone for Frame {
    fn clone(&self) -> Self {
        // SAFETY: self.0 is a live buffer this value holds a reference to.
        unsafe { CFRetain(self.0) };
        Self(self.0)
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: releases the reference taken when this value was made.
        unsafe { CFRelease(self.0) };
    }
}

impl Frame {
    /// Width and height in pixels.
    pub(crate) fn size(&self) -> (u32, u32) {
        // SAFETY: plain getters on a live buffer.
        unsafe {
            (
                CVPixelBufferGetWidth(self.0) as u32,
                CVPixelBufferGetHeight(self.0) as u32,
            )
        }
    }

    /// The `src` pixel rect of the frame as RGBA, scaled to `out_w` x `out_h` (an exact copy when the
    /// sizes match, else an area average). `None` if the frame cannot be read or `src` is empty.
    pub(crate) fn read(&self, src: Rect, out_w: u32, out_h: u32) -> Option<Image> {
        let (fw, fh) = self.size();
        let left = src.left.clamp(0, fw as i32) as usize;
        let top = src.top.clamp(0, fh as i32) as usize;
        let right = src.right.clamp(0, fw as i32) as usize;
        let bottom = src.bottom.clamp(0, fh as i32) as usize;
        if right <= left || bottom <= top || out_w == 0 || out_h == 0 {
            return None;
        }
        // SAFETY: the buffer is live; it is locked read-only for the copy below and unlocked after.
        if unsafe { CVPixelBufferLockBaseAddress(self.0, LOCK_READ_ONLY) } != 0 {
            return None;
        }
        // SAFETY: valid while locked.
        let (base, row) = unsafe {
            (
                CVPixelBufferGetBaseAddress(self.0).cast::<u8>(),
                CVPixelBufferGetBytesPerRow(self.0),
            )
        };
        let out = if base.is_null() {
            None
        } else {
            // SAFETY: a locked 32BGRA buffer spans `row * height` bytes from its base address.
            let pixels = unsafe { std::slice::from_raw_parts(base, row * fh as usize) };
            Some(scale_bgra(
                pixels,
                row,
                (left, top, right - left, bottom - top),
                out_w as usize,
                out_h as usize,
            ))
        };
        // SAFETY: matches the lock above.
        unsafe { CVPixelBufferUnlockBaseAddress(self.0, LOCK_READ_ONLY) };
        Image::new(out_w, out_h, out?).ok()
    }
}

/// Copy or area-average `src` (x, y, w, h) of a BGRA buffer into `out_w` x `out_h` RGBA. Each output
/// pixel averages up to 4 x 2 samples spread over its area, enough to see a typed letter in a settle
/// look without walking every source pixel.
fn scale_bgra(
    px: &[u8],
    row: usize,
    (sx, sy, sw, sh): (usize, usize, usize, usize),
    out_w: usize,
    out_h: usize,
) -> Vec<u8> {
    let mut out = vec![0u8; out_w * out_h * 4];
    if sw == out_w && sh == out_h {
        for y in 0..sh {
            let from = (sy + y) * row + sx * 4;
            let src = &px[from..from + sw * 4];
            let dst = &mut out[y * out_w * 4..(y + 1) * out_w * 4];
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 255;
            }
        }
        return out;
    }
    for oy in 0..out_h {
        let y0 = sy + oy * sh / out_h;
        let y1 = (sy + (oy + 1) * sh / out_h).max(y0 + 1);
        // Two sample rows per box: reading a stream frame is memory bound (an IOSurface), and the
        // rows are what cost; four columns per row keep a typed letter visible.
        let ystep = ((y1 - y0) / 2).max(1);
        for ox in 0..out_w {
            let x0 = sx + ox * sw / out_w;
            let x1 = (sx + (ox + 1) * sw / out_w).max(x0 + 1);
            let xstep = ((x1 - x0) / 4).max(1);
            let (mut b, mut g, mut r, mut n) = (0u32, 0u32, 0u32, 0u32);
            let mut y = y0;
            while y < y1 {
                let mut x = x0;
                while x < x1 {
                    let i = y * row + x * 4;
                    b += u32::from(px[i]);
                    g += u32::from(px[i + 1]);
                    r += u32::from(px[i + 2]);
                    n += 1;
                    x += xstep;
                }
                y += ystep;
            }
            let o = (oy * out_w + ox) * 4;
            out[o] = (r / n) as u8;
            out[o + 1] = (g / n) as u8;
            out[o + 2] = (b / n) as u8;
            out[o + 3] = 255;
        }
    }
    out
}

/// What the stream's handler queue shares with readers.
#[derive(Default)]
pub(crate) struct Feed {
    latest: Mutex<Option<Frame>>,
    stopped: AtomicBool,
}

/// The newest complete frame in a sample, retained.
fn complete_frame(sample: *mut OpaqueSample) -> Option<Frame> {
    if sample.is_null() {
        return None;
    }
    // SAFETY: the sample is live for the handler call; every CF read below borrows from it.
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        if !attachments.is_null() && CFArrayGetCount(attachments) > 0 {
            let dict = CFArrayGetValueAtIndex(attachments, 0);
            let key: *const c_void = (SCStreamFrameInfoStatus as *const _ as *const c_void).cast();
            let number = CFDictionaryGetValue(dict, key);
            let mut status: i64 = FRAME_COMPLETE;
            if !number.is_null() {
                CFNumberGetValue(number, CF_NUMBER_SINT64, (&raw mut status).cast());
            }
            if status != FRAME_COMPLETE {
                return None;
            }
        }
        let buffer = CMSampleBufferGetImageBuffer(sample);
        if buffer.is_null() {
            return None;
        }
        CFRetain(buffer);
        Some(Frame(buffer))
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and Sink has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "WinhandsFrameSink"]
    #[ivars = Arc<Feed>]
    struct Sink;

    unsafe impl NSObjectProtocol for Sink {}

    unsafe impl SCStreamOutput for Sink {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: *mut OpaqueSample, kind: SCStreamOutputType) {
            if kind.0 != SCStreamOutputType::Screen.0 {
                return;
            }
            if let Some(frame) = complete_frame(sample) {
                *self.ivars().latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
            }
        }
    }

    unsafe impl SCStreamDelegate for Sink {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, _error: &NSError) {
            self.ivars().stopped.store(true, Ordering::Relaxed);
        }
    }
);

/// One running stream.
struct Live {
    stream: Shared<Retained<SCStream>>,
    _sink: Shared<Retained<Sink>>,
    _queue: DispatchRetained<DispatchQueue>,
    feed: Arc<Feed>,
    /// The stream's frame size in pixels.
    size: (u32, u32),
    last_use: Instant,
}

impl Live {
    fn stop(self) {
        // SAFETY: stopping a live stream; the handler is optional.
        unsafe { self.stream.0.stopCaptureWithCompletionHandler(None) };
    }
}

type LiveMap = Arc<Mutex<HashMap<u32, Live>>>;

/// Streams by CoreGraphics display id.
#[derive(Default)]
pub(crate) struct Feeds {
    live: LiveMap,
    reaper: OnceLock<()>,
}

/// Is streaming switched on (`WINHANDS_NO_STREAM` unset)?
fn enabled() -> bool {
    std::env::var_os("WINHANDS_NO_STREAM").is_none()
}

/// The stream size for a display of `points` at `scale`: native pixels, capped at
/// [`MAX_STREAM_WIDTH`] wide with the aspect kept.
fn stream_size(points: Rect, scale: f64) -> (u32, u32) {
    let w = (f64::from(points.width()) * scale).round().max(1.0);
    let h = (f64::from(points.height()) * scale).round().max(1.0);
    let cap = f64::from(MAX_STREAM_WIDTH);
    if w <= cap {
        (w as u32, h as u32)
    } else {
        (MAX_STREAM_WIDTH, (h * cap / w).round().max(1.0) as u32)
    }
}

impl Feeds {
    /// The newest frame of display `cg_id` (bounds `points` at `scale`) and the frame's pixels per
    /// point, starting its stream if none runs. `None` until a new stream's first frame arrives, or
    /// when streaming is off or failed: use the one-shot path.
    pub(crate) fn frame(
        &self,
        cg_id: u32,
        points: Rect,
        scale: f64,
        display: impl FnOnce() -> Option<Retained<SCDisplay>>,
    ) -> Option<(Frame, f64)> {
        if !enabled() {
            return None;
        }
        let size = stream_size(points, scale);
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(l) = live.get(&cg_id)
            && (l.feed.stopped.load(Ordering::Relaxed) || l.size != size)
        {
            // Stopped by the system, or the display changed resolution: start over.
            if let Some(old) = live.remove(&cg_id) {
                old.stop();
            }
        }
        if let Some(l) = live.get_mut(&cg_id) {
            l.last_use = Instant::now();
            let frame = l.feed.latest.lock().unwrap_or_else(|e| e.into_inner()).clone()?;
            let per_point = f64::from(frame.size().0) / f64::from(points.width().max(1));
            return Some((frame, per_point));
        }
        let display = display()?;
        let started = start(&display, size)?;
        live.insert(cg_id, started);
        drop(live);
        self.start_reaper();
        None
    }

    fn start_reaper(&self) {
        let live = Arc::clone(&self.live);
        self.reaper.get_or_init(move || {
            thread::Builder::new()
                .name("winhands-stream-reaper".into())
                .spawn(move || {
                    loop {
                        thread::sleep(REAP_EVERY);
                        let mut map = live.lock().unwrap_or_else(|e| e.into_inner());
                        let idle: Vec<u32> = map
                            .iter()
                            .filter(|(_, l)| {
                                l.last_use.elapsed() > IDLE || l.feed.stopped.load(Ordering::Relaxed)
                            })
                            .map(|(id, _)| *id)
                            .collect();
                        for id in idle {
                            if let Some(l) = map.remove(&id) {
                                l.stop();
                            }
                        }
                    }
                })
                .ok();
        });
    }
}

/// Start a stream of `display` at `size` pixels. `None` if ScreenCaptureKit refuses.
fn start(display: &SCDisplay, size: (u32, u32)) -> Option<Live> {
    let feed = Arc::new(Feed::default());
    let sink: Retained<Sink> = {
        let this = Sink::alloc().set_ivars(Arc::clone(&feed));
        // SAFETY: NSObject's init on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    };
    // SAFETY: property setters on new objects and the documented stream setup calls, all with live
    // arguments. The sink and queue are kept in `Live` for as long as the stream runs.
    unsafe {
        let filter = SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            display,
            &NSArray::<SCWindow>::new(),
        );
        let cfg = SCStreamConfiguration::new();
        cfg.setWidth(size.0 as usize);
        cfg.setHeight(size.1 as usize);
        cfg.setShowsCursor(false);
        cfg.setPixelFormat(PIXEL_FORMAT_BGRA);
        cfg.setQueueDepth(5);
        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &cfg,
            Some(ProtocolObject::from_ref(&*sink)),
        );
        let queue = DispatchQueue::new("winhands.frames", None);
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*sink),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .ok()?;
        let failed = Arc::clone(&feed);
        let done = RcBlock::new(move |err: *mut NSError| {
            if !err.is_null() {
                failed.stopped.store(true, Ordering::Relaxed);
            }
        });
        stream.startCaptureWithCompletionHandler(Some(&done));
        Some(Live {
            stream: Shared(stream),
            _sink: Shared(sink),
            _queue: queue,
            feed,
            size,
            last_use: Instant::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra(w: usize, h: usize, f: impl Fn(usize, usize) -> [u8; 4]) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                v.extend_from_slice(&f(x, y));
            }
        }
        v
    }

    #[test]
    fn a_same_size_read_is_an_exact_copy_with_red_and_blue_swapped() {
        let px = bgra(4, 2, |x, y| [x as u8, y as u8, 200, 0]);
        let out = scale_bgra(&px, 16, (1, 0, 2, 2), 2, 2);
        assert_eq!(&out[..8], &[200, 0, 1, 255, 200, 0, 2, 255]);
        assert_eq!(&out[8..12], &[200, 1, 1, 255]);
    }

    #[test]
    fn a_shrunk_read_averages_its_area() {
        // left half black, right half white, shrunk to 2 x 1
        let px = bgra(8, 8, |x, _| if x < 4 { [0, 0, 0, 0] } else { [255, 255, 255, 0] });
        let out = scale_bgra(&px, 32, (0, 0, 8, 8), 2, 1);
        assert_eq!(out, vec![0, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn a_small_change_survives_the_shrink() {
        // one bright 2 x 2 spot (a typed letter) in a dark 16 x 16 frame still lifts its output pixel
        let px = bgra(16, 16, |x, y| if (4..6).contains(&x) && (4..6).contains(&y) { [255; 4] } else { [0; 4] });
        let out = scale_bgra(&px, 64, (0, 0, 16, 16), 2, 2);
        assert!(out[0] > 0, "the spot was averaged away");
    }

    #[test]
    fn a_huge_display_streams_capped_with_its_aspect() {
        let r = Rect { left: 0, top: 0, right: 3008, bottom: 1692 };
        assert_eq!(stream_size(r, 2.0), (3840, 2160));
        assert_eq!(stream_size(Rect { left: 0, top: 0, right: 1920, bottom: 1080 }, 1.0), (1920, 1080));
    }
}
