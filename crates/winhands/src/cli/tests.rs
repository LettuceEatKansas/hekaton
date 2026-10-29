//! Ported from `tests/test_cli_doctor.py`, plus the Rust-only parts of `doctor` (backend permissions,
//! displays and OCR) and the `status` report.

use serde_json::json;
use wh_core::backend::{Capabilities, Rect};
use wh_core::env::MapEnv;
use wh_core::paths::Paths;

use super::*;

fn cfg(env: MapEnv) -> Config {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("wh-doctor-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    Config::new(Paths::in_dir(dir), env)
}

fn facts() -> BackendFacts {
    BackendFacts {
        name: "fake".into(),
        capabilities: Capabilities {
            ocr: true,
            ..Capabilities::default()
        },
        permissions: Vec::new(),
        elevated: None,
        displays: vec![Display {
            id: 1,
            name: "main".into(),
            rect: Rect::from_xywh(0, 0, 3840, 2160),
            scale: 1.5,
            primary: true,
        }],
        ocr_engine: "winrt".into(),
    }
}

fn joined(lines: &[String]) -> String {
    lines.join("\n")
}

#[test]
fn doctor_all_ok() {
    let c = cfg(MapEnv::new()
        .with("WINHANDS_ONLY_APPS", "notepad.exe")
        .with("WINHANDS_ACKNOWLEDGED", "1"));
    let health = json!({"version": "1.2.3", "elevated": true, "ocr": "rapidocr"});
    let (lines, warn) = doctor_lines(Some(&facts()), Some(&health), true, "token", &c);
    let out = joined(&lines);
    assert!(!warn, "{out}");
    for needle in [
        "OK   daemon: reachable (version 1.2.3)",
        "OK   elevation: daemon is elevated",
        "OK   ocr engine: rapidocr",
        "OK   token: present",
        "OK   coordinate mode: pixels",
        "OK   default allowlist (WINHANDS_ONLY_APPS): notepad.exe",
        "OK   backend: fake",
        "OK   display 1: 3840x2160 at scale 1.5 (primary)",
        "OK   ocr: available (winrt)",
    ] {
        assert!(out.contains(needle), "{needle} in\n{out}");
    }
    assert!(!out.contains("WARN"));
}

#[test]
fn doctor_daemon_unreachable() {
    let c = cfg(MapEnv::new().with("WINHANDS_ACKNOWLEDGED", "1"));
    let (lines, warn) = doctor_lines(Some(&facts()), None, false, "C:\\t\\token", &c);
    let out = joined(&lines);
    assert!(warn);
    assert!(out.contains("WARN daemon: not reachable"));
    assert!(
        !out.contains("elevation: daemon"),
        "nothing to ask the unreachable daemon"
    );
    assert!(out.contains("WARN token: missing (C:\\t\\token)"));
    assert!(out.contains("WARN default allowlist"));
}

#[test]
fn doctor_reachable_but_not_elevated_and_no_allowlist() {
    let c = cfg(MapEnv::new()
        .with("WINHANDS_COORDS", "norm1000")
        .with("WINHANDS_ACKNOWLEDGED", "1"));
    let health = json!({"version": "1.2.3", "elevated": false, "ocr": null});
    let (lines, warn) = doctor_lines(Some(&facts()), Some(&health), true, "token", &c);
    let out = joined(&lines);
    assert!(warn);
    assert!(out.contains("WARN elevation: daemon is NOT elevated"));
    assert!(
        out.contains("OK   ocr engine: untried"),
        "no ocr yet: informational, not a failure"
    );
    assert!(out.contains("OK   coordinate mode: norm1000"));
    assert!(out.contains("WARN default allowlist"));
}

#[test]
fn doctor_says_when_nobody_has_acknowledged() {
    let c = cfg(MapEnv::new().with("WINHANDS_ONLY_APPS", "a.exe"));
    let (lines, warn) = doctor_lines(Some(&facts()), None, true, "t", &c);
    assert!(warn);
    let out = joined(&lines);
    assert!(
        out.contains("WARN acknowledged: NO") && out.contains("winhands acknowledge"),
        "{out}"
    );
}

#[test]
fn doctor_reports_each_os_permission_and_how_to_fix_a_missing_one() {
    let mut f = facts();
    f.name = "macos".into();
    f.permissions = vec![
        PermissionStatus {
            name: "Accessibility".into(),
            granted: true,
            needed_for: "input and the AX tree".into(),
            fix: "System Settings > Privacy & Security > Accessibility".into(),
        },
        PermissionStatus {
            name: "Screen Recording".into(),
            granted: false,
            needed_for: "screenshots".into(),
            fix: "System Settings > Privacy & Security > Screen Recording".into(),
        },
    ];
    let c = cfg(MapEnv::new()
        .with("WINHANDS_ONLY_APPS", "a.exe")
        .with("WINHANDS_ACKNOWLEDGED", "1"));
    let (lines, warn) = doctor_lines(
        Some(&f),
        Some(&json!({"version": "x", "elevated": true})),
        true,
        "t",
        &c,
    );
    let out = joined(&lines);
    assert!(warn);
    assert!(out.contains("OK   permission Accessibility: granted"));
    assert!(
        out.contains("WARN permission Screen Recording: NOT granted. Needed for screenshots. Fix: System Settings"),
        "{out}"
    );
}

#[test]
fn doctor_on_macos_does_not_ask_for_an_elevated_daemon() {
    let mut f = facts();
    f.name = "macos".into();
    let c = cfg(MapEnv::new()
        .with("WINHANDS_ONLY_APPS", "a.exe")
        .with("WINHANDS_ACKNOWLEDGED", "1"));
    let health = json!({"version": "x", "elevated": false});
    let (lines, warn) = doctor_lines(Some(&f), Some(&health), true, "t", &c);
    assert!(!warn, "{}", joined(&lines));
    assert!(!joined(&lines).contains("elevat"), "{}", joined(&lines));
}

#[test]
fn doctor_says_when_it_runs_outside_the_app_bundle_on_macos() {
    use std::path::Path;
    let outside = Path::new("/Users/jon/src/winhands-rs/target/release/winhands");
    let note = bundle_note(outside, true).expect("outside the bundle on macOS");
    assert!(
        note.starts_with("NOTE not running from winhands.app"),
        "{note}"
    );
    assert!(note.contains("install.sh"));
    let inside = Path::new("/Users/jon/Applications/winhands.app/Contents/MacOS/winhands");
    assert_eq!(bundle_note(inside, true), None);
    assert_eq!(
        bundle_note(outside, false),
        None,
        "nothing to say off macOS"
    );
}

#[test]
fn doctor_reports_elevation_of_this_shell_on_windows() {
    let mut f = facts();
    f.name = "windows".into();
    f.capabilities.elevation = true;
    f.elevated = Some(false);
    let c = cfg(MapEnv::new()
        .with("WINHANDS_ONLY_APPS", "a.exe")
        .with("WINHANDS_ACKNOWLEDGED", "1"));
    let (lines, warn) = doctor_lines(
        Some(&f),
        Some(&json!({"version": "x", "elevated": true})),
        true,
        "t",
        &c,
    );
    assert!(!warn, "{}", joined(&lines));
    assert!(joined(&lines).contains("elevation (this shell): not elevated"));
}

#[test]
fn doctor_warns_when_the_backend_has_no_ocr_or_no_displays() {
    let mut f = facts();
    f.capabilities.ocr = false;
    f.displays.clear();
    let c = cfg(MapEnv::new()
        .with("WINHANDS_ONLY_APPS", "a.exe")
        .with("WINHANDS_ACKNOWLEDGED", "1"));
    let (lines, warn) = doctor_lines(
        Some(&f),
        Some(&json!({"version": "x", "elevated": true})),
        true,
        "t",
        &c,
    );
    let out = joined(&lines);
    assert!(warn);
    assert!(out.contains("WARN ocr: not available on this backend yet"));
    assert!(out.contains("WARN displays: none found"));
}

#[test]
fn backend_facts_reads_a_fake_backend() {
    let fake = wh_core::backend::fake::FakeBackend::new();
    let f = backend_facts(&fake);
    assert_eq!(f.name, "fake");
    assert!(f.capabilities.ocr);
}

#[test]
fn status_when_stopped_names_the_start_command() {
    let starter = Starter::SelfDaemon(PathBuf::from("C:\\w\\winhands.exe"));
    let (text, code) = status_text(None, Some(77), Some(&starter));
    assert_eq!(code, 1);
    assert!(text.starts_with("winhands: stopped\n"));
    assert!(text.contains("pid file: stale (77)"));
    assert!(text.contains("start it with: winhands start"));
    assert!(text.contains("start method: \"C:\\w\\winhands.exe\" daemon"));
    let (text, _) = status_text(None, None, None);
    assert!(text.contains("pid file: none"));
}

#[test]
fn status_when_running_prints_the_python_lines() {
    let health = json!({
        "version": "0.2.0", "elevated": false, "paused": true, "hotkey": "ctrl+alt+shift+f12",
        "idle_remaining_seconds": 59.5, "idle_timeout_seconds": 60, "impl": "winhands-rs", "backend": "windows"
    });
    let (text, code) = status_text(Some(&health), Some(1234), None);
    assert_eq!(code, 0);
    assert!(text.contains("winhands: running (pid 1234)"));
    assert!(text.contains("  version: 0.2.0"));
    assert!(text.contains("  elevated: False"));
    assert!(text.contains("  paused: True (kill hotkey: ctrl+alt+shift+f12)"));
    assert!(text.contains("  idle shutdown in: 59s (timeout 60s)"));
    assert!(text.contains("implementation: winhands-rs (windows backend)"));
    let health = json!({"version": "0.1.0", "elevated": true, "paused": false, "hotkey": null,
                        "idle_remaining_seconds": -1.0, "idle_timeout_seconds": 0});
    let (text, _) = status_text(Some(&health), None, None);
    assert!(text.contains("running (pid unknown)"));
    assert!(text.contains("(kill hotkey: none registered)"));
    assert!(text.contains("idle shutdown in: disabled (timeout 0s)"));
}

#[test]
fn options_are_pulled_out_in_both_spellings() {
    let mut args: Vec<String> = [
        "--daemon-cmd",
        "x daemon",
        "keep",
        "--daemon-task=t",
        "--no-hotkey",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let opts = start_options(&mut args).unwrap();
    assert_eq!(opts.cmd.as_deref(), Some("x daemon"));
    assert_eq!(opts.task.as_deref(), Some("t"));
    assert_eq!(args, ["keep", "--no-hotkey"]);
    assert!(take_flag(&mut args, "--no-hotkey"));
    assert!(!take_flag(&mut args, "--no-hotkey"));
    let mut dangling: Vec<String> = vec!["--daemon-cmd".into()];
    assert!(start_options(&mut dangling).is_err());
}

#[test]
fn unknown_commands_exit_two_and_version_and_help_exit_zero() {
    assert_eq!(main(vec!["frobnicate".into()]), 2);
    assert_eq!(main(vec!["--version".into()]), 0);
    assert_eq!(main(vec!["--help".into()]), 0);
}

#[test]
fn the_exe_is_wh_when_it_is_called_wh() {
    use std::ffi::OsStr;
    assert!(invoked_as_wh(OsStr::new("wh")));
    assert!(invoked_as_wh(OsStr::new("wh.exe")));
    assert!(invoked_as_wh(OsStr::new("WH.EXE")));
    assert!(invoked_as_wh(OsStr::new("/usr/local/bin/wh")));
    assert!(!invoked_as_wh(OsStr::new("winhands.exe")));
    assert!(!invoked_as_wh(OsStr::new("whoami")));
    assert!(!invoked_as_wh(OsStr::new("")));
    #[cfg(windows)]
    assert!(invoked_as_wh(OsStr::new(r"C:\tools\bin\wh.exe")));
}

#[test]
fn only_a_person_at_a_terminal_can_release_the_gaming_lock() {
    let c = cfg(MapEnv::new());
    let dir = c.paths().dir();
    let mut out = Vec::new();
    assert_eq!(run_unlock(dir, false, &mut || None, &mut out), 0, "not on");
    wh_core::safety::lock(dir, "gaming").unwrap();
    // a model running `unlock` through a non-interactive shell
    let mut out = Vec::new();
    assert_eq!(
        run_unlock(dir, false, &mut || Some("unlock".into()), &mut out),
        1
    );
    assert!(wh_core::safety::locked(dir).is_some());
    // a person who does not type the word
    assert_eq!(
        run_unlock(dir, true, &mut || Some("yes".into()), &mut out),
        1
    );
    assert!(wh_core::safety::locked(dir).is_some());
    assert_eq!(
        run_unlock(dir, true, &mut || Some("unlock\n".into()), &mut out),
        0
    );
    assert!(wh_core::safety::locked(dir).is_none());
}

#[test]
fn only_a_person_at_a_terminal_can_approve_a_chat_code() {
    let code = |c: &str| vec![c.to_string()];
    let mut sent: Vec<(String, String)> = Vec::new();
    let mut out = Vec::new();
    // a model running `approve` through a non-interactive shell: nothing reaches the daemon
    let mut send = |route: &str, body: &[u8]| {
        sent.push((
            route.to_string(),
            String::from_utf8_lossy(body).into_owned(),
        ));
        Ok((
            200,
            br#"{"ok":true,"result":"approved K7QM: Click Send"}"#.to_vec(),
        ))
    };
    assert_eq!(
        run_answer(&code("k7qm"), true, false, &mut send, &mut out),
        1
    );
    assert!(String::from_utf8_lossy(&out).contains("real terminal"));
    // a person at a terminal
    assert_eq!(
        run_answer(&code("k7qm"), true, true, &mut send, &mut out),
        0
    );
    // deny is fine from anywhere
    assert_eq!(
        run_answer(&code("K7QM"), false, false, &mut send, &mut out),
        0
    );
    assert_eq!(
        sent,
        [
            ("/approve".to_string(), r#"{"code":"K7QM"}"#.to_string()),
            ("/deny".to_string(), r#"{"code":"K7QM"}"#.to_string()),
        ]
    );
}

#[test]
fn an_approval_code_must_look_like_one_and_a_refusal_is_reported() {
    let mut never = |_: &str, _: &[u8]| -> Result<(u16, Vec<u8>), String> { panic!("not sent") };
    let mut out = Vec::new();
    for bad in [
        vec![],
        vec!["ABCD".to_string()],
        vec!["K7Q".to_string()],
        vec!["K7QM".into(), "X".into()],
    ] {
        assert_eq!(
            run_answer(&bad, true, true, &mut never, &mut out),
            2,
            "{bad:?}"
        );
    }
    assert_eq!(code_arg(&[" k7qm ".into()]), Ok("K7QM".into()));
    let mut refused = |_: &str, _: &[u8]| {
        Ok((
            400,
            br#"{"ok":false,"error":"no waiting approval K7QM"}"#.to_vec(),
        ))
    };
    let mut out = Vec::new();
    assert_eq!(
        run_answer(&["K7QM".into()], true, true, &mut refused, &mut out),
        1
    );
    assert!(String::from_utf8_lossy(&out).contains("no waiting approval K7QM"));
    let mut down = |_: &str, _: &[u8]| Err("connection refused".to_string());
    assert_eq!(
        run_answer(&["K7QM".into()], false, true, &mut down, &mut out),
        1
    );
}
