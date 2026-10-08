//! The macOS backend: CGEvent input, ScreenCaptureKit capture, the AX tree, Vision OCR.
//!
//! [`MacBackend`] implements `wh_core::backend::Backend`. The platform code only compiles on macOS;
//! the pure helpers it leans on (key table, point and pixel arithmetic, AX role mapping, OCR box
//! flipping, window-list parsing, typing plans) build and are unit-tested on every OS.
//!
//! Permissions: the process needs Accessibility (input, the AX tree) and Screen Recording
//! (screenshots, other apps' window titles). Every method that needs one returns a
//! `BackendError::Denied` naming the System Settings pane when it is missing, and
//! `Backend::permissions` reports them for `winhands doctor`.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

mod geom;
mod keymap;
mod launchutil;
mod ocrgeom;
mod roles;
mod textops;
mod wininfo;

#[cfg(target_os = "macos")]
mod apps;
#[cfg(target_os = "macos")]
mod ax;
#[cfg(target_os = "macos")]
mod backend;
#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod cf;
#[cfg(target_os = "macos")]
mod clip;
#[cfg(target_os = "macos")]
mod displays;
#[cfg(target_os = "macos")]
mod hotkey;
#[cfg(target_os = "macos")]
mod input;
#[cfg(target_os = "macos")]
mod launch;
#[cfg(target_os = "macos")]
mod ocr;
#[cfg(target_os = "macos")]
mod perm;
#[cfg(target_os = "macos")]
mod procs;
#[cfg(target_os = "macos")]
mod text;
#[cfg(target_os = "macos")]
mod tree;
#[cfg(target_os = "macos")]
mod winlist;

#[cfg(target_os = "macos")]
pub use backend::MacBackend;

/// Turn the main thread's run loop for up to `seconds`, returning early when it has nothing to do.
///
/// Carbon hotkeys (the kill hotkey) and `NSWorkspace` state only update while the main thread's
/// run loop runs, so the daemon's main thread should call this in a loop (or park in
/// [`run_main_run_loop_forever`]) while worker threads serve requests.
#[cfg(target_os = "macos")]
pub fn run_main_run_loop(seconds: f64) {
    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    // SAFETY: reading an immutable CoreFoundation constant.
    let mode = unsafe { kCFRunLoopDefaultMode };
    CFRunLoop::run_in_mode(mode, seconds, false);
}

/// Park the calling thread in the main run loop until the process exits.
#[cfg(target_os = "macos")]
pub fn run_main_run_loop_forever() -> ! {
    loop {
        run_main_run_loop(1.0e6);
    }
}
