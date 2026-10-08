//! [`MacBackend`]: the `Backend` trait implemented over the pieces in the sibling modules.

use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use wh_core::backend::{
    AppTarget, Backend, BackendError, BackendResult, Capabilities, CaptureTarget, Caret,
    ClipboardContent, Display, DocumentHit, Element, ElementAction, ElementHit, ElementId,
    HotkeyChord, Image, InputGuard, LaunchOutcome, LaunchSpec, MenuItem, MouseButton, OcrResult,
    PermissionStatus, Point, ProcessInfo, Rect, ScrollDirection, TreeLimits, TreeSnapshot,
    WindowInfo, WindowRef,
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
use crate::{clip, displays, launch, ocr, perm, text, tree, winlist};

/// The macOS backend. Cheap to share: every method takes `&self`.
pub struct MacBackend {
    input: Input,
    capturer: Capturer,
    registry: Registry,
    procs: Procs,
    hotkeys: Hotkeys,
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
    fn paste_text(&self, text: &str, guard: InputGuard<'_>) -> BackendResult<()> {
        guard()?;
        perm::require_post_events("paste text")?;
        perm::check_secure_typing()?;
        let saved = clip::read()?;
        clip::set_text(text)?;
        sleep(Duration::from_millis(40));
        let result = self.key_chord(&[Key::Meta, Key::Char('v')], 1, guard);
        // Let the app read the pasteboard before the old contents go back.
        sleep(Duration::from_millis(250));
        match saved {
            ClipboardContent::Text(old) => {
                let _ = clip::set_text(&old);
            }
            ClipboardContent::Empty => {
                let _ = clip::set_text("");
            }
            _ => {}
        }
        result
    }

    fn capture_region(&self, region: Rect) -> BackendResult<Image> {
        let list = displays::list()?;
        let specs: Vec<_> = list.iter().map(displays::DisplayInfo::spec).collect();
        let plan = plan_capture(&specs, region)
            .ok_or_else(|| BackendError::Os("that region is outside every display".to_string()))?;
        self.capturer.capture_plan(&plan, &list)
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

    fn virtual_screen(&self) -> BackendResult<Rect> {
        Ok(displays::union(&displays::list()?))
    }

    fn capture(&self, target: CaptureTarget) -> BackendResult<Image> {
        match target {
            CaptureTarget::VirtualDesktop => {
                let region = displays::union(&displays::list()?);
                self.capture_region(region)
            }
            CaptureTarget::Display(n) => {
                let list = displays::list()?;
                let d = usize::try_from(n)
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|i| list.get(i))
                    .ok_or_else(|| BackendError::Os(format!("there is no display {n}")))?;
                self.capture_region(d.rect)
            }
            CaptureTarget::Region(r) => self.capture_region(r),
            CaptureTarget::Window(w) => {
                let info = winlist::info(w, &self.display_rects())?;
                let list = displays::list()?;
                let specs: Vec<_> = list.iter().map(displays::DisplayInfo::spec).collect();
                let scale =
                    display_for_point(&specs, info.rect.center()).map_or(1.0, |i| specs[i].scale);
                let (_, id) = window_parts(w);
                match self.capturer.capture_window(id, info.rect, scale) {
                    Ok(img) => Ok(img),
                    // A window ScreenCaptureKit will not isolate is captured from the screen.
                    Err(BackendError::Denied(e)) => Err(BackendError::Denied(e)),
                    Err(_) => self.capture_region(info.rect),
                }
            }
        }
    }

    // ------------------------------------------------------------------ windows

    fn list_windows(&self, include_hidden: bool) -> BackendResult<Vec<WindowInfo>> {
        Ok(winlist::list(include_hidden, &self.display_rects()))
    }

    fn window_info(&self, window: WindowRef) -> BackendResult<WindowInfo> {
        winlist::info(window, &self.display_rects())
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
        winlist::bring_to_front(window, &self.display_rects())
    }

    fn restore_window(&self, window: WindowRef, timeout: Duration) -> BackendResult<()> {
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
        tree::perform(&self.registry, id, action)
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
        _budget: Duration,
    ) -> BackendResult<Vec<DocumentHit>> {
        text::find_in_documents(window, query, max_hits, &self.display_rects())
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
        self.input.mouse_button(button, down)
    }

    fn mouse_click(
        &self,
        at: Point,
        button: MouseButton,
        count: u8,
        guard: InputGuard<'_>,
    ) -> BackendResult<()> {
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

    fn type_text(&self, text: &str, guard: InputGuard<'_>) -> BackendResult<()> {
        if utf16_len(text) > PASTE_ABOVE_UNITS {
            // Pasting is only safe when nothing but text (or nothing) would be overwritten.
            if matches!(
                clip::read(),
                Ok(ClipboardContent::Text(_) | ClipboardContent::Empty)
            ) {
                return self.paste_text(text, guard);
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
            window_parts(window),
            rect,
            grid,
        ))
    }

    fn window_exists(&self, window: WindowRef) -> bool {
        // Cheaper than a full `window_info`: one window-server lookup, no Accessibility.
        let (pid, id) = window_parts(window);
        winlist::copy_list(
            objc2_core_graphics::CGWindowListOption::OptionIncludingWindow,
            id,
        )
        .iter()
        .any(|r| r.id == id && r.pid == pid)
    }
}
