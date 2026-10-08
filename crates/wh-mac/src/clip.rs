//! The clipboard: `NSPasteboard.generalPasteboard`.

use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypeString};
use objc2_foundation::{NSString, NSURL};
use wh_core::backend::{BackendError, BackendResult, ClipboardContent};

/// What the clipboard holds: copied files first (a Finder copy also carries the file name as
/// text), then text, then "something else".
pub(crate) fn read() -> BackendResult<ClipboardContent> {
    let pb = NSPasteboard::generalPasteboard();
    // SAFETY: reading immutable AppKit constants.
    let (file_type, string_type) = unsafe { (NSPasteboardTypeFileURL, NSPasteboardTypeString) };
    let mut files = Vec::new();
    if let Some(items) = pb.pasteboardItems() {
        for item in items.iter() {
            if let Some(url) = item
                .stringForType(file_type)
                .and_then(|s| NSURL::URLWithString(&s))
                .and_then(|u| u.path())
            {
                files.push(url.to_string());
            }
        }
    }
    if !files.is_empty() {
        return Ok(ClipboardContent::Files(files));
    }
    if let Some(s) = pb.stringForType(string_type) {
        return Ok(ClipboardContent::Text(s.to_string()));
    }
    let has_any = pb.types().is_some_and(|t| t.count() > 0);
    Ok(if has_any {
        ClipboardContent::Other
    } else {
        ClipboardContent::Empty
    })
}

/// Replace the clipboard with text.
pub(crate) fn set_text(text: &str) -> BackendResult<()> {
    let pb = NSPasteboard::generalPasteboard();
    // SAFETY: reading an immutable AppKit constant.
    let string_type = unsafe { NSPasteboardTypeString };
    pb.clearContents();
    if pb.setString_forType(&NSString::from_str(text), string_type) {
        Ok(())
    } else {
        Err(BackendError::Os("the pasteboard refused the text".into()))
    }
}

/// The pasteboard's change counter.
pub(crate) fn sequence() -> u64 {
    NSPasteboard::generalPasteboard().changeCount().max(0) as u64
}
