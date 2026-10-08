//! Finding and starting apps: names to bundles, then `/usr/bin/open` (which goes through Launch
//! Services, so app activation, URL handlers and documents all behave like a double click).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSBundle, NSString};
use wh_core::backend::{AppTarget, BackendError, BackendResult, LaunchOutcome, LaunchSpec};

use crate::launchutil::{
    app_search_dirs, looks_like_bundle_id, looks_like_url, match_app_name, split_args,
};

const OPEN: &str = "/usr/bin/open";
const MDFIND: &str = "/usr/bin/mdfind";
/// Where command line tools live when a LaunchAgent's own PATH is nearly empty.
const EXTRA_BIN_DIRS: [&str; 5] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
];

fn expand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{}/{rest}", home.trim_end_matches('/')),
        _ => p.to_string(),
    }
}

fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn find_on_path(name: &str) -> Option<String> {
    if name.contains('/') {
        return None;
    }
    let path_var = std::env::var("PATH").unwrap_or_default();
    path_var
        .split(':')
        .chain(EXTRA_BIN_DIRS)
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join(name))
        .find(|p| is_executable_file(p))
        .map(|p| p.display().to_string())
}

fn find_app_bundle(name: &str) -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    for dir in app_search_dirs(&home) {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let entries: Vec<String> = rd
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        if let Some(hit) = match_app_name(&entries, name) {
            return Some(format!("{dir}/{hit}"));
        }
    }
    spotlight_app(name)
}

/// Last resort: ask Spotlight for an application with this display name.
fn spotlight_app(name: &str) -> Option<String> {
    if name.contains(['\'', '"', '\\']) {
        return None;
    }
    let query = format!("kMDItemKind == 'Application' && kMDItemDisplayName == '{name}'c");
    let out = Command::new(MDFIND)
        .arg(query)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.ends_with(".app"))
        .map(str::to_string)
}

fn bundle_installed(id: &str) -> bool {
    NSWorkspace::sharedWorkspace()
        .URLForApplicationWithBundleIdentifier(&NSString::from_str(id))
        .is_some()
}

/// Resolve a bare name, bundle id, path or URL to something launchable.
pub(crate) fn resolve_app(name: &str) -> BackendResult<Option<AppTarget>> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if looks_like_url(name) {
        return Ok(Some(AppTarget::Other(name.to_string())));
    }
    if name.starts_with('/') || name.starts_with("~/") {
        let p = expand_home(name);
        let path = Path::new(&p);
        return Ok(if !path.exists() {
            None
        } else if p.ends_with(".app") || p.ends_with(".app/") {
            Some(AppTarget::Shortcut(p.trim_end_matches('/').to_string()))
        } else if is_executable_file(path) {
            Some(AppTarget::Executable(p))
        } else {
            Some(AppTarget::Other(p))
        });
    }
    if looks_like_bundle_id(name) && bundle_installed(name) {
        return Ok(Some(AppTarget::Packaged(name.to_string())));
    }
    if let Some(app) = find_app_bundle(name) {
        return Ok(Some(AppTarget::Shortcut(app)));
    }
    Ok(find_on_path(name).map(AppTarget::Executable))
}

fn run_open(args: &[String]) -> BackendResult<()> {
    let out = Command::new(OPEN)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| BackendError::Os(format!("could not run open: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(BackendError::Os(if why.is_empty() {
        format!("open failed with {}", out.status)
    } else {
        format!("open failed: {why}")
    }))
}

/// The pid of a running app with this bundle id, waiting briefly for a fresh launch to appear.
fn pid_for_bundle(id: &str) -> Option<u32> {
    let wanted = NSString::from_str(id);
    let deadline = Instant::now() + Duration::from_millis(2000);
    loop {
        let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&wanted);
        if let Some(a) = apps.iter().next() {
            return u32::try_from(a.processIdentifier()).ok();
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(80));
    }
}

fn bundle_id_of_path(path: &str) -> Option<String> {
    NSBundle::bundleWithPath(&NSString::from_str(path))
        .and_then(|b| b.bundleIdentifier())
        .map(|s| s.to_string())
}

/// Start something.
pub(crate) fn launch(spec: &LaunchSpec) -> BackendResult<LaunchOutcome> {
    let args = split_args(&spec.args);
    match &spec.target {
        AppTarget::Executable(path) => {
            let mut child = Command::new(path)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| BackendError::Os(format!("could not start {path}: {e}")))?;
            let pid = child.id();
            // Reap it when it exits so it never lingers as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(LaunchOutcome { pid: Some(pid) })
        }
        AppTarget::Shortcut(path) => {
            let mut cmd = vec!["-a".to_string(), path.clone()];
            if !args.is_empty() {
                cmd.push("--args".to_string());
                cmd.extend(args);
            }
            run_open(&cmd)?;
            let pid = bundle_id_of_path(path).and_then(|id| pid_for_bundle(&id));
            Ok(LaunchOutcome { pid })
        }
        AppTarget::Packaged(id) => {
            let mut cmd = vec!["-b".to_string(), id.clone()];
            if !args.is_empty() {
                cmd.push("--args".to_string());
                cmd.extend(args);
            }
            run_open(&cmd)?;
            Ok(LaunchOutcome {
                pid: pid_for_bundle(id),
            })
        }
        AppTarget::Other(what) => {
            run_open(std::slice::from_ref(what))?;
            Ok(LaunchOutcome { pid: None })
        }
    }
}
