//! Starting a long-lived child (the daemon) without it keeping this process's pipes open.

use windows::Win32::Foundation::{HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation};
use windows::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

/// Make this process's standard input, output and error handles non-inheritable.
///
/// Rust starts a child with `bInheritHandles = TRUE`, so a daemon started from `winhands mcp`, `wh` or
/// `winhands start` would inherit the caller's stdout pipe and hold it open for as long as it runs: a
/// parent that reads the output until end-of-file (a script capturing `wh`, an MCP host closing the
/// shim) would wait for the daemon's idle timeout. A child given explicit `Stdio` handles still gets
/// its own inheritable copies of those, and this process keeps using its own handles as before.
pub fn keep_std_handles_private() {
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle only reads this process's standard handle table.
        let Ok(handle) = (unsafe { GetStdHandle(which) }) else {
            continue;
        };
        if handle.is_invalid() || handle.0.is_null() {
            continue;
        }
        // SAFETY: `handle` is this process's own standard handle; clearing the inherit flag changes
        // nothing about how this process uses it.
        let _ = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) };
    }
}
