//! The `winhands` command line: `daemon | mcp | wh | status | start | stop | pause | resume |
//! acknowledge | doctor` (Python `winhands/cli.py`, plus `pause`, `resume` and the Rust `doctor`).

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use wh_core::ack::{AckInfo, run_acknowledge};
use wh_core::backend::{Backend, Capabilities, Display, PermissionStatus};
use wh_core::config::Config;
use wh_core::mcp::ShimDefaults;

use crate::link::{DaemonLink, Failure, SHIM_TIMEOUT, WH_TIMEOUT};
use crate::platform;
use crate::serve::{DaemonOptions, run_daemon};
use crate::shim::{Activity, Shim, reaper_for, serve_stdio, start_reaper};
use crate::start::Starter;
use crate::wh::{self, Exit};

/// The short usage line.
pub const USAGE: &str = "usage: hands (or winhands) <acknowledge|status|start|stop|pause|resume|doctor|daemon|mcp|wh|--version|--build-info>";

fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("winhands"))
}

/// Pull `--name value` and `--name=value` out of `args`, returning the value of the last one.
fn take_option(args: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let mut found = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            if i + 1 >= args.len() {
                return Err(format!("winhands: {name} needs a value"));
            }
            found = Some(args[i + 1].clone());
            args.drain(i..i + 2);
        } else if let Some(v) = args[i].strip_prefix(&format!("{name}=")) {
            found = Some(v.to_string());
            args.remove(i);
        } else {
            i += 1;
        }
    }
    Ok(found)
}

fn take_flag(args: &mut Vec<String>, name: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != name);
    args.len() != before
}

/// Options shared by the commands that may have to start the daemon.
struct StartOptions {
    cmd: Option<String>,
    task: Option<String>,
}

fn start_options(args: &mut Vec<String>) -> Result<StartOptions, String> {
    Ok(StartOptions {
        cmd: take_option(args, "--daemon-cmd")?,
        task: take_option(args, "--daemon-task")?,
    })
}

/// A link to the local daemon on the configured port.
fn local_link(cfg: &Config, opts: &StartOptions, timeout: Duration) -> DaemonLink {
    let starter = Starter::resolve(cfg, exe_path(), opts.cmd.as_deref(), opts.task.as_deref());
    DaemonLink::new(
        "127.0.0.1",
        cfg.port(),
        cfg.paths().clone(),
        Some(starter),
        timeout,
    )
    .with_start_wait(start_wait(cfg))
}

/// How long a started daemon gets to come up: `daemon_wait_s` in `config.json` or
/// `WINHANDS_DAEMON_WAIT_S`, default 25.
fn start_wait(cfg: &Config) -> Duration {
    let secs = wh_core::config::to_f64(&cfg.get("daemon_wait_s")).filter(|s| *s > 0.0);
    secs.map_or(crate::link::START_WAIT, Duration::from_secs_f64)
}

/// Was this executable started under the name `wh` (a copy or link of `winhands.exe`)? Then the
/// whole command line is `wh`'s.
pub fn invoked_as_wh(argv0: &std::ffi::OsStr) -> bool {
    std::path::Path::new(argv0)
        .file_stem()
        .is_some_and(|stem| stem.eq_ignore_ascii_case("wh"))
}

/// The command line entry point: `args` without the program name. Returns the exit code.
pub fn main(args: Vec<String>) -> i32 {
    let Some(first) = args.first().map(String::as_str) else {
        // No argument means run the service. A scheduled task and a double-click both land here, and
        // printing usage into a hidden window helps nobody.
        return cmd_daemon(Vec::new());
    };
    let rest: Vec<String> = args[1..].to_vec();
    match first {
        "-h" | "--help" => {
            println!("{USAGE}");
            0
        }
        "--version" | "version" => {
            println!(
                "{} source={} dirty={} protocol={} target={}",
                env!("CARGO_PKG_VERSION"),
                env!("HANDS_SOURCE_COMMIT"),
                env!("HANDS_SOURCE_DIRTY"),
                crate::serve::PROTOCOL_VERSION,
                env!("HANDS_BUILD_TARGET")
            );
            0
        }
        "--build-info" => {
            println!("{}", crate::serve::build_info());
            0
        }
        "daemon" => cmd_daemon(rest),
        "mcp" | "shim" => cmd_mcp(rest),
        "wh" => cmd_wh(rest),
        "status" => cmd_status(),
        "start" => cmd_start(rest),
        "stop" => cmd_stop(),
        "pause" => cmd_pause(true),
        "resume" => cmd_pause(false),
        "acknowledge" => cmd_acknowledge(),
        "doctor" => cmd_doctor(rest),
        other => {
            println!("winhands: unknown command {:?}", other);
            2
        }
    }
}

// ------------------------------------------------------------------------------ daemon

fn cmd_daemon(rest: Vec<String>) -> i32 {
    run_daemon_command(rest, false)
}

/// The entry point of `winhandsd`, the daemon built for the Windows GUI subsystem so a scheduled
/// task can start it without a console window. Same daemon as `winhands daemon`; with no console its
/// messages and panics go to `daemon.log` in the data directory instead. `args` is what the process
/// was started with (`--no-hotkey` is the only option).
pub fn main_hidden(args: Vec<String>) -> i32 {
    run_daemon_command(args, true)
}

fn run_daemon_command(mut rest: Vec<String>, hidden: bool) -> i32 {
    let no_hotkey = take_flag(&mut rest, "--no-hotkey");
    let cfg = Arc::new(Config::from_system());
    let opts = DaemonOptions {
        no_hotkey,
        log: hidden.then(|| cfg.paths().daemon_log()),
        ..DaemonOptions::default()
    };
    if let Some(log) = opts.log.clone() {
        // there is no stderr to print a panic to
        std::panic::set_hook(Box::new(move |info| {
            crate::serve::append_log(&log, &format!("panic: {info}"));
        }));
    }
    if let Some(extra) = rest.first() {
        opts.err(&format!(
            "winhands daemon: unexpected argument {extra:?}. usage: winhands daemon [--no-hotkey]"
        ));
        return 2;
    }
    let backend = match platform::backend() {
        Ok(b) => b,
        Err(e) => {
            opts.err(&format!("winhands: {e}"));
            return 1;
        }
    };
    platform::run_daemon_thread(move || run_daemon(backend, cfg, &opts))
}

// ------------------------------------------------------------------------------ mcp

fn cmd_mcp(mut rest: Vec<String>) -> i32 {
    let opts = match start_options(&mut rest) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if let Some(extra) = rest.first() {
        eprintln!(
            "winhands mcp: unexpected argument {extra:?}. usage: winhands mcp [--daemon-cmd CMD] [--daemon-task NAME]"
        );
        return 2;
    }
    let cfg = Config::from_system();
    let link = Arc::new(local_link(&cfg, &opts, SHIM_TIMEOUT));
    let defaults = ShimDefaults::from_config(&cfg);
    let activity = Activity::new();
    let cleanup = platform::backend()
        .ok()
        .and_then(|backend| reaper_for(backend, &cfg, activity.clone()))
        .map(start_reaper);
    let result = serve_stdio(Shim::new(link, defaults, activity));
    if let Some(cleanup) = cleanup {
        cleanup.run();
    }
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("winhands mcp: {e}");
            1
        }
    }
}

// ------------------------------------------------------------------------------ wh

fn cmd_wh(rest: Vec<String>) -> i32 {
    let cfg = Config::from_system();
    // WINHANDS_CONNECT_HOST points the client at another machine or a VM; only a daemon on THIS
    // machine can be started from here.
    let host = wh_core::env::Env::var(cfg.env(), "WINHANDS_CONNECT_HOST")
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let local = wh_core::config::LOOPBACK_ADDRESSES.contains(&host.to_lowercase().as_str());
    let starter = local.then(|| Starter::resolve(&cfg, exe_path(), None, None));
    let mut link = DaemonLink::new(host, cfg.port(), cfg.paths().clone(), starter, WH_TIMEOUT);
    link = link.with_start_wait(start_wait(&cfg));
    if local {
        link = link.with_start_notice("wh: winhands daemon is not running, starting it...");
    }
    let base = link.base();
    let hint = link.start_hint();
    let mut call = |tool: &str, payload: &serde_json::Map<String, Value>| {
        link.post(tool, &Value::Object(payload.clone())).map_err(|f| match f {
            Failure::Timeout => Exit(format!(
                "wh {tool}: timed out talking to the daemon at {base}. Outcome unknown: work may \
                 still be running. Inspect the screen before retrying."
            )),
            Failure::NoDaemon => Exit(format!(
                "wh: no winhands daemon answered at {base} and it could not be started \
                 automatically. Start it with: {hint}."
            )),
            Failure::Io(why) => Exit(format!("wh {tool}: {why}")),
        })
    };
    let stdin = io::stdin();
    wh::run(
        &rest,
        &cfg,
        &mut stdin.lock(),
        &mut io::stdout(),
        &mut io::stderr(),
        &std::env::temp_dir(),
        &mut call,
    )
}

// ------------------------------------------------------------------------------ status, start, stop

fn pid_file(cfg: &Config) -> Option<u32> {
    std::fs::read_to_string(cfg.paths().pid())
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn cmd_status() -> i32 {
    let cfg = Config::from_system();
    let link = local_link(
        &cfg,
        &StartOptions {
            cmd: None,
            task: None,
        },
        Duration::from_secs(2),
    );
    let health = link.health(Duration::from_secs(2));
    let (text, code) = status_text(health.as_ref(), pid_file(&cfg), link.starter());
    print!("{text}");
    code
}

/// The `winhands status` report and its exit code (0 running, 1 stopped).
pub fn status_text(
    health: Option<&Value>,
    pid: Option<u32>,
    starter: Option<&Starter>,
) -> (String, i32) {
    let method = starter.map_or_else(|| "none".to_string(), Starter::describe);
    let Some(health) = health else {
        let pid_line = match pid {
            Some(p) => format!("stale ({p})"),
            None => "none".to_string(),
        };
        return (
            format!(
                "winhands: stopped\n  start method: {method}\n  pid file: {pid_line}\n  start it with: winhands start\n"
            ),
            1,
        );
    };
    let remaining = health
        .get("idle_remaining_seconds")
        .and_then(Value::as_f64)
        .unwrap_or(-1.0);
    let remaining_text = if remaining < 0.0 {
        "disabled".to_string()
    } else {
        format!("{}s", remaining as i64)
    };
    let show = |key: &str| {
        health
            .get(key)
            .map_or_else(|| "None".to_string(), wh_core::textutil::py_str)
    };
    let hotkey = match health.get("hotkey") {
        Some(Value::String(h)) if !h.is_empty() => h.clone(),
        _ => "none registered".to_string(),
    };
    let mut text = format!(
        "winhands: running (pid {})\n  version: {}\n  elevated: {}\n  paused: {} (kill hotkey: {hotkey})\n  \
         idle shutdown in: {remaining_text} (timeout {}s)\n  start method: {method}\n",
        pid.map_or_else(|| "unknown".to_string(), |p| p.to_string()),
        show("version"),
        py_bool(health.get("elevated")),
        py_bool(health.get("paused")),
        show("idle_timeout_seconds"),
    );
    if let Some(imp) = health.get("impl").and_then(Value::as_str) {
        let backend = health.get("backend").and_then(Value::as_str).unwrap_or("?");
        text.push_str(&format!("  implementation: {imp} ({backend} backend)\n"));
    }
    (text, 0)
}

fn py_bool(v: Option<&Value>) -> &'static str {
    if v.is_some_and(wh_core::config::truthy) {
        "True"
    } else {
        "False"
    }
}

fn cmd_start(mut rest: Vec<String>) -> i32 {
    let opts = match start_options(&mut rest) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let cfg = Config::from_system();
    let link = local_link(&cfg, &opts, Duration::from_secs(5));
    if link.start_and_wait(Duration::from_secs(15)) {
        println!("winhands: running");
        0
    } else {
        println!(
            "winhands: could not confirm the daemon started. Start it by hand with: {}",
            link.start_hint()
        );
        1
    }
}

fn cmd_stop() -> i32 {
    let cfg = Config::from_system();
    let link = local_link(
        &cfg,
        &StartOptions {
            cmd: None,
            task: None,
        },
        Duration::from_secs(5),
    );
    // An authenticated stop: the daemon may be elevated, so an ordinary shell cannot kill it.
    let _ = link.send("POST", "/shutdown", Some(b"{}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while link.health(Duration::from_secs(1)).is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if link.health(Duration::from_secs(1)).is_some()
        && let Some(pid) = pid_file(&cfg)
    {
        kill(pid);
    }
    println!("winhands: stop requested");
    0
}

fn kill(pid: u32) {
    let result = if cfg!(windows) {
        std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    } else {
        std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
    };
    let _ = result;
}

fn cmd_pause(pause: bool) -> i32 {
    let cfg = Config::from_system();
    let link = local_link(
        &cfg,
        &StartOptions {
            cmd: None,
            task: None,
        },
        Duration::from_secs(5),
    );
    let route = if pause { "/pause" } else { "/resume" };
    match link.send("POST", route, Some(b"{}")) {
        Ok(reply) => {
            let data: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
            if reply.status == 200 {
                println!(
                    "winhands: {}",
                    data.get("result").and_then(Value::as_str).unwrap_or("done")
                );
                0
            } else {
                let why = data
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("refused");
                eprintln!("winhands: {why}");
                1
            }
        }
        Err(e) => {
            eprintln!("winhands: the daemon did not answer ({e}). Is it running? winhands status");
            1
        }
    }
}

// ------------------------------------------------------------------------------ acknowledge

fn cmd_acknowledge() -> i32 {
    let cfg = Config::from_system();
    let info = AckInfo::from_env(cfg.env());
    let interactive = io::stdin().is_terminal();
    let mut read_answer = || {
        print!("> ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        match io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    };
    match run_acknowledge(
        &cfg,
        &info,
        interactive,
        &mut read_answer,
        &mut io::stdout(),
    ) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("winhands: could not write the acknowledgement: {e}");
            1
        }
    }
}

// ------------------------------------------------------------------------------ doctor

/// What `winhands doctor` found out about this machine's backend.
#[derive(Debug, Clone)]
pub struct BackendFacts {
    /// `windows`, `macos`, ...
    pub name: String,
    /// What the backend can do here.
    pub capabilities: Capabilities,
    /// OS permissions (macOS); empty where the OS has no such gate.
    pub permissions: Vec<PermissionStatus>,
    /// Is this process elevated (Windows)?
    pub elevated: Option<bool>,
    /// The monitors and their scale (the DPI picture).
    pub displays: Vec<Display>,
    /// Which OCR engine answers.
    pub ocr_engine: String,
}

/// Read the facts off a backend. Never prompts and never sends input.
pub fn backend_facts(backend: &dyn Backend) -> BackendFacts {
    let _ = backend.init();
    BackendFacts {
        name: backend.name().to_string(),
        capabilities: backend.capabilities(),
        permissions: backend.permissions().unwrap_or_default(),
        elevated: backend.own_process().ok().flatten().map(|p| p.elevated),
        displays: backend.displays().unwrap_or_default(),
        ocr_engine: backend.ocr_engine_name(),
    }
}

/// The `winhands doctor` lines and whether anything warned. One OK or WARN line per check; a pure
/// read: nothing is written and no daemon state is touched.
pub fn doctor_lines(
    facts: Option<&BackendFacts>,
    health: Option<&Value>,
    token_present: bool,
    token_path: &str,
    cfg: &Config,
) -> (Vec<String>, bool) {
    let mut lines = Vec::new();
    let mut warn = false;
    let ok = |lines: &mut Vec<String>, msg: String| lines.push(format!("OK   {msg}"));
    let mut bad = |lines: &mut Vec<String>, msg: String| {
        lines.push(format!("WARN {msg}"));
        warn = true;
    };
    match facts {
        None => bad(
            &mut lines,
            "backend: none for this OS (winhands drives Windows and macOS)".into(),
        ),
        Some(f) => {
            ok(&mut lines, format!("backend: {}", f.name));
            for p in &f.permissions {
                if p.granted {
                    ok(&mut lines, format!("permission {}: granted", p.name));
                } else {
                    bad(
                        &mut lines,
                        format!(
                            "permission {}: NOT granted. Needed for {}. Fix: {}",
                            p.name, p.needed_for, p.fix
                        ),
                    );
                }
            }
            if f.capabilities.elevation {
                match f.elevated {
                    Some(true) => ok(&mut lines, "elevation (this shell): elevated".into()),
                    Some(false) => ok(
                        &mut lines,
                        "elevation (this shell): not elevated; the daemon is the process that needs it".into(),
                    ),
                    None => bad(&mut lines, "elevation: could not be read".into()),
                }
            }
            if f.displays.is_empty() {
                bad(&mut lines, "displays: none found".into());
            }
            for (i, d) in f.displays.iter().enumerate() {
                ok(
                    &mut lines,
                    format!(
                        "display {}: {}x{} at scale {}{}",
                        i + 1,
                        d.rect.width(),
                        d.rect.height(),
                        d.scale,
                        if d.primary { " (primary)" } else { "" }
                    ),
                );
            }
            if f.capabilities.ocr {
                ok(&mut lines, format!("ocr: available ({})", f.ocr_engine));
            } else {
                bad(
                    &mut lines,
                    format!("ocr: not available on this backend yet ({})", f.ocr_engine),
                );
            }
        }
    }
    match health {
        None => bad(&mut lines, "daemon: not reachable (winhands start)".into()),
        Some(h) => {
            let version = h
                .get("version")
                .map_or_else(|| "None".to_string(), wh_core::textutil::py_str);
            ok(&mut lines, format!("daemon: reachable (version {version})"));
            if facts.is_some_and(|f| f.name == "macos") {
                // no UAC-style boundary on macOS: nothing to report
            } else if h.get("elevated").is_some_and(wh_core::config::truthy) {
                ok(&mut lines, "elevation: daemon is elevated".into());
            } else {
                bad(
                    &mut lines,
                    "elevation: daemon is NOT elevated (admin windows will not respond)".into(),
                );
            }
            let ocr = h
                .get("ocr")
                .and_then(Value::as_str)
                .filter(|o| !o.is_empty())
                .unwrap_or("untried");
            ok(&mut lines, format!("ocr engine: {ocr}"));
        }
    }
    if token_present {
        ok(&mut lines, "token: present".into());
    } else {
        bad(&mut lines, format!("token: missing ({token_path})"));
    }
    ok(&mut lines, format!("coordinate mode: {}", cfg.coords()));
    if cfg.acknowledged() {
        ok(
            &mut lines,
            format!("acknowledged: yes ({})", cfg.paths().ack().display()),
        );
    } else {
        bad(
            &mut lines,
            format!(
                "acknowledged: NO; every action that clicks, types, opens or drags is refused. \
                 The owner of this machine runs: {}",
                wh_core::config::ACK_COMMAND
            ),
        );
    }
    let only_apps = cfg.only_apps_default();
    if only_apps.is_empty() {
        bad(
            &mut lines,
            "default allowlist (WINHANDS_ONLY_APPS): none set; computer tool runs unrestricted by default".into(),
        );
    } else {
        ok(
            &mut lines,
            format!(
                "default allowlist (WINHANDS_ONLY_APPS): {}",
                only_apps.join(", ")
            ),
        );
    }
    (lines, warn)
}

/// On macOS, a line saying whose privacy grants a `doctor` run reflects: macOS charges them to the
/// app that started the process, so outside `winhands.app` they are Terminal's, not winhands's.
pub fn bundle_note(exe: &std::path::Path, macos: bool) -> Option<String> {
    (macos && !crate::start::inside_app_bundle(exe)).then(|| {
        format!(
            "NOTE not running from winhands.app ({}): macOS shows the privacy grants of the app \
             that started this process (Terminal), not winhands.app's. packaging/macos/install.sh \
             runs doctor as the app for you",
            exe.display()
        )
    })
}

fn cmd_doctor(mut rest: Vec<String>) -> i32 {
    let prompt = take_flag(&mut rest, "--prompt");
    if let Some(extra) = rest.first() {
        eprintln!(
            "winhands doctor: unexpected argument {extra:?}. usage: winhands doctor [--prompt]"
        );
        return 2;
    }
    if let Some(note) = bundle_note(&exe_path(), cfg!(target_os = "macos")) {
        println!("{note}");
    }
    if prompt {
        if platform::request_permission_prompts() {
            println!(
                "NOTE asked macOS to show its permission prompts for this app. In System Settings > \
                 Privacy & Security, switch winhands ON under Accessibility and Screen & System Audio Recording"
            );
        } else {
            println!("NOTE --prompt does nothing on this OS (it has no permission prompts)");
        }
    }
    let cfg = Config::from_system();
    let link = local_link(
        &cfg,
        &StartOptions {
            cmd: None,
            task: None,
        },
        Duration::from_secs(2),
    );
    let facts = platform::backend().ok().map(|b| backend_facts(b.as_ref()));
    let health = link.health(Duration::from_secs(2));
    let token_present = !wh_core::token::read_token(cfg.paths()).is_empty();
    let (lines, warn) = doctor_lines(
        facts.as_ref(),
        health.as_ref(),
        token_present,
        &cfg.paths().token().display().to_string(),
        &cfg,
    );
    for line in lines {
        println!("{line}");
    }
    i32::from(warn)
}

#[cfg(test)]
mod tests;
