//! Running applications: bundle ids, the frontmost app, activation. `NSRunningApplication` is
//! queried by pid; the frontmost app is read from Accessibility first because
//! `NSWorkspace.frontmostApplication` only refreshes while the main run loop turns.

use objc2::rc::Retained;
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};

use crate::ax::{self, Ax};

/// The running application for a pid.
pub(crate) fn running(pid: i32) -> Option<Retained<NSRunningApplication>> {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
}

/// Its bundle identifier, empty when it has none (a command line tool).
pub(crate) fn bundle_id(pid: i32) -> String {
    running(pid)
        .and_then(|a| a.bundleIdentifier())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Its display name.
pub(crate) fn name(pid: i32) -> String {
    running(pid)
        .and_then(|a| a.localizedName())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Path of its `.app` bundle.
pub(crate) fn bundle_path(pid: i32) -> Option<String> {
    running(pid)
        .and_then(|a| a.bundleURL())
        .and_then(|u| u.path())
        .map(|p| p.to_string())
}

/// The pid of the frontmost application.
pub(crate) fn frontmost_pid() -> Option<i32> {
    if ax::trusted()
        && let Some(pid) = Ax::system_wide()
            .element("AXFocusedApplication")
            .and_then(|a| a.pid())
    {
        return Some(pid);
    }
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|a| a.processIdentifier())
}

/// Is the app hidden (Cmd+H)?
pub(crate) fn is_hidden(pid: i32) -> bool {
    running(pid).is_some_and(|a| a.isHidden())
}

/// Unhide the app.
pub(crate) fn unhide(pid: i32) -> bool {
    running(pid).is_some_and(|a| a.unhide())
}

/// Ask the system to activate the app (all its windows).
pub(crate) fn activate(pid: i32) -> bool {
    running(pid)
        .is_some_and(|a| a.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows))
}

/// Is the app built on Chromium or Electron? Their accessibility tree stays empty until asked.
pub(crate) fn is_chromium_like(pid: i32) -> bool {
    const CHROMIUM_IDS: [&str; 8] = [
        "com.google.Chrome",
        "com.google.Chrome.beta",
        "com.google.Chrome.canary",
        "com.microsoft.edgemac",
        "com.brave.Browser",
        "org.chromium.Chromium",
        "com.vivaldi.Vivaldi",
        "com.operasoftware.Opera",
    ];
    let id = bundle_id(pid);
    if CHROMIUM_IDS.contains(&id.as_str()) {
        return true;
    }
    let Some(path) = bundle_path(pid) else {
        return false;
    };
    let frameworks = std::path::Path::new(&path).join("Contents/Frameworks");
    [
        "Electron Framework.framework",
        "Chromium Embedded Framework.framework",
    ]
    .iter()
    .any(|f| frameworks.join(f).exists())
}
