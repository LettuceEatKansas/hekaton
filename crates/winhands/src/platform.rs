//! The backend for the OS this binary was built for, and the one place where the macOS main thread
//! matters.

use std::sync::Arc;

use wh_core::backend::Backend;

/// The Windows backend.
#[cfg(windows)]
pub fn backend() -> Result<Arc<dyn Backend>, String> {
    Ok(Arc::new(wh_win::WindowsBackend::new()))
}

/// The macOS backend.
#[cfg(target_os = "macos")]
pub fn backend() -> Result<Arc<dyn Backend>, String> {
    Ok(Arc::new(wh_mac::MacBackend::new()))
}

/// Neither: winhands drives a Windows or a macOS desktop.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn backend() -> Result<Arc<dyn Backend>, String> {
    Err("winhands drives a Windows or macOS desktop; this OS has no backend".to_string())
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
    wh_mac::run_main_run_loop_forever()
}

/// See the macOS version.
#[cfg(not(target_os = "macos"))]
pub fn run_daemon_thread(work: impl FnOnce() -> i32 + Send + 'static) -> i32 {
    work()
}
