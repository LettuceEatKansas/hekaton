//! macOS privacy grants (TCC) and Secure Input. Every check is a no-prompt preflight; a missing
//! grant becomes a [`BackendError::Denied`] that names the System Settings pane to open.

use objc2_core_graphics::{CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess};
use wh_core::backend::{BackendError, BackendResult, PermissionStatus};

use crate::ax;
use crate::keymap::{PANE_ACCESSIBILITY, PANE_SCREEN_RECORDING};

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    /// HIToolbox: is any app (a password field, a terminal's Secure Keyboard Entry) holding
    /// Secure Event Input?
    fn IsSecureEventInputEnabled() -> u8;
}

/// Accessibility (AX) is granted to this process.
pub(crate) fn accessibility_granted() -> bool {
    ax::trusted()
}

/// Posting synthetic input events is allowed (it rides on the Accessibility grant).
pub(crate) fn post_events_granted() -> bool {
    CGPreflightPostEventAccess()
}

/// Screen capture (Screen & System Audio Recording) is granted.
pub(crate) fn screen_capture_granted() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Is Secure Event Input on right now?
pub(crate) fn secure_input_enabled() -> bool {
    // SAFETY: no arguments, no preconditions.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

/// Fail unless Accessibility is granted. `what` says what was being attempted.
pub(crate) fn require_accessibility(what: &str) -> BackendResult<()> {
    if accessibility_granted() {
        Ok(())
    } else {
        Err(BackendError::Denied(format!(
            "macOS has not granted winhands Accessibility access, so it cannot {what}. Open \
             {PANE_ACCESSIBILITY}, switch winhands on, then try again"
        )))
    }
}

/// Fail unless synthetic input may be posted.
pub(crate) fn require_post_events(what: &str) -> BackendResult<()> {
    if post_events_granted() || accessibility_granted() {
        Ok(())
    } else {
        Err(BackendError::Denied(format!(
            "macOS has not granted winhands permission to control the keyboard and mouse, so it \
             cannot {what}. Open {PANE_ACCESSIBILITY}, switch winhands on, then try again"
        )))
    }
}

/// Fail unless Screen Recording is granted.
pub(crate) fn require_screen_capture(what: &str) -> BackendResult<()> {
    if screen_capture_granted() {
        Ok(())
    } else {
        Err(BackendError::Denied(format!(
            "macOS has not granted winhands Screen Recording access, so it cannot {what}. Open \
             {PANE_SCREEN_RECORDING}, switch winhands on, then restart winhands and try again"
        )))
    }
}

/// The message for typing into a password field while Secure Input is on.
pub(crate) fn secure_input_error() -> BackendError {
    BackendError::Denied(
        "a password field has focus and macOS Secure Input is on: synthetic keystrokes cannot \
         reach it. Type it yourself, or click out of the field"
            .to_string(),
    )
}

/// The report `winhands doctor` prints.
pub(crate) fn report() -> Vec<PermissionStatus> {
    vec![
        PermissionStatus {
            name: "Accessibility".into(),
            granted: accessibility_granted(),
            needed_for: "reading app windows and controls, clicking buttons by name".into(),
            fix: PANE_ACCESSIBILITY.into(),
        },
        PermissionStatus {
            name: "Post input events".into(),
            granted: post_events_granted(),
            needed_for: "moving the mouse, clicking, typing and key shortcuts".into(),
            fix: PANE_ACCESSIBILITY.into(),
        },
        PermissionStatus {
            name: "Screen Recording".into(),
            granted: screen_capture_granted(),
            needed_for: "screenshots, OCR, and other apps' window titles".into(),
            fix: PANE_SCREEN_RECORDING.into(),
        },
    ]
}

/// Ask macOS to show its permission prompts for this process, so the app appears in the Privacy
/// lists in System Settings without a manual "+": Accessibility (`AXIsProcessTrustedWithOptions`
/// with the prompt option), posting input events and Screen Recording. Each call returns at once and
/// the prompt appears asynchronously; the return values are ignored because [`report`] is the truth.
pub(crate) fn request_all() {
    use objc2_application_services::{AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt};
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};
    use objc2_core_graphics::{CGRequestPostEventAccess, CGRequestScreenCaptureAccess};

    // SAFETY: reading an immutable CoreFoundation constant.
    let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
    let options = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[CFBoolean::new(true)]);
    let options: &CFDictionary = options.as_ref();
    // SAFETY: `options` maps a CFString key to a CFBoolean, which is what the API documents.
    let _ = unsafe { AXIsProcessTrustedWithOptions(Some(options)) };
    let _ = CGRequestPostEventAccess();
    let _ = CGRequestScreenCaptureAccess();
}

/// The error for a click or keystroke aimed at a system prompt that ignores software input.
pub(crate) fn protected_prompt_error(app: &str) -> BackendError {
    BackendError::Denied(format!(
        "{app} is a macOS authorization or privacy prompt, which ignores clicks and keystrokes          from software by design. Answer it yourself"
    ))
}

/// Refuse to click on a system prompt (an admin password sheet, a privacy consent dialog).
pub(crate) fn check_protected_at(p: wh_core::backend::Point) -> BackendResult<()> {
    let Some(window) = crate::winlist::at_point(p) else {
        return Ok(());
    };
    let (pid, _) = crate::wininfo::window_parts(window);
    let bundle = crate::apps::bundle_id(pid);
    if crate::roles::is_protected_prompt(&bundle) {
        return Err(protected_prompt_error(&crate::apps::name(pid)));
    }
    Ok(())
}

/// Refuse to type when Secure Input is on and the focused control is a password field: the
/// keystrokes would be dropped without a word. Anything that cannot be checked is let through.
pub(crate) fn check_secure_typing() -> BackendResult<()> {
    if !secure_input_enabled() || !accessibility_granted() {
        return Ok(());
    }
    if let Some(pid) = crate::apps::frontmost_pid()
        && crate::roles::is_protected_prompt(&crate::apps::bundle_id(pid))
    {
        return Err(protected_prompt_error(&crate::apps::name(pid)));
    }
    let focused = ax::Ax::system_wide().element("AXFocusedUIElement");
    if let Some(el) = focused {
        let role = el.string("AXRole").unwrap_or_default();
        let sub = el.string("AXSubrole");
        if crate::roles::is_secure_field(&role, sub.as_deref()) {
            return Err(secure_input_error());
        }
    }
    Ok(())
}
