//! Environment access behind a trait, so config and path code is testable without touching the
//! process environment (Python tests use `monkeypatch.setenv`; Rust tests build a [`MapEnv`]).

use std::collections::HashMap;

/// A source of environment variables.
pub trait Env: Send + Sync {
    /// The value of `key`, or `None` when it is not set. An empty string is a value.
    fn var(&self, key: &str) -> Option<String>;
}

/// The real process environment. Non-Unicode values read as unset.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEnv;

impl Env for SystemEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// An in-memory environment for tests and for embedding.
#[derive(Debug, Clone, Default)]
pub struct MapEnv(HashMap<String, String>);

impl MapEnv {
    /// An empty environment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder form of [`MapEnv::set`].
    pub fn with(mut self, key: &str, value: &str) -> Self {
        self.set(key, value);
        self
    }

    /// Set or replace a variable.
    pub fn set(&mut self, key: &str, value: &str) {
        self.0.insert(key.to_string(), value.to_string());
    }

    /// Remove a variable.
    pub fn remove(&mut self, key: &str) {
        self.0.remove(key);
    }
}

impl Env for MapEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_env_sets_reads_and_removes() {
        let mut env = MapEnv::new().with("A", "1");
        assert_eq!(env.var("A").as_deref(), Some("1"));
        env.set("A", "");
        assert_eq!(env.var("A").as_deref(), Some(""));
        env.remove("A");
        assert_eq!(env.var("A"), None);
    }
}
