//! First-run acknowledgement (owner decision R3, 2026-09-24).
//!
//! Nothing that clicks, types, opens or drags happens until the machine's owner has typed the
//! phrase at their own terminal. The marker file (`acknowledged.json`) is written only by
//! [`run_acknowledge`], a command the person runs; no request to the daemon can create it. Reads are
//! exempt so a client can take a screenshot, list windows and explain itself first.
//!
//! Port of `config.acknowledged()` / `config.ack_refusal()` (see [`crate::config`]),
//! `daemon._check_acknowledged()`, and `cli.cmd_acknowledge()` / `cli.write_ack()`.

use std::fmt;
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::actions::{ACK_EXEMPT_ACTIONS, ACK_EXEMPT_ROUTES};
use crate::config::Config;
use crate::paths::Paths;

/// What the owner types to accept. Compared case-insensitively.
pub const ACK_PHRASE: &str = "I understand";

/// Printed before the owner is asked to accept (cli.py `ACK_WARNING`).
pub const ACK_WARNING: &str = "hekaton: read this before you accept it.

  * It controls your real mouse, keyboard and windows, as administrator. Anything you can do at
    this machine, it can do: click buttons, type into any app, open programs, drag files.
  * It takes screenshots of every monitor and can read the text in any window, including windows
    you did not point it at.
  * Any program running as you on this machine can read its token file and get the same power.
    That includes anything you install, and anything that talks to a program you installed.
  * There is a pause hotkey (hekaton status prints it), and it pauses itself while you are using
    the mouse or keyboard, but neither is a security boundary.

To remove it later: run uninstall.ps1 from an elevated prompt (it removes the scheduled task),
then delete this folder and %LOCALAPPDATA%\\hekaton.

Full detail is in SECURITY.md.
";

/// A request was refused because the machine has not been acknowledged (Python `PermissionError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckRefused(pub String);

impl fmt::Display for AckRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AckRefused {}

/// Refuse anything that clicks, types, opens or drags until the owner has acknowledged.
///
/// `route` is the HTTP route (`"click"`, `"computer"`, `"windows"`, ...). For `"computer"`, `args`
/// is the call's JSON (`{"actions": [...]}` or a single action) and every step must be a read.
/// Python `daemon._check_acknowledged(path, args)`.
pub fn check_acknowledged(cfg: &Config, route: &str, args: &Value) -> Result<(), AckRefused> {
    if cfg.acknowledged() {
        return Ok(());
    }
    let what = if route == "computer" {
        let single = [args.clone()];
        let steps: &[Value] = match args.get("actions") {
            Some(Value::Array(list)) if !list.is_empty() => list,
            _ => &single,
        };
        let blocked = steps.iter().find(|s| {
            !s.get("action")
                .and_then(Value::as_str)
                .is_some_and(|a| ACK_EXEMPT_ACTIONS.contains(&a))
        });
        let Some(step) = blocked else {
            return Ok(());
        };
        let name = match step.get("action") {
            Some(Value::String(s)) => s.clone(),
            _ => "None".to_string(),
        };
        format!("the {name} action")
    } else if ACK_EXEMPT_ROUTES.contains(&route) {
        return Ok(());
    } else {
        format!("the {route} action")
    };
    Err(AckRefused(crate::config::ack_refusal(&what)))
}

/// Facts recorded in the marker. Supplied by the caller because they come from the OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckInfo {
    /// The signed-in user name.
    pub user: String,
    /// The host name.
    pub host: String,
    /// A platform string, for the record.
    pub os: String,
    /// The hekaton version.
    pub version: String,
}

impl AckInfo {
    /// Best-effort facts from the environment. The version is this crate's.
    pub fn from_env(env: &dyn crate::env::Env) -> Self {
        let user = env
            .var("USERNAME")
            .or_else(|| env.var("USER"))
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        let host = env
            .var("COMPUTERNAME")
            .or_else(|| env.var("HOSTNAME"))
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        Self {
            user,
            host,
            os: std::env::consts::OS.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// `YYYY-MM-DDTHH:MM:SS+0000` for a Unix time. The marker is only checked for existence, so UTC
/// stands in for Python's local-time `%z` form.
pub fn format_utc(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let rem = unix_secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+0000",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn now_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_utc(secs)
}

/// Write the marker file. `how` records the route taken, for the record, not for the check.
/// Python `cli.write_ack(how)`.
pub fn write_ack(paths: &Paths, how: &str, info: &AckInfo) -> io::Result<()> {
    std::fs::create_dir_all(paths.dir())?;
    let mut doc = Map::new();
    doc.insert("acknowledged_at".into(), Value::from(now_utc()));
    doc.insert("user".into(), Value::from(info.user.clone()));
    doc.insert("host".into(), Value::from(info.host.clone()));
    doc.insert("os".into(), Value::from(info.os.clone()));
    doc.insert("version".into(), Value::from(info.version.clone()));
    doc.insert("how".into(), Value::from(how));
    let mut text = serde_json::to_string_pretty(&Value::Object(doc)).map_err(io::Error::other)?;
    text.push('\n');
    std::fs::write(paths.ack(), text)
}

/// The `hekaton acknowledge` command. Deliberately interactive: refuses unless a person is typing
/// at a terminal, so an agent with a shell cannot clear its own gate by piping an answer at it.
///
/// `interactive` says whether stdin is a terminal; `read_answer` reads one line (None on EOF or
/// interrupt). Returns the process exit code. Python `cli.cmd_acknowledge`.
pub fn run_acknowledge(
    cfg: &Config,
    info: &AckInfo,
    interactive: bool,
    read_answer: &mut dyn FnMut() -> Option<String>,
    out: &mut dyn Write,
) -> io::Result<i32> {
    writeln!(out, "{ACK_WARNING}")?;
    if cfg.acknowledged() {
        writeln!(
            out,
            "hekaton: already acknowledged ({}).",
            cfg.paths().ack().display()
        )?;
        return Ok(0);
    }
    if !interactive {
        writeln!(
            out,
            "hekaton: this command has to be typed by the person at this machine, in their\n\
             own terminal window. Its input is not a terminal here, so nothing was recorded."
        )?;
        return Ok(1);
    }
    writeln!(out, "If you accept that, type: {ACK_PHRASE}")?;
    let Some(answer) = read_answer() else {
        writeln!(out, "\nhekaton: nothing recorded.")?;
        return Ok(1);
    };
    if answer.trim().to_lowercase() != ACK_PHRASE.to_lowercase() {
        writeln!(
            out,
            "hekaton: that was not the phrase, so nothing was recorded. \
             hekaton stays refused for everything that clicks, types, opens or drags."
        )?;
        return Ok(1);
    }
    write_ack(cfg.paths(), "typed at a terminal", info)?;
    writeln!(
        out,
        "hekaton: acknowledged. Marker written to {}.",
        cfg.paths().ack().display()
    )?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ACK_COMMAND;
    use crate::env::MapEnv;
    use crate::testutil::TempDir;
    use serde_json::json;

    /// Ported fixture `unacknowledged`: a machine where nobody has acknowledged anything yet.
    fn unacknowledged(label: &str) -> (TempDir, Config) {
        let dir = TempDir::new(label);
        let cfg = Config::new(Paths::in_dir(dir.path()), MapEnv::new());
        (dir, cfg)
    }

    fn info() -> AckInfo {
        AckInfo {
            user: "jon".into(),
            host: "box".into(),
            os: "test".into(),
            version: "0.0.0".into(),
        }
    }

    #[test]
    fn default_is_refused() {
        let (_d, cfg) = unacknowledged("ack-default");
        assert!(!cfg.acknowledged());
    }

    #[test]
    fn acting_routes_refuse_and_name_the_command() {
        let (_d, cfg) = unacknowledged("ack-routes");
        for route in ["click", "type", "key", "open", "drag", "hover", "scroll"] {
            let err = check_acknowledged(&cfg, route, &json!({}))
                .unwrap_err()
                .to_string();
            assert!(err.contains(ACK_COMMAND), "{err}");
            assert!(err.contains(route), "{err}");
        }
    }

    #[test]
    fn passive_routes_work_before_acknowledgement() {
        let (_d, cfg) = unacknowledged("ack-passive");
        for route in ["windows", "snapshot", "screenshot", "files"] {
            check_acknowledged(&cfg, route, &json!({})).unwrap();
        }
    }

    #[test]
    fn computer_reads_work_and_computer_input_refuses() {
        let (_d, cfg) = unacknowledged("ack-computer");
        for action in ["screenshot", "list_windows", "cursor_position", "read"] {
            check_acknowledged(&cfg, "computer", &json!({ "action": action })).unwrap();
        }
        for action in [
            "left_click",
            "type",
            "key",
            "open_app",
            "left_click_drag",
            "clipboard_set",
        ] {
            let err = check_acknowledged(&cfg, "computer", &json!({ "action": action }))
                .unwrap_err()
                .to_string();
            assert!(err.contains("acknowledge"), "{err}");
        }
    }

    #[test]
    fn a_batch_refuses_if_any_step_acts() {
        let (_d, cfg) = unacknowledged("ack-batch");
        let reads = json!({"actions": [{"action": "screenshot"}, {"action": "list_windows"}]});
        check_acknowledged(&cfg, "computer", &reads).unwrap();
        let mixed = json!({"actions": [
            {"action": "screenshot"}, {"action": "left_click", "coordinate": [1, 2]}]});
        let err = check_acknowledged(&cfg, "computer", &mixed)
            .unwrap_err()
            .to_string();
        assert!(err.contains("left_click"), "{err}");
    }

    #[test]
    fn a_step_with_no_action_name_is_not_a_read() {
        let (_d, cfg) = unacknowledged("ack-noaction");
        let err = check_acknowledged(&cfg, "computer", &json!({}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("the None action"), "{err}");
    }

    #[test]
    fn marker_file_clears_the_gate() {
        let (_d, cfg) = unacknowledged("ack-marker");
        std::fs::write(cfg.paths().ack(), r#"{"acknowledged_at": "now"}"#).unwrap();
        assert!(cfg.acknowledged());
        check_acknowledged(&cfg, "click", &json!({"target": "text:Save"})).unwrap();
    }

    #[test]
    fn env_override_for_ci() {
        let dir = TempDir::new("ack-env");
        let on = Config::new(
            Paths::in_dir(dir.path()),
            MapEnv::new().with("HEKATON_ACKNOWLEDGED", "1"),
        );
        assert!(on.acknowledged());
        let off = Config::new(
            Paths::in_dir(dir.path()),
            MapEnv::new().with("HEKATON_ACKNOWLEDGED", "0"),
        );
        assert!(!off.acknowledged());
    }

    // ---- the command a human has to type ----

    fn run(cfg: &Config, interactive: bool, answer: Option<&str>) -> (i32, String) {
        let mut out = Vec::new();
        let mut answer = answer.map(str::to_string);
        let code =
            run_acknowledge(cfg, &info(), interactive, &mut || answer.take(), &mut out).unwrap();
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn acknowledge_refuses_when_nobody_is_typing() {
        let (_d, cfg) = unacknowledged("ack-pipe");
        let (code, out) = run(&cfg, false, Some(ACK_PHRASE));
        assert_eq!(code, 1);
        assert!(!cfg.paths().ack().exists());
        assert!(
            out.contains("mouse, keyboard and windows") && out.contains("uninstall"),
            "{out}"
        );
    }

    #[test]
    fn acknowledge_needs_the_phrase() {
        let (_d, cfg) = unacknowledged("ack-phrase");
        let (code, _) = run(&cfg, true, Some("yes"));
        assert_eq!(code, 1);
        assert!(!cfg.paths().ack().exists());
    }

    #[test]
    fn acknowledge_stops_on_end_of_input() {
        let (_d, cfg) = unacknowledged("ack-eof");
        let (code, out) = run(&cfg, true, None);
        assert_eq!(code, 1);
        assert!(out.contains("nothing recorded"));
        assert!(!cfg.paths().ack().exists());
    }

    #[test]
    fn acknowledge_writes_the_marker() {
        let (_d, cfg) = unacknowledged("ack-write");
        let (code, _) = run(&cfg, true, Some("  i UNDERSTAND "));
        assert_eq!(code, 0, "the phrase is trimmed and case-insensitive");
        assert!(cfg.acknowledged());
        check_acknowledged(&cfg, "type", &json!({"text": "hi"})).unwrap();
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(cfg.paths().ack()).unwrap()).unwrap();
        assert_eq!(doc["how"], "typed at a terminal");
        assert_eq!(doc["user"], "jon");
        let stamp = doc["acknowledged_at"].as_str().unwrap();
        assert_eq!(stamp.len(), "2026-10-08T12:00:00+0000".len(), "{stamp}");
    }

    #[test]
    fn acknowledging_twice_says_so_and_changes_nothing() {
        let (_d, cfg) = unacknowledged("ack-twice");
        run(&cfg, true, Some(ACK_PHRASE));
        let before = std::fs::read_to_string(cfg.paths().ack()).unwrap();
        let (code, out) = run(&cfg, true, Some(ACK_PHRASE));
        assert_eq!(code, 0);
        assert!(out.contains("already acknowledged"));
        assert_eq!(std::fs::read_to_string(cfg.paths().ack()).unwrap(), before);
    }

    #[test]
    fn utc_formatting_matches_known_dates() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00+0000");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00+0000");
        assert_eq!(format_utc(1_791_460_800), "2026-10-08T12:00:00+0000");
    }
}
