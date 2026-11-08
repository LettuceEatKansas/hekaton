//! Starting the daemon on demand (Python `cli.start_daemon`, `hk.start_daemon`).
//!
//! The Python build runs the elevated scheduled task `hekaton daemon`. That task still points at the
//! Python exe until the swap, so the Rust build does not assume it. How a missing daemon is started is
//! chosen, first match wins, by:
//!
//! 1. `--daemon-cmd "<command line>"` or `daemon_cmd` in `config.json` / `HEKATON_DAEMON_CMD`: run
//!    that command detached (for example `"C:\Program Files\hekaton\hekaton.exe" daemon`);
//! 2. `--daemon-task <name>` or `daemon_task` / `HEKATON_DAEMON_TASK`: `schtasks /run /tn <name>`
//!    (Windows only; this is how an elevated daemon is started without a prompt);
//! 3. on macOS, `launchctl kickstart gui/<uid>/<label>` when `daemon_launchd_label` /
//!    `HEKATON_DAEMON_LAUNCHD_LABEL` is set, or when this executable runs from inside a
//!    `hekaton.app` bundle (label `org.hekaton.app`, the LaunchAgent that
//!    `packaging/macos/install.sh` installs). launchd then starts the daemon as its own process, so
//!    macOS charges the Accessibility and Screen Recording grants to hekaton.app and not to Claude or
//!    Terminal. Only when no such agent is loaded does it fall back to spawning the daemon (5), with a
//!    warning;
//! 4. on Windows, `hekatond.exe` next to this executable, detached: the same daemon built for the
//!    GUI subsystem, so no console window appears;
//! 5. otherwise: this same executable with the argument `daemon`, detached, running as the user who
//!    started the shim.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use hekaton_core::config::{Config, to_text};

/// The LaunchAgent `packaging/macos/install.sh` installs (also the app's bundle id).
pub const LAUNCHD_LABEL: &str = "org.hekaton.app";

/// How a missing daemon gets started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Starter {
    /// Run this command line, detached.
    Command(Vec<String>),
    /// Run this scheduled task (`schtasks /run /tn NAME`).
    Task(String),
    /// Start this LaunchAgent (`launchctl kickstart gui/<uid>/<label>`, macOS). `exe` is what runs
    /// as `exe daemon`, detached, when no such agent is loaded.
    Launchd { label: String, exe: PathBuf },
    /// Run the console-less `hekatond` found next to this executable, detached (Windows).
    Hidden(PathBuf),
    /// Run this executable's `daemon` subcommand, detached.
    SelfDaemon(PathBuf),
}

/// Is `exe` inside a `hekaton.app` bundle (`.../hekaton.app/Contents/MacOS/hekaton`)?
pub fn inside_app_bundle(exe: &Path) -> bool {
    exe.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("hekaton.app"))
    })
}

/// The LaunchAgent to kickstart, if any: `daemon_launchd_label` from the config, else the default
/// label when `exe` runs from inside the app bundle. Always `None` off macOS.
fn launchd_label(cfg: &Config, exe: &Path, macos: bool) -> Option<String> {
    if !macos {
        return None;
    }
    setting(cfg, "daemon_launchd_label")
        .or_else(|| inside_app_bundle(exe).then(|| LAUNCHD_LABEL.to_string()))
}

/// The launchd service target of a LaunchAgent in a user's graphical session.
pub fn launchd_target(uid: u32, label: &str) -> String {
    format!("gui/{uid}/{label}")
}

/// A command line split into words: whitespace separates, double quotes group, nothing else is
/// special (a Windows path keeps its backslashes).
pub fn split_command(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut started = false;
    for c in line.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                started = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(current);
    }
    words
}

fn setting(cfg: &Config, key: &str) -> Option<String> {
    let text = to_text(&cfg.get(key));
    let text = text.trim();
    (!text.is_empty() && text != "null").then(|| text.to_string())
}

impl Starter {
    /// Pick the start method. `exe` is this executable (the default method runs it).
    pub fn resolve(
        cfg: &Config,
        exe: PathBuf,
        cmd_flag: Option<&str>,
        task_flag: Option<&str>,
    ) -> Starter {
        Self::resolve_for(cfg, exe, cmd_flag, task_flag, cfg!(target_os = "macos"))
    }

    /// [`Starter::resolve`] with the OS given, so the macOS choice is testable everywhere.
    fn resolve_for(
        cfg: &Config,
        exe: PathBuf,
        cmd_flag: Option<&str>,
        task_flag: Option<&str>,
        macos: bool,
    ) -> Starter {
        let cmd = cmd_flag
            .map(str::to_string)
            .filter(|c| !c.trim().is_empty())
            .or_else(|| setting(cfg, "daemon_cmd"));
        if let Some(cmd) = cmd {
            let words = split_command(&cmd);
            if !words.is_empty() {
                return Starter::Command(words);
            }
        }
        let task = task_flag
            .map(str::to_string)
            .filter(|t| !t.trim().is_empty())
            .or_else(|| setting(cfg, "daemon_task"));
        if let Some(task) = task {
            return Starter::Task(task);
        }
        if let Some(label) = launchd_label(cfg, &exe, macos) {
            return Starter::Launchd { label, exe };
        }
        match hidden_daemon_next_to(&exe, cfg!(windows)) {
            Some(hidden) => Starter::Hidden(hidden),
            None => Starter::SelfDaemon(exe),
        }
    }

    /// What to tell a person who has to start it by hand.
    pub fn describe(&self) -> String {
        match self {
            Starter::Command(words) => words
                .iter()
                .map(|w| {
                    if w.contains(' ') {
                        format!("\"{w}\"")
                    } else {
                        w.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
            Starter::Task(name) => format!("schtasks /run /tn \"{name}\""),
            Starter::Launchd { label, .. } => {
                format!("launchctl kickstart gui/$(id -u)/{label}")
            }
            Starter::Hidden(path) => format!("\"{}\"", path.display()),
            Starter::SelfDaemon(exe) => format!("\"{}\" daemon", exe.display()),
        }
    }

    /// Ask the OS to start the daemon. Returns as soon as it is started; the caller waits for
    /// `/health`. Output of a spawned daemon goes to `log` (rotated at 1 MB).
    pub fn start(&self, log: &Path) -> io::Result<()> {
        match self {
            Starter::Command(words) => spawn_detached(words, log),
            Starter::SelfDaemon(exe) => {
                spawn_detached(&[exe.to_string_lossy().into_owned(), "daemon".into()], log)
            }
            Starter::Hidden(path) => spawn_detached(&[path.to_string_lossy().into_owned()], log),
            Starter::Task(name) => run_task(name),
            Starter::Launchd { label, exe } => start_launchd(label, exe, log),
        }
    }
}

/// Run `launchctl` with `args`; an error when it cannot run or exits non-zero.
fn launchctl(args: &[&str]) -> io::Result<()> {
    let status = Command::new("launchctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "launchctl {} exited with {status}",
            args.first().copied().unwrap_or("")
        )))
    }
}

/// Start the LaunchAgent with `launchctl kickstart` (no `-k`: a running daemon is left alone), so
/// launchd, not this process, is the daemon's parent and macOS charges the privacy grants to the
/// app. A failed kickstart with the agent loaded is returned as an error (the caller polls anyway:
/// it is also what "already running" can look like). With no agent loaded the daemon is spawned
/// as a child instead, with a warning on stderr and in the log.
fn start_launchd(label: &str, exe: &Path, log: &Path) -> io::Result<()> {
    let Some(uid) = crate::platform::user_id() else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "launchd exists on macOS only",
        ));
    };
    let target = launchd_target(uid, label);
    let why = match launchctl(&["kickstart", &target]) {
        Ok(()) => return Ok(()),
        Err(why) => why,
    };
    if launchctl(&["print", &target]).is_ok() {
        return Err(why);
    }
    let warning = format!(
        "warning: the LaunchAgent {label} is not loaded ({why}), so the daemon is started as a \
         child of this process. macOS then charges Accessibility and Screen Recording to the app \
         that started it (Claude or Terminal), not to hekaton.app. Run packaging/macos/install.sh"
    );
    eprintln!("hekaton: {warning}");
    crate::serve::append_log(log, &warning);
    spawn_detached(&[exe.to_string_lossy().into_owned(), "daemon".into()], log)
}

/// The name of the console-less daemon executable.
pub const HIDDEN_DAEMON: &str = "hekatond.exe";

/// `hekatond.exe` in the same folder as `exe`, if `windows` and the file is there.
pub fn hidden_daemon_next_to(exe: &Path, windows: bool) -> Option<PathBuf> {
    if !windows {
        return None;
    }
    let candidate = exe.with_file_name(HIDDEN_DAEMON);
    candidate.is_file().then_some(candidate)
}

fn run_task(name: &str) -> io::Result<()> {
    if !cfg!(windows) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "scheduled tasks exist on Windows only",
        ));
    }
    let mut cmd = Command::new("schtasks");
    cmd.args(["/run", "/tn", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    no_window(&mut cmd);
    let status = cmd.status()?;
    if status.success() {
        Ok(())
    } else {
        // Non-zero is also what "already running" looks like while another caller's start is still
        // binding the port, so the caller polls anyway, just not for long.
        Err(io::Error::other(format!("schtasks exited with {status}")))
    }
}

#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}

fn spawn_detached(argv: &[String], log: &Path) -> io::Result<()> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty daemon command"))?;
    keep_std_handles_private();
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null());
    match open_log(log) {
        Ok(file) => {
            let err = file.try_clone()?;
            cmd.stdout(file).stderr(err);
        }
        Err(_) => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    detach(&mut cmd);
    let mut child = cmd.spawn()?;
    // reap it when it exits, so a daemon that idles out does not linger as a zombie
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// A daemon that outlives its starter must not inherit the starter's pipes (see
/// `hekaton_win::keep_std_handles_private`).
#[cfg(windows)]
fn keep_std_handles_private() {
    hekaton_win::keep_std_handles_private();
}

#[cfg(not(windows))]
fn keep_std_handles_private() {}

fn open_log(path: &Path) -> io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    hekaton_core::audit::rotate(path, 1_000_000);
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(not(any(windows, unix)))]
fn detach(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;
    use hekaton_core::env::MapEnv;
    use hekaton_core::paths::Paths;

    fn cfg(env: MapEnv, file: Option<&str>) -> (Config, std::path::PathBuf) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hk-start-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths::in_dir(&dir);
        if let Some(text) = file {
            std::fs::write(paths.config(), text).unwrap();
        }
        (Config::new(paths, env), dir)
    }

    fn exe() -> PathBuf {
        PathBuf::from("C:\\tools\\hekaton.exe")
    }

    #[test]
    fn splitting_keeps_quoted_paths_with_spaces_and_backslashes() {
        assert_eq!(
            split_command(r#""C:\Program Files\hekaton\hekaton.exe" daemon --no-hotkey"#),
            [
                "C:\\Program Files\\hekaton\\hekaton.exe",
                "daemon",
                "--no-hotkey"
            ]
        );
        assert_eq!(split_command("  a   b  "), ["a", "b"]);
        assert_eq!(split_command(r#"x "" y"#), ["x", "", "y"]);
        assert!(split_command("   ").is_empty());
    }

    #[test]
    fn with_nothing_configured_the_binary_starts_itself() {
        let (c, _) = cfg(MapEnv::new(), None);
        assert_eq!(
            Starter::resolve(&c, exe(), None, None),
            Starter::SelfDaemon(exe())
        );
    }

    #[test]
    fn a_task_in_the_config_file_is_run_with_schtasks() {
        let (c, _) = cfg(MapEnv::new(), Some(r#"{"daemon_task": "hekaton daemon"}"#));
        let s = Starter::resolve(&c, exe(), None, None);
        assert_eq!(s, Starter::Task("hekaton daemon".into()));
        assert_eq!(s.describe(), "schtasks /run /tn \"hekaton daemon\"");
    }

    #[test]
    fn a_command_beats_a_task_and_a_flag_beats_the_config() {
        let (c, _) = cfg(
            MapEnv::new().with("HEKATON_DAEMON_CMD", "env-hekaton daemon"),
            Some(r#"{"daemon_task": "t", "daemon_cmd": "file-hekaton daemon"}"#),
        );
        assert_eq!(
            Starter::resolve(&c, exe(), None, Some("flag-task")),
            Starter::Command(vec!["env-hekaton".into(), "daemon".into()]),
            "the environment beats the file, and a command beats a task"
        );
        assert_eq!(
            Starter::resolve(&c, exe(), Some("flag-hekaton daemon"), None),
            Starter::Command(vec!["flag-hekaton".into(), "daemon".into()])
        );
    }

    #[test]
    fn an_empty_command_is_not_a_command() {
        let (c, _) = cfg(MapEnv::new(), None);
        assert_eq!(
            Starter::resolve(&c, exe(), Some("  "), Some("")),
            Starter::SelfDaemon(exe())
        );
    }

    #[test]
    fn the_console_less_daemon_next_to_the_exe_is_found_on_windows_only() {
        let (_, dir) = cfg(MapEnv::new(), None);
        let exe = dir.join("hekaton.exe");
        assert_eq!(hidden_daemon_next_to(&exe, true), None, "nothing there yet");
        std::fs::write(dir.join(HIDDEN_DAEMON), b"x").unwrap();
        let hidden = dir.join(HIDDEN_DAEMON);
        assert_eq!(hidden_daemon_next_to(&exe, true), Some(hidden.clone()));
        assert_eq!(hidden_daemon_next_to(&exe, false), None, "not off Windows");
        // it also works when the exe runs under another name (hk.exe is a copy)
        assert_eq!(
            hidden_daemon_next_to(&dir.join("hk.exe"), true),
            Some(hidden.clone())
        );
        let s = Starter::Hidden(hidden.clone());
        assert_eq!(s.describe(), format!("\"{}\"", hidden.display()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_configured_task_or_command_beats_the_console_less_daemon() {
        // resolve() only looks next to the real exe path it is given
        let (c, dir) = cfg(MapEnv::new(), Some(r#"{"daemon_task": "hekaton daemon"}"#));
        std::fs::write(dir.join(HIDDEN_DAEMON), b"x").unwrap();
        let exe = dir.join("hekaton.exe");
        assert_eq!(
            Starter::resolve(&c, exe.clone(), None, None),
            Starter::Task("hekaton daemon".into())
        );
        let (c, dir2) = cfg(MapEnv::new(), None);
        std::fs::write(dir2.join(HIDDEN_DAEMON), b"x").unwrap();
        let exe = dir2.join("hekaton.exe");
        let s = Starter::resolve(&c, exe.clone(), None, None);
        if cfg!(windows) {
            assert_eq!(s, Starter::Hidden(dir2.join(HIDDEN_DAEMON)));
        } else {
            assert_eq!(s, Starter::SelfDaemon(exe));
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    fn app_exe() -> PathBuf {
        PathBuf::from("/Users/you/Applications/hekaton.app/Contents/MacOS/hekaton")
    }

    #[test]
    fn inside_the_app_bundle_macos_kickstarts_the_launch_agent() {
        let (c, _) = cfg(MapEnv::new(), None);
        let s = Starter::resolve_for(&c, app_exe(), None, None, true);
        assert_eq!(
            s,
            Starter::Launchd {
                label: LAUNCHD_LABEL.into(),
                exe: app_exe()
            }
        );
        assert_eq!(
            s.describe(),
            "launchctl kickstart gui/$(id -u)/org.hekaton.app"
        );
    }

    #[test]
    fn launchd_is_for_macos_only_and_needs_the_bundle_or_a_label() {
        let (c, _) = cfg(MapEnv::new(), None);
        assert_eq!(
            Starter::resolve_for(&c, app_exe(), None, None, false),
            Starter::SelfDaemon(app_exe()),
            "not off macOS"
        );
        let dev = PathBuf::from("/Users/you/src/hekaton-rs/target/debug/hekaton");
        assert_eq!(
            Starter::resolve_for(&c, dev.clone(), None, None, true),
            Starter::SelfDaemon(dev),
            "a development build outside the bundle spawns itself"
        );
    }

    #[test]
    fn a_configured_launchd_label_works_outside_the_bundle_and_loses_to_cmd_and_task() {
        let dev = PathBuf::from("/opt/hekaton/hekaton");
        let (c, _) = cfg(
            MapEnv::new(),
            Some(r#"{"daemon_launchd_label": "org.example.hk"}"#),
        );
        assert_eq!(
            Starter::resolve_for(&c, dev.clone(), None, None, true),
            Starter::Launchd {
                label: "org.example.hk".into(),
                exe: dev.clone()
            }
        );
        assert_eq!(
            Starter::resolve_for(&c, dev.clone(), None, Some("t"), true),
            Starter::Task("t".into())
        );
        assert_eq!(
            Starter::resolve_for(&c, dev, Some("x daemon"), None, true),
            Starter::Command(vec!["x".into(), "daemon".into()])
        );
        let (c, _) = cfg(
            MapEnv::new().with("HEKATON_DAEMON_LAUNCHD_LABEL", "env.label"),
            Some(r#"{"daemon_launchd_label": "file.label"}"#),
        );
        assert_eq!(
            Starter::resolve_for(&c, app_exe(), None, None, true),
            Starter::Launchd {
                label: "env.label".into(),
                exe: app_exe()
            },
            "the environment beats the file, and both beat the default label"
        );
    }

    #[test]
    fn the_app_bundle_is_found_by_its_folder_name_in_any_case() {
        assert!(inside_app_bundle(&app_exe()));
        assert!(inside_app_bundle(Path::new(
            "/Applications/Hekaton.App/Contents/MacOS/hekaton"
        )));
        assert!(!inside_app_bundle(Path::new("/usr/local/bin/hekaton")));
        assert!(!inside_app_bundle(Path::new("/x/nothekaton.app/y")));
        assert_eq!(
            launchd_target(501, LAUNCHD_LABEL),
            "gui/501/org.hekaton.app"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn launchd_is_refused_off_macos() {
        let log = std::env::temp_dir().join("hk-start-unused-launchd.log");
        let s = Starter::Launchd {
            label: "x".into(),
            exe: PathBuf::from("hekaton"),
        };
        assert!(s.start(&log).is_err());
    }

    #[test]
    fn describing_quotes_words_with_spaces() {
        let s = Starter::Command(vec!["C:\\Program Files\\w.exe".into(), "daemon".into()]);
        assert_eq!(s.describe(), "\"C:\\Program Files\\w.exe\" daemon");
        assert_eq!(
            Starter::SelfDaemon(exe()).describe(),
            "\"C:\\tools\\hekaton.exe\" daemon"
        );
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        let log = std::env::temp_dir().join(format!("hk-start-log-{}.log", std::process::id()));
        let s = Starter::Command(vec!["definitely-not-a-program-hekaton".into()]);
        assert!(s.start(&log).is_err());
        let _ = std::fs::remove_file(&log);
    }

    #[cfg(not(windows))]
    #[test]
    fn a_scheduled_task_is_refused_off_windows() {
        let log = std::env::temp_dir().join("hk-start-unused.log");
        assert!(Starter::Task("x".into()).start(&log).is_err());
    }
}
