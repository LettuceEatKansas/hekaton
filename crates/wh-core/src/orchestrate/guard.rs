//! Where input may land. Port of the guards in `computer.py`: `_host_at`, `_refuse_host`,
//! `_check_allowed`, `_check_launch_allowed`, `_window_titled`, `_refuse_explorer_open`,
//! `_check_irreversible`, `_check_wait_path_allowed`, and `core.check_not_protected`.
//!
//! The `only_apps` allowlist is per call. Entries are process names (`notepad.exe`), optionally scoped
//! to a window title (`msedge.exe:inprivate`, or `explorer.exe:re:^folder-\d+` for a regex). The
//! Windows shell surfaces a person uses to get between apps are always allowed alongside an
//! allowlist, except the desktop, the taskbar and the Run dialog, which are a way out of it.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};

use super::Ctx;
use super::args::{Args, is_truthy};
use super::ctx::{is_desktop_or_taskbar, is_file_manager, is_run_dialog};
use super::error::{StepError, StepResult};
use super::files::expand_vars;
use crate::backend::{BackendError, BackendResult, Point, WindowInfo, WindowRef};
use crate::caller::agent_host_refusal;
use crate::textutil::py_repr;

/// Shell surfaces a person uses to get between apps: always allowed alongside an allowlist.
/// 2026-09-21: Start menu and search are NOT here. Under an allowlist, typing into Start launched the
/// Microsoft Store and the model installed Paint. And ApplicationFrameHost is resolved to the Store
/// app it hosts, not allowed wholesale.
const SHELL: [&str; 3] = [
    "explorer.exe",
    "shellexperiencehost.exe",
    "textinputhost.exe",
];

/// Names of the buttons a click should not land on by accident under an allowlist. Whole-word match
/// ("Get" must not fire on "Getting Started").
const IRREVERSIBLE_WORDS: &[&str] = &[
    "send",
    "submit",
    "buy",
    "purchase",
    "pay",
    "place order",
    "get",
    "install",
    "delete",
    "remove",
    "post",
    "publish",
    "uninstall",
];

static IRREVERSIBLE_MATCHERS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    IRREVERSIBLE_WORDS
        .iter()
        .map(|word| {
            (
                *word,
                Regex::new(&format!(r"\b{}\b", regex::escape(word))).expect("irreversible"),
            )
        })
        .collect()
});

/// Friendly launch names and the program they start (`_FRIENDLY_EXE`).
const FRIENDLY_EXE: [(&str, &str); 7] = [
    ("calculator", "calculatorapp.exe"),
    ("calc", "calculatorapp.exe"),
    ("explorer", "explorer.exe"),
    ("notepad", "notepad.exe"),
    ("winver", "winver.exe"),
    ("msedge", "msedge.exe"),
    ("edge", "msedge.exe"),
];

/// The real head of a launch command: a quoted path, an unquoted absolute path that happens to contain
/// spaces ("C:\Program Files (x86)\...\msedge.exe") and ends in .exe, or a bare word.
static EXE_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r#"^(?:"([^"]+)"|([a-zA-Z]:[\\/][^\r\n]+?\.exe)|(\S+))"#)
        .case_insensitive(true)
        .build()
        .expect("EXE_HEAD")
});
static DRIVE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z]:[\\/]").expect("DRIVE_PATH"));
static URI_SCHEME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z][\w+.-]*:").expect("URI_SCHEME"));

/// Lowercase the process-name part of an `only_apps` entry, leave the title-scope part (after the first
/// ':') exactly as given. 2026-09-21: lowercasing the WHOLE entry broke a "re:" regex scope that had any
/// uppercase in it (e.g. "msedge.exe:re:\bWiki\b"), silently turning it into a pattern that could never
/// match. Python `_normalize_only_app`.
pub(crate) fn normalize_only_app(entry: &str) -> String {
    match entry.split_once(':') {
        Some((proc_part, rest)) => format!("{}:{rest}", proc_part.to_lowercase()),
        None => entry.to_lowercase(),
    }
}

/// The last `\` or `/` separated piece of a path.
pub(crate) fn basename(path: &str) -> String {
    path.replace('/', "\\")
        .rsplit('\\')
        .next()
        .unwrap_or("")
        .to_string()
}

/// The program a launch command actually starts, and whatever text follows it.
///
/// 2026-09-21 red-team finding: scanning the WHOLE string for any ".exe"-looking word let
/// "powershell -c start calculatorapp.exe" through, when the process that actually starts is
/// powershell.exe, which can do anything. Only the first token counts. Python `_real_launch_target`.
pub(crate) fn real_launch_target(low: &str) -> (String, String) {
    let Some(caps) = EXE_HEAD.captures(low) else {
        return (String::new(), String::new());
    };
    let head = caps
        .get(1)
        .or_else(|| caps.get(2))
        .or_else(|| caps.get(3))
        .map_or("", |m| m.as_str())
        .trim();
    let end = caps.get(0).map_or(0, |m| m.end());
    let rest = low[end..].trim().to_string();
    let base = basename(head).to_lowercase();
    if base.ends_with(".exe") {
        return (base, rest);
    }
    let friendly = FRIENDLY_EXE
        .iter()
        .find(|(name, _)| *name == base)
        .map_or("", |(_, exe)| *exe);
    (friendly.to_string(), rest)
}

/// Does a scope part match? `re:<pattern>` is a regex searched in the text, case-insensitively;
/// anything else is a plain substring.
fn scope_matches(part: &str, haystack: &str, case_insensitive_regex: bool) -> bool {
    if let Some(pattern) = part.strip_prefix("re:") {
        RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive_regex)
            .build()
            .is_ok_and(|rx| rx.is_match(haystack))
    } else {
        haystack.contains(part)
    }
}

/// Lexically absolute and tidy: `.` dropped, `..` collapsed, lowercased, `/` between pieces. Python
/// `os.path.normcase(os.path.abspath(p))` on Windows.
fn norm_abs(path: &str) -> String {
    let abs = std::path::absolute(Path::new(path)).unwrap_or_else(|_| PathBuf::from(path));
    let mut out: Vec<String> = Vec::new();
    for comp in abs.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str().to_string_lossy().to_lowercase()),
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                if out.len() > 1 || out.last().is_some_and(|l| !l.ends_with(':')) {
                    out.pop();
                }
            }
            Component::Normal(s) => out.push(s.to_string_lossy().to_lowercase()),
        }
    }
    out.join("/")
}

impl Ctx<'_> {
    // ------------------------------------------------------------------ protected processes

    /// Process names the owner listed in `protected.txt` (one per line), lowercased.
    pub(crate) fn load_protected(&self) -> BTreeSet<String> {
        std::fs::read_to_string(self.cfg().paths().protected())
            .map(|text| {
                text.lines()
                    .map(|l| l.trim().to_lowercase())
                    .filter(|l| !l.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Refuse a window whose process the owner protected. Python `core.check_not_protected`.
    pub(crate) fn check_not_protected(&self, w: WindowRef) -> StepResult<()> {
        let Some(name) = self.app_of(w) else {
            return Ok(());
        };
        if self.load_protected().contains(&name) {
            return Err(StepError::permission(format!(
                "process {} is protected, action refused",
                py_repr(&name)
            )));
        }
        Ok(())
    }

    // ------------------------------------------------------------------ the process driving us

    /// Why this window belongs to whoever is driving winhands, or "" (Python
    /// `core.agent_host_refusal`).
    pub(crate) fn host_refusal(&self, w: WindowRef) -> String {
        agent_host_refusal(self.be(), self.cfg(), self.who, w)
    }

    /// Is this a window winhands must not act on (its own driver, or an owner-protected app)?
    pub(crate) fn is_host(&self, w: WindowRef) -> bool {
        !self.host_refusal(w).is_empty()
    }

    /// Refuse a spot that belongs to the process driving winhands (or one of its parents). A terminal
    /// or editor window that is NOT driving winhands is ordinary work and is allowed. Python
    /// `_refuse_host`.
    pub(crate) fn refuse_host(&self, w: WindowRef) -> StepResult<()> {
        let reason = self.host_refusal(w);
        if reason.is_empty() {
            Ok(())
        } else {
            Err(StepError::target(format!("refused: {reason}")))
        }
    }

    /// Refuse input that would land in an agent host window, a protected window, a UAC consent
    /// window, or (under an allowlist) a window the task may not touch. Python `_host_at`.
    pub(crate) fn host_at(&self, p: Point) -> StepResult<()> {
        self.check_cancelled()?;
        let Some(hit) = self.be().window_at_point(p).ok().flatten() else {
            return Ok(());
        };
        let root = self.root_of(hit);
        self.check_not_protected(root)?;
        self.refuse_host(root)?;
        let proc = self.info(root).map(|i| i.app).unwrap_or_default();
        // the owner's hard line
        self.o
            .policy
            .check_consent_window(&proc, "the window under the pointer")?;
        self.check_allowed(root)
    }

    /// Recheck the window immediately before a keyboard injection group.
    pub(crate) fn keyboard_guard(&self) -> BackendResult<()> {
        self.input_guard()?;
        if let Some(window) = self.be().foreground_window()? {
            self.keyboard_window_guard(window)?;
        }
        Ok(())
    }

    pub(crate) fn keyboard_window_guard(&self, window: WindowRef) -> BackendResult<()> {
        self.input_guard()?;
        let check = || -> StepResult<()> {
            let root = self.root_of(window);
            self.check_not_protected(root)?;
            self.refuse_host(root)?;
            self.check_allowed(root)
        };
        check().map_err(|error| BackendError::Denied(error.to_string()))
    }

    /// Bind one text operation to its permitted foreground window and live process owner.
    pub(crate) fn typing_target(&self) -> BackendResult<(WindowRef, u32)> {
        self.input_guard()?;
        let window = self
            .be()
            .foreground_window()?
            .ok_or_else(|| BackendError::Denied("no foreground window to type into".into()))?;
        let pid = self.be().window_pid(window)?;
        if pid == 0 {
            return Err(BackendError::WindowGone);
        }
        self.keyboard_window_guard(window)?;
        self.typing_target_guard((window, pid))?;
        Ok((window, pid))
    }

    /// Every injected group checks identity, even between full policy validations.
    pub(crate) fn typing_target_guard(&self, target: (WindowRef, u32)) -> BackendResult<()> {
        self.input_guard()?;
        if self.be().foreground_window()? != Some(target.0)
            || self.be().window_pid(target.0)? != target.1
        {
            return Err(BackendError::Denied(
                "typing target changed; remaining text was not sent".into(),
            ));
        }
        Ok(())
    }

    /// Recheck the resolved point immediately before a pointer injection group.
    pub(crate) fn pointer_guard(&self, point: Point) -> BackendResult<()> {
        self.input_guard()?;
        self.host_at(point)
            .map_err(|error| BackendError::Denied(error.to_string()))
    }

    /// The same restrictions apply to coordinate and text-targeted clicks.
    pub(crate) fn check_click(&self, point: Point, double: bool, args: &Args) -> StepResult<()> {
        self.input_guard()?;
        self.host_at(point)?;
        if double {
            let hit = self.be().window_at_point(point).ok().flatten();
            self.refuse_explorer_open(hit.map(|window| self.root_of(window)))?;
        }
        self.check_irreversible(point, args)
    }

    // ------------------------------------------------------------------ only_apps

    fn allowed_list(&self) -> String {
        self.o
            .only_apps
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Under `only_apps`, input that would land in any other app's window is refused before it is sent.
    ///
    /// 2026-09-21 overnight eval: local Qwen, asked to count files in Explorer, clicked into the
    /// owner's running game. A popup used to be approved by sharing a pid with an allowed window, which
    /// also approved every OTHER top-level window that process happened to own (the owner's own
    /// Explorer window, sharing explorer.exe's pid with an eval's allowed folder window). Popups are
    /// now approved by walking real window-owner relationships instead. Python `_check_allowed`.
    pub(crate) fn check_allowed(&self, w: WindowRef) -> StepResult<()> {
        self.check_allowed_at(w, 0)
    }

    fn check_allowed_at(&self, w: WindowRef, depth: u32) -> StepResult<()> {
        if self.o.only_apps.is_empty() {
            return Ok(());
        }
        let info: Option<WindowInfo> = self.info(w);
        if let Some(i) = &info
            && (is_desktop_or_taskbar(i) || is_run_dialog(i))
        {
            return Err(StepError::target(format!(
                "refused: that window is the desktop, taskbar or the Run dialog; an allowlist is \
                 active (allowed: {})",
                self.allowed_list()
            )));
        }
        let mut name = self.app_of(w).unwrap_or_else(|| "?".to_string());
        if name == "applicationframehost.exe" {
            let hosted = self
                .be()
                .hosted_app(w)
                .ok()
                .flatten()
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            if !hosted.is_empty() {
                name = hosted;
            }
        }
        if name.is_empty() || self.o.only_apps.contains(&name) {
            return Ok(());
        }
        let file_manager = info.as_ref().is_some_and(is_file_manager);
        if SHELL.contains(&name.as_str()) && !(name == "explorer.exe" && file_manager) {
            // taskbar, desktop, Start: but a File Explorer window (the owner's own folders) needs
            // allowing
            return Ok(());
        }
        // "msedge.exe:InPrivate" allows that app only when its window title contains the part after
        // ':' (so an eval can use a private Edge window but never the owner's own Edge profile).
        let parts: Vec<&str> = self
            .o
            .only_apps
            .iter()
            .filter_map(|a| a.split_once(':'))
            .filter(|(proc_part, _)| *proc_part == name)
            .map(|(_, rest)| rest)
            .collect();
        if !parts.is_empty() {
            let title = info
                .as_ref()
                .map(|i| i.title.to_lowercase())
                .unwrap_or_default();
            if parts.iter().any(|p| scope_matches(p, &title, true)) {
                return Ok(());
            }
        }
        if depth < 8
            && let Ok(chain) = self.be().owner_chain(w)
        {
            for owner in chain {
                if self.check_allowed_at(owner, depth + 1).is_ok() {
                    return Ok(());
                }
            }
        }
        Err(StepError::target(format!(
            "refused: that window belongs to {name}, which this task may not touch (allowed: {})",
            self.allowed_list()
        )))
    }

    /// The first visible window whose title contains `text` and that the allowlist permits.
    /// Python `_window_titled`.
    pub(crate) fn window_titled(&self, text: &str) -> Option<WindowInfo> {
        let want = text.to_lowercase();
        let want = want.trim();
        if want.is_empty() {
            return None;
        }
        self.be()
            .list_windows(false)
            .ok()?
            .into_iter()
            .filter(|w| !w.hidden && w.title.to_lowercase().contains(want))
            .find(|w| self.check_allowed(w.window).is_ok())
    }

    /// Under only_apps, open_app may only start allowed apps, and an app allowed only by a title part
    /// like "msedge.exe:inprivate" must be launched with that flag (2026-09-21: Qwen tried
    /// `msedge.exe --new-window` without `--inprivate`, which would open the owner's own Edge profile).
    /// Python `_check_launch_allowed`.
    pub(crate) fn check_launch_allowed(&self, text: &str) -> StepResult<()> {
        self.check_cancelled()?;
        if self.o.only_apps.is_empty() {
            return Ok(());
        }
        let env = self.cfg().env();
        let low = text.to_lowercase().trim().to_string();
        let (mut exe, rest) = real_launch_target(&low);
        let names: BTreeSet<String> = self
            .o
            .only_apps
            .iter()
            .map(|a| a.split(':').next().unwrap_or("").to_string())
            .collect();
        // A bare app name ("excel") launches the program of the same name; allowed when exactly
        // that program is. A URI or Store id has ':' or '!' and never matches.
        if exe.is_empty()
            && low
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            && names.contains(&format!("{low}.exe"))
        {
            exe = format!("{low}.exe");
        }
        let names_list = names.iter().cloned().collect::<Vec<_>>().join(", ");
        if exe.is_empty() {
            // A name like "EvalPad" may only focus a window that is already open and allowed. A URI or
            // Store id ("ms-paint:", "...!App") would launch anything (2026-09-21: Paint got
            // installed).
            if self.window_titled(&low).is_some() {
                return Ok(());
            }
            return Err(StepError::target(format!(
                "refused: {} is not an app this task may open (allowed: {names_list})",
                py_repr(text)
            )));
        }
        // explorer.exe is in SHELL so its taskbar/desktop windows pass check_allowed, but LAUNCHING it
        // needs an explicit allowlist entry (2026-09-21 live check: "explorer C:/Users/x/Documents"
        // opened the owner's Documents under an allowlist of only Edge and EvalPad).
        if !names.contains(&exe) {
            return Err(StepError::target(format!(
                "refused: this task may not open {exe} (allowed: {names_list})"
            )));
        }
        if exe == "explorer.exe" {
            // 2026-09-21: "explorer ms-settings:" or a shell URI opens a Settings page or arbitrary
            // shell folder (::{CLSID}) outside anything a folder-scoped allowlist meant. explorer.exe
            // may only be launched bare, or pointed at a folder that actually exists.
            let rest_chars = rest.chars().count();
            let orig = text.trim();
            let orig_rest: String = orig
                .chars()
                .skip(orig.chars().count().saturating_sub(rest_chars))
                .collect();
            let arg = orig_rest.trim().trim_matches('"').to_string();
            if !arg.is_empty() {
                let is_drive_path = DRIVE_PATH.is_match(&arg);
                let is_uri = !is_drive_path && URI_SCHEME.is_match(&arg);
                if is_uri || arg.contains("://") || !Path::new(&expand_vars(env, &arg)).is_dir() {
                    return Err(StepError::target(format!(
                        "refused: this task may only open File Explorer with no argument or an \
                         existing folder path, not {}",
                        py_repr(&arg)
                    )));
                }
            }
            let scopes: Vec<String> = self
                .o
                .only_apps
                .iter()
                .filter_map(|a| a.strip_prefix("explorer.exe:"))
                .map(str::to_string)
                .collect();
            if !scopes.is_empty() && !self.o.only_apps.contains("explorer.exe") {
                // Title-scoped ("explorer.exe:folder-"): the window title is the folder name, so the
                // folder opened must be one the scope would allow afterwards.
                let leaf = if arg.is_empty() {
                    String::new()
                } else {
                    let expanded = expand_vars(env, &arg);
                    Path::new(&expanded)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_lowercase())
                        .unwrap_or_default()
                };
                if !scopes.iter().any(|s| scope_matches(s, &leaf, false)) {
                    let listed = scopes
                        .iter()
                        .map(|s| py_repr(s))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let shown = if arg.is_empty() {
                        "the default view".to_string()
                    } else {
                        arg.clone()
                    };
                    return Err(StepError::target(format!(
                        "refused: this task may only open File Explorer on a folder matching \
                         [{listed}], not {}",
                        py_repr(&shown)
                    )));
                }
            }
            return Ok(());
        }
        let scoped = self
            .o
            .only_apps
            .iter()
            .any(|a| a.split_once(':').is_some_and(|(p, _)| p == exe));
        if exe == "msedge.exe" && scoped && !self.o.only_apps.contains(&exe) {
            // 2026-09-21: "--inprivate" as a SUBSTRING matched a flag like "--inprivate-ish" too, and
            // nothing stopped the eval's own --user-data-dir from pointing at the owner's real Edge
            // profile once InPrivate was satisfied. Both a standalone --inprivate token and a
            // --user-data-dir under %TEMP% are required.
            let temp = env
                .var("TEMP")
                .unwrap_or_default()
                .trim()
                .to_lowercase()
                .replace('/', "\\");
            let tokens: Vec<&str> = low.split_whitespace().collect();
            let udd = tokens
                .iter()
                .find(|t| t.starts_with("--user-data-dir="))
                .copied()
                .unwrap_or("");
            let udd_path = udd
                .split_once('=')
                .map(|(_, p)| p.trim_matches('"').replace('/', "\\"))
                .unwrap_or_default();
            if !tokens.contains(&"--inprivate") || udd_path.is_empty() || temp.is_empty() || {
                let parent = norm_abs(&temp);
                let child = norm_abs(&udd_path);
                !(child == parent || child.starts_with(&format!("{parent}/")))
            } {
                return Err(StepError::target(
                    "refused: this task may only open Edge as a private window with its own temp \
                     profile: pass --inprivate and --user-data-dir=<a path under %TEMP%>",
                ));
            }
        }
        Ok(())
    }

    /// Under only_apps, Enter or a double-click on File Explorer's file list (not while typing a name)
    /// opens the selected file in its default app, outside the allowlist. 2026-09-21: twice an eval's
    /// Return opened a test file in the owner's Notepad.
    ///
    /// The caret check looks at `root`'s own thread, not the foreground window's: a double-click
    /// landing on a background Explorer window was judged by some other window's caret. Python
    /// `_refuse_explorer_open`.
    pub(crate) fn refuse_explorer_open(&self, root: Option<WindowRef>) -> StepResult<()> {
        let Some(root) = root else {
            return Ok(());
        };
        if self.o.only_apps.is_empty() {
            return Ok(());
        }
        let is_explorer = self.info(root).as_ref().is_some_and(is_file_manager);
        if !is_explorer || self.be().has_caret(Some(root)).unwrap_or(false) {
            return Ok(());
        }
        if !self.be().popup_menus().unwrap_or_default().is_empty() {
            // Same refusal, clearer reason: the highlighted item may be "Open" (h-contextmenu,
            // 2026-09-21: the model read the file-open message and gave up on the menu).
            return Err(StepError::target(
                "refused: Enter in this menu would run whichever item is highlighted, which can \
                 open the file in another app. Click the menu item instead (right_click lists each \
                 item's click point)",
            ));
        }
        Err(StepError::target(
            "refused: Enter or double-click here would open the selected file in another app. To \
             rename use F2; to read a file use its task's editor",
        ))
    }

    /// The name of the element under a screen point, or "" on any failure or if it did not answer in
    /// time. A hung or heavy app should never turn a click into a multi-second stall. Python
    /// `_irreversible_name_at`.
    pub(crate) fn irreversible_name_at(&self, p: Point) -> String {
        self.be()
            .element_at(p, 0, super::secs(0.35))
            .ok()
            .flatten()
            .map(|hit| hit.element.name)
            .unwrap_or_default()
    }

    /// Under only_apps, a left_click or double_click on something named Send, Buy, Delete and the like
    /// is refused unless the call says `allow_irreversible: true`. 2026-09-21: an eval clicking around
    /// a shopping or settings page has no way back from a wrong click the way it does from a wrong
    /// keystroke; a person double-checks the label before hitting Buy, so the tool does too. Python
    /// `_check_irreversible`.
    pub(crate) fn check_irreversible(&self, p: Point, args: &Args) -> StepResult<()> {
        // strict: the model's own flag is not the owner's approval
        let opted_in =
            is_truthy(args, "allow_irreversible") && self.cfg().profile().honours_call_opt_in();
        if self.o.only_apps.is_empty() || opted_in {
            return Ok(());
        }
        let name = self.irreversible_name_at(p);
        if name.is_empty() {
            return Ok(());
        }
        let low = name.to_lowercase();
        for (word, rx) in IRREVERSIBLE_MATCHERS.iter() {
            if rx.is_match(&low) {
                return Err(StepError::target(format!(
                    "refused: {} looks like an irreversible action ({}); pass \
                     \"allow_irreversible\": true to click it anyway",
                    py_repr(&name),
                    py_repr(word)
                )));
            }
        }
        Ok(())
    }

    /// Under only_apps, wait_for_file and list_files may only look under %TEMP%: a task scoped to a few
    /// apps should not let the model probe arbitrary files anywhere else on disk. No allowlist active
    /// (a person's own interactive use) is unrestricted. Python `_check_wait_path_allowed`.
    pub(crate) fn check_wait_path_allowed(&self, path: &str, what: &str) -> StepResult<()> {
        if self.o.only_apps.is_empty() {
            return Ok(());
        }
        // The file this very call saved through file_dialog (an allowed app's own dialog) may be
        // watched: checking your own save is not probing the disk (2026-10-28 gauntlet L1).
        let p_norm = norm_abs(path);
        if self
            .o
            .saved_paths
            .lock()
            .is_ok_and(|saved| saved.iter().any(|s| norm_abs(s) == p_norm))
        {
            return Ok(());
        }
        let env = self.cfg().env();
        let temp = env
            .var("TEMP")
            .filter(|t| !t.is_empty())
            .or_else(|| env.var("TMP").filter(|t| !t.is_empty()))
            .unwrap_or_default();
        if temp.is_empty() {
            return Err(StepError::target(
                "refused: no %TEMP% set, cannot verify wait_for_file's path is safe",
            ));
        }
        let temp_norm = norm_abs(&temp);
        if !(p_norm == temp_norm || p_norm.starts_with(&format!("{temp_norm}/"))) {
            return Err(StepError::target(format!(
                "refused: under this task's allowlist, {what} paths under {temp} (got {path})"
            )));
        }
        Ok(())
    }
}
