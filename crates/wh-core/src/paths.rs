//! Where winhands keeps its files. Port of the path constants in `winhands/config.py`.
//!
//! On Windows the directory is `%LOCALAPPDATA%\winhands`, exactly as the Python build, so both
//! builds read one config, one token and one acknowledgement marker. `LOCALAPPDATA` is honoured on
//! every OS when it is set (that is how Python resolves it); when it is not set the default
//! depends on the OS: Windows `<home>\AppData\Local`, macOS `<home>/Library/Application Support`,
//! elsewhere `$XDG_DATA_HOME` or `<home>/.local/share`.

use std::path::{Path, PathBuf};

use crate::env::Env;

/// The directory name under the per-user data folder.
pub const DIR_NAME: &str = "winhands";

/// Every file winhands owns, resolved against one data directory.
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

    /// Use `dir` itself as the winhands directory (tests, portable installs).
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The winhands data directory (Python `WINHANDS_DIR`).
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

    /// `acknowledged.json`, written by `winhands acknowledge` (Python `ACK_PATH`).
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
    fn localappdata_wins_and_names_every_file() {
        let env = MapEnv::new().with("LOCALAPPDATA", "/data");
        let p = Paths::from_env(&env);
        assert_eq!(p.dir(), Path::new("/data").join("winhands"));
        assert!(p.config().ends_with("config.json"));
        assert!(p.token().ends_with("token"));
        assert!(p.ack().ends_with("acknowledged.json"));
        assert!(p.protected().ends_with("protected.txt"));
        assert!(p.audit_log().ends_with("actions.jsonl"));
        assert!(p.recipes_dir().ends_with("recipes"));
    }

    #[test]
    fn falls_back_to_the_home_folder() {
        let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let env = MapEnv::new().with(key, "/home/jon");
        let p = Paths::from_env(&env);
        assert!(p.dir().starts_with("/home/jon"));
        assert!(p.dir().ends_with("winhands"));
    }
}
