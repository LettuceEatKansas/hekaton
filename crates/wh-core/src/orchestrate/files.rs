//! The filesystem half of the tool: `list_files` (Python `fast.do_files`), `wait_for_file`'s existence
//! check, and the path helpers (`os.path.expandvars`, `expanduser`) the launch and file actions use.
//! Nothing here touches a window, so it is plain `std::fs` and runs the same on every OS.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::Computer;
use super::error::{StepError, StepResult};
use crate::ack::format_utc;
use crate::env::Env;

impl Computer {
    /// A folder listing straight from the filesystem, for the daemon's `files` route and the
    /// `list_files` action: newest first by default. `sort` is `modified`, `name`, `size` or
    /// `created`; `pattern` is a glob relative to the folder. Python `fast.do_files`.
    pub fn list_files(
        &self,
        path: &str,
        sort: &str,
        desc: bool,
        limit: i64,
        pattern: &str,
    ) -> Result<String, StepError> {
        do_files(
            self.cfg.env(),
            &|secs| self.backend.local_utc_offset(secs).unwrap_or(0),
            path,
            sort,
            desc,
            limit,
            pattern,
        )
    }
}

/// `os.path.expandvars` as `ntpath` does it: `%NAME%`, `$NAME` and `${NAME}` are replaced when the
/// variable exists and left alone when it does not; `%%` and `$$` are a literal `%` and `$`.
pub(crate) fn expand_vars(env: &dyn Env, text: &str) -> String {
    let lookup = |name: &str| env.var(name).or_else(|| env.var(&name.to_uppercase()));
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '%' {
            if chars.get(i + 1) == Some(&'%') {
                out.push('%');
                i += 2;
                continue;
            }
            if let Some(len) = chars[i + 1..].iter().position(|&x| x == '%') {
                let name: String = chars[i + 1..i + 1 + len].iter().collect();
                if let Some(value) = lookup(&name) {
                    out.push_str(&value);
                } else {
                    out.push('%');
                    out.push_str(&name);
                    out.push('%');
                }
                i += len + 2;
                continue;
            }
        } else if c == '$' {
            if chars.get(i + 1) == Some(&'$') {
                out.push('$');
                i += 2;
                continue;
            }
            if chars.get(i + 1) == Some(&'{') {
                if let Some(len) = chars[i + 2..].iter().position(|&x| x == '}') {
                    let name: String = chars[i + 2..i + 2 + len].iter().collect();
                    if let Some(value) = lookup(&name) {
                        out.push_str(&value);
                    } else {
                        out.push_str("${");
                        out.push_str(&name);
                        out.push('}');
                    }
                    i += len + 3;
                    continue;
                }
            } else {
                let name: String = chars[i + 1..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphanumeric() || **c == '_' || **c == '-')
                    .collect();
                if !name.is_empty() {
                    let n = name.chars().count();
                    if let Some(value) = lookup(&name) {
                        out.push_str(&value);
                    } else {
                        out.push('$');
                        out.push_str(&name);
                    }
                    i += n + 1;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `Path.expanduser()`: a leading `~` is the home folder.
pub(crate) fn expand_user(env: &dyn Env, text: &str) -> String {
    if text == "~" || text.starts_with("~/") || text.starts_with("~\\") {
        let home = env
            .var("USERPROFILE")
            .or_else(|| env.var("HOME"))
            .unwrap_or_default();
        if !home.is_empty() {
            return format!("{home}{}", &text[1..]);
        }
    }
    text.to_string()
}

pub(crate) fn fmt_size(mut n: f64) -> String {
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if n < 1024.0 || unit == "TB" {
            return if unit == "B" {
                format!("{n:.0}{unit}")
            } else {
                format!("{n:.1}{unit}")
            };
        }
        n /= 1024.0;
    }
    format!("{n:.1}TB")
}

/// A `fnmatch`-style match of one path piece: `*`, `?`, `[abc]`, `[a-z]`, `[!x]`; case-insensitive
/// (Python's `glob` on Windows).
fn wild_match(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len()).any(|k| wild_match(rest, &name[k..])),
        Some(('?', rest)) => !name.is_empty() && wild_match(rest, &name[1..]),
        Some(('[', rest)) => {
            let Some(close) = rest.iter().skip(1).position(|&c| c == ']').map(|p| p + 1) else {
                return !name.is_empty()
                    && name[0].eq_ignore_ascii_case(&'[')
                    && wild_match(rest, &name[1..]);
            };
            let class = &rest[..close];
            let (negate, set) = match class.split_first() {
                Some(('!', s)) => (true, s),
                _ => (false, class),
            };
            let Some(&first) = name.first() else {
                return false;
            };
            let f = first.to_ascii_lowercase();
            let mut hit = false;
            let mut i = 0;
            while i < set.len() {
                if i + 2 < set.len() && set[i + 1] == '-' {
                    if set[i].to_ascii_lowercase() <= f && f <= set[i + 2].to_ascii_lowercase() {
                        hit = true;
                    }
                    i += 3;
                } else {
                    if set[i].to_ascii_lowercase() == f {
                        hit = true;
                    }
                    i += 1;
                }
            }
            hit != negate && wild_match(&rest[close + 1..], &name[1..])
        }
        Some((c, rest)) => {
            !name.is_empty() && c.eq_ignore_ascii_case(&name[0]) && wild_match(rest, &name[1..])
        }
    }
}

fn glob_into(dir: &Path, pieces: &[&str], out: &mut Vec<PathBuf>, depth: u32) {
    let Some((head, tail)) = pieces.split_first() else {
        return;
    };
    if depth > 24 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let entries: Vec<PathBuf> = read.filter_map(Result::ok).map(|e| e.path()).collect();
    if *head == "**" {
        // zero directories: the rest of the pattern right here
        if tail.is_empty() {
            for e in &entries {
                if e.is_dir() {
                    out.push(e.clone());
                }
            }
        } else {
            glob_into(dir, tail, out, depth + 1);
        }
        for e in &entries {
            if e.is_dir() {
                glob_into(e, pieces, out, depth + 1);
            }
        }
        return;
    }
    let pat: Vec<char> = head.chars().collect();
    for e in entries {
        let name: Vec<char> = e
            .file_name()
            .map(|n| n.to_string_lossy().chars().collect())
            .unwrap_or_default();
        if !wild_match(&pat, &name) {
            continue;
        }
        if tail.is_empty() {
            out.push(e);
        } else if e.is_dir() {
            glob_into(&e, tail, out, depth + 1);
        }
    }
}

/// `Path.glob(pattern)`.
fn glob(dir: &Path, pattern: &str) -> Vec<PathBuf> {
    let pieces: Vec<&str> = pattern
        .split(['/', '\\'])
        .filter(|p| !p.is_empty())
        .collect();
    let mut out = Vec::new();
    glob_into(dir, &pieces, &mut out, 0);
    out
}

fn secs_of(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// List a folder straight from the filesystem. Zero clicks, one call: what a person means by "what
/// are the newest files here". `utc_offset(unix_secs)` gives the local zone's offset in seconds (the
/// listing prints local times). Python `fast.do_files`.
pub(crate) fn do_files(
    env: &dyn Env,
    utc_offset: &dyn Fn(i64) -> i32,
    path: &str,
    sort: &str,
    desc: bool,
    limit: i64,
    pattern: &str,
) -> StepResult<String> {
    let expanded = expand_user(env, &expand_vars(env, path));
    let p = PathBuf::from(&expanded);
    if !p.exists() {
        return Err(StepError::target(format!(
            "path not found: {}",
            p.display()
        )));
    }
    if !p.is_dir() {
        return Err(StepError::target(format!("not a folder: {}", p.display())));
    }
    let sort_l = sort.to_lowercase();
    if !["modified", "name", "size", "created"].contains(&sort_l.as_str()) {
        return Err(StepError::value(
            "sort must be one of modified, name, size, created",
        ));
    }
    let entries: Vec<PathBuf> = if pattern.is_empty() {
        std::fs::read_dir(&p)
            .map(|r| r.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default()
    } else {
        glob(&p, pattern)
    };
    enum Key {
        Num(f64),
        Text(String),
    }
    let mut usable: Vec<(Key, PathBuf)> = Vec::new();
    for e in entries {
        let Ok(meta) = std::fs::metadata(&e) else {
            continue;
        };
        let key = match sort_l.as_str() {
            "name" => Key::Text(
                e.file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default(),
            ),
            "size" => Key::Num(meta.len() as f64),
            "created" => match meta.created().or_else(|_| meta.modified()) {
                Ok(t) => Key::Num(secs_of(t) as f64),
                Err(_) => continue,
            },
            _ => match meta.modified() {
                Ok(t) => Key::Num(secs_of(t) as f64),
                Err(_) => continue,
            },
        };
        usable.push((key, e));
    }
    let limit = limit.max(1) as usize;
    usable.sort_by(|a, b| {
        let ord = match (&a.0, &b.0) {
            (Key::Num(x), Key::Num(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
            (Key::Text(x), Key::Text(y)) => x.cmp(y),
            _ => std::cmp::Ordering::Equal,
        };
        if desc { ord.reverse() } else { ord }
    });
    let mut lines = vec![format!(
        "{} ({} items, by {sort}{})",
        p.display(),
        usable.len(),
        if desc { " desc" } else { "" }
    )];
    for (_, e) in usable.iter().take(limit) {
        let Ok(meta) = std::fs::metadata(e) else {
            continue;
        };
        let mtime = meta.modified().map(secs_of).unwrap_or(0);
        let local = mtime + i64::from(utc_offset(mtime));
        let stamp = format_utc(local);
        let when = format!("{} {}", &stamp[..10], &stamp[11..16]);
        let is_dir = meta.is_dir();
        let kind = if is_dir { "dir " } else { "file" };
        let size = if is_dir {
            String::new()
        } else {
            format!("  {}", fmt_size(meta.len() as f64))
        };
        let name = e
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        lines.push(format!("{kind}  {when}  {name}{size}"));
    }
    if usable.len() > limit {
        lines.push(format!(
            "... {} more, raise limit to see them",
            usable.len() - limit
        ));
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::MapEnv;
    use crate::testutil::TempDir;

    #[test]
    fn expand_vars_follows_ntpath() {
        let env = MapEnv::new().with("TEMP", "C:\\t").with("NAME", "x");
        assert_eq!(expand_vars(&env, "%TEMP%\\a"), "C:\\t\\a");
        assert_eq!(expand_vars(&env, "%temp%\\a"), "C:\\t\\a");
        assert_eq!(expand_vars(&env, "%NOPE%\\a"), "%NOPE%\\a");
        assert_eq!(expand_vars(&env, "$NAME/${NAME}/$$"), "x/x/$");
        assert_eq!(expand_vars(&env, "100%% $NOPE"), "100% $NOPE");
        assert_eq!(expand_vars(&env, "plain"), "plain");
    }

    #[test]
    fn sizes_print_like_python() {
        assert_eq!(fmt_size(0.0), "0B");
        assert_eq!(fmt_size(1023.0), "1023B");
        assert_eq!(fmt_size(1536.0), "1.5KB");
        assert_eq!(fmt_size(5.0 * 1024.0 * 1024.0), "5.0MB");
    }

    #[test]
    fn wildcards_match_case_insensitively() {
        let m = |p: &str, n: &str| {
            wild_match(
                &p.chars().collect::<Vec<_>>(),
                &n.chars().collect::<Vec<_>>(),
            )
        };
        assert!(m("*.txt", "A.TXT") && m("a?c", "abc") && m("[a-c]x", "bx") && m("[!a]x", "bx"));
        assert!(!m("*.txt", "a.md") && !m("a?c", "ac") && !m("[!a]x", "ax"));
    }

    #[test]
    fn a_listing_names_sorts_and_filters() {
        let tmp = TempDir::new("files");
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        std::fs::write(tmp.path().join("b.log"), "yy").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let env = MapEnv::new();
        let utc = |_| 0;
        let path = tmp.path().to_string_lossy().to_string();
        let out = do_files(&env, &utc, &path, "name", false, 20, "").unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].ends_with("(3 items, by name)"), "{}", lines[0]);
        assert!(
            lines[1].starts_with("file  ") && lines[1].ends_with("a.txt  1B"),
            "{}",
            lines[1]
        );
        assert!(
            lines[3].starts_with("dir   ") && lines[3].ends_with("sub"),
            "{}",
            lines[3]
        );
        let only = do_files(&env, &utc, &path, "modified", true, 20, "*.txt").unwrap();
        assert!(only.contains("a.txt") && !only.contains("b.log"));
        assert!(
            only.lines()
                .next()
                .unwrap()
                .ends_with("(1 items, by modified desc)")
        );
        let cut = do_files(&env, &utc, &path, "name", false, 1, "").unwrap();
        assert!(
            cut.ends_with("... 2 more, raise limit to see them"),
            "{cut}"
        );
    }

    #[test]
    fn a_missing_path_or_a_file_is_a_target_error() {
        let tmp = TempDir::new("files-err");
        let env = MapEnv::new();
        let file = tmp.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let utc = |_| 0;
        let e = do_files(
            &env,
            &utc,
            &tmp.path().join("nope").to_string_lossy(),
            "name",
            true,
            5,
            "",
        )
        .unwrap_err();
        assert!(matches!(e, StepError::Target(_)) && e.message().starts_with("path not found: "));
        let e = do_files(&env, &utc, &file.to_string_lossy(), "name", true, 5, "").unwrap_err();
        assert!(e.message().starts_with("not a folder: "));
        let e = do_files(
            &env,
            &utc,
            &tmp.path().to_string_lossy(),
            "age",
            true,
            5,
            "",
        )
        .unwrap_err();
        assert!(matches!(e, StepError::Value(_)));
    }

    // Ported from tests/test_20260919_fixes.py::test_files_limit_is_clamped.
    #[test]
    fn files_limit_is_clamped() {
        let tmp = TempDir::new("files-limit");
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        let env = MapEnv::new();
        let out = do_files(
            &env,
            &|_| 0,
            &tmp.path().to_string_lossy(),
            "modified",
            true,
            -5,
            "",
        )
        .unwrap();
        assert!(out.lines().count() >= 2);
    }

    #[test]
    fn local_time_shifts_the_printed_stamp() {
        let tmp = TempDir::new("files-tz");
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        let env = MapEnv::new();
        let path = tmp.path().to_string_lossy().to_string();
        let utc = do_files(&env, &|_| 0, &path, "name", false, 5, "").unwrap();
        let plus = do_files(&env, &|_| 3600 * 5, &path, "name", false, 5, "").unwrap();
        assert_ne!(utc.lines().nth(1), plus.lines().nth(1));
    }
}
