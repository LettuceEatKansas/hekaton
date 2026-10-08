//! The Windows backend: [`WindowsBackend`] implements [`wh_core::backend::Backend`] with Win32.
//!
//! Part 1 (this file and its modules): displays and capture, windows and focus, SendInput, the
//! clipboard, processes, launching, the kill hotkey, the local time zone. Part 2 adds UI Automation
//! (element trees, patterns, document text, menus) and WinRT OCR; those methods return
//! `BackendError::Unsupported("part 2")` until then.
//!
//! On every other OS this crate is empty.
#![cfg(windows)]

mod clipboard;
mod display;
mod hotkey;
mod input;
mod launch;
mod process;
mod resolve;
mod timezone;
mod util;
mod wnd;

use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use wh_core::backend::{
    AppTarget, Backend, BackendError, BackendResult, Capabilities, CaptureTarget, Caret,
    ClipboardContent, Display, DocumentHit, Element, ElementAction, ElementHit, ElementId,
    HotkeyChord, Image, InputGuard, LaunchOutcome, LaunchSpec, MenuItem, MouseButton, OcrResult,
    Point, ProcessInfo, Rect, ScrollDirection, TreeLimits, TreeSnapshot, WindowInfo, WindowRef,
};
use wh_core::keys::Key;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, PROCESS_PER_MONITOR_DPI_AWARE,
    SetProcessDpiAwareness, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::SetProcessDPIAware;

use crate::util::{ensure_com, hwnd_of};

/// Why a method is not there yet.
const PART_TWO: &str = "part 2";

fn part_two<T>() -> BackendResult<T> {
    Err(BackendError::Unsupported(PART_TWO.into()))
}

/// Per-monitor-v2 DPI awareness, falling back to older calls (Python `set_dpi_awareness`). After
/// this every coordinate the process sees is a physical pixel.
fn set_dpi_awareness() {
    // SAFETY: process-wide setting with no pointers; failing because awareness is already set is fine.
    let v2 = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if v2.is_ok() {
        return;
    }
    // SAFETY: as above.
    if unsafe { SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE) }.is_ok() {
        return;
    }
    // SAFETY: as above.
    let _ = unsafe { SetProcessDPIAware() };
}

/// The Windows implementation of [`Backend`].
pub struct WindowsBackend {
    dpi: Once,
    input: input::Injector,
    machine: launch::Machine,
    hotkey: Mutex<Option<String>>,
}

impl WindowsBackend {
    /// A backend. Nothing is touched until a method is called; the daemon calls
    /// [`Backend::init`] first.
    pub fn new() -> Self {
        Self {
            dpi: Once::new(),
            input: input::Injector::default(),
            machine: launch::Machine::default(),
            hotkey: Mutex::new(None),
        }
    }
}

impl Default for WindowsBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for WindowsBackend {
    // ------------------------------------------------------------------ identity and setup

    fn name(&self) -> &'static str {
        "windows"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Part 2 turns these on with UI Automation and WinRT OCR.
            accessibility_tree: false,
            document_text: false,
            ocr: false,
            popup_menus: false,
            file_dialog: false,
            caret: true,
            kill_hotkey: true,
            clipboard_files: true,
            elevation: true,
        }
    }

    fn init(&self) -> BackendResult<()> {
        self.dpi.call_once(set_dpi_awareness);
        ensure_com();
        Ok(())
    }

    // ------------------------------------------------------------------ displays and capture

    fn displays(&self) -> BackendResult<Vec<Display>> {
        display::monitors()
    }

    fn virtual_screen(&self) -> BackendResult<Rect> {
        Ok(display::virtual_screen_rect())
    }

    fn capture(&self, target: CaptureTarget) -> BackendResult<Image> {
        let rect = match target {
            CaptureTarget::VirtualDesktop => display::virtual_screen_rect(),
            CaptureTarget::Display(id) => {
                let all = display::monitors()?;
                let count = all.len();
                all.into_iter()
                    .find(|d| d.id == id)
                    .map(|d| d.rect)
                    .ok_or_else(|| {
                        BackendError::Os(format!(
                            "display {id} does not exist; this PC has {count} monitor(s)"
                        ))
                    })?
            }
            CaptureTarget::Region(rect) => rect,
            CaptureTarget::Window(window) => {
                let info = wnd::window_info(window)?;
                if info.minimized || info.hidden {
                    return Err(BackendError::Os(
                        "window hidden or minimized; open it before continuing".into(),
                    ));
                }
                info.rect
            }
        };
        display::capture_rect(rect)
    }

    // ------------------------------------------------------------------ windows

    fn list_windows(&self, include_hidden: bool) -> BackendResult<Vec<WindowInfo>> {
        Ok(wnd::list_windows(include_hidden))
    }

    fn window_info(&self, window: WindowRef) -> BackendResult<WindowInfo> {
        wnd::window_info(window)
    }

    fn foreground_window(&self) -> BackendResult<Option<WindowRef>> {
        Ok(wnd::foreground_window())
    }

    fn window_at_point(&self, p: Point) -> BackendResult<Option<WindowRef>> {
        Ok(wnd::window_at_point(p.x, p.y))
    }

    fn root_window(&self, window: WindowRef) -> BackendResult<WindowRef> {
        wnd::root_window(window)
    }

    fn owner_chain(&self, window: WindowRef) -> BackendResult<Vec<WindowRef>> {
        wnd::owner_chain(window)
    }

    fn hosted_app(&self, window: WindowRef) -> BackendResult<Option<String>> {
        wnd::hosted_app(window)
    }

    fn window_text(&self, window: WindowRef) -> BackendResult<String> {
        wnd::window_text(window)
    }

    fn bring_to_foreground(&self, window: WindowRef) -> BackendResult<()> {
        wnd::bring_to_foreground(window)
    }

    fn restore_window(&self, window: WindowRef, timeout: Duration) -> BackendResult<()> {
        wnd::restore_window(window, timeout)
    }

    fn wait_window_ready(&self, window: WindowRef, timeout: Duration) -> BackendResult<()> {
        wnd::wait_window_ready(window, timeout)
    }

    fn popup_menus(&self) -> BackendResult<Vec<WindowRef>> {
        Ok(wnd::popup_menus())
    }

    fn menu_items(&self, _budget: Duration) -> BackendResult<Vec<MenuItem>> {
        part_two()
    }

    // ------------------------------------------------------------------ accessibility (part 2)

    fn element_tree(
        &self,
        _window: WindowRef,
        _limits: &TreeLimits,
    ) -> BackendResult<TreeSnapshot> {
        part_two()
    }

    fn element(&self, _id: &ElementId) -> BackendResult<Element> {
        part_two()
    }

    fn element_window(&self, _id: &ElementId) -> BackendResult<WindowRef> {
        part_two()
    }

    fn element_at(
        &self,
        _p: Point,
        _max_ancestors: usize,
        _budget: Duration,
    ) -> BackendResult<Option<ElementHit>> {
        part_two()
    }

    fn focused_element(&self) -> BackendResult<Option<Element>> {
        part_two()
    }

    fn perform(&self, _id: &ElementId, _action: ElementAction) -> BackendResult<bool> {
        part_two()
    }

    fn find_file_dialog_field(&self, _window: WindowRef) -> BackendResult<Option<ElementId>> {
        part_two()
    }

    // ------------------------------------------------------------------ text

    fn document_text(
        &self,
        _window: WindowRef,
        _max_chars: usize,
    ) -> BackendResult<Option<String>> {
        part_two()
    }

    fn find_in_documents(
        &self,
        _window: WindowRef,
        _query: &str,
        _max_hits: usize,
        _budget: Duration,
    ) -> BackendResult<Vec<DocumentHit>> {
        part_two()
    }

    fn document_line_at(&self, _window: WindowRef, _p: Point) -> BackendResult<Option<String>> {
        part_two()
    }

    fn caret(&self) -> BackendResult<Option<Caret>> {
        Ok(wnd::caret())
    }

    fn has_caret(&self, window: Option<WindowRef>) -> BackendResult<bool> {
        Ok(wnd::has_caret(window))
    }

    fn focus_token(&self) -> BackendResult<u64> {
        Ok(wnd::focus_token())
    }

    // ------------------------------------------------------------------ input

    fn cursor_position(&self) -> BackendResult<Point> {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut pt = POINT::default();
        // SAFETY: `pt` is a live out-parameter.
        unsafe { GetCursorPos(&mut pt) }
            .map_err(|e| util::os_err("could not read the pointer position", &e))?;
        Ok(Point { x: pt.x, y: pt.y })
    }

    fn mouse_move(&self, to: Point) -> BackendResult<()> {
        self.input.move_to(to)
    }

    fn mouse_button(&self, button: MouseButton, down: bool) -> BackendResult<()> {
        self.input.button(button, down)
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
        self.input.key(key, false)
    }

    fn key_up(&self, key: Key) -> BackendResult<()> {
        self.input.key(key, true)
    }

    fn key_chord(&self, keys: &[Key], repeat: u32, guard: InputGuard<'_>) -> BackendResult<()> {
        self.input.chord(keys, repeat, guard)
    }

    fn type_text(&self, text: &str, guard: InputGuard<'_>) -> BackendResult<()> {
        self.input.type_text(text, guard)
    }

    fn input_events(&self) -> u64 {
        self.input.batches()
    }

    fn human_input_idle(&self) -> BackendResult<Duration> {
        use windows::Win32::System::SystemInformation::GetTickCount;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        // SAFETY: `info` is a live LASTINPUTINFO with cbSize set.
        if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
            return Err(BackendError::Os(
                "could not read the last input time".into(),
            ));
        }
        // SAFETY: no arguments, no preconditions.
        let now = unsafe { GetTickCount() };
        // Both are 32-bit millisecond tick counts that wrap together, so the difference wraps too.
        Ok(Duration::from_millis(u64::from(
            now.wrapping_sub(info.dwTime),
        )))
    }

    // ------------------------------------------------------------------ clipboard

    fn clipboard(&self) -> BackendResult<ClipboardContent> {
        clipboard::read()
    }

    fn set_clipboard_text(&self, text: &str) -> BackendResult<()> {
        clipboard::write_text(text)
    }

    fn clipboard_sequence(&self) -> BackendResult<Option<u64>> {
        Ok(clipboard::sequence())
    }

    // ------------------------------------------------------------------ OCR (part 2)

    fn ocr(&self, _image: &Image) -> BackendResult<OcrResult> {
        part_two()
    }

    fn ocr_engine_name(&self) -> String {
        "unavailable (part 2)".to_string()
    }

    // ------------------------------------------------------------------ processes

    fn process_table(&self) -> BackendResult<Vec<ProcessInfo>> {
        process::process_table()
    }

    fn process_info(&self, pid: u32) -> BackendResult<Option<ProcessInfo>> {
        process::process_info(pid)
    }

    fn peer_pid(&self, peer_port: u16, server_port: u16) -> BackendResult<Option<u32>> {
        process::peer_pid(peer_port, server_port)
    }

    // ------------------------------------------------------------------ launching

    fn resolve_app(&self, name: &str) -> BackendResult<Option<AppTarget>> {
        Ok(launch::resolve_app(&self.machine, name))
    }

    fn launch(&self, spec: &LaunchSpec) -> BackendResult<LaunchOutcome> {
        launch::launch(spec)
    }

    // ------------------------------------------------------------------ local time

    fn local_utc_offset(&self, unix_secs: i64) -> BackendResult<i32> {
        timezone::local_utc_offset(unix_secs)
    }

    // ------------------------------------------------------------------ kill hotkey

    fn register_kill_hotkey(
        &self,
        candidates: &[HotkeyChord],
        on_press: Arc<dyn Fn() + Send + Sync>,
    ) -> BackendResult<Option<String>> {
        let mut registered = self
            .hotkey
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // One kill hotkey per process: a second call reports the one already in place.
        if registered.is_some() {
            return Ok(registered.clone());
        }
        let chosen = hotkey::register(candidates, on_press)?;
        registered.clone_from(&chosen);
        Ok(chosen)
    }

    // ------------------------------------------------------------------ provided helpers

    fn window_exists(&self, window: WindowRef) -> bool {
        wnd::is_window(hwnd_of(window))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backend_is_usable_as_a_trait_object_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WindowsBackend>();
        let b: Box<dyn Backend> = Box::new(WindowsBackend::new());
        assert_eq!(b.name(), "windows");
    }

    #[test]
    fn capabilities_say_what_part_one_can_do() {
        let caps = WindowsBackend::new().capabilities();
        assert!(caps.caret && caps.kill_hotkey && caps.clipboard_files && caps.elevation);
        assert!(!caps.accessibility_tree && !caps.ocr && !caps.document_text);
        assert!(!caps.popup_menus && !caps.file_dialog);
    }

    #[test]
    fn the_accessibility_and_ocr_methods_wait_for_part_two() {
        let b = WindowsBackend::new();
        let part2 = BackendError::Unsupported("part 2".into());
        let w = WindowRef::from_raw(1);
        let id = ElementId("x".into());
        let image = Image::solid(1, 1, [0, 0, 0, 255]);
        let budget = Duration::from_millis(1);
        assert_eq!(b.menu_items(budget).unwrap_err(), part2);
        assert_eq!(
            b.element_tree(w, &TreeLimits::snapshot(1)).unwrap_err(),
            part2
        );
        assert_eq!(b.element(&id).unwrap_err(), part2);
        assert_eq!(b.element_window(&id).unwrap_err(), part2);
        assert_eq!(
            b.element_at(Point::new(0, 0), 1, budget).unwrap_err(),
            part2
        );
        assert_eq!(b.focused_element().unwrap_err(), part2);
        assert_eq!(b.perform(&id, ElementAction::Press).unwrap_err(), part2);
        assert_eq!(b.find_file_dialog_field(w).unwrap_err(), part2);
        assert_eq!(b.document_text(w, 10).unwrap_err(), part2);
        assert_eq!(b.find_in_documents(w, "q", 1, budget).unwrap_err(), part2);
        assert_eq!(b.document_line_at(w, Point::new(0, 0)).unwrap_err(), part2);
        assert_eq!(b.ocr(&image).unwrap_err(), part2);
        assert!(b.ocr_engine_name().contains("part 2"));
    }

    #[test]
    fn nothing_has_been_injected_yet() {
        assert_eq!(WindowsBackend::new().input_events(), 0);
    }
}
