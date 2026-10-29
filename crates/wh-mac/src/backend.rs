//! [`MacBackend`]: the `Backend` trait implemented over the pieces in the sibling modules.

use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use wh_core::backend::{
    AppTarget, Backend, BackendError, BackendResult, Capabilities, CaptureTarget, Caret,
    ClipboardContent, Display, DocumentHit, Element, ElementAction, ElementHit, ElementId,
    HotkeyChord, Image, InputGuard, LaunchOutcome, LaunchSpec, MenuItem, MouseButton, OcrResult,
    PermissionStatus, Point, ProcessInfo, Rect, ScrollDirection, TreeLimits, TreeSnapshot,
    WindowGeometry, WindowInfo, WindowOp, WindowRef,
};
use wh_core::keys::Key;

use crate::ax::Ax;
use crate::capture::Capturer;
use crate::geom::{display_for_point, plan_capture};
use crate::hotkey::Hotkeys;
use crate::input::Input;
use crate::procs::Procs;
use crate::textops::{PASTE_ABOVE_UNITS, utf16_len};
use crate::tree::Registry;
use crate::wininfo::window_parts;
use crate::{clip, displays, launch, ocr, perm, text, tree, winlist, winops};

/// The macOS backend. Cheap to share: every method takes `&self`.
pub struct MacBackend {
    input: Input,
    capturer: Capturer,
    registry: Registry,
    procs: Procs,
    hotkeys: Hotkeys,
    /// The last few `window_info` answers. One key press asks for the same window's details six
    /// times over in its safety checks, ~2 ms each from the window server (2026-10-29 profile).
    info_cache: std::sync::Mutex<Vec<(std::time::Instant, WindowRef, WindowInfo)>>,
    /// Bundle ids of on-screen surfaces by (pid, owner name), for [`Backend::screen_layers`].
    layer_bundles: std::sync::Mutex<std::collections::HashMap<(i32, String), String>>,
}

/// How long a cached `window_info` answer stays good. Short enough that a window the user drags or
/// retitles is read fresh on the next step; long enough to cover one step's guards.
const INFO_TTL: Duration = Duration::from_millis(60);

/// Seconds east of UTC in the user's time zone at a Unix time.
fn local_offset(unix_secs: i64) -> BackendResult<i32> {
    let t: libc::time_t = unix_secs;
    // SAFETY: an all-zero `tm` is a valid value (the zone name pointer is just null).
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call, and localtime_r is thread-safe.
    let done = unsafe { libc::localtime_r(&t, &mut tm) };
    if done.is_null() {
        return Err(BackendError::Os("localtime_r failed".into()));
    }
    i32::try_from(tm.tm_gmtoff)
        .map_err(|_| BackendError::Os("time zone offset out of range".into()))
}

impl Default for MacBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MacBackend {
    /// A backend with no state yet; grants are checked per call.
    pub fn new() -> Self {
        Self {
            input: Input::new(),
            capturer: Capturer::default(),
            registry: Registry::default(),
            procs: Procs::default(),
            hotkeys: Hotkeys::default(),
            info_cache: std::sync::Mutex::default(),
            layer_bundles: std::sync::Mutex::default(),
        }
    }

    fn forget_info(&self) {
        if let Ok(mut c) = self.info_cache.lock() {
            c.clear();
        }
    }

    /// Display rects, for fullscreen detection. Empty if the displays cannot be read.
    fn display_rects(&self) -> Vec<Rect> {
        displays::list()
            .map(|l| l.iter().map(|d| d.rect).collect())
            .unwrap_or_default()
    }

    /// Paste long text through the clipboard (Cmd+V) and put the old text back. Only used when
    /// the clipboard holds text or nothing, so nothing the user copied is lost.
    fn paste_text(
        &self,
        text: &str,
        saved: ClipboardContent,
        sequence: u64,
        guard: InputGuard<'_>,
    ) -> BackendResult<()> {
        guard()?;
        perm::require_post_events("paste text")?;
        perm::check_secure_typing()?;
        if clip::sequence() != sequence {
            return self.input.type_text(text, guard);
        }
        clip::set_text(text)?;
        let written_sequence = clip::sequence();
        sleep(Duration::from_millis(40));
        let paste_guard = || {
            guard()?;
            if clip::sequence() != written_sequence {
                return Err(BackendError::Os(
                    "clipboard changed before paste; inspect before retrying".into(),
                ));
            }
            Ok(())
        };
        let result = self.key_chord(&[Key::Meta, Key::Char('v')], 1, &paste_guard);
        // Let the app read the pasteboard before the old contents go back.
        sleep(Duration::from_millis(250));
        clip::restore_plain(saved, written_sequence);
        result
    }

    fn capture_region(&self, region: Rect, deadline: Instant) -> BackendResult<Image> {
        let list = displays::list()?;
        let specs: Vec<_> = list.iter().map(displays::DisplayInfo::spec).collect();
        let plan = plan_capture(&specs, region)
            .ok_or_else(|| BackendError::Os("that region is outside every display".to_string()))?;
        self.capturer.capture_plan(&plan, &list, deadline)
    }

    /// One display only: ScreenCaptureKit scales to the small size on the GPU, so the settle loop
    /// skips a full-resolution copy and a CPU resize on every look (2026-10-11).
    fn capture_region_small(&self, region: Rect, width: u32, height: u32) -> BackendResult<Image> {
        let deadline = Instant::now() + crate::capture::CAPTURE_TIMEOUT;
        let list = displays::list()?;
        let specs: Vec<_> = list.iter().map(displays::DisplayInfo::spec).collect();
        let plan = plan_capture(&specs, region)
            .ok_or_else(|| BackendError::Os("that region is outside every display".to_string()))?;
        let [part] = plan.parts.as_slice() else {
            return self.capturer.capture_plan(&plan, &list, deadline);
        };
        self.capturer
            .capture_small(&list[part.display], part.relative, width, height, deadline)
    }
}

impl Backend for MacBackend {
    fn name(&self) -> &'static str {
        "macos"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            accessibility_tree: true,
            document_text: true,
            ocr: true,
            popup_menus: true,
            file_dialog: true,
            caret: true,
            kill_hotkey: true,
            clipboard_files: true,
            elevation: false,
        }
    }

    fn init(&self) -> BackendResult<()> {
        // A process-wide ceiling on how long any Accessibility call waits for a hung app.
        Ax::system_wide().set_timeout(1.0);
        Ok(())
    }

    fn permissions(&self) -> BackendResult<Vec<PermissionStatus>> {
        Ok(perm::report())
    }

    // ------------------------------------------------------------------ displays and capture

    fn displays(&self) -> BackendResult<Vec<Display>> {
        Ok(displays::to_neutral(&displays::list()?))
    }

    fn capture_preview(&self, region: Rect, width: u32, height: u32) -> BackendResult<Image> {
        self.capture_region_small(region, width, height)
    }

    fn settle_in_background(&self) -> bool {
        // ScreenCaptureKit answers on its own queue, so a second thread may watch the screen. A
        // blind call returns ~300 ms sooner (2026-10-29). WINHANDS_SETTLE_SYNC=1 turns it off.
        std::env::var_os("WINHANDS_SETTLE_SYNC").is_none()
    }

    fn settle_look_interval(&self) -> Duration {
        // A look reads the live stream's newest frame in ~10 ms (see crate::stream), so looking
        // every 45 ms calls a still screen ~50 ms sooner. Two calm looks still span 90 ms, which
        // a macOS window or sheet animation (200-300 ms) does not pass for still.
        if std::env::var_os("WINHANDS_NO_STREAM").is_some() {
            Duration::from_millis(70)
        } else {
            Duration::from_millis(45)
        }
    }

    fn virtual_screen(&self) -> BackendResult<Rect> {
        Ok(displays::union(&displays::list()?))
    }

    fn capture(&self, target: CaptureTarget) -> BackendResult<Image> {
        let deadline = Instant::now() + crate::capture::CAPTURE_TIMEOUT;
        match target {
            CaptureTarget::VirtualDesktop => {
                let region = displays::union(&displays::list()?);
                self.capture_region(region, deadline)
            }
            CaptureTarget::Display(n) => {
                let list = displays::list()?;
                let d = usize::try_from(n)
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|i| list.get(i))
                    .ok_or_else(|| BackendError::Os(format!("there is no display {n}")))?;
                self.capture_region(d.rect, deadline)
            }
            CaptureTarget::Region(r) => self.capture_region(r, deadline),
            CaptureTarget::Window(w) => {
                let info = winlist::info(w, &self.display_rects())?;
                let list = displays::list()?;
                let specs: Vec<_> = list.iter().map(displays::DisplayInfo::spec).collect();
                let scale =
                    display_for_point(&specs, info.rect.center()).map_or(1.0, |i| specs[i].scale);
                let (_, id) = window_parts(w);
                match self.capturer.capture_window(id, info.rect, scale, deadline) {
                    Ok(img) => Ok(img),
                    // A window ScreenCaptureKit will not isolate is captured from the screen.
                    Err(BackendError::Denied(e)) => Err(BackendError::Denied(e)),
                    Err(BackendError::Timeout) => Err(BackendError::Timeout),
                    Err(_) => self.capture_region(info.rect, deadline),
                }
            }
        }
    }

    // ------------------------------------------------------------------ windows

    fn list_windows(&self, include_hidden: bool) -> BackendResult<Vec<WindowInfo>> {
        Ok(winlist::list(include_hidden, &self.display_rects()))
    }

    fn screen_layers(&self, banners: bool) -> BackendResult<Vec<wh_core::backend::ScreenLayer>> {
        Ok(winlist::screen_layers(
            &self.layer_bundles,
            &self.display_rects(),
            banners,
        ))
    }

    fn window_info(&self, window: WindowRef) -> BackendResult<WindowInfo> {
        let now = std::time::Instant::now();
        if let Ok(mut c) = self.info_cache.lock() {
            c.retain(|(t, _, _)| now.duration_since(*t) < INFO_TTL);
            if let Some((_, _, info)) = c.iter().find(|(_, w, _)| *w == window) {
                return Ok(info.clone());
            }
        }
        let info = winlist::info(window, &self.display_rects())?;
        if let Ok(mut c) = self.info_cache.lock() {
            if c.len() >= 8 {
                c.remove(0);
            }
            c.push((now, window, info.clone()));
        }
        Ok(info)
    }

    fn foreground_window(&self) -> BackendResult<Option<WindowRef>> {
        Ok(winlist::foreground())
    }

    fn window_at_point(&self, p: Point) -> BackendResult<Option<WindowRef>> {
        Ok(winlist::at_point(p))
    }

    fn root_window(&self, window: WindowRef) -> BackendResult<WindowRef> {
        // Every window is top level on macOS; sheets and popups are separate windows with owners.
        Ok(window)
    }

    fn owner_chain(&self, window: WindowRef) -> BackendResult<Vec<WindowRef>> {
        let info = winlist::info(window, &self.display_rects())?;
        Ok(info.owner.into_iter().collect())
    }

    fn hosted_app(&self, _window: WindowRef) -> BackendResult<Option<String>> {
        // No Windows-style frame host exists on macOS.
        Ok(None)
    }

    fn window_text(&self, window: WindowRef) -> BackendResult<String> {
        tree::window_text(window, &self.display_rects())
    }

    fn bring_to_foreground(&self, window: WindowRef) -> BackendResult<()> {
        self.forget_info();
        winlist::bring_to_front(window, &self.display_rects())
    }

    fn restore_window(&self, window: WindowRef, timeout: Duration) -> BackendResult<()> {
        self.forget_info();
        winlist::restore(window, timeout)
    }

    fn wait_window_ready(&self, window: WindowRef, timeout: Duration) -> BackendResult<()> {
        winlist::wait_ready(window, timeout);
        Ok(())
    }

    fn popup_menus(&self) -> BackendResult<Vec<WindowRef>> {
        Ok(winlist::popups().into_iter().map(|(w, _)| w).collect())
    }

    fn menu_items(&self, budget: Duration) -> BackendResult<Vec<MenuItem>> {
        tree::menu_items(budget)
    }

    // ------------------------------------------------------------------ accessibility

    fn element_tree(&self, window: WindowRef, limits: &TreeLimits) -> BackendResult<TreeSnapshot> {
        tree::element_tree(&self.registry, window, limits, &self.display_rects())
    }

    fn element(&self, id: &ElementId) -> BackendResult<Element> {
        tree::element(&self.registry, id)
    }

    fn element_window(&self, id: &ElementId) -> BackendResult<WindowRef> {
        tree::element_window(&self.registry, id)
    }

    fn element_at(
        &self,
        p: Point,
        max_ancestors: usize,
        budget: Duration,
    ) -> BackendResult<Option<ElementHit>> {
        tree::element_at(&self.registry, p, max_ancestors, budget)
    }

    fn focused_element(&self) -> BackendResult<Option<Element>> {
        tree::focused_element(&self.registry)
    }

    fn perform(&self, id: &ElementId, action: ElementAction) -> BackendResult<bool> {
        let acts = !matches!(action, ElementAction::Focus);
        let done = tree::perform(&self.registry, id, action)?;
        if done && acts {
            self.input.count_ax_action();
        }
        Ok(done)
    }

    fn find_file_dialog_field(&self, window: WindowRef) -> BackendResult<Option<ElementId>> {
        tree::find_file_dialog_field(&self.registry, window, &self.display_rects())
    }

    // ------------------------------------------------------------------ text

    fn document_text(&self, window: WindowRef, max_chars: usize) -> BackendResult<Option<String>> {
        text::document_text(window, max_chars, &self.display_rects())
    }

    fn find_in_documents(
        &self,
        window: WindowRef,
        query: &str,
        max_hits: usize,
        budget: Duration,
    ) -> BackendResult<Vec<DocumentHit>> {
        let started = Instant::now();
        let displays = self.display_rects();
        text::find_in_documents(
            window,
            query,
            max_hits,
            &displays,
            budget.saturating_sub(started.elapsed()),
        )
    }

    fn document_line_at(&self, window: WindowRef, p: Point) -> BackendResult<Option<String>> {
        text::line_at(window, p)
    }

    fn caret(&self) -> BackendResult<Option<Caret>> {
        text::caret()
    }

    fn has_caret(&self, window: Option<WindowRef>) -> BackendResult<bool> {
        text::has_caret(window)
    }

    fn focus_token(&self) -> BackendResult<u64> {
        Ok(text::focus_token())
    }

    // ------------------------------------------------------------------ input

    fn cursor_position(&self) -> BackendResult<Point> {
        self.input.cursor()
    }

    fn mouse_move(&self, to: Point) -> BackendResult<()> {
        self.input.mouse_move(to)
    }

    fn mouse_button(&self, button: MouseButton, down: bool) -> BackendResult<()> {
        if down {
            perm::check_protected_at(self.input.cursor()?)?;
        }
        self.input.mouse_button(button, down)
    }

    fn mouse_click(
        &self,
        at: Point,
        button: MouseButton,
        count: u8,
        guard: InputGuard<'_>,
    ) -> BackendResult<()> {
        perm::check_protected_at(at)?;
        self.input.click(at, button, count, guard)
    }

    fn mouse_scroll(
        &self,
        at: Point,
        direction: ScrollDirection,
        notches: u32,
        guard: InputGuard<'_>,
    ) -> BackendResult<()> {
        self.input.scroll(at, direction, notches, guard)
    }

    fn mouse_drag(&self, from: Point, to: Point, guard: InputGuard<'_>) -> BackendResult<()> {
        self.input.drag(from, to, guard)
    }

    fn key_down(&self, key: Key) -> BackendResult<()> {
        self.input.key_down(key)
    }

    fn key_up(&self, key: Key) -> BackendResult<()> {
        self.input.key_up(key)
    }

    fn insert_text(&self, text: &str) -> BackendResult<bool> {
        let done = text::insert_text(text)?;
        if done {
            self.input.count_ax_action();
        }
        Ok(done)
    }

    fn type_text(&self, text: &str, guard: InputGuard<'_>) -> BackendResult<()> {
        if utf16_len(text) > PASTE_ABOVE_UNITS {
            // Pasting is only safe when nothing but text (or nothing) would be overwritten.
            if let Some((saved, sequence)) = clip::plain_snapshot() {
                return self.paste_text(text, saved, sequence, guard);
            }
        }
        self.input.type_text(text, guard)
    }

    fn input_events(&self) -> u64 {
        self.input.events()
    }

    fn human_input_idle(&self) -> BackendResult<Duration> {
        Ok(self.input.idle())
    }

    // ------------------------------------------------------------------ clipboard

    fn clipboard(&self) -> BackendResult<ClipboardContent> {
        clip::read()
    }

    fn set_clipboard_text(&self, text: &str) -> BackendResult<()> {
        clip::set_text(text)
    }

    fn clipboard_sequence(&self) -> BackendResult<Option<u64>> {
        Ok(Some(clip::sequence()))
    }

    // ------------------------------------------------------------------ OCR

    fn ocr(&self, image: &Image) -> BackendResult<OcrResult> {
        ocr::recognize(image)
    }

    fn ocr_engine_name(&self) -> String {
        ocr::ENGINE.to_string()
    }

    fn warm_ocr(&self) -> BackendResult<()> {
        ocr::warm()
    }

    // ------------------------------------------------------------------ processes

    fn process_table(&self) -> BackendResult<Vec<ProcessInfo>> {
        Ok(self.procs.table())
    }

    fn process_info(&self, pid: u32) -> BackendResult<Option<ProcessInfo>> {
        Ok(self.procs.one(pid))
    }

    fn peer_pid(&self, _peer_port: u16, _server_port: u16) -> BackendResult<Option<u32>> {
        // TODO(R5b): map the loopback peer port to a pid with libproc (proc_pidfdinfo on each
        // process's sockets). Until then the core falls back to the parent-process chain.
        Err(BackendError::Unsupported(
            "finding the process behind a loopback connection is not implemented on macOS yet; \
             the caller guard uses the parent-process chain instead"
                .into(),
        ))
    }

    // ------------------------------------------------------------------ launching

    fn resolve_app(&self, name: &str) -> BackendResult<Option<AppTarget>> {
        launch::resolve_app(name)
    }

    fn launch(&self, spec: &LaunchSpec) -> BackendResult<LaunchOutcome> {
        launch::launch(spec)
    }

    // ------------------------------------------------------------------ window shortcuts

    fn window_geometry(&self, window: WindowRef) -> BackendResult<WindowGeometry> {
        winops::geometry(window, &self.display_rects())
    }

    fn window_op(&self, window: WindowRef, op: WindowOp) -> BackendResult<()> {
        self.forget_info();
        winops::op(window, op)
    }

    // `folder_window` and `set_folder_view` keep their defaults: the core opens Finder with `open`
    // and reports that the view and sort were not applied (they would need AppleScript).

    // ------------------------------------------------------------------ local time

    fn local_utc_offset(&self, unix_secs: i64) -> BackendResult<i32> {
        local_offset(unix_secs)
    }

    // ------------------------------------------------------------------ kill hotkey

    fn register_kill_hotkey(
        &self,
        candidates: &[HotkeyChord],
        on_press: Arc<dyn Fn() + Send + Sync>,
    ) -> BackendResult<Option<String>> {
        self.hotkeys.register(candidates, on_press)
    }

    // ------------------------------------------------------------------ provided helpers

    fn visible_fraction(&self, window: WindowRef, rect: Rect, grid: u32) -> BackendResult<f64> {
        // One window-server snapshot for all samples instead of one per sample.
        Ok(crate::wininfo::visible_fraction(
            &winlist::on_screen_list(),
            &self.display_rects(),
            window_parts(window),
            rect,
            grid,
        ))
    }

    fn window_exists(&self, window: WindowRef) -> bool {
        // Cheaper than a full `window_info`: one window-server lookup, no Accessibility.
        let (pid, id) = window_parts(window);
        winlist::lookup(pid, id).is_some()
    }
}

impl MacBackend {
    /// Diagnostics for the end-to-end tests: the window server's view of every on-screen window
    /// that contains `p`, front to back.
    #[doc(hidden)]
    pub fn debug_windows_at(&self, p: Point) -> Vec<String> {
        winlist::on_screen_list()
            .iter()
            .filter(|w| w.bounds.contains(p))
            .map(|w| {
                format!(
                    "id={} pid={} owner={:?} layer={} alpha={} title={:?} bounds={:?}",
                    w.id, w.pid, w.owner_name, w.layer, w.alpha, w.title, w.bounds
                )
            })
            .collect()
    }

    /// Diagnostics for the end-to-end tests: what Accessibility says about the focus.
    #[doc(hidden)]
    pub fn debug_focus(&self) -> String {
        tree::debug_focus()
    }
}
