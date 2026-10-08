//! The daemon's bearer token. Port of `config.token()`, `daemon._write_token()` and the
//! `secrets.compare_digest` check in the HTTP handler.
//!
//! The daemon writes a fresh 64 hex character token (32 random bytes) on every start; the shim and
//! the CLI read it back from `<winhands dir>/token`. The file is restricted to the current user
//! (`icacls` on Windows, mode 0600 elsewhere), best effort, exactly as Python does.

use std::io;
use std::path::Path;

use crate::env::Env;
use crate::paths::Paths;

/// The running daemon's token, or "" when there is none. Python `config.token()`.
pub fn read_token(paths: &Paths) -> String {
    std::fs::read_to_string(paths.token())
        .map(|t| t.trim().to_string())
        .unwrap_or_default()
}

/// 32 random bytes as 64 lowercase hex characters (`secrets.token_hex(32)`).
pub fn generate_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| io::Error::other(format!("no system randomness: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Create the winhands directory, write a fresh token and lock the file down. Returns the token.
/// Python `daemon._write_token()`. The lock-down failing is not an error.
pub fn write_token(paths: &Paths, env: &dyn Env) -> io::Result<String> {
    std::fs::create_dir_all(paths.dir())?;
    let token = generate_token()?;
    let file = paths.token();
    std::fs::write(&file, &token)?;
    restrict_to_owner(&file, env);
    Ok(token)
}

/// Constant-time token check for the `X-Winhands-Token` header. Python `secrets.compare_digest`.
///
/// Stricter than Python in one corner: an empty expected token never matches, so a daemon that has
/// not minted a token yet cannot be talked to with an empty header.
pub fn verify(presented: &str, expected: &str) -> bool {
    if expected.is_empty() || presented.len() != expected.len() {
        return false;
    }
    presented
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(windows)]
fn restrict_to_owner(file: &Path, env: &dyn Env) {
    let user = env.var("USERNAME").unwrap_or_default();
    if user.is_empty() {
        return;
    }
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW: the daemon may have no console (winhandsd), and a console program started from
    // a process without one would otherwise open its own window for a moment.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("icacls")
        .arg(file)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{user}:F"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

#[cfg(unix)]
fn restrict_to_owner(file: &Path, _env: &dyn Env) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(any(windows, unix)))]
fn restrict_to_owner(_file: &Path, _env: &dyn Env) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::{MapEnv, SystemEnv};
    use crate::testutil::TempDir;

    #[test]
    fn a_token_is_64_hex_chars_and_never_repeats() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(a, b);
    }

    #[test]
    fn write_then_read_round_trips_and_creates_the_directory() {
        let tmp = TempDir::new("token");
        let paths = Paths::in_dir(tmp.path().join("nested").join("winhands"));
        let written = write_token(&paths, &SystemEnv).unwrap();
        assert_eq!(read_token(&paths), written);
        assert_eq!(
            std::fs::read_to_string(paths.token()).unwrap(),
            written,
            "no trailing newline"
        );
    }

    #[test]
    fn a_missing_token_file_reads_as_empty() {
        let tmp = TempDir::new("token-none");
        assert_eq!(read_token(&Paths::in_dir(tmp.path())), "");
    }

    #[test]
    fn reading_strips_whitespace_like_python() {
        let tmp = TempDir::new("token-ws");
        let paths = Paths::in_dir(tmp.path());
        std::fs::write(paths.token(), "abc123\r\n").unwrap();
        assert_eq!(read_token(&paths), "abc123");
    }

    #[test]
    fn a_new_start_replaces_the_old_token() {
        let tmp = TempDir::new("token-replace");
        let paths = Paths::in_dir(tmp.path());
        let first = write_token(&paths, &MapEnv::new()).unwrap();
        let second = write_token(&paths, &MapEnv::new()).unwrap();
        assert_ne!(first, second);
        assert_eq!(read_token(&paths), second);
    }

    // Ported from tests/test_daemon_auth.py (wrong token 401, correct token 200, no token 401).
    #[test]
    fn verify_accepts_only_the_exact_token() {
        assert!(verify("test-token-12345", "test-token-12345"));
        assert!(!verify("wrong-token", "test-token-12345"));
        assert!(!verify("", "test-token-12345"));
        assert!(!verify("test-token-1234", "test-token-12345"));
        assert!(!verify("test-token-12346", "test-token-12345"));
    }

    #[test]
    fn verify_refuses_when_the_daemon_has_no_token_yet() {
        assert!(!verify("", ""));
    }
}
