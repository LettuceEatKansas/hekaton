//! Screen capture through DXGI Desktop Duplication.
//!
//! A GDI `BitBlt` from the screen reads the composed desktop back through the old graphics stack and
//! costs 25-30 ms at 2560x1440 however small the copy (measured 2026-10-28). Desktop Duplication
//! hands over the compositor's own frame on the GPU. When nothing changed since the last look,
//! `AcquireNextFrame` says so at once and the cached pixels are still the screen, so a settle look
//! at a calm screen costs microseconds and a full screenshot a GPU copy plus one memcpy.
//!
//! Anything unusual (a region spanning monitors, a rotated or HDR output, the secure desktop, a
//! driver that refuses duplication) returns `None` and the caller uses GDI. The duplication is
//! dropped after [`IDLE_RELEASE`] without use so an idle daemon holds nothing while a game runs.

use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use wh_core::backend::{Image, Point, Rect};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

/// Drop the duplication after this long without a capture.
const IDLE_RELEASE: Duration = Duration::from_secs(120);
/// After a failed open, use GDI for this long before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// One monitor being duplicated, with the last frame it delivered.
struct Output {
    rect: Rect,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    /// BGRA rows of the whole monitor, `width * 4` bytes each. Empty until the first frame.
    frame: Vec<u8>,
}

// SAFETY: D3D11 devices and DXGI duplications are free-threaded; the immediate context is not, and
// every use of it happens under the `Duplicator` mutex.
unsafe impl Send for Output {}

#[derive(Default)]
struct State {
    outputs: Option<Vec<Output>>,
    retry_at: Option<Instant>,
    last_used: Option<Instant>,
}

/// Lazily opened Desktop Duplication of every monitor.
pub(crate) struct Duplicator {
    state: Arc<Mutex<State>>,
    reaper: Once,
}

impl Default for Duplicator {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            reaper: Once::new(),
        }
    }
}

impl Duplicator {
    /// `rect` at full size, or `None` to fall back to GDI.
    pub(crate) fn capture(&self, rect: Rect) -> Option<Image> {
        self.with_output(rect, |out| {
            let (x0, y0) = (
                (rect.left - out.rect.left) as usize,
                (rect.top - out.rect.top) as usize,
            );
            let (w, h) = (rect.width() as usize, rect.height() as usize);
            let stride = out.width as usize * 4;
            let mut rgba = Vec::with_capacity(w * h * 4);
            for y in y0..y0 + h {
                let row = &out.frame[y * stride + x0 * 4..y * stride + (x0 + w) * 4];
                for p in row.as_chunks::<4>().0 {
                    rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
                }
            }
            Image::new(w as u32, h as u32, rgba).ok().map(|i| {
                i.with_scale(1.0).with_origin(Point {
                    x: rect.left,
                    y: rect.top,
                })
            })
        })
    }

    /// `rect` shrunk to `out_w` x `out_h` by averaging a 4x4 sample grid per output pixel: enough to
    /// tell whether the screen moved. `None` to fall back.
    pub(crate) fn preview(&self, rect: Rect, out_w: u32, out_h: u32) -> Option<Image> {
        self.with_output(rect, |out| {
            let (x0, y0) = (rect.left - out.rect.left, rect.top - out.rect.top);
            let (w, h) = (rect.width() as f64, rect.height() as f64);
            let stride = out.width as usize * 4;
            let mut rgba = Vec::with_capacity((out_w * out_h * 4) as usize);
            for oy in 0..out_h {
                for ox in 0..out_w {
                    let mut sum = [0u32; 3];
                    for sy in 0..4 {
                        let y = y0
                            + ((f64::from(oy) + (f64::from(sy) + 0.5) / 4.0) * h / f64::from(out_h))
                                as i32;
                        for sx in 0..4 {
                            let x = x0
                                + ((f64::from(ox) + (f64::from(sx) + 0.5) / 4.0) * w
                                    / f64::from(out_w)) as i32;
                            let i = y as usize * stride + x as usize * 4;
                            sum[0] += u32::from(out.frame[i + 2]);
                            sum[1] += u32::from(out.frame[i + 1]);
                            sum[2] += u32::from(out.frame[i]);
                        }
                    }
                    rgba.extend_from_slice(&[
                        (sum[0] / 16) as u8,
                        (sum[1] / 16) as u8,
                        (sum[2] / 16) as u8,
                        255,
                    ]);
                }
            }
            Image::new(out_w, out_h, rgba).ok().map(|i| {
                i.with_scale(f64::from(out_w) / w).with_origin(Point {
                    x: rect.left,
                    y: rect.top,
                })
            })
        })
    }

    /// Refresh the monitor holding all of `rect` and run `f` on it.
    fn with_output<T>(&self, rect: Rect, f: impl FnOnce(&Output) -> Option<T>) -> Option<T> {
        if rect.width() <= 0
            || rect.height() <= 0
            || std::env::var_os("WINHANDS_CAPTURE_GDI").is_some()
        {
            return None;
        }
        let mut st = self.state.lock().ok()?;
        let now = Instant::now();
        st.last_used = Some(now);
        self.start_reaper();
        for attempt in 0..2 {
            if st.outputs.is_none() {
                if st.retry_at.is_some_and(|t| now < t) {
                    return None;
                }
                match open() {
                    Ok(outputs) if !outputs.is_empty() => st.outputs = Some(outputs),
                    _ => {
                        st.retry_at = Some(now + RETRY_AFTER);
                        return None;
                    }
                }
            }
            let outputs = st.outputs.as_mut()?;
            let out = outputs.iter_mut().find(|o| contains(o.rect, rect))?;
            match out.refresh() {
                Ok(()) if !out.frame.is_empty() => return f(out),
                Ok(()) => return None,
                // Access lost (mode change, secure desktop): reopen once.
                Err(_) if attempt == 0 => st.outputs = None,
                Err(_) => {
                    st.outputs = None;
                    st.retry_at = Some(now + RETRY_AFTER);
                    return None;
                }
            }
        }
        None
    }

    /// One background thread that drops an idle duplication.
    fn start_reaper(&self) {
        let state = Arc::clone(&self.state);
        self.reaper.call_once(move || {
            let _ = std::thread::Builder::new()
                .name("dxgi-reaper".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(Duration::from_secs(5));
                        if let Ok(mut st) = state.lock()
                            && st.outputs.is_some()
                            && st.last_used.is_none_or(|t| t.elapsed() > IDLE_RELEASE)
                        {
                            st.outputs = None;
                        }
                    }
                });
        });
    }
}

fn contains(outer: Rect, inner: Rect) -> bool {
    inner.left >= outer.left
        && inner.top >= outer.top
        && inner.right <= outer.right
        && inner.bottom <= outer.bottom
}

impl Output {
    /// Pull the newest desktop image into `frame` if the screen changed since the last look.
    fn refresh(&mut self) -> windows::core::Result<()> {
        if !self.frame.is_empty() {
            return self.acquire(0).map(|_| ());
        }
        // A new duplication first hands over a frame with no desktop image in it (LastPresentTime 0,
        // measured 2026-10-28); the real desktop follows within a few ms. Wait for it, briefly.
        let deadline = Instant::now() + Duration::from_millis(400);
        while self.frame.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.acquire(left.as_millis() as u32)? {
                break;
            }
        }
        Ok(())
    }

    /// One acquire with `timeout_ms`. True when a frame was delivered (changed or not).
    fn acquire(&mut self, timeout_ms: u32) -> windows::core::Result<bool> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: valid duplication; both out-params live across the call.
        match unsafe {
            self.dup
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) => return Err(e),
        }
        let copied = self.copy_frame(&info, resource.as_ref());
        // SAFETY: a frame was acquired above and must be released before the next acquire.
        let released = unsafe { self.dup.ReleaseFrame() };
        copied.and(released).map(|()| true)
    }

    fn copy_frame(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<&IDXGIResource>,
    ) -> windows::core::Result<()> {
        // No new desktop image: only the pointer moved, or a new duplication's empty first frame.
        if info.LastPresentTime == 0 {
            return Ok(());
        }
        let Some(resource) = resource else {
            return Ok(());
        };
        let texture: ID3D11Texture2D = resource.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid texture, `desc` is written.
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(windows::core::Error::from(
                windows::Win32::Foundation::E_NOTIMPL,
            ));
        }
        if self.staging.is_none() || (desc.Width, desc.Height) != (self.width, self.height) {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            // SAFETY: valid device and description; `staging` receives the texture.
            unsafe {
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut staging))?
            };
            self.staging = staging;
            self.width = desc.Width;
            self.height = desc.Height;
        }
        let Some(staging) = self.staging.as_ref() else {
            return Ok(());
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: same-size, same-format textures on one device; the staging texture is CPU-readable
        // and unmapped before this function returns.
        unsafe {
            self.ctx.CopyResource(staging, &texture);
            self.ctx
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let row = self.width as usize * 4;
        let pitch = mapped.RowPitch as usize;
        self.frame.resize(row * self.height as usize, 0);
        let src = mapped.pData.cast::<u8>().cast_const();
        for y in 0..self.height as usize {
            // SAFETY: the mapping holds `height` rows of `pitch >= row` bytes; `frame` holds
            // `height` rows of `row` bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src.add(y * pitch),
                    self.frame.as_mut_ptr().add(y * row),
                    row,
                );
            }
        }
        // SAFETY: mapped above.
        unsafe { self.ctx.Unmap(staging, 0) };
        Ok(())
    }
}

/// Duplicate every monitor attached to the desktop that is not rotated.
fn open() -> windows::core::Result<Vec<Output>> {
    // SAFETY: plain factory creation.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut outputs = Vec::new();
    for a in 0.. {
        // SAFETY: valid factory; DXGI_ERROR_NOT_FOUND ends the list.
        let adapter = match unsafe { factory.EnumAdapters1(a) } {
            Ok(adapter) => adapter,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e),
        };
        let mut device: Option<(ID3D11Device, ID3D11DeviceContext)> = None;
        for o in 0.. {
            // SAFETY: valid adapter; DXGI_ERROR_NOT_FOUND ends the list.
            let output = match unsafe { adapter.EnumOutputs(o) } {
                Ok(output) => output,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e),
            };
            // SAFETY: valid output.
            let desc = unsafe { output.GetDesc()? };
            if !desc.AttachedToDesktop.as_bool()
                || (desc.Rotation != DXGI_MODE_ROTATION_IDENTITY
                    && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED)
            {
                continue;
            }
            if device.is_none() {
                let (mut dev, mut ctx) = (None, None);
                // SAFETY: a hardware device on this adapter; the out-params live across the call.
                unsafe {
                    D3D11CreateDevice(
                        &adapter,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        None,
                        D3D11_SDK_VERSION,
                        Some(&mut dev),
                        None,
                        Some(&mut ctx),
                    )?
                };
                if let (Some(dev), Some(ctx)) = (dev, ctx) {
                    device = Some((dev, ctx));
                }
            }
            let Some((dev, ctx)) = device.clone() else {
                continue;
            };
            let output1: IDXGIOutput1 = output.cast()?;
            // SAFETY: valid output and device. Refusal (unsupported, too many duplications) skips it.
            let Ok(dup) = (unsafe { output1.DuplicateOutput(&dev) }) else {
                continue;
            };
            let r = desc.DesktopCoordinates;
            outputs.push(Output {
                rect: Rect::new(r.left, r.top, r.right, r.bottom),
                device: dev,
                ctx,
                dup,
                staging: None,
                width: 0,
                height: 0,
                frame: Vec::new(),
            });
        }
    }
    Ok(outputs)
}
