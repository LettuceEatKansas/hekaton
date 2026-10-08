//! Small helpers shared by the modules: handle conversions, UTF-16, owned handles, thread COM.

use std::cell::Cell;
use std::ffi::c_void;

use wh_core::backend::{BackendError, WindowRef};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

/// A `WindowRef` from an `HWND`. The handle's bits are the ref, so one window keeps one ref for its
/// whole life and the core's `hwnd:N` specs stay valid.
pub(crate) fn window_ref(hwnd: HWND) -> WindowRef {
    WindowRef::from_raw(hwnd.0 as usize as u64)
}

/// The `HWND` a `WindowRef` was made from.
pub(crate) fn hwnd_of(window: WindowRef) -> HWND {
    HWND(window.raw() as usize as *mut c_void)
}

/// `Some(ref)` for a live handle, `None` for the null handle.
pub(crate) fn opt_ref(hwnd: HWND) -> Option<WindowRef> {
    (!hwnd.0.is_null()).then(|| window_ref(hwnd))
}

/// NUL-terminated UTF-16 for a Win32 `PCWSTR`.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 up to the first NUL, lossily decoded.
pub(crate) fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// The file name part of a Windows path (`C:\Windows\notepad.exe` is `notepad.exe`).
pub(crate) fn exe_basename(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// An OS failure as a short, factual backend error.
pub(crate) fn os_err(what: &str, e: &windows::core::Error) -> BackendError {
    BackendError::Os(format!("{what}: {e}"))
}

/// A kernel handle that closes itself.
pub(crate) struct OwnedHandle(pub HANDLE);

impl OwnedHandle {
    /// Wrap a handle the caller owns.
    pub(crate) fn new(h: HANDLE) -> Self {
        Self(h)
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle was opened by us, is closed exactly once here, and is never used again.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

thread_local! {
    static COM_READY: Cell<bool> = const { Cell::new(false) };
}

/// Give the calling thread a COM apartment (STA) if it has none. The shell objects behind `.lnk`
/// parsing and the Store app list want one; every thread that touches them calls this first.
/// A thread that already has an apartment of the other kind keeps it, which is fine here.
pub(crate) fn ensure_com() {
    COM_READY.with(|ready| {
        if !ready.get() {
            // SAFETY: plain COM init on this thread; S_FALSE and RPC_E_CHANGED_MODE both mean it has an apartment.
            let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            ready.set(true);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_refs_round_trip_through_handles() {
        let hwnd = HWND(0x0001_02A4_usize as *mut c_void);
        let r = window_ref(hwnd);
        assert_eq!(r.raw(), 0x0001_02A4);
        assert_eq!(hwnd_of(r), hwnd);
        assert_eq!(opt_ref(HWND(std::ptr::null_mut())), None);
        assert_eq!(opt_ref(hwnd), Some(r));
    }

    #[test]
    fn wide_strings_stop_at_the_first_nul() {
        let w = wide("hi\u{1F600}");
        assert_eq!(w.last(), Some(&0));
        assert_eq!(w.len(), 5); // h, i, surrogate pair, NUL
        assert_eq!(from_wide(&w), "hi\u{1F600}");
        assert_eq!(from_wide(&[0x61, 0, 0x62]), "a");
        assert_eq!(from_wide(&[]), "");
    }

    #[test]
    fn basename_takes_the_last_component() {
        assert_eq!(
            exe_basename(r"C:\Windows\System32\notepad.exe"),
            "notepad.exe"
        );
        assert_eq!(exe_basename("C:/x/y.EXE"), "y.EXE");
        assert_eq!(exe_basename("plain.exe"), "plain.exe");
    }
}
