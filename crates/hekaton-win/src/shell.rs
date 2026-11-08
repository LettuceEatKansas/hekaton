//! Explorer's view and sort through the Shell's own objects (Python `shortcuts.py`: `_shell_windows`,
//! `_same_location`, `_com_window`, `_com_ready`, `_com_apply`).
//!
//! `Shell.Application.Windows()` is the list of open Explorer windows; each is an `IWebBrowserApp`
//! whose `Document` is an `IShellFolderViewDual3`: `CurrentViewMode` and `IconSize` set the view,
//! `SortColumns` (`prop:-System.DateModified;`, a leading minus is descending) the sort. Both can be
//! read back, which is how the core knows it worked.
//!
//! The daemon may run elevated while Explorer is not, and COM can refuse to talk across that gap. That
//! shows up as an error from [`windows_list`] or as a window the list does not contain; the core then
//! falls back to the keyboard and a header click, so nothing here is fatal.

use std::thread::sleep;
use std::time::{Duration, Instant};

use hekaton_core::backend::{BackendResult, FolderSort, FolderView, FolderViewState, WindowRef};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    IShellFolderViewDual3, IShellWindows, IWebBrowserApp, ShellWindows,
};
use windows::core::{BSTR, Interface};

use crate::util::{ensure_com, os_err};

// ------------------------------------------------------------------------------------ pure rules

/// The `FOLDERVIEWMODE` and, for the icon views, the icon size that make a view
/// (`VIEWS` in `shortcuts.py`).
pub(crate) fn view_mode(view: FolderView) -> (u32, Option<i32>) {
    match view {
        FolderView::Details => (4, None),
        FolderView::List => (3, None),
        FolderView::Tiles => (6, None),
        FolderView::Content => (8, None),
        FolderView::Icons => (1, Some(48)),
        FolderView::Large => (1, Some(96)),
        FolderView::XLarge => (1, Some(256)),
        FolderView::Small => (2, None),
    }
}

/// The name of a view read back from the Shell (Python `_view_name`): the icon views are all mode 1
/// and told apart by icon size; modes hekaton never sets are named as they are.
pub(crate) fn view_name(mode: u32, icon_size: Option<i32>) -> String {
    match mode {
        1 => match icon_size.unwrap_or(0) {
            s if s >= 200 => "xlarge",
            s if s >= 80 => "large",
            _ => "icons",
        }
        .to_string(),
        2 => "small".to_string(),
        3 => "list".to_string(),
        4 => "details".to_string(),
        5 => "thumbnails".to_string(),
        6 => "tiles".to_string(),
        7 => "thumbstrip".to_string(),
        8 => "content".to_string(),
        other => format!("mode {other}"),
    }
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `%XX` escapes decoded as UTF-8 bytes (Python `urllib.request.url2pathname`'s unquote).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                b.get(i + 1).copied().and_then(hex),
                b.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The Windows path a `file:` URL names: `file:///C:/My%20Docs` is `C:\My Docs`, and
/// `file://server/share/dir` is `\\server\share\dir`. `None` for any other kind of address.
pub(crate) fn file_url_to_path(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("file:")?;
    let rest = rest.strip_prefix("//")?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let decoded = percent_decode(path);
    let path = decoded.replace('/', "\\");
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        // "\C:\x" -> "C:\x"
        let trimmed = path.strip_prefix('\\').unwrap_or(&path);
        Some(trimmed.to_string())
    } else {
        Some(format!("\\\\{host}{path}"))
    }
}

/// Does a window's `LocationURL` name this folder (Python `_same_location`)? Case and trailing
/// separators do not matter.
pub(crate) fn same_location(url: &str, folder: &str) -> bool {
    let Some(got) = file_url_to_path(url) else {
        return false;
    };
    let norm = |p: &str| {
        hekaton_core::folders::normpath(p)
            .trim_end_matches('\\')
            .to_lowercase()
    };
    norm(&got) == norm(folder)
}

// ------------------------------------------------------------------------------------ COM

/// One open Explorer window as the Shell lists it.
struct ShellWindow {
    app: IWebBrowserApp,
    hwnd: isize,
}

/// Every window the Shell lists, or an error when the Shell object cannot be reached at all.
fn windows_list() -> BackendResult<Vec<ShellWindow>> {
    ensure_com();
    // SAFETY: plain COM activation of the system's ShellWindows object on a thread with an apartment.
    let shell: IShellWindows = unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }
        .map_err(|e| os_err("the Shell's window list is not reachable", &e))?;
    // SAFETY: a plain property read.
    let count =
        unsafe { shell.Count() }.map_err(|e| os_err("could not count Explorer windows", &e))?;
    let mut out = Vec::new();
    for i in 0..count {
        // SAFETY: Item takes an index VARIANT and returns the window's dispatch interface.
        let Ok(item) = (unsafe { shell.Item(&VARIANT::from(i)) }) else {
            continue;
        };
        let Ok(app) = item.cast::<IWebBrowserApp>() else {
            continue;
        };
        // SAFETY: a plain property read on a live interface.
        let Ok(hwnd) = (unsafe { app.HWND() }) else {
            continue;
        };
        out.push(ShellWindow { app, hwnd: hwnd.0 });
    }
    Ok(out)
}

/// The Explorer window showing `folder`, if the Shell lists one. Python `_com_window(folder=...)`.
pub(crate) fn folder_window(folder: &str) -> BackendResult<Option<WindowRef>> {
    for w in windows_list()? {
        // SAFETY: a plain property read on a live interface.
        let url = unsafe { w.app.LocationURL() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        if same_location(&url, folder) {
            return Ok(Some(WindowRef::from_raw(w.hwnd as usize as u64)));
        }
    }
    Ok(None)
}

/// The view interface of the window with this handle, or showing this folder, once the Shell lists it
/// with a document. Errors from individual windows are skipped, as the Python search did.
fn find_view(hwnd: u64, folder: &str) -> BackendResult<Option<IShellFolderViewDual3>> {
    for w in windows_list()? {
        let by_handle = w.hwnd as usize as u64 == hwnd;
        // SAFETY: a plain property read on a live interface.
        let by_folder =
            unsafe { w.app.LocationURL() }.is_ok_and(|b| same_location(&b.to_string(), folder));
        if !(by_handle || by_folder) {
            continue;
        }
        // SAFETY: Document returns the window's current view as a dispatch interface, or fails.
        let Ok(doc) = (unsafe { w.app.Document() }) else {
            continue;
        };
        if let Ok(view) = doc.cast::<IShellFolderViewDual3>() {
            return Ok(Some(view));
        }
    }
    Ok(None)
}

/// Set the view and sort of the Explorer window `window` (or the one showing `folder`) and read both
/// back. Waits up to `timeout` for a window that has only just opened to show up in the Shell's list.
/// `Ok(None)` when it never did. Python `_com_ready` and `_com_apply`.
pub(crate) fn set_folder_view(
    window: WindowRef,
    folder: &str,
    view: Option<FolderView>,
    sort: Option<FolderSort>,
    timeout: Duration,
) -> BackendResult<Option<FolderViewState>> {
    let end = Instant::now() + timeout;
    let doc = loop {
        if let Some(doc) = find_view(window.raw(), folder)? {
            break doc;
        }
        if Instant::now() >= end {
            return Ok(None);
        }
        sleep(Duration::from_millis(200));
    };
    let mut state = FolderViewState::default();
    if let Some(v) = view {
        let (mode, size) = view_mode(v);
        let applied = (|| -> windows::core::Result<String> {
            // SAFETY: property writes and reads on a live view interface.
            unsafe {
                doc.SetCurrentViewMode(mode)?;
                if let Some(s) = size {
                    // a view that has no icon size is left alone
                    let _ = doc.SetIconSize(s);
                }
                let icon = doc.IconSize().ok();
                Ok(view_name(doc.CurrentViewMode()?, icon))
            }
        })();
        match applied {
            Ok(name) => state.view = name,
            Err(e) => state.errors.push(format!("view: {e}")),
        }
    }
    if let Some(s) = sort {
        let applied = (|| -> windows::core::Result<String> {
            // SAFETY: property writes and reads on a live view interface.
            unsafe {
                doc.SetSortColumns(&BSTR::from(s.shell_spec()))?;
                Ok(doc.SortColumns()?.to_string())
            }
        })();
        match applied {
            Ok(text) => {
                state.sort = FolderSort::from_shell_spec(&text)
                    .map(FolderSort::describe)
                    .unwrap_or_default();
            }
            Err(e) => state.errors.push(format!("sort: {e}")),
        }
    }
    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hekaton_core::backend::SortColumn;

    // Ported from test_view_modes_read_back_to_names.
    #[test]
    fn view_modes_read_back_to_names() {
        assert_eq!(view_name(4, None), "details");
        assert_eq!(view_name(3, None), "list");
        assert_eq!(view_name(6, None), "tiles");
        assert_eq!(view_name(8, None), "content");
        assert_eq!(view_name(2, None), "small");
        assert_eq!(view_name(1, Some(48)), "icons");
        assert_eq!(view_name(1, Some(96)), "large");
        assert_eq!(view_name(1, Some(256)), "xlarge");
        assert_eq!(view_name(1, None), "icons");
        assert_eq!(view_name(5, None), "thumbnails");
        assert_eq!(view_name(9, None), "mode 9");
    }

    #[test]
    fn every_view_maps_to_a_mode_that_reads_back_as_itself() {
        for v in FolderView::ALL {
            let (mode, size) = view_mode(v);
            assert_eq!(view_name(mode, size), v.name(), "{v:?}");
        }
    }

    // Ported from test_same_location_decodes_a_file_url.
    #[test]
    fn same_location_decodes_a_file_url() {
        assert!(same_location(
            "file:///C:/Users/ikill/My%20Docs",
            "C:\\Users\\ikill\\My Docs"
        ));
        assert!(same_location("file:///c:/users/x/", "C:\\Users\\x"));
        assert!(same_location(
            "file://server/share/dir",
            "\\\\server\\share\\dir"
        ));
        assert!(!same_location("file:///C:/Users/y", "C:\\Users\\x"));
        assert!(!same_location("http://example.com", "C:\\Users\\x"));
        assert!(!same_location("", "C:\\x"));
        // a non-ASCII folder name is percent-encoded as UTF-8
        assert!(same_location("file:///C:/caf%C3%A9", "C:\\caf\u{e9}"));
    }

    #[test]
    fn the_sort_text_the_shell_gets_is_what_python_sent() {
        let s = FolderSort {
            column: SortColumn::DateModified,
            descending: true,
        };
        assert_eq!(s.shell_spec(), "prop:-System.DateModified;");
    }

    /// Passive check against the real desktop: enumerating the Shell's windows must not crash or
    /// hang, whatever Explorer windows are open. It touches no window. Run with `--ignored`.
    #[test]
    #[ignore = "reads the live Shell window list"]
    fn the_live_shell_window_list_can_be_read() {
        match windows_list() {
            Ok(list) => eprintln!("the Shell lists {} Explorer window(s)", list.len()),
            Err(e) => eprintln!("the Shell window list is unreachable here: {e}"),
        }
        // a folder no window shows is simply not found
        let none = folder_window("C:\\hekaton-no-such-folder-xyz");
        assert!(matches!(none, Ok(None) | Err(_)));
    }
}
