//! The clipboard (Python `fast.py:_clipboard_get`, `_clipboard_set`, `_open_clipboard`,
//! `computer.py:_clipboard_summary`, `_clip_seq`).
//!
//! Raw Win32, like the Python build: another program may hold the clipboard for a moment, so opening
//! it retries for half a second. Reads look at files (`CF_HDROP`) before text, because a file copy
//! in Explorer also carries a text flavour of its own.

use std::thread::sleep;
use std::time::Duration;

use wh_core::backend::{BackendError, BackendResult, ClipboardContent};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, CountClipboardFormats, EmptyClipboard, GetClipboardData,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

use crate::util::{from_wide, os_err};

/// `CF_UNICODETEXT`.
const CF_UNICODETEXT: u32 = 13;
/// `CF_HDROP`: files copied in a file manager.
const CF_HDROP: u32 = 15;

/// An open clipboard, closed when dropped.
struct Open;

impl Open {
    /// Try 10 times, 50 ms apart (Python `_open_clipboard`).
    fn acquire() -> BackendResult<Self> {
        for attempt in 0..10 {
            // SAFETY: opening with no owner window, closed again by `Drop for Open`.
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Open);
            }
            if attempt < 9 {
                sleep(Duration::from_millis(50));
            }
        }
        Err(BackendError::Os(
            "the clipboard is held by another program; try again".into(),
        ))
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: this value only exists while the clipboard is open by this thread.
        let _ = unsafe { CloseClipboard() };
    }
}

fn format_available(format: u32) -> bool {
    // SAFETY: a plain query that does not need the clipboard to be open.
    unsafe { IsClipboardFormatAvailable(format) }.is_ok()
}

/// The file names of a `CF_HDROP` handle, full paths.
fn drop_files(handle: HANDLE) -> Vec<String> {
    let hdrop = HDROP(handle.0);
    // SAFETY: 0xFFFFFFFF asks for the count; `hdrop` came from GetClipboardData under an open clipboard.
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    (0..count)
        .map(|i| {
            let mut buf = vec![0u16; 1024];
            // SAFETY: `buf` has room for 1024 UTF-16 units; the call writes at most that many.
            let n = unsafe { DragQueryFileW(hdrop, i, Some(&mut buf)) } as usize;
            from_wide(&buf[..n.min(buf.len())])
        })
        .collect()
}

/// The text of a `CF_UNICODETEXT` handle, up to the NUL (bounded by the block's size).
fn unicode_text(handle: HANDLE) -> Option<String> {
    let global = HGLOBAL(handle.0);
    // SAFETY: the handle came from GetClipboardData; the lock is released below.
    let ptr = unsafe { GlobalLock(global) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: querying the size of the block we just locked.
    let units = unsafe { GlobalSize(global) } / 2;
    // SAFETY: the locked block holds `units` UTF-16 units, valid until GlobalUnlock.
    let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<u16>(), units) };
    let text = from_wide(slice);
    // SAFETY: releasing the lock taken above.
    let _ = unsafe { GlobalUnlock(global) };
    Some(text)
}

/// What the clipboard holds now.
pub(crate) fn read() -> BackendResult<ClipboardContent> {
    let _open = Open::acquire()?;
    if format_available(CF_HDROP) {
        // SAFETY: the clipboard is open (guard above).
        let files = match unsafe { GetClipboardData(CF_HDROP) } {
            Ok(h) => drop_files(h),
            Err(_) => Vec::new(),
        };
        return Ok(ClipboardContent::Files(files));
    }
    if format_available(CF_UNICODETEXT) {
        // SAFETY: the clipboard is open (guard above).
        let text = unsafe { GetClipboardData(CF_UNICODETEXT) }
            .ok()
            .and_then(unicode_text)
            .unwrap_or_default();
        return Ok(ClipboardContent::Text(text));
    }
    // SAFETY: a plain count on the open clipboard.
    Ok(if unsafe { CountClipboardFormats() } == 0 {
        ClipboardContent::Empty
    } else {
        ClipboardContent::Other
    })
}

/// UTF-16 with a terminating NUL, as bytes: the layout of a `CF_UNICODETEXT` block.
pub(crate) fn unicode_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// Replace the clipboard with text.
pub(crate) fn write_text(text: &str) -> BackendResult<()> {
    let data = unicode_bytes(text);
    let _open = Open::acquire()?;
    // SAFETY: the clipboard is open (guard above).
    unsafe { EmptyClipboard() }.map_err(|e| os_err("could not clear the clipboard", &e))?;
    // SAFETY: a zeroed movable block of the exact size; the clipboard owns it on success, we free it on failure.
    let block = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, data.len()) }
        .map_err(|e| os_err("could not allocate clipboard memory", &e))?;
    // SAFETY: locking the block just allocated.
    let ptr = unsafe { GlobalLock(block) };
    if ptr.is_null() {
        // SAFETY: the block is still ours.
        let _ = unsafe { GlobalFree(Some(block)) };
        return Err(BackendError::Os("could not lock clipboard memory".into()));
    }
    // SAFETY: the block is `data.len()` bytes and does not overlap `data`.
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.cast::<u8>(), data.len()) };
    // SAFETY: releasing the lock taken above.
    let _ = unsafe { GlobalUnlock(block) };
    // SAFETY: the clipboard is open and `block` is a movable block holding UTF-16 text.
    match unsafe { SetClipboardData(CF_UNICODETEXT, Some(HANDLE(block.0))) } {
        Ok(_) => Ok(()),
        Err(e) => {
            // SAFETY: the clipboard did not take the block, so it is still ours.
            let _ = unsafe { GlobalFree(Some(block)) };
            Err(os_err("could not set the clipboard", &e))
        }
    }
}

/// `GetClipboardSequenceNumber`, `None` when the OS gives none (0: no access to the window station).
pub(crate) fn sequence() -> Option<u64> {
    // SAFETY: no arguments, no preconditions.
    let n = unsafe { GetClipboardSequenceNumber() };
    (n != 0).then_some(u64::from(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_utf16_with_a_terminating_nul() {
        assert_eq!(unicode_bytes("hi"), [b'h', 0, b'i', 0, 0, 0]);
        assert_eq!(unicode_bytes(""), [0, 0]);
        // An emoji is a surrogate pair: D83D DE00.
        assert_eq!(unicode_bytes("\u{1F600}"), [0x3D, 0xD8, 0x00, 0xDE, 0, 0]);
    }

    #[test]
    fn newlines_are_kept_as_typed() {
        // Python wrote the text as is, with no \n to \r\n conversion; so do we.
        assert_eq!(unicode_bytes("a\nb").len(), (3 + 1) * 2);
    }
}
