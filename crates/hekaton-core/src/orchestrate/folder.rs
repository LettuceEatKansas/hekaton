//! The `folder` action: open a file manager at a path already in the requested view and sort, and read
//! the top `n` entries straight from the filesystem in that order, so the model never needs a picture
//! to answer. Port of `shortcuts.run_folder` and its helpers (`_open_explorer`, `_fallback_view`,
//! `_fallback_sort`, `_front_window`, `_find_open_window`).
//!
//! The order of trust is the same as everywhere else: the OS's own interface first (on Windows the
//! Shell's `IShellFolderViewDual`, behind [`Backend::set_folder_view`]), then the keyboard
//! (`Ctrl+Shift+<digit>` for a view), then a click on a column header whose result is checked against
//! the filesystem. Whatever the result, the listing comes from the filesystem and does not depend on
//! what the window shows.
//!
//! On macOS the folder is opened in Finder and the listing is returned, but setting the view and sort
//! is not done: it would need AppleScript, which is out of scope. The result says so.

use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use super::Ctx;
use super::args::{Args, text as text_arg};
use super::ctx::is_file_manager;
use super::error::{StepError, StepResult};
use super::files::{expand_user, expand_vars};
use super::secs;
use super::shortcuts::Target;
use crate::backend::{
    AppTarget, FolderSort, FolderView, FolderViewState, LaunchSpec, MouseButton, Point, Role,
    TreeLimits, WindowRef, no_guard,
};
use crate::folders;
use crate::textutil::py_repr;

static FOCUSED_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"focused w(\d+)").expect("FOCUSED_REF"));

/// Setting a view and sort needs the OS's folder-view interface, which macOS does not have here.
const MAC: bool = cfg!(target_os = "macos");

/// `os.path.splitext(name)[0]`.
fn stem_of(name: &str) -> &str {
    let lead = name.len() - name.trim_start_matches('.').len();
    match name[lead..].rfind('.') {
        Some(i) => &name[..lead + i],
        None => name,
    }
}

impl Ctx<'_> {
    /// Make `window` the window in front, or fail: keys go to whatever has focus.
    fn front_window(&self, window: WindowRef) -> StepResult<()> {
        self.be().bring_to_foreground(window)?;
        if self.fg_root() != Some(self.root_of(window)) {
            return Err(StepError::target(
                "could not bring the Explorer window to the front; not sending keys",
            ));
        }
        Ok(())
    }

    /// The first element of the folder window with this role and name, as its centre.
    fn header_item(&self, window: WindowRef, header: &str) -> Option<Point> {
        let tree = self
            .be()
            .element_tree(window, &TreeLimits::named(secs(1.0)))
            .ok()?;
        tree.root
            .children
            .iter()
            .find(|e| {
                matches!(&e.role, Role::Other(kind) if kind == "HeaderItem")
                    && e.name.trim().to_lowercase() == header.to_lowercase()
            })
            .and_then(|e| e.rect)
            .map(|r| r.center())
    }

    /// The name of the first row of the folder list the window shows.
    fn first_row_name(&self, window: WindowRef) -> Option<String> {
        let tree = self
            .be()
            .element_tree(window, &TreeLimits::named(secs(1.0)))
            .ok()?;
        tree.root
            .children
            .iter()
            .find(|e| matches!(e.role, Role::ListItem | Role::DataItem))
            .map(|e| e.name.trim().to_string())
    }

    /// Switch the view with Explorer's own `Ctrl+Shift+<digit>`. Not read back.
    fn fallback_view(&self, window: WindowRef, view: FolderView) -> StepResult<&'static str> {
        self.front_window(window)?;
        self.send_combo(&format!("ctrl+shift+{}", view.shortcut_digit()), 1)?;
        self.mark_ours();
        self.sleep(0.3);
        Ok(view.name())
    }

    /// Click the column header (toggling once if the first click went the other way) and check the
    /// first row against what the filesystem says the order should be. Returns a one-line verdict.
    fn fallback_sort(
        &self,
        window: WindowRef,
        folder: &str,
        sort: FolderSort,
    ) -> StepResult<String> {
        let header = sort.column.header();
        let Some(at) = self.header_item(window, header) else {
            return Ok(format!(
                "not applied (no {} column header: needs Details view, pass view=details)",
                py_repr(header)
            ));
        };
        let (rows, _) = folders::entries(Path::new(folder));
        let ordered = folders::sorted(&rows, sort);
        // Explorer may group folders first, and may hide known extensions
        let mut want: Vec<String> = Vec::new();
        for pick in [
            ordered.first(),
            ordered.iter().find(|r| r.dir),
            ordered.iter().find(|r| !r.dir),
        ]
        .into_iter()
        .flatten()
        {
            want.push(pick.name.to_lowercase());
            want.push(stem_of(&pick.name).to_lowercase());
        }
        self.front_window(window)?;
        let mut first = String::new();
        for _attempt in 0..2 {
            self.host_at(at)?;
            self.with_modifiers("", || {
                self.be()
                    .mouse_click(at, MouseButton::Left, 1, &no_guard)
                    .map_err(StepError::from)
            })?;
            self.mark_ours();
            self.sleep(0.4);
            let Some(row) = self.first_row_name(window) else {
                return Ok(format!(
                    "clicked the {} header; direction not verified",
                    py_repr(header)
                ));
            };
            if want.is_empty() {
                return Ok(format!(
                    "clicked the {} header; direction not verified",
                    py_repr(header)
                ));
            }
            if want.contains(&row.to_lowercase()) {
                return Ok(format!("{} (header click, verified)", sort.describe()));
            }
            first = row;
        }
        Ok(format!(
            "clicked the {} header twice but the first row is {}, not verified",
            py_repr(header),
            py_repr(&first)
        ))
    }

    /// An Explorer window for this exact title when nothing better is known (the Shell cannot say).
    fn find_open_folder_window(&self, folder: &str) -> Option<WindowRef> {
        let leaf = folders::normpath(folder)
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or("")
            .to_lowercase();
        self.c
            .window_refs()
            .list(false)
            .ok()?
            .into_iter()
            .find(|w| {
                w.title.to_lowercase() == leaf
                    && self.info(w.window).is_some_and(|i| is_file_manager(&i))
            })
            .map(|w| w.window)
    }

    /// Open Finder on a folder (macOS): reuse a Finder window titled with the folder's name, else
    /// start one with `open` and wait for it.
    fn open_finder_folder(&self, folder: &str) -> StepResult<(WindowRef, String)> {
        if let Some(w) = self.find_open_folder_window(folder) {
            self.check_not_protected(w)?;
            self.be().bring_to_foreground(w)?;
            return Ok((w, "reused the window already open on it".to_string()));
        }
        self.be().launch(&LaunchSpec {
            target: AppTarget::Other(folder.to_string()),
            args: String::new(),
            deelevate: true,
        })?;
        let (found, _) = self.poll(|| self.find_open_folder_window(folder), 8.0, 0.2);
        let Some(w) = found else {
            return Err(StepError::target(format!(
                "opened {folder} but no Finder window came up for it within 8s; list_windows shows \
                 where it went"
            )));
        };
        self.check_not_protected(w)?;
        self.be().bring_to_foreground(w)?;
        Ok((w, "opened a new window".to_string()))
    }

    /// Reuse an Explorer window already on this folder when the Shell can say so, else start a new
    /// one at the user's integrity level (`do_open` de-elevates). Returns the window and how.
    pub(crate) fn open_explorer(&self, folder: &str) -> StepResult<(WindowRef, String)> {
        if MAC {
            return self.open_finder_folder(folder);
        }
        if let Ok(Some(w)) = self.be().folder_window(folder)
            && self
                .be()
                .restore_window(w, Duration::from_secs(2))
                .and_then(|()| self.be().bring_to_foreground(w))
                .is_ok()
        {
            return Ok((w, "reused the window already open on it".to_string()));
        }
        let launch = format!("explorer.exe \"{folder}\"");
        match self.do_open(&launch, 8.0, true, false) {
            Ok(reply) => {
                let window = match FOCUSED_REF.captures(&reply) {
                    Some(m) => self.c.window_refs().resolve(Some(&format!("w{}", &m[1])))?,
                    None => self.fg_root().ok_or_else(|| {
                        StepError::target("window handle unavailable; refresh windows")
                    })?,
                };
                Ok((window, "opened a new window".to_string()))
            }
            // Explorer sometimes answers "already open" by raising the existing window instead of
            // making one
            Err(StepError::Target(why)) => match self.find_open_folder_window(folder) {
                Some(w) => {
                    self.be().bring_to_foreground(w)?;
                    Ok((w, "found the window already open on it".to_string()))
                }
                None => Err(StepError::Target(why)),
            },
            Err(other) => Err(other),
        }
    }

    /// The `folder` action. Python `shortcuts.run_folder`.
    pub(crate) fn run_folder(&self, step: &Args) -> StepResult<String> {
        let mut raw = text_arg(step, "path");
        if raw.is_empty() {
            raw = text_arg(step, "text");
        }
        let raw = raw.trim().trim_matches('"').to_string();
        if raw.is_empty() {
            return Err(StepError::value(
                "folder needs path: the folder to open, e.g. C:\\Users\\me\\Downloads",
            ));
        }
        let env = self.cfg().env();
        let repaired = super::open::repair_escapes(&raw, &|p| Path::new(p).is_file());
        let folder = folders::normpath(&expand_vars(env, &expand_user(env, &repaired)));
        if !Path::new(&folder).is_dir() {
            return Err(StepError::target(format!(
                "not a folder or not found: {folder}"
            )));
        }
        let null = Value::Null;
        let view =
            folders::parse_view(step.get("view").unwrap_or(&null)).map_err(StepError::value)?;
        let sort =
            folders::parse_sort(step.get("sort").unwrap_or(&null)).map_err(StepError::value)?;
        let n =
            folders::parse_count(step.get("list").unwrap_or(&null)).map_err(StepError::value)?;
        let launch = format!("explorer.exe \"{folder}\"");
        // the owner's hard line (credential stores)
        self.o.policy.check_command(&launch, "the folder path")?;
        // under only_apps: explorer.exe must be allowed, and this folder
        self.check_launch_allowed(&launch)?;
        if n > 0 {
            self.check_wait_path_allowed(&folder, "folder list may only read")?;
        }
        self.wait_for_human()?;
        let (window, how) = self.open_explorer(&folder)?;
        let listed = self.c.window_refs().list(false)?;
        let target: Target = self.target_of(window, &listed);
        self.guard_window(&target)?;

        let mut state = FolderViewState::default();
        let mut notes: Vec<String> = Vec::new();
        if view.is_some() || sort.is_some() {
            if MAC {
                notes.push(
                    "view and sort were not applied: setting them is not supported on macOS yet \
                     (Finder opened the folder; the listing below comes from the filesystem)"
                        .to_string(),
                );
            } else {
                match self.be().set_folder_view(
                    window,
                    &folder,
                    view,
                    sort,
                    Duration::from_millis(2500),
                ) {
                    Ok(Some(got)) => {
                        notes.extend(got.errors.iter().map(|e| format!("shell {e}")));
                        state = got;
                    }
                    Ok(None) => notes.push(
                        "shell api did not list the window; using keyboard and header clicks"
                            .to_string(),
                    ),
                    Err(e) => notes.push(format!(
                        "shell api unavailable ({e}); using keyboard and header clicks"
                    )),
                }
            }
        }
        // what the Shell did not confirm falls back to the keyboard (view) and a header click (sort)
        if let Some(v) = view
            && !MAC
        {
            if state.view.is_empty() {
                match self.fallback_view(window, v) {
                    Ok(name) => state.view = format!("{name} (keyboard, not read back)"),
                    Err(e) => notes.push(format!("view not applied: {e}")),
                }
            } else {
                state.view.push_str(" (shell)");
            }
        }
        if let Some(s) = sort
            && !MAC
        {
            if state.sort.is_empty() {
                match self.fallback_sort(window, &folder, s) {
                    Ok(verdict) => state.sort = verdict,
                    Err(e) => notes.push(format!("sort not applied: {e}")),
                }
            } else {
                state.sort.push_str(" (shell)");
            }
        }
        self.mark_ours();
        self.remember_target();
        let mut head = format!("folder {folder}: {how}, {}", target.label());
        if view.is_some() || sort.is_some() {
            let or_unset = |s: &str| {
                if s.is_empty() {
                    "not set".to_string()
                } else {
                    s.to_string()
                }
            };
            head.push_str(&format!(
                "; view {}; sort {}",
                or_unset(&state.view),
                or_unset(&state.sort)
            ));
        }
        let mut out = vec![head];
        out.extend(notes);
        if n > 0 {
            let order = sort.unwrap_or(FolderSort {
                column: crate::backend::SortColumn::DateModified,
                descending: true,
            });
            let (rows, skipped) = folders::entries(Path::new(&folder));
            out.push(folders::list_text(
                &folder,
                &rows,
                skipped,
                n,
                order,
                &|s| self.utc_offset(s),
            ));
        }
        Ok(out.join("\n"))
    }
}
