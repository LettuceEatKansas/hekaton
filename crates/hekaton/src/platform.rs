//! The backend for the OS this binary was built for, and the one place where the macOS main thread
//! matters.

use std::sync::Arc;

use hekaton_core::backend::Backend;

/// The Windows backend.
#[cfg(windows)]
pub fn backend() -> Result<Arc<dyn Backend>, String> {
    Ok(Arc::new(hekaton_win::WindowsBackend::new()))
}

/// The macOS backend.
#[cfg(target_os = "macos")]
pub fn backend() -> Result<Arc<dyn Backend>, String> {
    Ok(Arc::new(hekaton_mac::MacBackend::new()))
}

/// Neither: hekaton drives a Windows or a macOS desktop.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn backend() -> Result<Arc<dyn Backend>, String> {
    Err("hekaton drives a Windows or macOS desktop; this OS has no backend".to_string())
}

/// Ask the OS to show its permission prompts (macOS: Accessibility, Screen Recording) so this app
/// appears in the System Settings lists. False where the OS has no such prompts.
#[cfg(target_os = "macos")]
pub fn request_permission_prompts() -> bool {
    hekaton_mac::request_permission_prompts();
    true
}

/// See the macOS version.
#[cfg(not(target_os = "macos"))]
pub fn request_permission_prompts() -> bool {
    false
}

/// The real user id, for the launchd `gui/<uid>` domain; `None` off macOS.
#[cfg(target_os = "macos")]
pub fn user_id() -> Option<u32> {
    Some(hekaton_mac::current_uid())
}

/// See the macOS version.
#[cfg(not(target_os = "macos"))]
pub fn user_id() -> Option<u32> {
    None
}

/// Run `work` (the daemon) and return its exit code.
///
/// On macOS the backend needs the main thread to turn its run loop (Carbon hotkeys and `NSWorkspace`
/// state only update while it runs), so `work` runs on a worker thread and the main thread pumps the
/// run loop until the worker is done. Elsewhere `work` just runs here.
#[cfg(target_os = "macos")]
pub fn run_daemon_thread(work: impl FnOnce() -> i32 + Send + 'static) -> i32 {
    std::thread::spawn(move || {
        let code = work();
        std::process::exit(code);
    });
    hekaton_mac::run_main_run_loop_forever()
}

/// See the macOS version.
#[cfg(not(target_os = "macos"))]
pub fn run_daemon_thread(work: impl FnOnce() -> i32 + Send + 'static) -> i32 {
    work()
}
