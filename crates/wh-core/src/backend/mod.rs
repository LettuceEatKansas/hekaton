//! The platform boundary. One [`Backend`] trait, one implementation per OS (`wh-win`, `wh-mac`) and
//! an in-memory [`fake::FakeBackend`] for the core's own tests. Everything above the trait is
//! platform-agnostic.
//!
//! The trait was derived from the Python build's actual platform surface: every function in
//! `core.py`, `computer.py`, `fast.py`, `caller.py` and `daemon.py` that touches Win32, UI
//! Automation, OCR, capture, input, the clipboard or the process table. Each method's doc comment
//! names the Python function(s) it replaces as `file.py:function`.
//!
//! Design rules:
//!
//! * No OS type crosses the boundary (see [`types`]). Windows and elements are opaque handles.
//! * A backend does the OS call and nothing else. Policy lives in the core: which window an action
//!   may touch, destructive-command refusals, the caller guard, settle timing, retries.
//! * Methods take `&self` and the trait is `Send + Sync`; the daemon serves each call on its own
//!   thread. A backend initialises per-thread OS state (COM on Windows) lazily.
//! * Time budgets are explicit [`Duration`] arguments, so a hung app can never
//!   stall a call longer than the caller allows.
//! * Input methods take an [`InputGuard`], the Python `guard=` callback: run before every injected
//!   event, and an `Err` stops the rest. Pass [`no_guard`] when there is nothing to re-check.

pub mod excel;
#[cfg(any(test, feature = "testing"))]
pub mod fake;
#[cfg(any(test, feature = "testing"))]
mod fake_excel;
pub mod types;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub use types::*;

use crate::keys::Key;

/// A check run before each injected input event. Returning `Err` aborts the remaining events.
/// Python: the `guard` callable of `core.click_at`, `scroll_at`, `drag`, `send_key_combo`,
/// `send_unicode_text`, which re-checks that focus and the target window are still what the
/// caller verified (`core.py:_guard_input`).
pub type InputGuard<'a> = &'a (dyn Fn() -> BackendResult<()> + 'a);

/// The guard that allows everything.
pub fn no_guard() -> BackendResult<()> {
    Ok(())
}

/// Keys injected by this operation. Cleanup never consults the input guard.
/// Tracking starts before the first press so partial failures and unwinds release earlier keys.
pub(crate) struct HeldKeys<'a, B: Backend + ?Sized> {
    backend: &'a B,
    pressed: Vec<Key>,
}

impl<'a, B: Backend + ?Sized> HeldKeys<'a, B> {
    pub(crate) fn new(backend: &'a B) -> Self {
        Self {
            backend,
            pressed: Vec::new(),
        }
    }

    pub(crate) fn press(&mut self, key: Key) -> BackendResult<()> {
        self.backend.key_down(key)?;
        self.pressed.push(key);
        Ok(())
    }

    pub(crate) fn release(&mut self) -> BackendResult<()> {
        let mut first_error = None;
        let mut failed = Vec::new();
        for key in self.pressed.drain(..).rev() {
            if let Err(error) = self.backend.key_up(key) {
                failed.push(key);
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        failed.reverse();
        self.pressed = failed;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl<B: Backend + ?Sized> Drop for HeldKeys<'_, B> {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

/// The platform boundary. See the [module docs](self).
pub trait Backend: Send + Sync {
    // ------------------------------------------------------------------ identity and setup

    /// A short name for logs and `/health`: `windows`, `macos`, `fake`.
    ///
    /// No Python equivalent (the Python build was Windows only).
    fn name(&self) -> &'static str;

    /// What this backend can do here, so callers degrade instead of failing.
    ///
    /// No Python equivalent (the Python build was Windows only).
    fn capabilities(&self) -> Capabilities;

    /// One-time process setup. Windows: per-monitor-v2 DPI awareness and a COM apartment; macOS:
    /// nothing (permissions are checked per call). Idempotent.
    ///
    /// Replaces `core.py:set_dpi_awareness`, `core.py:com_init`.
    fn init(&self) -> BackendResult<()>;

    /// The OS permissions this backend needs and whether each is granted right now, for a
    /// `winhands doctor` report. Empty where the OS has no such gate (Windows). Never prompts.
    ///
    /// No Python equivalent (the Python build was Windows only).
    fn permissions(&self) -> BackendResult<Vec<PermissionStatus>> {
        Ok(Vec::new())
    }

    // ------------------------------------------------------------------ displays and capture

    /// Every display, primary first, then by left edge. Each carries its rect in input
    /// coordinates and its backing scale.
    ///
    /// Replaces `computer.py:monitors` (mss order, primary at 0,0 first).
    fn displays(&self) -> BackendResult<Vec<Display>>;

    /// The bounding rect of all displays in input coordinates.
    ///
    /// Replaces `core.py:_virtual_screen` (`SM_*VIRTUALSCREEN`).
    fn virtual_screen(&self) -> BackendResult<Rect>;

    /// Capture pixels. `Region` is a rect of the virtual desktop in input coordinates; the
    /// returned image is in physical pixels with [`Image::scale`] and [`Image::origin`] set.
    ///
    /// Replaces `core.py:grab` (and so `computer.py:_grab`, `screenshot_b64`, `settle`).
    fn capture(&self, target: CaptureTarget) -> BackendResult<Image>;

    /// True when the trailing settle of a call that returns no picture may finish on a background
    /// thread while the model thinks; the next call waits for it before acting. Off for backends
    /// whose clock or captures are not safe to drive from a second thread.
    fn settle_in_background(&self) -> bool {
        false
    }

    /// A cheap, roughly `width` x `height` look at `region`, for telling whether the screen is still
    /// moving. A backend that cannot shrink while copying returns the full-size capture.
    fn capture_preview(&self, region: Rect, width: u32, height: u32) -> BackendResult<Image> {
        let _ = (width, height);
        self.capture(CaptureTarget::Region(region))
    }

    // ------------------------------------------------------------------ windows

    /// Top-level application windows in z-order, topmost first (the `EnumWindows` order):
    /// visible, titled, not tool windows. `include_hidden` adds the compositor-cloaked ones that
    /// duplicate titles and cannot be foregrounded.
    ///
    /// Replaces `core.py:list_windows` (including `is_cloaked`, `_process_name`, `_is_elevated`).
    fn list_windows(&self, include_hidden: bool) -> BackendResult<Vec<WindowInfo>>;

    /// A live read of one window, whether or not `list_windows` would show it (a popup, a dialog,
    /// the desktop). `Err(WindowGone)` when it no longer exists.
    ///
    /// Replaces `core.py:window_process`, `window_pid`, `window_class`, `_window_rect`,
    /// `computer.py:_title`, `_class`, and the `user32.IsWindow` liveness checks.
    fn window_info(&self, window: WindowRef) -> BackendResult<WindowInfo>;

    /// The window in front, if any.
    ///
    /// Replaces `user32.GetForegroundWindow` as used by `computer.py:_fg_root`, `core.py:prepare_window`,
    /// `resolve_window_hwnd(None)`, `_confirm`.
    fn foreground_window(&self) -> BackendResult<Option<WindowRef>>;

    /// The top-level window under a screen point, honouring real z-order; `None` over nothing.
    ///
    /// Replaces `core.py:_hwnd_at` (`WindowFromPoint` then `GA_ROOT`).
    fn window_at_point(&self, p: Point) -> BackendResult<Option<WindowRef>>;

    /// The top-level window that contains `window` (itself for a top-level one). A UWP app's
    /// CoreWindow is a child of its ApplicationFrameHost frame; both are the same real window.
    ///
    /// Replaces `core.py:root_hwnd`.
    fn root_window(&self, window: WindowRef) -> BackendResult<WindowRef>;

    /// Windows that own `window`, nearest first: a popup is owned by its app's main window even
    /// though it is its own top-level window and often its own process.
    ///
    /// Replaces `computer.py:_owner_chain` (`GW_OWNER` walk plus `GA_ROOTOWNER`).
    fn owner_chain(&self, window: WindowRef) -> BackendResult<Vec<WindowRef>>;

    /// For a window that is only a frame around another app's content, the name of that app
    /// (Windows: the process inside an ApplicationFrameHost). `None` for an ordinary window.
    ///
    /// Replaces `computer.py:_uwp_app_name`.
    fn hosted_app(&self, window: WindowRef) -> BackendResult<Option<String>>;

    /// A small dialog's title plus its static text lines, for reporting what a popup asks.
    ///
    /// Replaces `computer.py:_window_text`.
    fn window_text(&self, window: WindowRef) -> BackendResult<String>;

    /// Bring a window to the front, restoring it first if minimised, without synthetic key events.
    /// Errors when focus cannot be obtained.
    ///
    /// Replaces `core.py:bring_to_foreground` (foreground-lock timeout, `AttachThreadInput`).
    fn bring_to_foreground(&self, window: WindowRef) -> BackendResult<()>;

    /// Un-minimise a window and wait, bounded, until the OS reports it visible. A window that is
    /// not minimised is left alone.
    ///
    /// Replaces `core.py:restore_window`.
    fn restore_window(&self, window: WindowRef, timeout: Duration) -> BackendResult<()>;

    /// Wait, bounded, until a restored window has real bounds and an accessibility tree that has
    /// begun to populate. Returns as soon as it looks usable; a timeout is not an error.
    ///
    /// Replaces `core.py:wait_window_ready`.
    fn wait_window_ready(&self, window: WindowRef, timeout: Duration) -> BackendResult<()>;

    /// Visible top-level popup windows that can hold a menu (Win32 `#32768`, XAML popup hosts).
    /// One cheap window enumeration, safe to poll.
    ///
    /// Replaces `computer.py:_popup_menu_hwnds`, `_wait_popup` (the poll is the core's).
    fn popup_menus(&self) -> BackendResult<Vec<WindowRef>>;

    /// The items of the open context menu, within `budget`: popup windows first, then the desktop's
    /// direct children that look like menus. Empty when none is open.
    ///
    /// Replaces `computer.py:_menu_items_once`, `_menu_items_under`.
    fn menu_items(&self, budget: Duration) -> BackendResult<Vec<MenuItem>>;

    // ------------------------------------------------------------------ accessibility

    /// Read a window's accessibility tree under `limits`. One cross-process query where the OS
    /// allows (UIA `FindAllBuildCache` with every printed property cached), a bounded walk
    /// otherwise. May wake an empty Chromium tree per [`TreeLimits::wake`].
    ///
    /// Replaces `core.py:snapshot`, `_snapshot_batched`, `_is_interactive`, `_item_state`,
    /// `_wake_chromium_by_raise`, `_is_chromium`, `computer.py:_uia_elements`, `_exposes_uia`,
    /// `_menu_items_under`.
    ///
    /// Contract the core relies on: with `flatten`, the kept elements are the root's children in
    /// document order and the window element itself is `root` (the core adds it when it searches names:
    /// Python's walk started at the window, so a search for a window's title finds the window). An
    /// element that exposes a value (UIA `ValuePattern`) carries it in `Element::value` even in a
    /// `Named` read: `read_field` and `read` print it. `wake_hint` is set only when
    /// [`WakePolicy::Never`] left an empty Chromium tree alone.
    fn element_tree(&self, window: WindowRef, limits: &TreeLimits) -> BackendResult<TreeSnapshot>;

    /// A fresh read of one element's properties (without children). `Err(ElementGone)` when it
    /// is gone.
    ///
    /// Replaces `core.py:_fresh_element`, `_refind_element`.
    fn element(&self, id: &ElementId) -> BackendResult<Element>;

    /// The top-level window an element lives in.
    ///
    /// Replaces `core.py:_fresh_element` (`GetTopLevelControl().NativeWindowHandle`).
    fn element_window(&self, id: &ElementId) -> BackendResult<WindowRef>;

    /// The element at a screen point and up to `max_ancestors` parents, answering within `budget`
    /// or returning `None`.
    ///
    /// Replaces `computer.py:_describe_point`, `_irreversible_name_at` (`ControlFromPoint`).
    fn element_at(
        &self,
        p: Point,
        max_ancestors: usize,
        budget: Duration,
    ) -> BackendResult<Option<ElementHit>>;

    /// The element with keyboard focus, with its value (unless it is a password field).
    ///
    /// Replaces `core.py:_typed_check` (`GetFocusedControl`), `computer.py:_doc_text_uia`.
    fn focused_element(&self) -> BackendResult<Option<Element>>;

    /// Act on an element. `Press` returns `Ok(false)` when it supports no primary action (the
    /// caller then falls back to a mouse click); a failure during the action is an error and the
    /// outcome is uncertain.
    ///
    /// Replaces `core.py:_try_patterns` (Invoke, Toggle, Select, Expand), `computer.py:_file_dialog`
    /// (`SetFocus`).
    fn perform(&self, id: &ElementId, action: ElementAction) -> BackendResult<bool>;

    /// The "File name" edit element of a standard Save As / Open dialog in `window`, if it has one.
    ///
    /// Replaces `computer.py:_file_dialog` (the `WalkControl` search for the edit named "File name").
    fn find_file_dialog_field(&self, window: WindowRef) -> BackendResult<Option<ElementId>>;

    // ------------------------------------------------------------------ text

    /// The whole text of a window's document or, failing that, of the focused text control. `None`
    /// when the window exposes no text (a canvas-drawn editor): the caller then falls back to
    /// select-all + copy.
    ///
    /// Replaces `computer.py:_doc_text_uia`, `_text_docs`.
    fn document_text(&self, window: WindowRef, max_chars: usize) -> BackendResult<Option<String>>;

    /// Matches for `query` in the window's documents, straight from their text: exact characters,
    /// including lines scrolled out of view (then the rects are `None`).
    ///
    /// Replaces `computer.py:_doc_text_hits`.
    fn find_in_documents(
        &self,
        window: WindowRef,
        query: &str,
        max_hits: usize,
        budget: Duration,
    ) -> BackendResult<Vec<DocumentHit>>;

    /// The document's own text of the line at a screen point, to replace an OCR guess.
    ///
    /// Replaces `computer.py:_exact_line_at` (`RangeFromPoint` + `ExpandToEnclosingUnit(Line)`).
    fn document_line_at(&self, window: WindowRef, p: Point) -> BackendResult<Option<String>>;

    /// [`Backend::document_line_at`] for many points at once, in order. Backends that pay a fixed
    /// cost per lookup (finding the window's documents, a thread hop) override this to pay it once:
    /// a zoom over 28 lines of text made 56 UIA calls, 784 ms, before this existed (2026-10-28).
    fn document_lines_at(
        &self,
        window: WindowRef,
        points: &[Point],
    ) -> BackendResult<Vec<Option<String>>> {
        points
            .iter()
            .map(|p| self.document_line_at(window, *p))
            .collect()
    }

    /// Where the foreground thread's text cursor is, if it shows one.
    ///
    /// Replaces `computer.py:_caret_screen_point`, `_gui_thread_info`.
    fn caret(&self) -> BackendResult<Option<Caret>>;

    /// Does the GUI thread of `window` (the foreground one when `None`) show a text caret, i.e.
    /// does an edit box have focus?
    ///
    /// Replaces `computer.py:_has_caret`.
    fn has_caret(&self, window: Option<WindowRef>) -> BackendResult<bool>;

    /// An opaque value that changes when keyboard focus moves to a different control. Used to
    /// notice that a shortcut opened something.
    ///
    /// Replaces `computer.py:_focused_hwnd`, `_wait_focus_change`.
    fn focus_token(&self) -> BackendResult<u64>;

    // ------------------------------------------------------------------ input

    /// The pointer position.
    ///
    /// Replaces `computer.py:_cursor` (`GetCursorPos`).
    fn cursor_position(&self) -> BackendResult<Point>;

    /// Move the pointer to `to` (absolute, input coordinates).
    ///
    /// Replaces `core.py:move_mouse`.
    fn mouse_move(&self, to: Point) -> BackendResult<()>;

    /// Press or release a button where the pointer is.
    ///
    /// Replaces `computer.py:_mouse_button`, `core.py:_mouse_event` (left_mouse_down / up).
    fn mouse_button(&self, button: MouseButton, down: bool) -> BackendResult<()>;

    /// Move to `at`, then press and release `button` `count` times (2 is a double click). Three
    /// clicks for a triple click are three calls with `count` 1, as Python does.
    ///
    /// Replaces `core.py:click_at`.
    fn mouse_click(
        &self,
        at: Point,
        button: MouseButton,
        count: u8,
        guard: InputGuard<'_>,
    ) -> BackendResult<()>;

    /// Move to `at` and scroll `notches` wheel notches in `direction`.
    ///
    /// Replaces `core.py:scroll_at`.
    fn mouse_scroll(
        &self,
        at: Point,
        direction: ScrollDirection,
        notches: u32,
        guard: InputGuard<'_>,
    ) -> BackendResult<()>;

    /// Press the left button at `from`, move to `to` in steps, release at `to`. The button is
    /// always released, even when the guard aborts mid-drag.
    ///
    /// Replaces `core.py:drag`.
    fn mouse_drag(&self, from: Point, to: Point, guard: InputGuard<'_>) -> BackendResult<()>;

    /// Press a key and leave it down.
    ///
    /// Replaces `core.py:_key_input(vk, up=False)` via `computer.py:_holding`.
    fn key_down(&self, key: Key) -> BackendResult<()>;

    /// Release a key.
    ///
    /// Replaces `core.py:_key_input(vk, up=True)` via `computer.py:_holding`.
    fn key_up(&self, key: Key) -> BackendResult<()>;

    /// Press `keys` in order, release them in reverse, `repeat` times, running `guard` before each
    /// round. A backend that can send the whole chord atomically (one `SendInput` batch) should
    /// override this default.
    ///
    /// Replaces `core.py:send_key_combo`.
    fn key_chord(&self, keys: &[Key], repeat: u32, guard: InputGuard<'_>) -> BackendResult<()> {
        for _ in 0..repeat.max(1) {
            guard()?;
            let mut held = HeldKeys::new(self);
            for key in keys {
                guard()?;
                held.press(*key)?;
            }
            held.release()?;
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    /// Type Unicode text as key events, one UTF-16 unit (surrogate pair for an emoji) at a time,
    /// running `guard` before each.
    ///
    /// Replaces `core.py:send_unicode_text`.
    fn type_text(&self, text: &str, guard: InputGuard<'_>) -> BackendResult<()>;

    /// How many input batches this backend has injected since it started. A call that fails before
    /// any input went out can then be told from one that did.
    ///
    /// Replaces `core.py:INPUT_EVENTS`.
    fn input_events(&self) -> u64;

    /// How long since the last input from any source, a person or us (`GetLastInputInfo`). The core
    /// compares it with the age of its own last injection to tell a human from itself.
    ///
    /// Replaces `computer.py:_last_input_tick`, `_wait_for_human`, `_mark_ours`.
    fn human_input_idle(&self) -> BackendResult<Duration>;

    // ------------------------------------------------------------------ clipboard

    /// What the clipboard holds. Retries briefly when another app holds it.
    ///
    /// Replaces `fast.py:_clipboard_get`, `_open_clipboard`, `computer.py:_clipboard_summary`
    /// (`CF_UNICODETEXT`, `CF_HDROP`).
    fn clipboard(&self) -> BackendResult<ClipboardContent>;

    /// Replace the clipboard with text.
    ///
    /// Replaces `fast.py:_clipboard_set`.
    fn set_clipboard_text(&self, text: &str) -> BackendResult<()>;

    /// A counter that changes whenever the clipboard does, `None` when the OS has none.
    ///
    /// Replaces `computer.py:_clip_seq` (`GetClipboardSequenceNumber`).
    fn clipboard_sequence(&self) -> BackendResult<Option<u64>>;

    // ------------------------------------------------------------------ OCR

    /// Recognise text in an image: lines with word boxes and confidence, in pixels of `image`.
    /// Errors with `Unsupported` when no engine is available.
    ///
    /// Replaces `core.py:_ocr_winrt`, `_get_ocr_engine` (the word-to-row grouping stays in core).
    fn ocr(&self, image: &Image) -> BackendResult<OcrResult>;

    /// Which engine answers, plus the last fallback reason, for `/health`.
    ///
    /// Replaces `core.py:ocr_engine_name`.
    fn ocr_engine_name(&self) -> String;

    /// Build the OCR engine now so the first call is fast. The daemon calls this at start.
    ///
    /// Replaces the warm-up in `daemon.py` (`_warm`).
    fn warm_ocr(&self) -> BackendResult<()> {
        Ok(())
    }

    // ------------------------------------------------------------------ processes

    /// Every process this account can see, for walking a parent chain.
    ///
    /// Replaces `caller.py:_process_table` (`CreateToolhelp32Snapshot`), `shim.py` process table.
    fn process_table(&self) -> BackendResult<Vec<ProcessInfo>>;

    /// One process: parent pid, executable name, creation time, elevation.
    ///
    /// Replaces `core.py:_process_name`, `_is_elevated`, `caller.py:_created_at`.
    fn process_info(&self, pid: u32) -> BackendResult<Option<ProcessInfo>>;

    /// The pid that owns the client end of a local TCP connection into this daemon, given the
    /// client's port and the daemon's port. `None` when it cannot be read.
    ///
    /// Replaces `caller.py:pid_for_peer` (`GetExtendedTcpTable`).
    fn peer_pid(&self, peer_port: u16, server_port: u16) -> BackendResult<Option<u32>>;

    // ------------------------------------------------------------------ launching

    /// Resolve a bare name ("notepad", "Hermes", "Paint") to something launchable: an executable on
    /// the path or in App Paths, a Start Menu shortcut, a packaged app.
    ///
    /// The answer follows the order the Python launcher tried things (`core.do_open`): for a plain
    /// name a Start Menu shortcut comes first (2026-09-20: PATH used to win, so `open("Hermes")`
    /// launched a venv's Hermes.EXE console tool instead of the desktop app), then a program on the
    /// path or in App Paths, then a packaged (Store) app. `None` for anything else, and the core then
    /// hands the text to the shell as it is.
    ///
    /// Replaces `core.py:resolve_app_exe`, `_app_paths_exe`, `find_app_shortcut`, `find_store_app`,
    /// `resolve_shortcut`, `_start_menu_roots`.
    fn resolve_app(&self, name: &str) -> BackendResult<Option<AppTarget>>;

    /// Start a program, shortcut, packaged app or document; at the user's normal integrity level
    /// when `spec.deelevate` and the daemon is elevated.
    ///
    /// Replaces `core.py:launch_deelevated`, `launch_deelevated_shell`, `_create_process_with_token`,
    /// `_get_deelevated_token`, `_find_explorer_pid`, the `subprocess.Popen` / `os.startfile`
    /// branches of `do_open`.
    fn launch(&self, spec: &LaunchSpec) -> BackendResult<LaunchOutcome>;

    // ------------------------------------------------------------------ window and folder shortcuts

    /// A window's shape beyond [`WindowInfo::rect`]: the whole frame, the visible bounds, the client
    /// area and whether it is minimised or maximised. The default reads `window_info`, reports the
    /// visible bounds as the frame, no client area, and never maximised; a backend that can tell
    /// overrides it.
    ///
    /// Replaces `shortcuts.py:_window_rect`, `_client_rect`, `_visible_rect`, `_state`.
    fn window_geometry(&self, window: WindowRef) -> BackendResult<WindowGeometry> {
        let info = self.window_info(window)?;
        Ok(WindowGeometry {
            frame: info.rect,
            visible: info.rect,
            client: None,
            state: if info.minimized {
                WindowState::Minimized
            } else {
                WindowState::Normal
            },
        })
    }

    /// Close, minimise, maximise, restore or move a window. Returns as soon as the request is made:
    /// a closing app may still be asking whether to save, and a hung one must not stall the call.
    /// The core polls [`Backend::window_geometry`] and [`Backend::window_exists`] to see the result.
    ///
    /// Replaces `shortcuts.py:_post_close` (`WM_CLOSE`), `_show_async` (`ShowWindowAsync`),
    /// `_set_pos` (`SetWindowPos` with `SWP_NOZORDER | SWP_NOACTIVATE`).
    fn window_op(&self, _window: WindowRef, _op: WindowOp) -> BackendResult<()> {
        Err(BackendError::Unsupported("window operations".into()))
    }

    /// A file-manager window already showing `folder`, if the OS can say (Windows: the Shell's list
    /// of open Explorer windows). `None` when none is open or the OS cannot tell.
    ///
    /// Replaces `shortcuts.py:_com_window(folder=...)`.
    fn folder_window(&self, _folder: &str) -> BackendResult<Option<WindowRef>> {
        Ok(None)
    }

    /// Set a file-manager window's view and sort through the OS's own interface (Windows: the Shell
    /// `IShellFolderViewDual`) and read them back. Waits up to `timeout` for a window that has only
    /// just opened to show up. `Ok(None)` when the window never appeared in that interface; an error
    /// when the interface is unavailable (an elevated daemon talking to a de-elevated Explorer, for
    /// one). The core falls back to the keyboard and a header click in either case.
    ///
    /// Replaces `shortcuts.py:_shell_windows`, `_com_ready`, `_com_apply`.
    fn set_folder_view(
        &self,
        _window: WindowRef,
        _folder: &str,
        _view: Option<FolderView>,
        _sort: Option<FolderSort>,
        _timeout: Duration,
    ) -> BackendResult<Option<FolderViewState>> {
        Err(BackendError::Unsupported("folder views".into()))
    }

    // ------------------------------------------------------------------ local time

    /// The local time zone's offset from UTC, in seconds east, at a Unix time. `list_files` prints
    /// file times the way the owner's clock shows them. A backend that cannot say leaves the default,
    /// and the core prints UTC.
    ///
    /// Replaces `datetime.fromtimestamp` (local time) in `fast.py:do_files`.
    fn local_utc_offset(&self, _unix_secs: i64) -> BackendResult<i32> {
        Err(BackendError::Unsupported("local time zone".into()))
    }

    // ------------------------------------------------------------------ kill hotkey

    /// Register the first chord of `candidates` the OS accepts as a global hotkey and call
    /// `on_press` each time it is pressed, from a thread the backend owns. Returns the chosen
    /// chord's name, or `None` when none could be registered.
    ///
    /// Replaces `daemon.py:_hotkey_thread`, `HOTKEY_CANDIDATES` (`RegisterHotKey`).
    fn register_kill_hotkey(
        &self,
        candidates: &[HotkeyChord],
        on_press: Arc<dyn Fn() + Send + Sync>,
    ) -> BackendResult<Option<String>>;

    // ------------------------------------------------------------------ provided helpers

    /// Is the point inside the virtual desktop?
    ///
    /// Replaces `core.py:_point_on_screen`.
    fn point_on_screen(&self, p: Point) -> BackendResult<bool> {
        Ok(self.virtual_screen()?.contains(p))
    }

    /// The live owner PID, without fetching titles, process names, or accessibility data.
    /// Native backends should override this for inexpensive per-injection identity checks.
    fn window_pid(&self, window: WindowRef) -> BackendResult<u32> {
        let pid = self.window_info(window)?.pid;
        if pid == 0 {
            Err(BackendError::WindowGone)
        } else {
            Ok(pid)
        }
    }

    /// Does the window still exist?
    ///
    /// Replaces `user32.IsWindow` as used by `core.py:resolve_window_hwnd`,
    /// `computer.py:_restore_target`, `core.py:_confirm`.
    fn window_exists(&self, window: WindowRef) -> bool {
        self.window_info(window).is_ok()
    }

    /// Are `a` and `b` the same real window (equal, or equal top-level windows)?
    ///
    /// Replaces `core.py:same_window`.
    fn same_window(&self, a: WindowRef, b: WindowRef) -> bool {
        a == b || matches!((self.root_window(a), self.root_window(b)), (Ok(x), Ok(y)) if x == y)
    }

    /// The fraction of a `grid x grid` sample of `rect` where `window` (or its root) is what is
    /// actually on screen, which reflects the real z-order.
    ///
    /// Replaces `core.py:_visible_fraction`.
    fn visible_fraction(&self, window: WindowRef, rect: Rect, grid: u32) -> BackendResult<f64> {
        if rect.is_empty() || grid == 0 {
            return Ok(0.0);
        }
        let (mut hits, mut total) = (0u32, 0u32);
        for i in 0..grid {
            for j in 0..grid {
                let x = rect.left
                    + (f64::from(rect.width()) * (f64::from(i) + 0.5) / f64::from(grid)) as i32;
                let y = rect.top
                    + (f64::from(rect.height()) * (f64::from(j) + 0.5) / f64::from(grid)) as i32;
                total += 1;
                if let Some(top) = self.window_at_point(Point { x, y })?
                    && self.same_window(top, window)
                {
                    hits += 1;
                }
            }
        }
        Ok(f64::from(hits) / f64::from(total))
    }

    /// Clipboard text, or "" when it holds none.
    fn clipboard_text(&self) -> BackendResult<String> {
        Ok(match self.clipboard()? {
            ClipboardContent::Text(t) => t,
            _ => String::new(),
        })
    }

    /// OCR a region of the screen: capture it, recognise it. Boxes are in pixels of the capture;
    /// the capture's origin and scale are returned for mapping back to screen coordinates.
    ///
    /// Replaces `computer.py:_ocr_zoom_items` (grab + `_ocr_winrt`), `_locate_text` (OCR half).
    fn ocr_region(&self, region: Rect) -> BackendResult<(OcrResult, Point, f64)> {
        let image = self.capture(CaptureTarget::Region(region))?;
        let result = self.ocr(&image)?;
        Ok((result, image.origin(), image.scale()))
    }

    /// The process this daemon runs as.
    fn own_process(&self) -> BackendResult<Option<ProcessInfo>> {
        self.process_info(std::process::id())
    }

    // ------------------------------------------------------------------ the Excel power channel

    /// Can this backend drive Excel (it is installed)? Reported by `status`; the `excel` action is
    /// still off until the owner turns the channel on.
    fn excel_available(&self) -> bool {
        false
    }

    /// One Excel operation on the instance this backend owns, started on demand and hidden. It never
    /// attaches to an Excel the person is using. Charts and pivot tables are not supported yet.
    fn excel(&self, _req: &ExcelRequest) -> BackendResult<ExcelReply> {
        Err(BackendError::Unsupported("the excel channel".into()))
    }

    /// Quit the owned Excel instance if there is one. Best effort; called when the tool is dropped
    /// and when the daemon stops.
    fn excel_shutdown(&self) {}
}
