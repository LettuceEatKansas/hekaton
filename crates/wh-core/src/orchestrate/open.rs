//! Starting and focusing apps, and the Save As / Open dialog. Port of `core.do_open` (the decision
//! tree; what a name resolves to is the backend's [`Backend::resolve_app`]) and `core._new_editor_tab`,
//! `core._settled`, `core._open_folder_path`, plus `computer._file_dialog` and
//! `computer._repair_escapes`.
//!
//! `resolve_app` must answer in the order the Python launcher tried things: for a plain name a Start
//! Menu shortcut comes first (2026-09-20: `PATH` used to win, so `open("Hermes")` launched a venv's
//! Hermes.EXE console tool instead of the desktop app), then a program on `PATH` or in App Paths, then
//! a Store (packaged) app.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};

use super::Ctx;
use super::error::{StepError, StepResult};
use super::files::expand_vars;
use super::guard::basename;
use super::refs::WindowLine;
use crate::backend::{
    AppTarget, BackendError, BackendResult, ElementAction, LaunchSpec, Point, WindowRef,
};
use crate::textutil::py_repr;

/// Editors that restore their last session on launch and keep tabs (`_TABBED_EDITORS`).
const TABBED_EDITORS: [&str; 3] = ["notepad", "notepad++", "wordpad"];

/// Bare names and the program they start (`known` in `do_open`).
const KNOWN: [(&str, &str); 6] = [
    ("notepad", "notepad.exe"),
    ("calc", "calc.exe"),
    ("calculator", "calc.exe"),
    ("explorer", "explorer.exe"),
    ("cmd", "cmd.exe"),
    ("paint", "mspaint.exe"),
];

/// Title words a Store app shows instead of its program name (`TITLE_ALIASES`).
const TITLE_ALIASES: [(&str, &str); 3] = [
    ("calc", "calculator"),
    ("mspaint", "paint"),
    ("cmd", "command prompt"),
];

static WINDOW_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[wW]\d+$").expect("WINDOW_REF"));
static SPLIT_ARGS: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r#"^(?:"([^"]+)"|(\S+?\.exe)|(\S+))\s+(.+)$"#)
        .case_insensitive(true)
        .build()
        .expect("SPLIT_ARGS")
});
static SPACED_EXE: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r"^([a-zA-Z]:[\\/].+?\.exe)\s+(.+)$")
        .case_insensitive(true)
        .build()
        .expect("SPACED_EXE")
});
static PLAIN_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w .+-]+$").expect("PLAIN_NAME"));
static EXE_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r#"^"?([a-zA-Z]:[\\/].+?\.exe)"#)
        .case_insensitive(true)
        .build()
        .expect("EXE_PREFIX")
});
static DRIVE_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z]:[\\/]").expect("DRIVE_START"));

fn known_exe(name: &str) -> Option<&'static str> {
    KNOWN.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)
}

/// Windows' `isabs`: a drive and a separator, or a leading separator.
fn is_abs(path: &str) -> bool {
    DRIVE_START.is_match(path) || path.starts_with(['/', '\\'])
}

fn control_escape(ch: char) -> Option<&'static str> {
    Some(match ch {
        '\t' => "\\t",
        '\u{8}' => "\\b",
        '\u{c}' => "\\f",
        '\r' => "\\r",
        '\n' => "\\n",
        '\u{b}' => "\\v",
        '\u{7}' => "\\a",
        _ => return None,
    })
}

/// A model writing a Windows path in JSON without doubling its backslashes sends tab and backspace
/// characters ("C:<TAB>Users<BS>in"). 2026-09-21 eval: four open_app calls failed that way. No app name
/// or path contains control characters, so turn them back.
///
/// Two readings exist: "\tools" typed as TAB+"ools" (put back "\t"), or a TAB used where a lone "\"
/// belongs ("C:<TAB>Program Files"). Keep whichever names an .exe that exists. Python
/// `_repair_escapes`; `is_file` answers whether a path is an existing file.
pub(crate) fn repair_escapes(text: &str, is_file: &dyn Fn(&str) -> bool) -> String {
    if !text.chars().any(|c| control_escape(c).is_some()) {
        return text.to_string();
    }
    let escaped: String = text
        .chars()
        .map(|c| control_escape(c).map_or_else(|| c.to_string(), str::to_string))
        .collect();
    let lone: String = text
        .chars()
        .map(|c| if control_escape(c).is_some() { '\\' } else { c })
        .collect();
    for cand in [&escaped, &lone] {
        if let Some(m) = EXE_PREFIX.captures(cand)
            && is_file(&m[1])
        {
            return cand.clone();
        }
    }
    escaped
}

/// Does a resolved name count as a program for [`split_launch`]? An executable does, and so does a
/// Start Menu shortcut: Python's resolver fell through to the shortcut's target, so
/// `brave --new-window url` split even when `brave` is only a shortcut here. A packaged app or
/// anything the shell opens does not (its name can hold spaces that are not arguments).
pub(crate) fn is_launchable(target: Option<&AppTarget>) -> bool {
    matches!(
        target,
        Some(AppTarget::Executable(_) | AppTarget::Shortcut(_))
    )
}

/// Split a launch string into the program and its arguments (`do_open`: "allow an argument after the
/// app, e.g. `explorer.exe C:\path`"). `knows` says a name is a program on `PATH`, an App Paths entry
/// or otherwise launchable; `exists` and `is_file` are filesystem probes. Returns `(head, args)`.
pub(crate) fn split_launch(
    app: &str,
    knows: &dyn Fn(&str) -> bool,
    exists: &dyn Fn(&str) -> bool,
    is_file: &dyn Fn(&str) -> bool,
) -> (String, String) {
    let lower = app.to_lowercase();
    if known_exe(&lower).is_some() || exists(app) {
        return (app.to_string(), String::new());
    }
    let mut split: Option<(String, String)> = SPLIT_ARGS.captures(app).map(|m| {
        let head = m
            .get(1)
            .or_else(|| m.get(2))
            .or_else(|| m.get(3))
            .map_or("", |x| x.as_str());
        (head.to_string(), m[4].trim().to_string())
    });
    // 2026-09-21 eval: models drop the quotes around "C:\Program Files (x86)\...\msedge.exe" and the
    // first space split it into "C:\Program". An unquoted absolute path to an existing .exe wins over
    // that split.
    if let Some(m) = SPACED_EXE.captures(app)
        && is_file(&m[1])
    {
        split = Some((m[1].to_string(), m[2].trim().to_string()));
    }
    if let Some((candidate, rest)) = split {
        let cand_exe = known_exe(&candidate.to_lowercase())
            .unwrap_or(&candidate)
            .to_string();
        if is_abs(&cand_exe) || cand_exe.to_lowercase().ends_with(".exe") || knows(&cand_exe) {
            return (candidate, rest);
        }
    }
    (app.to_string(), String::new())
}

/// Does a (lowercased) window title read like a new, unsaved document? `*untitled` does not: the star
/// marks a document that has been edited.
fn is_blank_title(title: &str) -> bool {
    let t = title.trim();
    t.starts_with("untitled") || t.starts_with("new ") || t.starts_with("document -")
}

fn removesuffix_exe(s: &str) -> String {
    let lower = s.to_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

impl Ctx<'_> {
    /// `core._guard_input`: the window must still be usable and still in front.
    fn guard_input(&self, w: WindowRef) -> StepResult<()> {
        self.check_not_protected(w)?;
        let Some(info) = self.info(w) else {
            return Err(StepError::target("window unavailable; refresh windows"));
        };
        if info.minimized || info.hidden {
            return Err(StepError::target(
                "window hidden or minimized; open it before continuing",
            ));
        }
        let front = self.be().foreground_window().ok().flatten();
        if !front.is_some_and(|f| self.be().same_window(f, w)) {
            return Err(StepError::target(
                "focus changed; retry after refreshing requested window",
            ));
        }
        Ok(())
    }

    /// Run a backend input call with a guard that fails with a [`StepError`]; the error comes back
    /// with its class intact instead of as an opaque OS failure.
    pub(crate) fn guarded<T>(
        &self,
        guard: impl Fn() -> StepResult<()>,
        run: impl FnOnce(&dyn Fn() -> BackendResult<()>) -> BackendResult<T>,
    ) -> StepResult<T> {
        let captured: RefCell<Option<StepError>> = RefCell::new(None);
        let wrapped = || {
            guard().map_err(|e| {
                let msg = e.message().to_string();
                *captured.borrow_mut() = Some(e);
                BackendError::Os(msg)
            })
        };
        match run(&wrapped) {
            Ok(v) => Ok(v),
            Err(be) => Err(captured.into_inner().unwrap_or_else(|| be.into())),
        }
    }

    fn window_line(&self, w: WindowRef) -> Option<WindowLine> {
        self.c
            .window_refs()
            .list(false)
            .ok()?
            .into_iter()
            .find(|l| l.window == w)
    }

    /// ctrl+n in a running tabbed editor, then report the title the new tab shows. Python
    /// `_new_editor_tab`.
    fn new_editor_tab(&self, w: &WindowLine) -> StepResult<String> {
        let before = w.title.clone();
        let target = w.window;
        let guard = || self.guard_input(target);
        guard()?;
        let keys = crate::keys::parse_key_combo(
            match crate::keys::Platform::current() {
                crate::keys::Platform::Windows => "ctrl+n",
                crate::keys::Platform::Mac => "cmd+n",
            },
            crate::keys::Platform::current(),
        )?;
        self.guarded(guard, |g| self.be().key_chord(&keys, 1, g))?;
        let mut title = before.clone();
        let deadline = self.now() + 1.5;
        while self.now() < deadline {
            self.sleep(0.1);
            let Some(cur) = self.window_line(target) else {
                break;
            };
            title = cur.title;
            if title != before {
                break;
            }
        }
        if title == before {
            return Ok(format!(
                "ok focused w{} \"{title}\" but no new tab appeared; the text you type next goes into \
                 that document. Press ctrl+n yourself or pass reuse=false",
                w.reference
            ));
        }
        Ok(format!(
            "ok new blank tab in w{} \"{title}\" (was showing \"{before}\")",
            w.reference
        ))
    }

    /// Does the window show a document that is empty and has no file name yet? The title must read
    /// like a new document (`Untitled - Notepad`, `New 1`, `Document - WordPad`; a leading `*` means it
    /// has been edited, so it is not blank) and, where the app exposes its text, the text must be empty.
    /// An empty file that already has a name is NOT blank: typing into it would change that file.
    pub(crate) fn is_blank_document(&self, w: WindowRef) -> bool {
        if !is_blank_title(&self.title_of(w)) {
            return false;
        }
        match self.be().document_text(w, 200) {
            Ok(Some(text)) => text.trim().is_empty(),
            // the app exposes no text: the title is all there is to go on
            _ => true,
        }
    }

    /// Make sure the window in front shows a blank document, or fail before anything is typed.
    ///
    /// 2026-10-08: the `note` recipe used to press Ctrl+N and expect a title with "Untitled" in it; it
    /// stopped (safely) whenever Notepad had restored old tabs and the title did not change. Now the
    /// document itself is checked, whether the app showed one already or this makes one with the
    /// app's own new-document shortcut: never an existing document, never a blind keystroke.
    pub(crate) fn ensure_blank_document(&self) -> StepResult<String> {
        let Some(front) = self.fg_root() else {
            return Err(StepError::target(
                "no window is in front to check for a blank document; nothing was typed",
            ));
        };
        // the refusals typing into this window would have
        self.check_not_protected(front)?;
        self.refuse_host(front)?;
        self.check_allowed(front)?;
        if self.is_blank_document(front) {
            return Ok("blank document confirmed".to_string());
        }
        let seen = self.title_of(front);
        let keys = crate::keys::parse_key_combo(
            match crate::keys::Platform::current() {
                crate::keys::Platform::Windows => "ctrl+n",
                crate::keys::Platform::Mac => "cmd+n",
            },
            crate::keys::Platform::current(),
        )?;
        self.guarded(
            || self.guard_input(front),
            |g| self.be().key_chord(&keys, 1, g),
        )?;
        self.mark_ours();
        // the new document may be a new tab or a new window: judge whatever is in front now
        let (blank, _) = self.poll(
            || {
                let now = self.fg_root()?;
                (self.is_blank_document(now) && self.check_allowed(now).is_ok()).then_some(())
            },
            2.0,
            0.1,
        );
        if blank.is_some() {
            return Ok("opened a new blank document (verified)".to_string());
        }
        Err(StepError::target(format!(
            "refused: no blank document could be made ({} is showing {}); nothing was typed. Open a \
             new document yourself, then type",
            self.fg_root()
                .map_or_else(|| "no window".to_string(), |w| self.title_of(w)),
            py_repr(&seen)
        )))
    }

    /// Wait until the window title stops changing (session restore renames it as tabs load).
    fn settled(&self, w: WindowLine) -> WindowLine {
        let (timeout, quiet) = (2.0, 0.4);
        let mut last = w;
        let mut last_change = self.now();
        let deadline = self.now() + timeout;
        while self.now() < deadline {
            self.sleep(0.1);
            let Some(cur) = self.window_line(last.window) else {
                break;
            };
            if cur.title != last.title {
                last = cur;
                last_change = self.now();
            } else if self.now() - last_change >= quiet {
                break;
            }
        }
        last
    }

    /// Open a folder in Explorer. Focus an Explorer window already showing it when there is one.
    fn open_folder_path(&self, path: &str, wait: f64) -> StepResult<String> {
        let name = {
            let b = basename(path.trim_end_matches(['/', '\\']));
            if b.is_empty() { path.to_string() } else { b }
        };
        for w in self.c.window_refs().list(false)? {
            if w.process.to_lowercase() == "explorer.exe"
                && w.title.to_lowercase().starts_with(&name.to_lowercase())
            {
                self.check_not_protected(w.window)?;
                self.be().bring_to_foreground(w.window)?;
                return Ok(format!(
                    "ok focused w{} \"{}\" (folder already open)",
                    w.reference, w.title
                ));
            }
        }
        self.do_open(&format!("explorer.exe \"{path}\""), wait, true, false)
    }

    /// Focus only on an exact ref (`wN`) or exact full-title match. Otherwise (default `new`) launch a
    /// fresh instance and target its new window. `new=false` also allows a substring match against
    /// existing windows before launching (opt-in, since that can grab an unrelated pre-existing
    /// window). Python `core.do_open`.
    pub(crate) fn do_open(
        &self,
        app: &str,
        wait: f64,
        new: bool,
        reuse: bool,
    ) -> StepResult<String> {
        // the owner's hard line
        self.o.policy.check_command(app, "the launch string")?;
        let wins = self.c.window_refs().list(false)?;
        let app_stripped = app.trim().to_string();
        let focus = |w: &WindowLine| -> StepResult<()> {
            self.check_not_protected(w.window)?;
            self.be().bring_to_foreground(w.window)?;
            Ok(())
        };

        if WINDOW_REF.is_match(&app_stripped) {
            let idx: u32 = app_stripped[1..].parse().unwrap_or(u32::MAX);
            for w in &wins {
                if w.reference == idx {
                    focus(w)?;
                    return Ok(format!("ok focused w{} \"{}\"", w.reference, w.title));
                }
            }
            return Err(StepError::target(format!("no window ref {}", py_repr(app))));
        }

        for w in &wins {
            if w.title.trim().to_lowercase() == app_stripped.to_lowercase() {
                focus(w)?;
                return Ok(format!("ok focused w{} \"{}\"", w.reference, w.title));
            }
        }

        if !new {
            for w in &wins {
                if w.title
                    .to_lowercase()
                    .contains(&app_stripped.to_lowercase())
                {
                    focus(w)?;
                    return Ok(format!("ok focused w{} \"{}\"", w.reference, w.title));
                }
            }
        }

        let before_pids: BTreeSet<u32> = wins.iter().map(|w| w.pid).collect();
        let before_windows: BTreeSet<WindowRef> = wins.iter().map(|w| w.window).collect();

        // 2026-09-19: focus a running instance before launching another. Launching Notepad while it
        // runs restores its whole old session (every tab, unsaved ones included) and the new window
        // can vanish again, so a following "type" lands somewhere unknown. Only for a bare app name:
        // "explorer C:\path" still launches because the argument matters. reuse=False forces a
        // launch.
        if reuse && !app_stripped.contains(char::is_whitespace) {
            let lower = app_stripped.to_lowercase();
            let stem = removesuffix_exe(&basename(known_exe(&lower).unwrap_or(&app_stripped)));
            let alias = TITLE_ALIASES
                .iter()
                .find(|(k, _)| *k == stem)
                .map_or(stem.clone(), |(_, v)| (*v).to_string());
            let title_is = |w: &WindowLine| {
                let t = w.title.to_lowercase();
                [&stem, &alias].iter().any(|n| {
                    t == **n || t.ends_with(&format!(" - {n}")) || t.ends_with(&format!(" | {n}"))
                })
            };
            // Store apps (Calculator) run under ApplicationFrameHost, so match the title too.
            let same: Vec<&WindowLine> = wins
                .iter()
                .filter(|w| !w.cloaked && (removesuffix_exe(&w.process) == stem || title_is(w)))
                .collect();
            if !same.is_empty() {
                let w = same.iter().find(|w| w.focused).unwrap_or(&same[0]);
                focus(w)?;
                if new && TABBED_EDITORS.contains(&stem.as_str()) {
                    // 2026-09-20: open('Notepad', new=True) focused a running Notepad with 15 tabs and
                    // the next type() landed in whatever tab was showing. A caller asking for new
                    // wants a blank document, so make one in the running instance and say which tab
                    // now has focus.
                    return self.new_editor_tab(w);
                }
                return Ok(format!(
                    "ok focused w{} \"{}\" (already running; pass reuse=false to launch another \
                     instance)",
                    w.reference, w.title
                ));
            }
        }

        // 2026-09-17: allow an argument after the app, e.g. "explorer.exe C:\path" or
        // "explorer C:\path". Real agents need one call to open a folder or a file.
        let env = self.cfg().env();
        let (head, launch_args) = split_launch(
            &app_stripped,
            &|name| is_launchable(self.be().resolve_app(name).ok().flatten().as_ref()),
            &|p| Path::new(p).exists(),
            &|p| Path::new(p).is_file(),
        );
        let app_stripped = head;
        let lower = app_stripped.to_lowercase();
        let from_known = known_exe(&lower);
        let mut exe = from_known.map_or_else(|| app_stripped.clone(), str::to_string);

        // 2026-09-20: a bare app name means what a person means by it: the Start Menu entry. PATH used
        // to win, so open("Hermes") launched a venv's Hermes.EXE console tool instead of the desktop
        // app. (The backend answers `resolve_app` in that order.)
        let mut resolved: Option<AppTarget> = None;
        let plain = from_known.is_none()
            && launch_args.is_empty()
            && PLAIN_NAME.is_match(&exe)
            && !exe.to_lowercase().ends_with(".exe");
        if plain && let Ok(Some(t)) = self.be().resolve_app(&exe) {
            if let AppTarget::Shortcut(p) = &t {
                exe = p.clone();
            }
            resolved = Some(t);
        }
        let expanded = expand_vars(env, &app_stripped);
        if Path::new(&expanded).is_dir() {
            // A folder path is not a program. Launching it as one failed with "CreateProcessWithTokenW
            // ... Access is denied".
            return self.open_folder_path(&expanded, wait);
        }
        let exe_l = exe.to_lowercase();
        let target: AppTarget;
        if exe_l.ends_with(".lnk") || exe_l.ends_with(".app") {
            target = AppTarget::Shortcut(exe.clone());
        } else {
            if resolved.is_none() && !is_abs(&exe) {
                resolved = self.be().resolve_app(&exe).ok().flatten();
            }
            let resolved_path = match &resolved {
                Some(AppTarget::Executable(p)) => Some(p.clone()),
                _ if is_abs(&exe) => Some(exe.clone()),
                _ => None,
            };
            let is_real_exe = resolved_path.is_some() || exe_l.ends_with(".exe");
            if is_real_exe {
                target = AppTarget::Executable(resolved_path.unwrap_or_else(|| exe.clone()));
            } else if Path::new(&exe).exists() {
                target = AppTarget::Other(exe.clone());
            } else {
                // Not a program and not a file: a Start Menu shortcut, else a Store app (2026-09-20:
                // Paint, Calculator and the Snipping Tool have no exe on PATH). Launched through the
                // shell, de-elevated like a folder open.
                let alt = match resolved {
                    Some(t @ (AppTarget::Shortcut(_) | AppTarget::Packaged(_))) => Some(t),
                    _ => self.be().resolve_app(&app_stripped).ok().flatten(),
                };
                target = match alt {
                    Some(AppTarget::Shortcut(p)) => AppTarget::Shortcut(p),
                    Some(AppTarget::Packaged(id)) => AppTarget::Packaged(id),
                    _ => AppTarget::Other(exe.clone()),
                };
            }
        }
        self.be().launch(&LaunchSpec {
            target,
            args: launch_args.clone(),
            // 2026-09-17: launched apps used to inherit the daemon's elevated token. Start them at
            // the interactive user's normal integrity level instead.
            deelevate: true,
        })?;

        let deadline = self.now() + wait;
        while self.now() < deadline {
            self.sleep(0.2);
            let wins2 = self.c.window_refs().list(false)?;
            for w in wins2 {
                // Explorer reuses one process for every window, so a new PID never appears. Accept a
                // new window handle from a known app process as well.
                if !before_pids.contains(&w.pid) || !before_windows.contains(&w.window) {
                    self.check_not_protected(w.window)?;
                    self.be().bring_to_foreground(w.window)?;
                    let launched_stem = removesuffix_exe(&basename(&w.process));
                    let mut w = w;
                    if new
                        && TABBED_EDITORS.contains(&launched_stem.as_str())
                        && launch_args.is_empty()
                    {
                        // 2026-09-20: a fresh Notepad restores its last session and shows whatever tab
                        // was open (an unsaved debug file, in the test). Wait for the title to settle;
                        // if it is not a blank document, open a blank tab.
                        w = self.settled(w);
                        let t = w.title.trim().to_lowercase();
                        if !is_blank_title(&t) {
                            return self.new_editor_tab(&w);
                        }
                    }
                    return Ok(format!(
                        "ok focused w{} \"{}\" pid:{}",
                        w.reference, w.title, w.pid
                    ));
                }
            }
        }
        // 2026-09-19: a browser cold start can take 15 to 30 s. The process is running; the window is
        // just not up yet. Say so, or the model opens it a second time.
        Err(StepError::target(format!(
            "{} was started but showed no window within {wait:.0}s. It is probably still loading: \
             call wait_for('window:<expected title>', timeout=30) or windows() in a moment. Do not \
             open it again, that makes a second copy",
            py_repr(app)
        )))
    }

    /// Fill the open Save As / Open dialog's File name box with `path` and confirm.
    ///
    /// 2026-09-21 overnight evals: a local model lost 2 to 6 turns per task fighting the Save As
    /// dialog (triple-clicking the name box, clicking the folder list, typing the path twice).
    /// Windows' common file dialog is the same in almost every app and exposes an edit box named
    /// "File name:", so set it through accessibility and press Enter: one step, no pixel aim needed.
    /// Python `_file_dialog`.
    pub(crate) fn file_dialog(&self, path: &str) -> StepResult<String> {
        let mut path = path.to_string();
        if path.len() > 2 && path.as_bytes()[0].is_ascii_alphabetic() && path[1..].starts_with(":/")
        {
            // The dialog rejects "C:/x/y.txt" as an invalid name (found by hand 2026-09-21 morning).
            path = path.replace('/', "\\");
        }
        path = repair_escapes(&path, &|p| Path::new(p).is_file());
        // the owner's hard line
        self.o.policy.check_path(&path, "the file dialog path")?;
        let deadline = self.now() + 3.0;
        let mut edit = None;
        let mut dlg_root: Option<WindowRef> = None;
        while self.now() < deadline && edit.is_none() {
            let fg = self.be().foreground_window().ok().flatten();
            dlg_root = fg;
            if let Some(fg) = fg {
                edit = self.be().find_file_dialog_field(fg).ok().flatten();
            }
            if edit.is_none() {
                self.sleep(0.2);
            }
        }
        let Some(edit) = edit else {
            return Err(StepError::target(
                "no Save As / Open dialog with a 'File name' box is in front; open it first (e.g. \
                 ctrl+shift+s or ctrl+o)",
            ));
        };
        let dialog = dlg_root.unwrap_or_else(|| WindowRef::from_raw(0));
        // 2026-09-21: under only_apps, the dialog itself was never checked, so it filled and confirmed
        // a file path in whatever app's Save As happened to be in front, allowlisted or not.
        self.check_allowed(self.root_of(dialog))?;
        // macOS Save and Open panels do not take a folder typed into the name box: go to the folder
        // first (cmd+shift+g), then the box gets the file name alone.
        let mut typed = path.clone();
        if !cfg!(windows)
            && let Some((dir, name)) = path.rsplit_once('/')
            && !dir.is_empty()
            && !name.is_empty()
        {
            self.send_combo("cmd+shift+g", 1)?;
            self.sleep(0.7);
            self.type_text(dir)?;
            self.sleep(0.3);
            self.send_combo("enter", 1)?;
            self.sleep(0.9);
            typed = name.to_string();
        }
        // Not SetValue: the dialog shows the new text but keeps its own copy of the name, so Enter
        // saved the old name instead of the path (tested 2026-09-21). Focus the box, select all,
        // paste: the dialog sees real keyboard input.
        if self.be().perform(&edit, ElementAction::Focus).is_err()
            && let Ok(el) = self.be().element(&edit)
            && let Some(r) = el.rect
        {
            let c = r.center();
            let _ = self.be().mouse_click(
                Point { x: c.x, y: c.y },
                crate::backend::MouseButton::Left,
                1,
                &crate::backend::no_guard,
            );
        }
        self.sleep(0.1);
        // Check the box really holds the path before Enter. 2026-09-21 morning, by hand: a paste into a
        // box prefilled with the current name did not land, and Enter asked to overwrite the OLD file.
        let mut shown = String::new();
        let mut landed = false;
        for attempt in 0..3 {
            self.send_combo(self.select_all(), 1)?;
            if attempt < 2 {
                self.type_text(&typed)?;
            } else {
                // last try: real keystrokes, no clipboard
                for ch in typed.chars() {
                    let mut buf = [0u8; 4];
                    self.be()
                        .type_text(ch.encode_utf8(&mut buf), &crate::backend::no_guard)?;
                }
            }
            self.sleep(0.15);
            shown = match self.be().element(&edit) {
                Ok(el) => el.value.unwrap_or_default().trim().to_string(),
                Err(_) => typed.clone(),
            };
            if shown.to_lowercase() == typed.to_lowercase() {
                landed = true;
                break;
            }
        }
        if !landed {
            return Ok(format!(
                "file dialog: could not get {} into the File name box (it shows {}); nothing was \
                 confirmed",
                py_repr(&path),
                py_repr(&shown)
            ));
        }
        self.send_combo("enter", 1)?;
        // Wait for the dialog to go. A new standard popup (#32770) of the same program means a
        // question like "already exists, replace?" or "not valid": report its text, never answer it.
        let end = self.now() + 3.0;
        while self.now() < end {
            self.sleep(0.2);
            let fg_now = self.be().foreground_window().ok().flatten();
            if let Some(fg_now) = fg_now
                && fg_now != dialog
                && self.class_of(fg_now) == "#32770"
                && self.same_pid(fg_now, dialog)
            {
                let text: String = self
                    .be()
                    .window_text(fg_now)
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect();
                return Ok(format!(
                    "file dialog: entered {path}, but a popup asks: {}. Nothing was answered; decide \
                     and click it yourself",
                    py_repr(&text)
                ));
            }
            let gone =
                self.info(dialog).is_none_or(|i| i.hidden) || self.be().element(&edit).is_err();
            if gone {
                return Ok(format!("file dialog: entered {path} and confirmed"));
            }
        }
        Ok(format!(
            "file dialog: entered {path} and pressed Enter, but a dialog is still open (maybe 'already \
             exists, replace?' or an invalid name): look at the screenshot"
        ))
    }

    fn same_pid(&self, a: WindowRef, b: WindowRef) -> bool {
        match (self.info(a), self.info(b)) {
            (Some(x), Some(y)) => x.pid == y.pid,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ported from tests/test_computer.py::test_repair_escapes_* ----

    #[test]
    fn repair_escapes_restores_backslashes() {
        let none = |_: &str| false;
        assert_eq!(repair_escapes("C:\tUsers\u{8}in", &none), r"C:\tUsers\bin");
        assert_eq!(
            repair_escapes(r"C:\Program Files\x.exe", &none),
            r"C:\Program Files\x.exe"
        );
    }

    #[test]
    fn repair_escapes_picks_the_reading_that_exists() {
        // "C:<TAB>Program Files<BS>app.exe --x" read as a lone backslash names a file that exists
        let mangled = "C:\tProgram Files\\app.exe --x";
        let exists = |p: &str| p == "C:\\Program Files\\app.exe";
        assert_eq!(
            repair_escapes(mangled, &exists),
            "C:\\Program Files\\app.exe --x"
        );
        let none = |_: &str| false;
        assert_eq!(
            repair_escapes(mangled, &none),
            "C:\\tProgram Files\\app.exe --x"
        );
    }

    #[test]
    fn a_launch_string_splits_into_program_and_arguments() {
        let never = |_: &str| false;
        let on_path = |n: &str| n == "brave";
        // an unquoted absolute path with spaces that names an existing .exe keeps its arguments
        // (ported from test_open_unquoted_exe_path_with_spaces_keeps_args)
        let exe = r"C:\Program Files (x86)\app.exe";
        let is_file = |p: &str| p == exe;
        let (head, args) = split_launch(
            &format!("{exe} --inprivate https://example.com"),
            &never,
            &never,
            &is_file,
        );
        assert_eq!(
            (head.as_str(), args.as_str()),
            (exe, "--inprivate https://example.com")
        );
        // a bare .exe followed by an argument
        let (head, args) = split_launch("explorer.exe C:\\path", &never, &never, &never);
        assert_eq!((head.as_str(), args.as_str()), ("explorer.exe", "C:\\path"));
        // a quoted path
        let (head, args) = split_launch("\"C:\\x y\\a.exe\" -f", &never, &never, &never);
        assert_eq!((head.as_str(), args.as_str()), ("C:\\x y\\a.exe", "-f"));
        // a name found on PATH
        let (head, args) = split_launch("brave --new-window url", &on_path, &never, &never);
        assert_eq!(
            (head.as_str(), args.as_str()),
            ("brave", "--new-window url")
        );
        // an unknown word is not split
        let (head, args) = split_launch("some random words", &never, &never, &never);
        assert_eq!((head.as_str(), args.as_str()), ("some random words", ""));
        // a known name and an existing path are left whole
        let (head, args) = split_launch("notepad", &never, &never, &never);
        assert_eq!((head.as_str(), args.as_str()), ("notepad", ""));
        let exists = |_: &str| true;
        let (head, args) = split_launch("C:\\a b\\c.txt d", &never, &exists, &never);
        assert_eq!((head.as_str(), args.as_str()), ("C:\\a b\\c.txt d", ""));
    }
}
