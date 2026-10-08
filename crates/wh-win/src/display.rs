//! Monitors and screen capture (Python `computer.py:monitors`, `core.py:_virtual_screen`, `grab`).
//!
//! The process is per-monitor-v2 DPI aware (see `WindowsBackend::init`), so every coordinate here is
//! a physical pixel and a display's scale is 1.0. Capture is a GDI `BitBlt` from the screen DC into
//! a top-down 32-bit DIB section, the same call `mss` makes, then one pass to RGBA.

use std::ffi::c_void;
use std::mem::size_of;

use wh_core::backend::{BackendError, BackendResult, Display, Image, Point, Rect};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetMonitorInfoW, HBITMAP,
    HDC, HGDIOBJ, HMONITOR, MONITORINFO, MONITORINFOEXW, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};
use windows::core::BOOL;

use crate::util::{from_wide, os_err};

/// `MONITORINFOF_PRIMARY`.
const MONITORINFOF_PRIMARY: u32 = 1;

/// The rect of the whole desktop across every monitor (Python `_virtual_screen`).
pub(crate) fn virtual_screen_rect() -> Rect {
    // SAFETY: GetSystemMetrics takes a plain index and has no memory-safety preconditions.
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    Rect::from_xywh(x, y, w, h)
}

/// One monitor as the OS reports it, before ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Monitor {
    pub rect: Rect,
    pub primary: bool,
    pub name: String,
}

/// Primary first, then left to right (ties top to bottom), numbered from 1 (Python `monitors`).
pub(crate) fn order_monitors(mut mons: Vec<Monitor>) -> Vec<Display> {
    mons.sort_by_key(|m| (!m.primary, m.rect.left, m.rect.top));
    mons.into_iter()
        .zip(1u32..)
        .map(|(m, id)| Display {
            id,
            name: m.name,
            rect: m.rect,
            scale: 1.0,
            primary: m.primary,
        })
        .collect()
}

unsafe extern "system" fn collect_monitor(
    hmon: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: `lparam` is the address of the `Vec<Monitor>` that `monitors` keeps alive for the call.
    let out = unsafe { &mut *(lparam.0 as *mut Vec<Monitor>) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: `info` is a live MONITORINFOEXW with cbSize set; the cast is the documented way to pass it.
    let ok = unsafe { GetMonitorInfoW(hmon, std::ptr::from_mut(&mut info).cast::<MONITORINFO>()) };
    if ok.as_bool() {
        let r = info.monitorInfo.rcMonitor;
        out.push(Monitor {
            rect: Rect::new(r.left, r.top, r.right, r.bottom),
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            name: from_wide(&info.szDevice),
        });
    }
    BOOL(1)
}

/// Every display, ordered (Python `computer.monitors`).
pub(crate) fn monitors() -> BackendResult<Vec<Display>> {
    let mut found: Vec<Monitor> = Vec::new();
    // SAFETY: the callback only touches the Vec behind `lparam`, which outlives this synchronous call.
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(std::ptr::from_mut(&mut found) as isize),
        )
    };
    if !ok.as_bool() || found.is_empty() {
        return Err(BackendError::Os("could not enumerate the monitors".into()));
    }
    Ok(order_monitors(found))
}

// ------------------------------------------------------------------------------------ capture

/// GDI objects for one capture, released in reverse order of creation.
struct Grab {
    screen: HDC,
    mem: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}

impl Drop for Grab {
    fn drop(&mut self) {
        // SAFETY: each object was created in `capture_rect`, is owned by this struct, and is released once.
        unsafe {
            SelectObject(self.mem, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.mem);
            ReleaseDC(None, self.screen);
        }
    }
}

/// BGRA (what GDI writes) to RGBA with an opaque alpha.
pub(crate) fn bgra_to_rgba(src: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; src.len()];
    let (dst_px, _) = out.as_chunks_mut::<4>();
    let (src_px, _) = src.as_chunks::<4>();
    for (d, s) in dst_px.iter_mut().zip(src_px) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = 255;
    }
    out
}

/// Copy `rect` (virtual desktop coordinates) from the screen into an RGBA image. Parts of the rect
/// outside the desktop come back black, as with `mss`.
pub(crate) fn capture_rect(rect: Rect) -> BackendResult<Image> {
    let (w, h) = (rect.width(), rect.height());
    if w <= 0 || h <= 0 {
        return Err(BackendError::Os(format!(
            "cannot capture an empty region ({w}x{h})"
        )));
    }
    let byte_len = (w as usize) * (h as usize) * 4;

    // SAFETY: a null window means the whole screen; the returned DC is released by `Grab::drop`.
    let screen = unsafe { GetDC(None) };
    if screen.is_invalid() {
        return Err(BackendError::Os(
            "could not get the screen device context".into(),
        ));
    }
    // SAFETY: `screen` is a valid DC; the compatible DC is released by `Grab::drop`.
    let mem = unsafe { CreateCompatibleDC(Some(screen)) };
    if mem.is_invalid() {
        // SAFETY: releasing the DC we just got.
        unsafe { ReleaseDC(None, screen) };
        return Err(BackendError::Os(
            "could not create a memory device context".into(),
        ));
    }

    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // negative: top-down rows, no flip needed
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: `info` describes a 32-bit top-down DIB; `bits` receives the section's pixel pointer.
    let bitmap = unsafe { CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0) };
    let bitmap = match bitmap {
        Ok(b) if !bits.is_null() => b,
        other => {
            // SAFETY: unwinding the two objects created above, nothing else holds them.
            unsafe {
                let _ = DeleteDC(mem);
                ReleaseDC(None, screen);
            }
            return Err(match other {
                Err(e) => os_err("could not create the capture bitmap", &e),
                Ok(_) => BackendError::Os("could not create the capture bitmap".into()),
            });
        }
    };
    // SAFETY: `mem` and `bitmap` are valid; the previous object is restored in `Grab::drop`.
    let previous = unsafe { SelectObject(mem, HGDIOBJ(bitmap.0)) };
    let grab = Grab {
        screen,
        mem,
        bitmap,
        previous,
    };

    // SAFETY: both DCs are valid and the destination bitmap is selected into `mem`.
    let blit = unsafe {
        BitBlt(
            grab.mem,
            0,
            0,
            w,
            h,
            Some(grab.screen),
            rect.left,
            rect.top,
            SRCCOPY | CAPTUREBLT,
        )
    };
    if let Err(e) = blit {
        return Err(os_err(
            "screen capture failed (is the desktop locked or the session disconnected?)",
            &e,
        ));
    }
    // SAFETY: the DIB section owns `byte_len` bytes at `bits` while `grab` lives and BitBlt has finished.
    let pixels = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), byte_len) };
    let rgba = bgra_to_rgba(pixels);
    drop(grab);

    let image = Image::new(w as u32, h as u32, rgba)?;
    Ok(image.with_scale(1.0).with_origin(Point {
        x: rect.left,
        y: rect.top,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(l: i32, t: i32, w: i32, h: i32, primary: bool) -> Monitor {
        Monitor {
            rect: Rect::from_xywh(l, t, w, h),
            primary,
            name: format!("\\\\.\\DISPLAY{l}"),
        }
    }

    #[test]
    fn primary_comes_first_then_left_to_right() {
        let ordered = order_monitors(vec![
            mon(2560, 0, 1920, 1080, false),
            mon(-1920, 0, 1920, 1080, false),
            mon(0, 0, 2560, 1440, true),
        ]);
        let lefts: Vec<i32> = ordered.iter().map(|d| d.rect.left).collect();
        assert_eq!(lefts, [0, -1920, 2560]);
        assert_eq!(ordered.iter().map(|d| d.id).collect::<Vec<_>>(), [1, 2, 3]);
        assert!(ordered[0].primary && !ordered[1].primary);
        assert!(ordered.iter().all(|d| d.scale == 1.0));
    }

    #[test]
    fn monitors_stacked_on_the_same_left_edge_order_top_down() {
        let ordered = order_monitors(vec![
            mon(0, 1080, 1920, 1080, false),
            mon(0, 0, 1920, 1080, true),
            mon(1920, -400, 1920, 1080, false),
            mon(1920, 0, 1920, 1080, false),
        ]);
        let tops: Vec<(i32, i32)> = ordered.iter().map(|d| (d.rect.left, d.rect.top)).collect();
        assert_eq!(tops, [(0, 0), (0, 1080), (1920, -400), (1920, 0)]);
    }

    #[test]
    fn gdi_pixels_become_opaque_rgba() {
        let bgra = [10, 20, 30, 0, 1, 2, 3, 99];
        assert_eq!(bgra_to_rgba(&bgra), [30, 20, 10, 255, 3, 2, 1, 255]);
        assert!(bgra_to_rgba(&[]).is_empty());
    }

    #[test]
    fn an_empty_region_is_refused_before_touching_gdi() {
        assert!(capture_rect(Rect::new(5, 5, 5, 50)).is_err());
        assert!(capture_rect(Rect::new(5, 5, 50, 4)).is_err());
    }
}
