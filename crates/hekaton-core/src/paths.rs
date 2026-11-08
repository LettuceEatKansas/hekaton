//! Where hekaton keeps its files. Port of the path constants in `config.py`.
//!
//! On Windows the directory is `%LOCALAPPDATA%\hekaton`. `LOCALAPPDATA` is honoured on
//! every OS when it is set (that is how Python resolves it); when it is not set the default
//! depends on the OS: Windows `<home>\AppData\Local`, macOS `<home>/Library/Application Support`,
//! elsewhere `$XDG_DATA_HOME` or `<home>/.local/share`.

use std::path::{Path, PathBuf};

use crate::env::Env;

/// The directory name under the per-user data folder.
pub const DIR_NAME: &str = "hekaton";

/// The directory name before the tool took the name Hekaton everywhere (2026-11-07). The one place
/// the old name is kept: [`move_old_dir`] carries an existing install's files over.
const OLD_DIR_NAME: &str = "winhands";

/// Move the data directory of an install from before the rename to [`DIR_NAME`], once: only when
/// the old one exists and the new one does not. The config, token, acknowledgement, recipes and
/// action log come along, so nothing has to be set up again. Returns what it did, for the log.
pub fn move_old_dir(env: &dyn Env) -> Option<String> {
    let base = app_data_base(env);
    let (old, new) = (base.join(OLD_DIR_NAME), base.join(DIR_NAME));
    if !old.is_dir() || new.exists() {
        return None;
    }
    Some(match std::fs::rename(&old, &new) {
        Ok(()) => format!("moved {} to {}", old.display(), new.display()),
        Err(e) => format!("could not move {} to {}: {e}", old.display(), new.display()),
    })
}

/// Every file hekaton owns, resolved against one data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    dir: PathBuf,
}

impl Paths {
    /// Resolve the data directory from the environment (see the module docs).
    pub fn from_env(env: &dyn Env) -> Self {
        Self {
            dir: app_data_base(env).join(DIR_NAME),
        }
    }

    /// Use `dir` itself as the hekaton directory (tests, portable installs).
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The hekaton data directory (Python `HEKATON_DIR`).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `config.json` (Python `CONFIG_PATH`).
    pub fn config(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    /// `token`, rewritten by the daemon on each start (Python `TOKEN_PATH`).
    pub fn token(&self) -> PathBuf {
        self.dir.join("token")
    }

    /// `acknowledged.json`, written by `hekaton acknowledge` (Python `ACK_PATH`).
    pub fn ack(&self) -> PathBuf {
        self.dir.join("acknowledged.json")
    }

    /// `protected.txt`, one lowercased process name per line (Python `core.PROTECTED_PATH`).
    pub fn protected(&self) -> PathBuf {
        self.dir.join("protected.txt")
    }

    /// `actions.jsonl`, the audit log (Python `daemon.LOG_PATH`).
    pub fn audit_log(&self) -> PathBuf {
        self.dir.join("actions.jsonl")
    }

    /// `daemon.pid`, written while the daemon runs (Python `daemon.PID_PATH`).
    pub fn pid(&self) -> PathBuf {
        self.dir.join("daemon.pid")
    }

    /// `daemon.log`, where a daemon started without a console writes its messages.
    pub fn daemon_log(&self) -> PathBuf {
        self.dir.join("daemon.log")
    }

    /// `shims/`, one `<pid>.json` per running MCP shim (Python `shim._shims_dir`).
    pub fn shims_dir(&self) -> PathBuf {
        self.dir.join("shims")
    }

    /// `recipes\`, the owner's own recipes (Python `recipes.user_dir()`). They override bundled ones
    /// of the same name.
    pub fn recipes_dir(&self) -> PathBuf {
        self.dir.join("recipes")
    }
}

fn home(env: &dyn Env) -> PathBuf {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env.var(key)
        .filter(|h| !h.is_empty())
        .or_else(|| env.var("HOME").filter(|h| !h.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn app_data_base(env: &dyn Env) -> PathBuf {
    if let Some(local) = env.var("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        return PathBuf::from(local);
    }
    let home = home(env);
    if cfg!(windows) {
        home.join("AppData").join("Local")
    } else if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else {
        match env.var("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            Some(xdg) => PathBuf::from(xdg),
            None => home.join(".local").join("share"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::MapEnv;

    #[test]
    fn an_install_from_before_the_rename_moves_over_once() {
        let base = std::env::temp_dir().join(format!("hekaton-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(OLD_DIR_NAME)).unwrap();
        std::fs::write(base.join(OLD_DIR_NAME).join("config.json"), "{}").unwrap();
        let env = MapEnv::new().with("LOCALAPPDATA", base.to_str().unwrap());
        let said = move_old_dir(&env).expect("moved");
        assert!(said.starts_with("moved"), "{said}");
        assert!(Paths::from_env(&env).config().is_file());
        assert!(!base.join(OLD_DIR_NAME).exists());
        // nothing left to move, and a new directory is never overwritten
        assert_eq!(move_old_dir(&env), None);
        std::fs::create_dir_all(base.join(OLD_DIR_NAME)).unwrap();
        assert_eq!(move_old_dir(&env), None);
        assert!(Paths::from_env(&env).config().is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn localappdata_wins_and_names_every_file() {
        let env = MapEnv::new().with("LOCALAPPDATA", "/data");
        let p = Paths::from_env(&env);
        assert_eq!(p.dir(), Path::new("/data").join("hekaton"));
        assert!(p.config().ends_with("config.json"));
        assert!(p.token().ends_with("token"));
        assert!(p.ack().ends_with("acknowledged.json"));
        assert!(p.protected().ends_with("protected.txt"));
        assert!(p.audit_log().ends_with("actions.jsonl"));
        assert!(p.pid().ends_with("daemon.pid"));
        assert!(p.daemon_log().ends_with("daemon.log"));
        assert!(p.shims_dir().ends_with("shims"));
        assert!(p.recipes_dir().ends_with("recipes"));
    }

    #[test]
    fn falls_back_to_the_home_folder() {
        let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let env = MapEnv::new().with(key, "/home/ada");
        let p = Paths::from_env(&env);
        assert!(p.dir().starts_with("/home/ada"));
        assert!(p.dir().ends_with("hekaton"));
    }
}
