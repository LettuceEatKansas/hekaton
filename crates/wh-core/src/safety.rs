//! Named safety profiles and the gaming lock.
//!
//! A profile changes limits, never protections: every profile keeps `only_apps`, the irreversible
//! button guard, the human-activity pause, the acknowledgement gate, protected processes,
//! `read_only`, stop and cancel, and typing focus binding. `strict` adds checks on top; `hackathon`
//! only shortens waits. The profile comes from the owner's config or environment (`safety_profile`), never
//! from a call, so a model cannot pick a looser one.
//!
//! The gaming lock refuses every input until it is released. The owner turns it on with
//! `hekaton lock` (or `game_lock: "auto"` turns it on whenever a game is running) and off with
//! `hekaton unlock`, which asks at a real terminal: a flag in a call, or a command a model runs
//! through a non-interactive shell, is not the owner saying yes.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use crate::backend::ProcessInfo;

/// A named set of limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Longer waits, smaller clipboard cap, no per-call `allow_destructive`, every acting call
    /// needs an allowlist, and the human-activity pause cannot be turned off.
    Strict,
    /// The built-in defaults.
    Normal,
    /// Shorter waits and a bigger clipboard cap for a supervised demo. Protections unchanged.
    Hackathon,
}

impl Profile {
    /// Every profile name, for messages.
    pub const NAMES: &'static str = "strict, normal, hackathon";

    /// Parse a configured name. Unknown names are `None`; the caller treats that as `strict`, so a
    /// typo never loosens anything.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "strict" => Some(Self::Strict),
            "" | "normal" | "default" => Some(Self::Normal),
            "hackathon" | "fast" => Some(Self::Hackathon),
            _ => None,
        }
    }

    /// The name shown in status.
    pub fn name(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Normal => "normal",
            Self::Hackathon => "hackathon",
        }
    }

    /// The value this profile uses for `key` in place of the built-in default. An explicit
    /// environment or `config.json` value still wins.
    pub fn default_for(self, key: &str) -> Option<Value> {
        Some(match (self, key) {
            (Self::Strict, "settle_ms") => Value::from(2500),
            (Self::Strict, "loading_ms") => Value::from(8000),
            (Self::Strict, "clip_max") => Value::from(5000),
            (Self::Strict, "idle_min") => Value::from(30),
            (Self::Hackathon, "settle_ms") => Value::from(1000),
            (Self::Hackathon, "loading_ms") => Value::from(4000),
            (Self::Hackathon, "clip_max") => Value::from(100_000),
            _ => return None,
        })
    }

    /// May a call's own `allow_destructive` turn the hard line off? Not under `strict`: a flag the
    /// model wrote is not the owner's approval.
    pub fn honours_call_opt_in(self) -> bool {
        self != Self::Strict
    }

    /// Must every acting call carry an `only_apps` allowlist?
    pub fn needs_allowlist(self) -> bool {
        self == Self::Strict
    }
}

/// The marker file that holds the gaming lock on. Its text is the owner's reason.
pub fn lock_path(dir: &Path) -> std::path::PathBuf {
    dir.join("game.lock")
}

/// Turn the gaming lock on, with an optional reason.
pub fn lock(dir: &Path, reason: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(lock_path(dir), reason.trim())
}

/// Turn the gaming lock off. `Ok(false)` when it was not on.
pub fn unlock(dir: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(lock_path(dir)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// The owner's lock and its reason ("" when none was given), or `None` when the lock is off.
pub fn locked(dir: &Path) -> Option<String> {
    let path = lock_path(dir);
    path.exists()
        .then(|| std::fs::read_to_string(path).unwrap_or_default())
}

/// A running game, for `game_lock: "auto"`: any process started from a Steam library
/// (`steamapps\common`) or named in `game_processes`. Returns its exe name.
pub fn running_game(procs: &[ProcessInfo], named: &BTreeSet<String>) -> Option<String> {
    procs.iter().find_map(|p| {
        let name = p.exe_name.to_ascii_lowercase();
        let steam = p.exe_path.as_deref().is_some_and(|path| {
            let path = path.to_ascii_lowercase().replace('/', "\\");
            path.contains("\\steamapps\\common\\")
        });
        (steam || named.contains(&name)).then(|| p.exe_name.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn proc(name: &str, path: Option<&str>) -> ProcessInfo {
        ProcessInfo {
            pid: 1,
            parent_pid: 0,
            exe_name: name.into(),
            exe_path: path.map(Into::into),
            started_at: None,
            elevated: false,
        }
    }

    #[test]
    fn an_unknown_profile_name_is_not_a_profile() {
        assert_eq!(Profile::from_name("Strict"), Some(Profile::Strict));
        assert_eq!(Profile::from_name(""), Some(Profile::Normal));
        assert_eq!(Profile::from_name("hackathon"), Some(Profile::Hackathon));
        assert_eq!(Profile::from_name("yolo"), None);
    }

    #[test]
    fn profiles_change_limits_and_leave_other_keys_alone() {
        assert_eq!(
            Profile::Strict.default_for("settle_ms"),
            Some(Value::from(2500))
        );
        assert_eq!(
            Profile::Hackathon.default_for("clip_max"),
            Some(Value::from(100_000))
        );
        assert_eq!(Profile::Normal.default_for("settle_ms"), None);
        for p in [Profile::Strict, Profile::Normal, Profile::Hackathon] {
            for key in ["human_pause", "read_only", "only_apps", "protect_processes"] {
                assert_eq!(p.default_for(key), None, "{} touches {key}", p.name());
            }
        }
        assert!(!Profile::Strict.honours_call_opt_in());
        assert!(Profile::Hackathon.honours_call_opt_in());
    }

    #[test]
    fn the_lock_file_turns_on_and_off() {
        let tmp = TempDir::new("lock");
        assert_eq!(locked(tmp.path()), None);
        lock(tmp.path(), "playing Deadlock\n").unwrap();
        assert_eq!(locked(tmp.path()).as_deref(), Some("playing Deadlock"));
        assert!(unlock(tmp.path()).unwrap());
        assert!(!unlock(tmp.path()).unwrap());
        assert_eq!(locked(tmp.path()), None);
    }

    #[test]
    fn a_steam_game_or_a_named_exe_is_a_game() {
        let none = BTreeSet::new();
        let steam = proc(
            "Deadlock.exe",
            Some(r"D:\SteamLibrary\steamapps\common\Deadlock\game\bin\win64\deadlock.exe"),
        );
        let notepad = proc("notepad.exe", Some(r"C:\Windows\System32\notepad.exe"));
        assert_eq!(running_game(std::slice::from_ref(&notepad), &none), None);
        assert_eq!(
            running_game(&[notepad.clone(), steam], &none).as_deref(),
            Some("Deadlock.exe")
        );
        let mac = proc(
            "Hades",
            Some("/Users/x/Library/Application Support/Steam/steamapps/common/Hades/Hades.app/Contents/MacOS/Hades"),
        );
        assert_eq!(running_game(&[mac], &none).as_deref(), Some("Hades"), "a Mac Steam library");
        let named = BTreeSet::from(["javaw.exe".to_string()]);
        let mc = proc("javaw.exe", None);
        assert_eq!(running_game(&[mc], &named).as_deref(), Some("javaw.exe"));
    }
}
