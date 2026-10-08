//! Starting the daemon on demand (Python `cli.start_daemon`, `wh.start_daemon`).
//!
//! The Python build runs the elevated scheduled task `winhands daemon`. That task still points at the
//! Python exe until the swap, so the Rust build does not assume it. How a missing daemon is started is
//! chosen, first match wins, by:
//!
//! 1. `--daemon-cmd "<command line>"` or `daemon_cmd` in `config.json` / `WINHANDS_DAEMON_CMD`: run
//!    that command detached (for example `"C:\Program Files\winhands\winhands.exe" daemon`);
//! 2. `--daemon-task <name>` or `daemon_task` / `WINHANDS_DAEMON_TASK`: `schtasks /run /tn <name>`
//!    (Windows only; this is how an elevated daemon is started without a prompt);
//! 3. on Windows, `winhandsd.exe` next to this executable, detached: the same daemon built for the
//!    GUI subsystem, so no console window appears;
//! 4. otherwise: this same executable with the argument `daemon`, detached, running as the user who
//!    started the shim.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use wh_core::config::{Config, to_text};

/// How a missing daemon gets started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Starter {
    /// Run this command line, detached.
    Command(Vec<String>),
    /// Run this scheduled task (`schtasks /run /tn NAME`).
    Task(String),
    /// Run the console-less `winhandsd` found next to this executable, detached (Windows).
    Hidden(PathBuf),
    /// Run this executable's `daemon` subcommand, detached.
    SelfDaemon(PathBuf),
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
        }
    }
}

/// The name of the console-less daemon executable.
pub const HIDDEN_DAEMON: &str = "winhandsd.exe";

/// `winhandsd.exe` in the same folder as `exe`, if `windows` and the file is there.
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
/// `wh_win::keep_std_handles_private`).
#[cfg(windows)]
fn keep_std_handles_private() {
    wh_win::keep_std_handles_private();
}

#[cfg(not(windows))]
fn keep_std_handles_private() {}

fn open_log(path: &Path) -> io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    wh_core::audit::rotate(path, 1_000_000);
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
    use wh_core::env::MapEnv;
    use wh_core::paths::Paths;

    fn cfg(env: MapEnv, file: Option<&str>) -> (Config, std::path::PathBuf) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("wh-start-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths::in_dir(&dir);
        if let Some(text) = file {
            std::fs::write(paths.config(), text).unwrap();
        }
        (Config::new(paths, env), dir)
    }

    fn exe() -> PathBuf {
        PathBuf::from("C:\\tools\\winhands.exe")
    }

    #[test]
    fn splitting_keeps_quoted_paths_with_spaces_and_backslashes() {
        assert_eq!(
            split_command(r#""C:\Program Files\winhands\winhands.exe" daemon --no-hotkey"#),
            [
                "C:\\Program Files\\winhands\\winhands.exe",
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
        let (c, _) = cfg(MapEnv::new(), Some(r#"{"daemon_task": "winhands daemon"}"#));
        let s = Starter::resolve(&c, exe(), None, None);
        assert_eq!(s, Starter::Task("winhands daemon".into()));
        assert_eq!(s.describe(), "schtasks /run /tn \"winhands daemon\"");
    }

    #[test]
    fn a_command_beats_a_task_and_a_flag_beats_the_config() {
        let (c, _) = cfg(
            MapEnv::new().with("WINHANDS_DAEMON_CMD", "env-winhands daemon"),
            Some(r#"{"daemon_task": "t", "daemon_cmd": "file-winhands daemon"}"#),
        );
        assert_eq!(
            Starter::resolve(&c, exe(), None, Some("flag-task")),
            Starter::Command(vec!["env-winhands".into(), "daemon".into()]),
            "the environment beats the file, and a command beats a task"
        );
        assert_eq!(
            Starter::resolve(&c, exe(), Some("flag-winhands daemon"), None),
            Starter::Command(vec!["flag-winhands".into(), "daemon".into()])
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
        let exe = dir.join("winhands.exe");
        assert_eq!(hidden_daemon_next_to(&exe, true), None, "nothing there yet");
        std::fs::write(dir.join(HIDDEN_DAEMON), b"x").unwrap();
        let hidden = dir.join(HIDDEN_DAEMON);
        assert_eq!(hidden_daemon_next_to(&exe, true), Some(hidden.clone()));
        assert_eq!(hidden_daemon_next_to(&exe, false), None, "not off Windows");
        // it also works when the exe runs under another name (wh.exe is a copy)
        assert_eq!(
            hidden_daemon_next_to(&dir.join("wh.exe"), true),
            Some(hidden.clone())
        );
        let s = Starter::Hidden(hidden.clone());
        assert_eq!(s.describe(), format!("\"{}\"", hidden.display()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_configured_task_or_command_beats_the_console_less_daemon() {
        // resolve() only looks next to the real exe path it is given
        let (c, dir) = cfg(MapEnv::new(), Some(r#"{"daemon_task": "winhands daemon"}"#));
        std::fs::write(dir.join(HIDDEN_DAEMON), b"x").unwrap();
        let exe = dir.join("winhands.exe");
        assert_eq!(
            Starter::resolve(&c, exe.clone(), None, None),
            Starter::Task("winhands daemon".into())
        );
        let (c, dir2) = cfg(MapEnv::new(), None);
        std::fs::write(dir2.join(HIDDEN_DAEMON), b"x").unwrap();
        let exe = dir2.join("winhands.exe");
        let s = Starter::resolve(&c, exe.clone(), None, None);
        if cfg!(windows) {
            assert_eq!(s, Starter::Hidden(dir2.join(HIDDEN_DAEMON)));
        } else {
            assert_eq!(s, Starter::SelfDaemon(exe));
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn describing_quotes_words_with_spaces() {
        let s = Starter::Command(vec!["C:\\Program Files\\w.exe".into(), "daemon".into()]);
        assert_eq!(s.describe(), "\"C:\\Program Files\\w.exe\" daemon");
        assert_eq!(
            Starter::SelfDaemon(exe()).describe(),
            "\"C:\\tools\\winhands.exe\" daemon"
        );
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        let log = std::env::temp_dir().join(format!("wh-start-log-{}.log", std::process::id()));
        let s = Starter::Command(vec!["definitely-not-a-program-winhands".into()]);
        assert!(s.start(&log).is_err());
        let _ = std::fs::remove_file(&log);
    }

    #[cfg(not(windows))]
    #[test]
    fn a_scheduled_task_is_refused_off_windows() {
        let log = std::env::temp_dir().join("wh-start-unused.log");
        assert!(Starter::Task("x".into()).start(&log).is_err());
    }
}
