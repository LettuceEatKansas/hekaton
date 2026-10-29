//! The clipboard: `NSPasteboard.generalPasteboard`.

use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypeString};
use objc2_foundation::{NSString, NSURL};
use wh_core::backend::{BackendError, BackendResult, ClipboardContent};

/// Snapshot only clipboard data the plain-text paste shortcut can restore without format loss.
/// None means use keyboard input instead. Never clears the pasteboard while inspecting it.
pub(crate) fn plain_snapshot() -> Option<(ClipboardContent, u64)> {
    let pb = NSPasteboard::generalPasteboard();
    let before = sequence();
    // SAFETY: reading an immutable AppKit constant.
    let string_type = unsafe { NSPasteboardTypeString };
    let types: Vec<String> = pb
        .types()
        .map(|types| types.iter().map(|kind| kind.to_string()).collect())
        .unwrap_or_default();
    let items = pb.pasteboardItems().map_or(0, |items| items.count());
    if !crate::textops::pasteboard_is_plain(&types, &string_type.to_string(), items) {
        return None;
    }
    let saved = read().ok()?;
    (sequence() == before && matches!(saved, ClipboardContent::Text(_) | ClipboardContent::Empty))
        .then_some((saved, before))
}

/// Restore only while our write is still current. A newer copy belongs to the user.
/// NSPasteboard has no atomic compare-and-swap; the check prevents observed concurrent changes.
pub(crate) fn restore_plain(saved: ClipboardContent, written_sequence: u64) {
    if sequence() != written_sequence {
        return;
    }
    match saved {
        ClipboardContent::Text(text) => {
            let _ = set_text(&text);
        }
        ClipboardContent::Empty => {
            NSPasteboard::generalPasteboard().clearContents();
        }
        _ => {}
    }
}

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
