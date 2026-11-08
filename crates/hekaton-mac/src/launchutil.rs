//! Launch helpers with no OS calls: splitting an argument string the way a shell would, where app
//! bundles live, and matching a spoken app name to a bundle file name. Tested on every OS.

/// Folders searched for `<Name>.app`, in order. `home` is the user's home directory.
pub(crate) fn app_search_dirs(home: &str) -> Vec<String> {
    let mut dirs = vec![
        "/Applications".to_string(),
        "/System/Applications".to_string(),
        "/Applications/Utilities".to_string(),
        "/System/Applications/Utilities".to_string(),
        "/System/Library/CoreServices".to_string(),
        "/System/Library/CoreServices/Applications".to_string(),
    ];
    if !home.is_empty() {
        dirs.push(format!("{}/Applications", home.trim_end_matches('/')));
    }
    dirs
}

/// Pick the best match for `wanted` among the file names of a folder: exact name first (case
/// insensitive, with or without `.app`), then a name that starts with it.
pub(crate) fn match_app_name<'a>(entries: &'a [String], wanted: &str) -> Option<&'a str> {
    let wanted = wanted.trim();
    let wanted = wanted
        .strip_suffix(".app")
        .or_else(|| wanted.strip_suffix(".APP"))
        .unwrap_or(wanted)
        .to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    let stem = |e: &str| e.strip_suffix(".app").map(str::to_lowercase);
    entries
        .iter()
        .find(|e| stem(e).is_some_and(|s| s == wanted))
        .or_else(|| {
            entries
                .iter()
                .find(|e| stem(e).is_some_and(|s| s.starts_with(&wanted)))
        })
        .map(String::as_str)
}

/// `com.apple.TextEdit` style reverse-DNS identifier: no spaces or slashes, at least two dots.
pub(crate) fn looks_like_bundle_id(s: &str) -> bool {
    s.matches('.').count() >= 2
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !s.starts_with('.')
        && !s.ends_with('.')
}

/// The arguments of `open` that start an app (`flag` is `-a` or `-b`). A lone URL is something for
/// the app to open (`open -a Safari https://...`, the page opens in that browser); anything else is
/// the app's own command line, after `--args`.
pub(crate) fn open_app_args(flag: &str, app: &str, args: &[String]) -> Vec<String> {
    let mut cmd = vec![flag.to_string(), app.to_string()];
    match args {
        [] => {}
        [url] if looks_like_url(url) => cmd.push(url.clone()),
        _ => {
            cmd.push("--args".to_string());
            cmd.extend(args.iter().cloned());
        }
    }
    cmd
}

/// A URL the system can open (`https://`, `mailto:`, `tel:`, any `scheme://`).
pub(crate) fn looks_like_url(s: &str) -> bool {
    let s = s.trim();
    if s.contains(char::is_whitespace) {
        return false;
    }
    if let Some((scheme, rest)) = s.split_once(':') {
        let scheme_ok = !scheme.is_empty()
            && scheme.len() > 1
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        return scheme_ok
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && (rest.starts_with("//")
                || scheme.starts_with("x-")
                || matches!(scheme, "mailto" | "tel" | "sms" | "facetime"));
    }
    false
}

/// Split an argument string like a POSIX shell: whitespace separates, single and double quotes
/// group, backslash escapes outside single quotes. An unterminated quote runs to the end.
pub(crate) fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, c) if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (Some(q), c) if c == q => quote = None,
            (Some('"') | None, '\\') => match chars.next() {
                Some(n) => {
                    cur.push(n);
                    started = true;
                }
                None => {
                    cur.push('\\');
                    started = true;
                }
            },
            (_, c) => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn search_dirs_cover_system_and_user_apps() {
        let d = app_search_dirs("/Users/jon/");
        assert!(d.contains(&"/Applications".to_string()));
        assert!(d.contains(&"/System/Applications".to_string()));
        assert_eq!(d.last().unwrap(), "/Users/jon/Applications");
        assert!(
            !app_search_dirs("")
                .iter()
                .any(|p| p.starts_with("/Applications/Applications"))
        );
        assert_eq!(app_search_dirs("").len(), 6);
    }

    #[test]
    fn app_names_match_exactly_before_by_prefix() {
        let e = names(&[
            "Safari.app",
            "Safari Technology Preview.app",
            "Notes.app",
            "readme.txt",
        ]);
        assert_eq!(match_app_name(&e, "safari"), Some("Safari.app"));
        assert_eq!(match_app_name(&e, "Safari.app"), Some("Safari.app"));
        assert_eq!(match_app_name(&e, "NOTES"), Some("Notes.app"));
        assert_eq!(
            match_app_name(&e, "safari tech"),
            Some("Safari Technology Preview.app")
        );
        assert_eq!(match_app_name(&e, "readme"), None);
        assert_eq!(match_app_name(&e, "  "), None);
        assert_eq!(match_app_name(&e, "photoshop"), None);
    }

    #[test]
    fn exact_beats_prefix_even_when_the_prefix_comes_first() {
        let e = names(&["Paintbrush.app", "Paint.app"]);
        assert_eq!(match_app_name(&e, "paint"), Some("Paint.app"));
    }

    #[test]
    fn bundle_ids() {
        assert!(looks_like_bundle_id("com.apple.TextEdit"));
        assert!(looks_like_bundle_id("org.mozilla.firefox"));
        assert!(!looks_like_bundle_id("notes.txt"));
        assert!(!looks_like_bundle_id("Safari"));
        assert!(!looks_like_bundle_id("/Applications/a.b.c"));
        assert!(!looks_like_bundle_id("a.b."));
    }

    #[test]
    fn urls() {
        assert!(looks_like_url("https://example.com/a?b=1"));
        assert!(looks_like_url("mailto:jon@example.com"));
        assert!(looks_like_url(
            "x-apple.systempreferences:com.apple.preference.security"
        ));
        assert!(!looks_like_url("10:30"));
        assert!(!looks_like_url("report:final"));
        assert!(!looks_like_url("/Users/jon/file.txt"));
        assert!(!looks_like_url("C:\\Windows"));
        assert!(!looks_like_url("hello world: x"));
        assert!(!looks_like_url("notes"));
    }

    #[test]
    fn a_lone_url_is_opened_by_the_app_and_other_args_go_after_args() {
        let s = |v: &[&str]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            open_app_args(
                "-a",
                "/Applications/Safari.app",
                &s(&["https://example.com"])
            ),
            ["-a", "/Applications/Safari.app", "https://example.com"]
        );
        assert_eq!(
            open_app_args(
                "-b",
                "com.apple.Safari",
                &s(&["--flag", "https://example.com"])
            ),
            [
                "-b",
                "com.apple.Safari",
                "--args",
                "--flag",
                "https://example.com"
            ]
        );
        assert_eq!(
            open_app_args("-a", "/Applications/TextEdit.app", &s(&["notes.txt"])),
            ["-a", "/Applications/TextEdit.app", "--args", "notes.txt"]
        );
        assert_eq!(
            open_app_args("-b", "com.apple.Safari", &[]),
            ["-b", "com.apple.Safari"]
        );
    }

    #[test]
    fn args_split_like_a_shell() {
        assert_eq!(split_args("a b  c"), ["a", "b", "c"]);
        assert_eq!(
            split_args(r#"--name "Jon Smith" 'x y'"#),
            ["--name", "Jon Smith", "x y"]
        );
        assert_eq!(split_args(r"a\ b c"), ["a b", "c"]);
        assert_eq!(split_args(r#""a \"quoted\" word""#), [r#"a "quoted" word"#]);
        assert_eq!(split_args(r"'back\slash'"), [r"back\slash"]);
        assert_eq!(split_args(""), Vec::<String>::new());
        assert_eq!(split_args("   "), Vec::<String>::new());
        assert_eq!(split_args("''"), [""]);
        assert_eq!(
            split_args(r#"open "unterminated rest"#),
            ["open", "unterminated rest"]
        );
        assert_eq!(split_args("ab\\"), ["ab\\"]);
    }
}
