//! Turning a name a person types ("notepad", "Hermes", "Paint") into something launchable
//! (Python `core.py:resolve_app_exe`, `_app_paths_exe`, `find_app_shortcut`, `find_store_app`,
//! `resolve_shortcut`, `_start_menu_roots`).
//!
//! The order follows the Python launcher: for a plain name a Start Menu shortcut comes first
//! (2026-09-20: PATH used to win, so `open("Hermes")` launched a venv's console tool instead of the
//! desktop app), then a program on PATH or in App Paths, then a packaged (Store) app. Every source is
//! a method of [`Sources`], so the order is tested without a registry or a Start Menu.

use std::path::{Path, PathBuf};

use wh_core::backend::AppTarget;

// ------------------------------------------------------------------------------------ the order

/// Where names can come from. The real implementation reads the machine; tests use a table.
pub(crate) trait Sources {
    /// Does this path exist?
    fn exists(&self, path: &str) -> bool;
    /// `shutil.which`: the name or `name.exe` on PATH, never a `.lnk`.
    fn which(&self, name: &str) -> Option<String>;
    /// The registered path in HKCU then HKLM `App Paths`.
    fn app_paths(&self, name: &str) -> Option<String>;
    /// The best Start Menu shortcut for the name.
    fn shortcut(&self, name: &str) -> Option<String>;
    /// The `.exe` a shortcut points at, when it exists.
    fn shortcut_target(&self, lnk: &str) -> Option<String>;
    /// The app id of a packaged app whose display name matches.
    fn store_app(&self, name: &str) -> Option<String>;
}

/// A name that means "the app called that" rather than a file or a command line:
/// `[\w .+-]+` and no `.exe` suffix (the core's `PLAIN_NAME`).
pub(crate) fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || matches!(c, ' ' | '.' | '+' | '-'))
        && !name.to_lowercase().ends_with(".exe")
}

/// An absolute Windows path: a drive letter and a separator, or a UNC path.
pub(crate) fn is_abs_path(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        || s.starts_with("\\\\")
}

/// Resolve `name` through `src` in the launcher's order.
pub(crate) fn resolve_with(name: &str, src: &dyn Sources) -> Option<AppTarget> {
    let n = name.trim();
    if n.is_empty() {
        return None;
    }
    if is_abs_path(n) && src.exists(n) {
        return Some(if n.to_lowercase().ends_with(".lnk") {
            AppTarget::Shortcut(n.to_string())
        } else {
            AppTarget::Executable(n.to_string())
        });
    }
    let plain = is_plain_name(n);
    if plain && let Some(lnk) = src.shortcut(n) {
        return Some(AppTarget::Shortcut(lnk));
    }
    if let Some(p) = src.which(n) {
        return Some(AppTarget::Executable(p));
    }
    if let Some(p) = src.app_paths(n) {
        return Some(AppTarget::Executable(p));
    }
    if !plain && let Some(lnk) = src.shortcut(n) {
        // Python's last step of resolve_app_exe: the shortcut's own target, when it is a program.
        return Some(match src.shortcut_target(&lnk) {
            Some(exe) => AppTarget::Executable(exe),
            None => AppTarget::Shortcut(lnk),
        });
    }
    if !n.contains(['\\', '/', ':'])
        && let Some(id) = src.store_app(n)
    {
        return Some(AppTarget::Packaged(id));
    }
    None
}

// ------------------------------------------------------------------------------------ PATH

/// Python `shutil.which` for Windows, minus the current directory: `name` with a known extension is
/// tried as is, otherwise with each `PATHEXT` extension. A name with a directory part is tried only
/// at that path.
pub(crate) fn which(
    name: &str,
    path_dirs: &[PathBuf],
    pathext: &[String],
    is_file: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let lower = name.to_lowercase();
    let has_ext = pathext.iter().any(|e| lower.ends_with(&e.to_lowercase()));
    let files: Vec<String> = if has_ext {
        vec![name.to_string()]
    } else {
        pathext.iter().map(|e| format!("{name}{e}")).collect()
    };
    if name.contains(['\\', '/']) {
        return files.iter().map(PathBuf::from).find(|p| is_file(p));
    }
    for dir in path_dirs {
        for f in &files {
            let candidate = dir.join(f);
            if is_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// `PATHEXT` split into extensions, with Windows' default when unset.
pub(crate) fn parse_pathext(raw: Option<&str>) -> Vec<String> {
    let raw = raw
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(".COM;.EXE;.BAT;.CMD");
    raw.split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect()
}

// ------------------------------------------------------------------------------------ App Paths

/// Which registry hive an App Paths lookup reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hive {
    /// `HKEY_CURRENT_USER`.
    CurrentUser,
    /// `HKEY_LOCAL_MACHINE`.
    LocalMachine,
}

/// The subkey under a hive that registers `brave` or `brave.exe`.
pub(crate) fn app_paths_key(name: &str) -> String {
    let file = if name.to_lowercase().ends_with(".exe") {
        name.to_string()
    } else {
        format!("{name}.exe")
    };
    format!(r"Software\Microsoft\Windows\CurrentVersion\App Paths\{file}")
}

/// A registered default value as a path: environment variables expanded, surrounding quotes removed.
pub(crate) fn clean_app_path(raw: &str, expand: &dyn Fn(&str) -> String) -> String {
    expand(raw).trim_matches('"').to_string()
}

/// Registered application path for a name: HKCU first, then HKLM, the first that exists
/// (Python `_app_paths_exe`).
pub(crate) fn app_paths_lookup(
    name: &str,
    read: &dyn Fn(Hive, &str) -> Option<String>,
    expand: &dyn Fn(&str) -> String,
    exists: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let key = app_paths_key(name);
    for hive in [Hive::CurrentUser, Hive::LocalMachine] {
        if let Some(raw) = read(hive, &key) {
            let path = clean_app_path(&raw, expand);
            if exists(&path) {
                return Some(path);
            }
        }
    }
    None
}

// ------------------------------------------------------------------------------------ Start Menu

/// The two Start Menu program folders: all users, then this user (Python `_start_menu_roots`).
pub(crate) fn start_menu_roots(program_data: Option<&str>, app_data: Option<&str>) -> Vec<PathBuf> {
    let tail = Path::new("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs");
    vec![
        PathBuf::from(program_data.unwrap_or("C:/ProgramData")).join(&tail),
        PathBuf::from(app_data.unwrap_or("")).join(&tail),
    ]
}

/// Every `.lnk` under a folder, a bounded depth down.
pub(crate) fn list_shortcuts(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if depth < 8 {
                    walk(&path, depth + 1, out);
                }
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
            {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out
}

/// Exact name beats prefix beats substring, uninstallers are ignored, the shallowest and shortest
/// wins (Python `find_app_shortcut`).
pub(crate) fn pick_shortcut(candidates: &[PathBuf], name: &str) -> Option<PathBuf> {
    let n = name.trim().to_lowercase();
    if n.is_empty() {
        return None;
    }
    let (mut exact, mut starts, mut contains) = (Vec::new(), Vec::new(), Vec::new());
    for p in candidates {
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if stem.contains("uninstall") {
            continue;
        }
        let group = if stem == n {
            &mut exact
        } else if stem.starts_with(&n) {
            &mut starts
        } else if stem.contains(&n) {
            &mut contains
        } else {
            continue;
        };
        group.push((p.components().count(), stem.chars().count(), p));
    }
    for mut group in [exact, starts, contains] {
        if !group.is_empty() {
            group.sort_by_key(|(depth, len, _)| (*depth, *len));
            return Some(group[0].2.clone());
        }
    }
    None
}

// ------------------------------------------------------------------------------------ .lnk files

/// What a shortcut launches.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LnkTarget {
    /// The target path, "" when the shortcut has none stored as a path (a packaged app).
    pub target: String,
    /// The arguments stored in the shortcut.
    pub args: String,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn c_string(b: &[u8], at: usize) -> Option<String> {
    let rest = b.get(at..)?;
    let end = rest.iter().position(|&c| c == 0)?;
    // The local base path is stored in the system ANSI code page; ASCII covers real install paths.
    Some(rest[..end].iter().map(|&c| char::from(c)).collect())
}

/// Read the target and arguments out of a `.lnk` file's bytes ([MS-SHLLINK]): the local base path of
/// the LinkInfo block plus its suffix, and the arguments string. A fallback for when the shell's own
/// parser (`IShellLink`) is not available; it cannot see a target stored only in the item-ID list.
pub(crate) fn parse_lnk(bytes: &[u8]) -> Option<LnkTarget> {
    if u32_at(bytes, 0)? != 0x4C || bytes.len() < 0x4C {
        return None;
    }
    let flags = u32_at(bytes, 0x14)?;
    let has_id_list = flags & 1 != 0;
    let has_link_info = flags & 2 != 0;
    let unicode = flags & 0x80 != 0;
    let mut pos = 0x4C;
    if has_id_list {
        pos += 2 + usize::from(u16_at(bytes, pos)?);
    }
    let mut target = String::new();
    if has_link_info {
        let size = u32_at(bytes, pos)? as usize;
        let info = bytes.get(pos..pos + size)?;
        let info_flags = u32_at(info, 8)?;
        if info_flags & 1 != 0 {
            let base_off = u32_at(info, 16)? as usize;
            let suffix_off = u32_at(info, 24)? as usize;
            target = c_string(info, base_off)?;
            target.push_str(&c_string(info, suffix_off).unwrap_or_default());
        }
        pos += size;
    }
    // StringData: name, relative path, working dir, arguments, icon location, in that order.
    let mut args = String::new();
    for (bit, is_args) in [(0x04, false), (0x08, false), (0x10, false), (0x20, true)] {
        if flags & bit == 0 {
            continue;
        }
        let count = usize::from(u16_at(bytes, pos)?);
        pos += 2;
        let text = if unicode {
            let raw = bytes.get(pos..pos + count * 2)?;
            pos += count * 2;
            let units: Vec<u16> = raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            String::from_utf16_lossy(&units)
        } else {
            let raw = bytes.get(pos..pos + count)?;
            pos += count;
            raw.iter().map(|&c| char::from(c)).collect()
        };
        if is_args {
            args = text;
        }
    }
    Some(LnkTarget { target, args })
}

// ------------------------------------------------------------------------------------ Store apps

/// A Store app list in the order the shell gave it: `(lowercased display name, app id)`. Entries
/// with no `!` in the id are classic programs (resolved elsewhere) and are left out.
pub(crate) fn build_store_index(apps: &[(String, String)]) -> Vec<(String, String)> {
    let mut index: Vec<(String, String)> = Vec::new();
    for (name, id) in apps {
        if !id.contains('!') {
            continue;
        }
        let key = name.trim().to_lowercase();
        let id = id.trim().to_string();
        match index.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = id,
            None => index.push((key, id)),
        }
    }
    index
}

/// Exact display name first, then the first that starts with it (Python `find_store_app`).
pub(crate) fn match_store_app(index: &[(String, String)], name: &str) -> Option<String> {
    let n = name.trim().to_lowercase();
    if n.is_empty() {
        return None;
    }
    index
        .iter()
        .find(|(k, _)| *k == n)
        .or_else(|| index.iter().find(|(k, _)| k.starts_with(&n)))
        .map(|(_, id)| id.clone())
}

// ------------------------------------------------------------------------------------ command lines

/// `"exe" args`, trimmed (Python `f'"{exe}" {args}'.strip()`).
pub(crate) fn command_line(exe: &str, args: &str) -> String {
    format!("\"{exe}\" {args}").trim().to_string()
}

/// The Explorer arguments that open a packaged app (`shell:AppsFolder\<app id>`).
pub(crate) fn apps_folder_arg(app_id: &str) -> String {
    format!("shell:AppsFolder\\{app_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A table-driven machine.
    #[derive(Default)]
    struct Table {
        files: Vec<&'static str>,
        path: HashMap<&'static str, &'static str>,
        app_paths: HashMap<&'static str, &'static str>,
        shortcuts: HashMap<&'static str, &'static str>,
        targets: HashMap<&'static str, &'static str>,
        store: HashMap<&'static str, &'static str>,
    }

    impl Sources for Table {
        fn exists(&self, p: &str) -> bool {
            self.files.contains(&p)
        }
        fn which(&self, n: &str) -> Option<String> {
            self.path.get(n).map(|s| (*s).to_string())
        }
        fn app_paths(&self, n: &str) -> Option<String> {
            self.app_paths.get(n).map(|s| (*s).to_string())
        }
        fn shortcut(&self, n: &str) -> Option<String> {
            self.shortcuts.get(n).map(|s| (*s).to_string())
        }
        fn shortcut_target(&self, l: &str) -> Option<String> {
            self.targets.get(l).map(|s| (*s).to_string())
        }
        fn store_app(&self, n: &str) -> Option<String> {
            self.store.get(n).map(|s| (*s).to_string())
        }
    }

    #[test]
    fn a_plain_name_prefers_the_start_menu_over_path() {
        let mut t = Table::default();
        t.shortcuts.insert("hermes", r"C:\Start\Hermes.lnk");
        t.path.insert("hermes", r"C:\venv\Scripts\Hermes.EXE");
        assert_eq!(
            resolve_with("hermes", &t),
            Some(AppTarget::Shortcut(r"C:\Start\Hermes.lnk".into()))
        );
    }

    #[test]
    fn then_path_then_app_paths_then_the_store() {
        let mut t = Table::default();
        t.path.insert("taskmgr", r"C:\Windows\System32\Taskmgr.exe");
        t.app_paths.insert("brave", r"C:\Brave\brave.exe");
        t.store.insert("paint", "Microsoft.Paint_8wekyb3d8bbwe!App");
        assert_eq!(
            resolve_with("taskmgr", &t),
            Some(AppTarget::Executable(
                r"C:\Windows\System32\Taskmgr.exe".into()
            ))
        );
        assert_eq!(
            resolve_with("brave", &t),
            Some(AppTarget::Executable(r"C:\Brave\brave.exe".into()))
        );
        assert_eq!(
            resolve_with("paint", &t),
            Some(AppTarget::Packaged(
                "Microsoft.Paint_8wekyb3d8bbwe!App".into()
            ))
        );
        assert_eq!(resolve_with("nothing-here", &t), None);
        assert_eq!(resolve_with("   ", &t), None);
    }

    #[test]
    fn a_name_that_is_not_plain_skips_the_shortcut_first_rule() {
        let mut t = Table::default();
        t.shortcuts.insert("code.exe", r"C:\Start\Code.lnk");
        t.targets.insert(r"C:\Start\Code.lnk", r"C:\Code\Code.exe");
        t.path.insert("code.exe", r"C:\Tools\code.exe");
        // PATH wins for "code.exe" because it is not a plain name.
        assert_eq!(
            resolve_with("code.exe", &t),
            Some(AppTarget::Executable(r"C:\Tools\code.exe".into()))
        );
        t.path.clear();
        // With no PATH hit, the shortcut's own target is the answer, else the shortcut itself.
        assert_eq!(
            resolve_with("code.exe", &t),
            Some(AppTarget::Executable(r"C:\Code\Code.exe".into()))
        );
        t.targets.clear();
        assert_eq!(
            resolve_with("code.exe", &t),
            Some(AppTarget::Shortcut(r"C:\Start\Code.lnk".into()))
        );
    }

    #[test]
    fn an_existing_absolute_path_is_taken_as_is() {
        let t = Table {
            files: vec![r"C:\Apps\x.exe", r"C:\Apps\y.lnk"],
            ..Table::default()
        };
        assert_eq!(
            resolve_with(r"C:\Apps\x.exe", &t),
            Some(AppTarget::Executable(r"C:\Apps\x.exe".into()))
        );
        assert_eq!(
            resolve_with(r"C:\Apps\y.lnk", &t),
            Some(AppTarget::Shortcut(r"C:\Apps\y.lnk".into()))
        );
        assert_eq!(resolve_with(r"C:\Apps\gone.exe", &t), None);
    }

    #[test]
    fn paths_and_uris_never_reach_the_store_list() {
        let mut t = Table::default();
        t.store.insert(r"c:\paint", "X!App");
        t.store.insert("ms-settings:", "X!App");
        assert_eq!(resolve_with(r"c:\paint", &t), None);
        assert_eq!(resolve_with("ms-settings:", &t), None);
    }

    #[test]
    fn plain_names_follow_the_core_pattern() {
        for ok in [
            "notepad",
            "Visual Studio Code",
            "notepad++",
            "7-zip",
            "my_app 2",
            "Caf\u{e9}",
        ] {
            assert!(is_plain_name(ok), "{ok}");
        }
        for bad in [
            "",
            "notepad.exe",
            "NOTEPAD.EXE",
            "a\\b",
            "x:y",
            "it's",
            "a/b",
            "a;b",
        ] {
            assert!(!is_plain_name(bad), "{bad}");
        }
    }

    #[test]
    fn absolute_paths_need_a_drive_or_unc_root() {
        assert!(is_abs_path(r"C:\x"));
        assert!(is_abs_path("d:/x"));
        assert!(is_abs_path(r"\\server\share\x.exe"));
        assert!(!is_abs_path("C:x"));
        assert!(!is_abs_path("notepad.exe"));
        assert!(!is_abs_path(r"sub\x.exe"));
    }

    #[test]
    fn which_adds_pathext_unless_the_name_already_has_one() {
        let dirs = vec![PathBuf::from(r"C:\A"), PathBuf::from(r"C:\B")];
        let ext = parse_pathext(Some(".COM;.EXE;.BAT"));
        let present = [r"c:\b\tool.exe", r"c:\a\tool.bat", r"c:\b\run.bat"];
        // Windows paths compare without case.
        let is_file = |p: &Path| present.contains(&p.to_string_lossy().to_lowercase().as_str());
        // Directory by directory, extensions in PATHEXT order: A has only tool.BAT, so it wins over B's .EXE.
        assert_eq!(
            which("tool", &dirs, &ext, &is_file),
            Some(PathBuf::from(r"C:\A\tool.BAT"))
        );
        // A name that already ends in a known extension is tried as is.
        assert_eq!(
            which("tool.exe", &dirs, &ext, &is_file),
            Some(PathBuf::from(r"C:\B\tool.exe"))
        );
        assert_eq!(
            which("run.bat", &dirs, &ext, &is_file),
            Some(PathBuf::from(r"C:\B\run.bat"))
        );
        assert_eq!(which("missing", &dirs, &ext, &is_file), None);
        // A name with a directory part is not searched for on PATH.
        assert_eq!(which(r"sub\tool", &dirs, &ext, &is_file), None);
    }

    #[test]
    fn pathext_has_a_default_and_trims() {
        assert_eq!(parse_pathext(None), [".COM", ".EXE", ".BAT", ".CMD"]);
        assert_eq!(parse_pathext(Some("")), [".COM", ".EXE", ".BAT", ".CMD"]);
        assert_eq!(parse_pathext(Some(" .EXE ; ;.PS1")), [".EXE", ".PS1"]);
    }

    #[test]
    fn app_paths_keys_and_values_are_cleaned() {
        assert_eq!(
            app_paths_key("brave"),
            r"Software\Microsoft\Windows\CurrentVersion\App Paths\brave.exe"
        );
        assert_eq!(
            app_paths_key("Brave.EXE"),
            r"Software\Microsoft\Windows\CurrentVersion\App Paths\Brave.EXE"
        );
        let expand = |s: &str| s.replace("%ProgramFiles%", r"C:\Program Files");
        assert_eq!(
            clean_app_path("\"%ProgramFiles%\\Brave\\brave.exe\"", &expand),
            r"C:\Program Files\Brave\brave.exe"
        );
    }

    #[test]
    fn app_paths_reads_the_user_hive_before_the_machine_hive() {
        let read = |hive: Hive, key: &str| -> Option<String> {
            assert!(key.ends_with(r"App Paths\foo.exe"));
            match hive {
                Hive::CurrentUser => Some(r"C:\Gone\foo.exe".to_string()),
                Hive::LocalMachine => Some("\"C:\\Here\\foo.exe\"".to_string()),
            }
        };
        let expand = |s: &str| s.to_string();
        let exists = |p: &str| p == r"C:\Here\foo.exe";
        // HKCU's entry points at a file that is gone, so HKLM's wins.
        assert_eq!(
            app_paths_lookup("foo", &read, &expand, &exists),
            Some(r"C:\Here\foo.exe".to_string())
        );
        assert_eq!(app_paths_lookup("foo", &read, &expand, &|_| false), None);
        assert_eq!(
            app_paths_lookup("foo", &|_, _| None, &expand, &exists),
            None
        );
    }

    #[test]
    fn start_menu_roots_follow_the_environment() {
        let roots = start_menu_roots(Some(r"D:\PD"), Some(r"C:\Users\x\AppData\Roaming"));
        assert_eq!(
            roots[0],
            Path::new(r"D:\PD").join(r"Microsoft\Windows\Start Menu\Programs")
        );
        assert_eq!(
            roots[1],
            Path::new(r"C:\Users\x\AppData\Roaming").join(r"Microsoft\Windows\Start Menu\Programs")
        );
        assert!(start_menu_roots(None, None)[0].starts_with("C:/ProgramData"));
    }

    fn lnk(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn exact_beats_prefix_beats_substring() {
        let c = lnk(&[
            r"C:\SM\Programs\Discord Canary.lnk",
            r"C:\SM\Programs\Old Discord.lnk",
            r"C:\SM\Programs\Discord Inc\Discord.lnk",
        ]);
        assert_eq!(
            pick_shortcut(&c, "discord"),
            Some(PathBuf::from(r"C:\SM\Programs\Discord Inc\Discord.lnk"))
        );
        let no_exact = lnk(&[r"C:\SM\Old Discord.lnk", r"C:\SM\Discord Canary.lnk"]);
        assert_eq!(
            pick_shortcut(&no_exact, "DISCORD"),
            Some(PathBuf::from(r"C:\SM\Discord Canary.lnk"))
        );
        assert_eq!(
            pick_shortcut(&lnk(&[r"C:\SM\Old Discord.lnk"]), "cord"),
            Some(PathBuf::from(r"C:\SM\Old Discord.lnk"))
        );
        assert_eq!(pick_shortcut(&c, "firefox"), None);
        assert_eq!(pick_shortcut(&c, "  "), None);
    }

    #[test]
    fn uninstallers_are_ignored_and_the_shallowest_wins() {
        let c = lnk(&[
            r"C:\Users\x\AppData\Roaming\SM\Programs\Obsidian.lnk",
            r"C:\ProgramData\SM\Programs\Obsidian.lnk",
            r"C:\ProgramData\SM\Programs\Uninstall Obsidian.lnk",
        ]);
        assert_eq!(
            pick_shortcut(&c, "obsidian"),
            Some(PathBuf::from(r"C:\ProgramData\SM\Programs\Obsidian.lnk"))
        );
        assert_eq!(
            pick_shortcut(&lnk(&[r"C:\SM\Uninstall Foo.lnk"]), "foo"),
            None
        );
    }

    #[test]
    fn shortcuts_are_found_on_disk_recursively() {
        let dir = std::env::temp_dir().join(format!("wh-win-lnk-{}", std::process::id()));
        let nested = dir.join("Vendor").join("Suite");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("Top.LNK"), b"x").unwrap();
        std::fs::write(nested.join("Deep.lnk"), b"x").unwrap();
        std::fs::write(nested.join("readme.txt"), b"x").unwrap();
        let mut found: Vec<String> = list_shortcuts(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(found, ["Deep.lnk", "Top.LNK"]);
        assert!(list_shortcuts(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A minimal shortcut: header, LinkInfo with a local base path, and unicode or ANSI StringData.
    fn lnk_bytes(
        base: &str,
        suffix: &str,
        args: Option<&str>,
        unicode: bool,
        id_list: bool,
    ) -> Vec<u8> {
        let mut flags = 0x02u32; // HasLinkInfo
        if id_list {
            flags |= 0x01;
        }
        if args.is_some() {
            flags |= 0x20;
        }
        if unicode {
            flags |= 0x80;
        }
        let mut out = vec![0u8; 0x4C];
        out[..4].copy_from_slice(&0x4Cu32.to_le_bytes());
        out[0x14..0x18].copy_from_slice(&flags.to_le_bytes());
        if id_list {
            out.extend(6u16.to_le_bytes());
            out.extend([0xAA; 6]);
        }
        let mut info = vec![0u8; 28];
        let base_off = 28u32;
        let suffix_off = base_off + base.len() as u32 + 1;
        info[4..8].copy_from_slice(&28u32.to_le_bytes()); // header size
        info[8..12].copy_from_slice(&1u32.to_le_bytes()); // VolumeIDAndLocalBasePath
        info[16..20].copy_from_slice(&base_off.to_le_bytes());
        info[24..28].copy_from_slice(&suffix_off.to_le_bytes());
        info.extend(base.as_bytes());
        info.push(0);
        info.extend(suffix.as_bytes());
        info.push(0);
        let size = info.len() as u32;
        info[..4].copy_from_slice(&size.to_le_bytes());
        out.extend(info);
        if let Some(a) = args {
            if unicode {
                let units: Vec<u16> = a.encode_utf16().collect();
                out.extend((units.len() as u16).to_le_bytes());
                out.extend(units.iter().flat_map(|u| u.to_le_bytes()));
            } else {
                out.extend((a.len() as u16).to_le_bytes());
                out.extend(a.as_bytes());
            }
        }
        out
    }

    #[test]
    fn a_shortcut_file_yields_its_target_and_arguments() {
        let bytes = lnk_bytes(
            r"C:\Program Files\Brave\Application",
            r"\brave.exe",
            Some("--new-window"),
            true,
            true,
        );
        assert_eq!(
            parse_lnk(&bytes),
            Some(LnkTarget {
                target: r"C:\Program Files\Brave\Application\brave.exe".into(),
                args: "--new-window".into()
            })
        );
        let ansi = lnk_bytes(r"C:\Apps", r"\a.exe", Some("-x 1"), false, false);
        assert_eq!(
            parse_lnk(&ansi),
            Some(LnkTarget {
                target: r"C:\Apps\a.exe".into(),
                args: "-x 1".into()
            })
        );
        let no_args = lnk_bytes(r"C:\Apps", r"\a.exe", None, true, false);
        assert_eq!(parse_lnk(&no_args).unwrap().args, "");
    }

    #[test]
    fn damaged_shortcut_files_are_refused_not_trusted() {
        assert_eq!(parse_lnk(&[]), None);
        assert_eq!(parse_lnk(b"not a shortcut at all, just text of some length........................................................"), None);
        let mut cut = lnk_bytes(r"C:\Apps", r"\a.exe", Some("arg"), true, true);
        cut.truncate(cut.len() - 4);
        assert_eq!(parse_lnk(&cut), None);
        let mut short = vec![0u8; 0x20];
        short[..4].copy_from_slice(&0x4Cu32.to_le_bytes());
        assert_eq!(parse_lnk(&short), None);
    }

    #[test]
    fn the_store_index_keeps_packaged_apps_in_shell_order() {
        let apps: Vec<(String, String)> = [
            ("Paint", "Microsoft.Paint_8wekyb3d8bbwe!App"),
            ("Notepad++", r"C:\Program Files\Notepad++\notepad++.exe"),
            ("Snipping Tool", "Microsoft.ScreenSketch_8wekyb3d8bbwe!App"),
            (
                "Paint 3D",
                "Microsoft.MSPaint_8wekyb3d8bbwe!Microsoft.MSPaint",
            ),
        ]
        .iter()
        .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
        .collect();
        let index = build_store_index(&apps);
        assert_eq!(index.len(), 3);
        assert_eq!(
            match_store_app(&index, "PAINT"),
            Some("Microsoft.Paint_8wekyb3d8bbwe!App".into())
        );
        assert_eq!(
            match_store_app(&index, "snip"),
            Some("Microsoft.ScreenSketch_8wekyb3d8bbwe!App".into())
        );
        // "paint 3" is only a prefix of the third entry.
        assert_eq!(
            match_store_app(&index, "paint 3"),
            Some("Microsoft.MSPaint_8wekyb3d8bbwe!Microsoft.MSPaint".into())
        );
        assert_eq!(match_store_app(&index, "notepad++"), None);
        assert_eq!(match_store_app(&index, ""), None);
    }

    #[test]
    fn a_repeated_display_name_keeps_its_place_and_takes_the_last_id() {
        let apps: Vec<(String, String)> = vec![
            ("Clock".into(), "A!App".into()),
            ("Maps".into(), "M!App".into()),
            ("clock".into(), "B!App".into()),
        ];
        let index = build_store_index(&apps);
        assert_eq!(
            index,
            [
                ("clock".to_string(), "B!App".to_string()),
                ("maps".to_string(), "M!App".to_string())
            ]
        );
    }

    #[test]
    fn command_lines_quote_the_program_and_trim() {
        assert_eq!(
            command_line(r"C:\Program Files\x\x.exe", ""),
            "\"C:\\Program Files\\x\\x.exe\""
        );
        assert_eq!(command_line("a.exe", "--b c"), "\"a.exe\" --b c");
        assert_eq!(
            apps_folder_arg("Microsoft.Paint_8wekyb3d8bbwe!App"),
            "shell:AppsFolder\\Microsoft.Paint_8wekyb3d8bbwe!App"
        );
    }
}
