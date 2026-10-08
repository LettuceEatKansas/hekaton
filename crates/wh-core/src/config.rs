//! Runtime configuration. Port of `winhands/config.py`.
//!
//! Precedence, highest first: environment variable `WINHANDS_<KEY UPPER>`, then `config.json`
//! (same file, same flat JSON object as the Python build), then the built-in default. Both builds
//! therefore read one config.
//!
//! The Python source is the spec. Differences, all on malformed input only:
//! * a typed accessor falls back to the built-in default when a value cannot be converted
//!   (Python raises `ValueError` at the call site);
//! * [`Config::get`] itself keeps the Python behaviour (an unparsable integer env var comes back as the raw
//!   string).

use std::fmt;
use std::path::Path;

use serde_json::{Map, Value};

use crate::env::{Env, SystemEnv};
use crate::paths::Paths;

/// The exact line the machine's owner has to type. Every refusal quotes this one string.
///
/// Python quoted `bin\winhands.exe acknowledge`; the Rust build is one binary, so the command is
/// the subcommand name.
pub const ACK_COMMAND: &str = "winhands acknowledge";

/// Addresses `bind` may hold without `allow_remote_bind`.
pub const LOOPBACK_ADDRESSES: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

/// Every key this module knows, in the order of the Python `DEFAULTS` table.
pub const KEYS: [&str; 25] = [
    "port",
    "bind",
    "allow_remote_bind",
    "remote_caller",
    "human_pause",
    "idle_min",
    "idle_timeout_seconds",
    "no_tray",
    "only_apps",
    "coords",
    "shot_long",
    "auto_shot",
    "settle_ms",
    "settle_floor_ms",
    "shim_idle_s",
    "img",
    "mark",
    "loading_ms",
    "clip_max",
    "display",
    "zoom_ocr",
    "snap",
    "uia_ms",
    "paste_over",
    "protect_processes",
];

/// The built-in default of `key`, or `None` for a key winhands does not know.
///
/// `idle_timeout_seconds` is `Some(Value::Null)`: known, unset by default.
pub fn default_value(key: &str) -> Option<Value> {
    Some(match key {
        // daemon HTTP port (shim and CLI dial this)
        "port" => Value::from(47811),
        // address the daemon listens on; anything non-loopback also needs allow_remote_bind
        "bind" => Value::from("127.0.0.1"),
        "allow_remote_bind" => Value::from(false),
        // "1" says the caller is NOT a local process, so the caller lookup is not trusted
        "remote_caller" => Value::from(false),
        // "0" stops waiting for a person who is using the mouse
        "human_pause" => Value::from(true),
        // daemon exits after this many idle minutes (0 = never); 2026-10-08: raised to 60
        "idle_min" => Value::from(60),
        // same in seconds; wins over idle_min when set
        "idle_timeout_seconds" => Value::Null,
        // "1" skips the system-tray icon thread
        "no_tray" => Value::from(false),
        // comma list, default allowlist when a call sends none
        "only_apps" => Value::from(""),
        // "pixels" or "norm1000" coordinate convention
        "coords" => Value::from("pixels"),
        // long edge (px) screenshots are scaled down to; 2026-10-08: 1024 (was 1280)
        "shot_long" => Value::from(1024),
        // "0" skips the trailing full-screen screenshot
        "auto_shot" => Value::from(true),
        // max ms run_many waits for the screen to stop moving
        "settle_ms" => Value::from(1500),
        // min ms the screen is watched after an acting call (450 ms for fullscreen / no-UIA windows)
        "settle_floor_ms" => Value::from(130),
        // an idle MCP shim with a newer sibling under the same spawner exits (0 = never)
        "shim_idle_s" => Value::from(300),
        // screenshot encoding, "jpeg" or "png"
        "img" => Value::from("jpeg"),
        // "0" skips drawing the last-click marker
        "mark" => Value::from(true),
        // max ms a loading indicator gets before giving up
        "loading_ms" => Value::from(6000),
        // clipboard_set text length cap
        "clip_max" => Value::from(20000),
        // "x,y,w,h" fallback screen rect (no monitor picked)
        "display" => Value::from(""),
        // "0" disables OCR fallback for zoom/find_text
        "zoom_ocr" => Value::from(true),
        // "1" makes every call snap
        "snap" => Value::from(false),
        // ms UIA gets before falling back to OCR
        "uia_ms" => Value::from(300),
        // char count above which type() pastes instead of types
        "paste_over" => Value::from(6),
        // comma-separated exe names never clicked or typed into, whoever is calling
        "protect_processes" => Value::from(""),
        _ => return None,
    })
}

/// `bind` asks for a non-loopback address and `allow_remote_bind` was not set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBindRefused(pub String);

impl fmt::Display for RemoteBindRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RemoteBindRefused {}

/// Python truthiness of a JSON value (`bool(x)`).
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `int(x)` for the value kinds a config can hold. `None` where Python would raise.
pub fn to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Python `float(x)` for the value kinds a config can hold. `None` where Python would raise.
pub fn to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

/// Python `str(x)` for a value read from config. `None` becomes "" (callers write `str(x or "")`).
pub fn to_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Environment plus config file plus defaults, resolved at call time like the Python module.
pub struct Config {
    paths: Paths,
    env: Box<dyn Env>,
}

impl Config {
    /// A config over explicit paths and an explicit environment (tests, embedding).
    pub fn new(paths: Paths, env: impl Env + 'static) -> Self {
        Self {
            paths,
            env: Box::new(env),
        }
    }

    /// The real process environment and the standard data directory.
    pub fn from_system() -> Self {
        Self {
            paths: Paths::from_env(&SystemEnv),
            env: Box::new(SystemEnv),
        }
    }

    /// The paths this config reads and that the token, ack marker and audit log live under.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// The environment this config reads.
    pub fn env(&self) -> &dyn Env {
        self.env.as_ref()
    }

    /// `config.json` as a flat object. Missing, unreadable, not JSON, or not an object: empty.
    /// Python `_read_config_file`.
    pub fn read_file(&self) -> Map<String, Value> {
        let Ok(text) = std::fs::read_to_string(self.paths.config()) else {
            return Map::new();
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        }
    }

    /// Python `get(key)`: env var > config.json > default. A bool-typed default reads an env var
    /// as `raw != "0"`; an int-typed default parses it, and keeps the raw string when that fails.
    pub fn get(&self, key: &str) -> Value {
        let env_key = format!("WINHANDS_{}", key.to_uppercase());
        let default = default_value(key);
        if let Some(raw) = self.env.var(&env_key) {
            match &default {
                Some(Value::Bool(_)) => return Value::from(raw != "0"),
                Some(Value::Number(_)) => {
                    if let Ok(n) = raw.trim().parse::<i64>() {
                        return Value::from(n);
                    }
                }
                _ => {}
            }
            return Value::from(raw);
        }
        if let Some(v) = self.read_file().get(key) {
            return v.clone();
        }
        default.unwrap_or(Value::Null)
    }

    fn flag(&self, key: &str) -> bool {
        truthy(&self.get(key))
    }

    fn int(&self, key: &str) -> i64 {
        to_i64(&self.get(key))
            .or_else(|| default_value(key).as_ref().and_then(to_i64))
            .unwrap_or(0)
    }

    fn float(&self, key: &str) -> f64 {
        to_f64(&self.get(key))
            .or_else(|| default_value(key).as_ref().and_then(to_f64))
            .unwrap_or(0.0)
    }

    fn text(&self, key: &str) -> String {
        to_text(&self.get(key))
    }

    /// The running daemon's token, or "" when there is none. Python `token()`.
    pub fn token(&self) -> String {
        crate::token::read_token(&self.paths)
    }

    /// Daemon port. Python `port()`. Out of range values fall back to the default.
    pub fn port(&self) -> u16 {
        u16::try_from(self.int("port")).unwrap_or(47811)
    }

    fn bind_text(&self) -> String {
        let want = self.text("bind");
        let want = want.trim();
        if want.is_empty() {
            "127.0.0.1".to_string()
        } else {
            want.to_string()
        }
    }

    /// The address the daemon listens on. Always loopback unless the owner set BOTH `bind` and
    /// `allow_remote_bind`; one without the other is an error and the daemon must not listen.
    /// Python `bind_host()`.
    pub fn bind_host(&self) -> Result<String, RemoteBindRefused> {
        let want = self.bind_text();
        if LOOPBACK_ADDRESSES.contains(&want.to_lowercase().as_str()) {
            return Ok(want);
        }
        if !self.flag("allow_remote_bind") {
            return Err(RemoteBindRefused(format!(
                "bind is set to {want:?}, which is not loopback, and allow_remote_bind is not set. \
                 winhands refuses to listen beyond 127.0.0.1 unless both are set on purpose. \
                 To allow it, set WINHANDS_ALLOW_REMOTE_BIND=1 (or \"allow_remote_bind\": true in \
                 {}) and read the \"Bind address\" section of SECURITY.md first: the token \
                 becomes the only thing between the network and an elevated desktop.",
                self.paths.config().display()
            )));
        }
        Ok(want)
    }

    /// Is the configured listen address something other than loopback? Never errors.
    /// Python `bind_is_remote()`.
    pub fn bind_is_remote(&self) -> bool {
        !LOOPBACK_ADDRESSES.contains(&self.bind_text().to_lowercase().as_str())
    }

    /// Must the process on the other end of the socket be treated as NOT the real caller?
    /// True when `remote_caller` is set and always true for a non-loopback bind.
    /// Python `remote_caller()`.
    pub fn remote_caller(&self) -> bool {
        self.flag("remote_caller") || self.bind_is_remote()
    }

    /// Idle shutdown in seconds (0 = never). An explicit `idle_timeout_seconds` wins over
    /// `idle_min * 60`. Python `idle_timeout_seconds()`.
    pub fn idle_timeout_seconds(&self) -> i64 {
        to_i64(&self.get("idle_timeout_seconds")).unwrap_or_else(|| self.int("idle_min") * 60)
    }

    /// Python `no_tray()`.
    pub fn no_tray(&self) -> bool {
        self.flag("no_tray")
    }

    /// Python `only_apps_default()`: the comma list, trimmed, empties dropped.
    pub fn only_apps_default(&self) -> Vec<String> {
        self.text("only_apps")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Process names never clicked or typed into, lowercased. Accepts a comma string or a JSON
    /// list. Python `protected_processes()`.
    pub fn protected_processes(&self) -> std::collections::BTreeSet<String> {
        let raw = self.get("protect_processes");
        let parts: Vec<String> = match &raw {
            Value::Array(items) => items.iter().map(to_text).collect(),
            other => to_text(other).split(',').map(str::to_string).collect(),
        };
        parts
            .iter()
            .map(|p| p.trim().to_lowercase())
            .filter(|p| !p.is_empty())
            .collect()
    }

    /// Should an acting step wait for a person who just used the input devices?
    /// Python `human_pause()`.
    pub fn human_pause(&self) -> bool {
        self.flag("human_pause")
    }

    /// "pixels" or "norm1000", lowercased. Python `coords()`.
    pub fn coords(&self) -> String {
        self.text("coords").to_lowercase()
    }

    /// Long edge of a default screenshot. Python `shot_long()`.
    pub fn shot_long(&self) -> i64 {
        self.int("shot_long")
    }

    /// Python `auto_shot()`.
    pub fn auto_shot(&self) -> bool {
        self.flag("auto_shot")
    }

    /// Upper bound of a settle wait, seconds. Python `settle_max_s()`.
    pub fn settle_max_s(&self) -> f64 {
        self.float("settle_ms") / 1000.0
    }

    /// Minimum watch time of the trailing settle, seconds, never negative.
    /// Python `settle_floor_s()`.
    pub fn settle_floor_s(&self) -> f64 {
        self.float("settle_floor_ms").max(0.0) / 1000.0
    }

    /// Python `shim_idle_s()`.
    pub fn shim_idle_s(&self) -> i64 {
        self.int("shim_idle_s")
    }

    /// "jpeg" or "png", lowercased. Python `img_format()`.
    pub fn img_format(&self) -> String {
        self.text("img").to_lowercase()
    }

    /// Python `mark()`.
    pub fn mark(&self) -> bool {
        self.flag("mark")
    }

    /// Python `loading_max_s()`.
    pub fn loading_max_s(&self) -> f64 {
        self.float("loading_ms") / 1000.0
    }

    /// Python `clip_max()`.
    pub fn clip_max(&self) -> i64 {
        self.int("clip_max")
    }

    /// Python `display_spec()`: "x,y,w,h" or "".
    pub fn display_spec(&self) -> String {
        self.text("display").trim().to_string()
    }

    /// Python `zoom_ocr()`.
    pub fn zoom_ocr(&self) -> bool {
        self.flag("zoom_ocr")
    }

    /// Python `snap()`.
    pub fn snap(&self) -> bool {
        self.flag("snap")
    }

    /// Python `uia_time_box_s()`.
    pub fn uia_time_box_s(&self) -> f64 {
        self.float("uia_ms") / 1000.0
    }

    /// Python `paste_over()`.
    pub fn paste_over(&self) -> i64 {
        self.int("paste_over")
    }

    /// Has the owner of this machine acknowledged what winhands can do? True only if the marker
    /// file exists, or `WINHANDS_ACKNOWLEDGED` is set to something other than "0" (the CI escape
    /// hatch; an empty value counts as unset). Python `acknowledged()`.
    pub fn acknowledged(&self) -> bool {
        if let Some(raw) = self.env.var("WINHANDS_ACKNOWLEDGED")
            && !raw.is_empty()
        {
            return raw != "0";
        }
        Path::new(&self.paths.ack()).is_file()
    }

    /// The refusal every gated route returns. Names the action and the exact command to run.
    /// Python `ack_refusal(what)`.
    pub fn ack_refusal(&self, what: &str) -> String {
        ack_refusal(what)
    }
}

/// See [`Config::ack_refusal`]. Does not need a config.
pub fn ack_refusal(what: &str) -> String {
    format!(
        "winhands has not been acknowledged on this machine, so it refuses {what}. \
         This is not something an agent can clear for you: the person sitting at this machine \
         has to open a terminal in the winhands folder and run\n    {ACK_COMMAND}\n\
         which prints what this tool can do and then records that they accepted it. Reads \
         (screenshot, list_windows, cursor_position, read) work before that, so you can \
         explain yourself first. Unattended machines may set WINHANDS_ACKNOWLEDGED=1 instead."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::MapEnv;
    use crate::testutil::TempDir;

    /// A config over a fresh temp dir. Ported fixture `_isolated_config`.
    struct Fixture {
        _dir: TempDir,
        cfg: Config,
    }

    impl Fixture {
        fn new(name: &str, env: MapEnv) -> Self {
            let dir = TempDir::new(name);
            Self {
                cfg: Config::new(Paths::in_dir(dir.path()), env),
                _dir: dir,
            }
        }
        fn write_config(&self, text: &str) {
            std::fs::write(self.cfg.paths().config(), text).unwrap();
        }
    }

    fn fx(name: &str) -> Fixture {
        Fixture::new(name, MapEnv::new())
    }

    fn fx_env(name: &str, env: MapEnv) -> Fixture {
        Fixture::new(name, env)
    }

    // ---- ported from tests/test_config_and_idle.py ----

    #[test]
    fn default_used_when_nothing_set() {
        let f = fx("default");
        assert_eq!(f.cfg.port(), 47811);
        assert_eq!(f.cfg.idle_timeout_seconds(), 60 * 60);
    }

    #[test]
    fn config_file_overrides_default() {
        let f = fx("file");
        f.write_config(r#"{"port": 9999, "idle_timeout_seconds": 42}"#);
        assert_eq!(f.cfg.port(), 9999);
        assert_eq!(f.cfg.idle_timeout_seconds(), 42);
    }

    #[test]
    fn env_var_overrides_config_file() {
        let f = fx_env("envfile", MapEnv::new().with("WINHANDS_PORT", "12345"));
        f.write_config(r#"{"port": 9999}"#);
        assert_eq!(f.cfg.port(), 12345);
    }

    #[test]
    fn missing_config_file_is_ignored() {
        let f = fx("missing");
        assert!(!f.cfg.paths().config().exists());
        assert_eq!(f.cfg.get("port"), Value::from(47811));
    }

    #[test]
    fn a_config_file_with_a_byte_order_mark_is_ignored_like_python() {
        // json.loads rejects a leading BOM, so the Python build reads defaults from such a file.
        let f = fx("bom");
        f.write_config("\u{feff}{\"port\": 9999}");
        assert_eq!(f.cfg.port(), 47811);
    }

    #[test]
    fn a_config_file_that_is_not_an_object_is_ignored() {
        let f = fx("not-object");
        f.write_config("[1, 2, 3]");
        assert_eq!(f.cfg.port(), 47811);
    }

    #[test]
    fn malformed_config_file_falls_back_to_default() {
        let f = fx("malformed");
        f.write_config("not json");
        assert_eq!(f.cfg.port(), 47811);
    }

    #[test]
    fn bind_defaults_to_loopback() {
        let f = fx("bind-default");
        assert_eq!(f.cfg.bind_host().unwrap(), "127.0.0.1");
        assert_eq!(default_value("bind"), Some(Value::from("127.0.0.1")));
        assert_eq!(default_value("allow_remote_bind"), Some(Value::from(false)));
    }

    #[test]
    fn a_non_loopback_bind_is_refused_without_the_second_switch() {
        let f = fx_env(
            "bind-refused",
            MapEnv::new().with("WINHANDS_BIND", "0.0.0.0"),
        );
        let err = f.cfg.bind_host().unwrap_err().to_string();
        assert!(
            err.contains("allow_remote_bind") && err.contains("SECURITY.md"),
            "{err}"
        );
    }

    #[test]
    fn allow_remote_bind_alone_changes_nothing() {
        let f = fx_env(
            "bind-alone",
            MapEnv::new().with("WINHANDS_ALLOW_REMOTE_BIND", "1"),
        );
        assert_eq!(f.cfg.bind_host().unwrap(), "127.0.0.1");
    }

    #[test]
    fn both_switches_together_bind_wide() {
        let env = MapEnv::new()
            .with("WINHANDS_BIND", "20.20.20.21")
            .with("WINHANDS_ALLOW_REMOTE_BIND", "1");
        let f = fx_env("bind-both", env);
        assert_eq!(f.cfg.bind_host().unwrap(), "20.20.20.21");
    }

    #[test]
    fn an_empty_bind_falls_back_to_loopback_rather_than_failing() {
        let f = fx_env("bind-blank", MapEnv::new().with("WINHANDS_BIND", "   "));
        assert_eq!(f.cfg.bind_host().unwrap(), "127.0.0.1");
    }

    #[test]
    fn the_config_file_can_set_both_as_well() {
        let f = fx("bind-file");
        f.write_config(r#"{"bind": "::1"}"#);
        assert_eq!(f.cfg.bind_host().unwrap(), "::1"); // still loopback, no second switch needed
        f.write_config(r#"{"bind": "10.0.0.5", "allow_remote_bind": true}"#);
        assert_eq!(f.cfg.bind_host().unwrap(), "10.0.0.5");
    }

    #[test]
    fn human_pause_is_on_by_default_and_opt_out_only() {
        assert!(fx("pause-default").cfg.human_pause());
        let off = fx_env("pause-off", MapEnv::new().with("WINHANDS_HUMAN_PAUSE", "0"));
        assert!(!off.cfg.human_pause());
    }

    // ---- ported from tests/test_20261008_speed_fixes.py and test_shutdown_hygiene.py ----

    #[test]
    fn default_shot_long_is_1024() {
        assert_eq!(default_value("shot_long"), Some(Value::from(1024)));
        assert_eq!(fx("shot-long").cfg.shot_long(), 1024);
    }

    #[test]
    fn settle_floor_config_default_and_override() {
        assert_eq!(default_value("settle_floor_ms"), Some(Value::from(130)));
        assert!((fx("floor").cfg.settle_floor_s() - 0.13).abs() < 1e-9);
        let f = fx_env(
            "floor-env",
            MapEnv::new().with("WINHANDS_SETTLE_FLOOR_MS", "200"),
        );
        assert!((f.cfg.settle_floor_s() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn shim_idle_default_is_five_minutes() {
        assert_eq!(default_value("shim_idle_s"), Some(Value::from(300)));
        assert_eq!(fx("shim-idle").cfg.shim_idle_s(), 300);
    }

    #[test]
    fn daemon_idle_default_is_60_minutes() {
        assert_eq!(default_value("idle_min"), Some(Value::from(60)));
        assert_eq!(fx("idle-60").cfg.idle_timeout_seconds(), 3600);
    }

    #[test]
    fn idle_min_env_sets_and_zero_disables() {
        let five = fx_env("idle-5", MapEnv::new().with("WINHANDS_IDLE_MIN", "5"));
        assert_eq!(five.cfg.idle_timeout_seconds(), 300);
        let zero = fx_env("idle-0", MapEnv::new().with("WINHANDS_IDLE_MIN", "0"));
        assert_eq!(zero.cfg.idle_timeout_seconds(), 0);
    }

    #[test]
    fn explicit_idle_seconds_still_wins() {
        let env = MapEnv::new()
            .with("WINHANDS_IDLE_MIN", "5")
            .with("WINHANDS_IDLE_TIMEOUT_SECONDS", "42");
        assert_eq!(fx_env("idle-explicit", env).cfg.idle_timeout_seconds(), 42);
    }

    // ---- ported from tests/test_20260924_decisions.py (config half) ----

    #[test]
    fn acknowledgement_is_refused_by_default() {
        assert!(!fx("ack-default").cfg.acknowledged());
    }

    #[test]
    fn marker_file_clears_the_gate() {
        let f = fx("ack-marker");
        std::fs::write(f.cfg.paths().ack(), r#"{"acknowledged_at": "now"}"#).unwrap();
        assert!(f.cfg.acknowledged());
    }

    #[test]
    fn env_override_for_ci() {
        let on = fx_env(
            "ack-env-1",
            MapEnv::new().with("WINHANDS_ACKNOWLEDGED", "1"),
        );
        assert!(on.cfg.acknowledged());
        let off = fx_env(
            "ack-env-0",
            MapEnv::new().with("WINHANDS_ACKNOWLEDGED", "0"),
        );
        std::fs::write(off.cfg.paths().ack(), "{}").unwrap();
        assert!(
            !off.cfg.acknowledged(),
            "an explicit 0 beats the marker file, as in Python"
        );
        let empty = fx_env(
            "ack-env-empty",
            MapEnv::new().with("WINHANDS_ACKNOWLEDGED", ""),
        );
        assert!(!empty.cfg.acknowledged(), "an empty value counts as unset");
    }

    // ---- extra coverage of the Python typing rules ----

    #[test]
    fn bool_env_means_not_zero_and_int_env_falls_back_to_raw_text() {
        let env = MapEnv::new()
            .with("WINHANDS_MARK", "0")
            .with("WINHANDS_AUTO_SHOT", "yes")
            .with("WINHANDS_PORT", "abc");
        let f = fx_env("typing", env);
        assert!(!f.cfg.mark());
        assert!(f.cfg.auto_shot());
        assert_eq!(f.cfg.get("port"), Value::from("abc"));
        assert_eq!(
            f.cfg.port(),
            47811,
            "typed accessor falls back to the default"
        );
    }

    #[test]
    fn every_default_in_the_table_matches_config_py() {
        let expect: [(&str, Value); 25] = [
            ("port", Value::from(47811)),
            ("bind", Value::from("127.0.0.1")),
            ("allow_remote_bind", Value::from(false)),
            ("remote_caller", Value::from(false)),
            ("human_pause", Value::from(true)),
            ("idle_min", Value::from(60)),
            ("idle_timeout_seconds", Value::Null),
            ("no_tray", Value::from(false)),
            ("only_apps", Value::from("")),
            ("coords", Value::from("pixels")),
            ("shot_long", Value::from(1024)),
            ("auto_shot", Value::from(true)),
            ("settle_ms", Value::from(1500)),
            ("settle_floor_ms", Value::from(130)),
            ("shim_idle_s", Value::from(300)),
            ("img", Value::from("jpeg")),
            ("mark", Value::from(true)),
            ("loading_ms", Value::from(6000)),
            ("clip_max", Value::from(20000)),
            ("display", Value::from("")),
            ("zoom_ocr", Value::from(true)),
            ("snap", Value::from(false)),
            ("uia_ms", Value::from(300)),
            ("paste_over", Value::from(6)),
            ("protect_processes", Value::from("")),
        ];
        for (key, value) in expect {
            assert_eq!(default_value(key), Some(value), "default of {key}");
            assert!(KEYS.contains(&key), "{key} missing from KEYS");
        }
        assert_eq!(default_value("nope"), None);
    }

    #[test]
    fn remote_caller_is_forced_on_by_a_non_loopback_bind() {
        let f = fx("remote-default");
        assert!(!f.cfg.remote_caller());
        let flag = fx_env(
            "remote-flag",
            MapEnv::new().with("WINHANDS_REMOTE_CALLER", "1"),
        );
        assert!(flag.cfg.remote_caller());
        let wide = fx_env(
            "remote-wide",
            MapEnv::new().with("WINHANDS_BIND", "10.1.1.1"),
        );
        assert!(wide.cfg.bind_is_remote() && wide.cfg.remote_caller());
    }

    #[test]
    fn protect_processes_takes_a_comma_string_or_a_list_and_lowercases() {
        let f = fx_env(
            "protect-env",
            MapEnv::new().with("WINHANDS_PROTECT_PROCESSES", " KeePass.exe, ,Vault.EXE"),
        );
        let got: Vec<String> = f.cfg.protected_processes().into_iter().collect();
        assert_eq!(got, ["keepass.exe", "vault.exe"]);
        let g = fx("protect-file");
        g.write_config(r#"{"protect_processes": ["A.exe", " b.exe "]}"#);
        let got: Vec<String> = g.cfg.protected_processes().into_iter().collect();
        assert_eq!(got, ["a.exe", "b.exe"]);
        assert!(fx("protect-none").cfg.protected_processes().is_empty());
    }

    #[test]
    fn only_apps_default_splits_and_trims() {
        let f = fx_env(
            "only-apps",
            MapEnv::new().with("WINHANDS_ONLY_APPS", "calc.exe, notepad.exe,,"),
        );
        assert_eq!(f.cfg.only_apps_default(), ["calc.exe", "notepad.exe"]);
    }

    #[test]
    fn coords_and_img_are_lowercased_and_file_strings_convert() {
        let f = fx("lowercase");
        f.write_config(
            r#"{"coords": "NORM1000", "img": "PNG", "settle_ms": "2500", "port": "8080"}"#,
        );
        assert_eq!(f.cfg.coords(), "norm1000");
        assert_eq!(f.cfg.img_format(), "png");
        assert!((f.cfg.settle_max_s() - 2.5).abs() < 1e-9);
        assert_eq!(f.cfg.port(), 8080);
    }

    #[test]
    fn negative_settle_floor_is_clamped_to_zero() {
        let f = fx_env(
            "floor-neg",
            MapEnv::new().with("WINHANDS_SETTLE_FLOOR_MS", "-50"),
        );
        assert_eq!(f.cfg.settle_floor_s(), 0.0);
    }

    #[test]
    fn ack_refusal_names_the_action_and_the_command() {
        let msg = ack_refusal("the left_click action");
        assert!(msg.contains(ACK_COMMAND) && msg.contains("the left_click action"));
        assert!(msg.contains("WINHANDS_ACKNOWLEDGED=1"));
    }
}
