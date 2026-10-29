//! An in-memory [`Backend`] for the core's unit tests, available to other crates behind the
//! `testing` feature. It models just enough of a desktop to exercise policy code: windows with a
//! z-order, displays, a screen image, accessibility trees, an input log, a clipboard, OCR output,
//! a process table, launches and a kill hotkey. Every setter is `&self` (interior mutability) so a
//! test can hold the fake behind an `Arc<dyn Backend>` and still script it.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::*;
use crate::keys::Key;

/// One thing the fake was asked to inject, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// `mouse_move`.
    MouseMove(Point),
    /// `mouse_button`.
    MouseButton {
        /// Which button.
        button: MouseButton,
        /// Pressed (true) or released.
        down: bool,
    },
    /// `mouse_click`.
    Click {
        /// Where.
        at: Point,
        /// Which button.
        button: MouseButton,
        /// 1 single, 2 double.
        count: u8,
    },
    /// `mouse_scroll`.
    Scroll {
        /// Where.
        at: Point,
        /// Which way.
        direction: ScrollDirection,
        /// Wheel notches.
        notches: u32,
    },
    /// `mouse_drag`.
    Drag {
        /// Start.
        from: Point,
        /// End.
        to: Point,
    },
    /// `key_down`.
    KeyDown(Key),
    /// `key_up`.
    KeyUp(Key),
    /// One character of `type_text`.
    Type(char),
}

/// Called after a key chord is pressed, with the chord's keys. A test uses it to script what the
/// shortcut does (open a tab, copy to the clipboard, paste into a field).
pub type ChordHook = Arc<dyn Fn(&FakeBackend, &[Key]) + Send + Sync>;

/// Called after a launch is recorded, so a test can make the launched app's window appear.
pub type LaunchHook = Arc<dyn Fn(&FakeBackend, &LaunchSpec) + Send + Sync>;

/// Called after a click is recorded, with where it landed, so a test can script what the click does
/// (a column header re-sorting a list).
pub type ClickHook = Arc<dyn Fn(&FakeBackend, Point) + Send + Sync>;

struct State {
    windows: Vec<WindowInfo>,
    window_info_reads: u64,
    next_window: u64,
    foreground: Option<WindowRef>,
    point_override: Option<Option<WindowRef>>,
    roots: HashMap<WindowRef, WindowRef>,
    hosted: HashMap<WindowRef, String>,
    window_text: HashMap<WindowRef, String>,
    displays: Vec<Display>,
    screens: VecDeque<Image>,
    last_screen: Option<Image>,
    captures: u64,
    /// `capture_preview` shrinks like the Windows backend instead of returning a full capture.
    shrink_previews: bool,
    previews: u64,
    /// [`Backend::settle_in_background`].
    background_settle: bool,
    /// The rectangle of every capture, in order.
    capture_log: Vec<Rect>,
    trees: HashMap<WindowRef, Element>,
    performed: Vec<(ElementId, ElementAction)>,
    /// `insert_text` succeeds (a native text box has focus) and what it inserted.
    direct_insert: bool,
    inserted: Vec<String>,
    focused: Option<Element>,
    documents: HashMap<WindowRef, String>,
    line_at: HashMap<(WindowRef, i32, i32), String>,
    caret: Option<Caret>,
    has_caret: bool,
    window_carets: HashMap<WindowRef, bool>,
    focus_token: u64,
    popups: Vec<WindowRef>,
    menu_items: Vec<MenuItem>,
    file_fields: HashMap<WindowRef, ElementId>,
    cursor: Point,
    events: Vec<InputEvent>,
    batches: u64,
    idle: Duration,
    idle_reads: u64,
    clipboard: ClipboardContent,
    /// Every text `set_clipboard_text` wrote, in order.
    clipboard_writes: Vec<String>,
    clip_seq: u64,
    ocr: OcrResult,
    ocr_available: bool,
    ocr_calls: u64,
    processes: Vec<ProcessInfo>,
    peers: HashMap<(u16, u16), u32>,
    apps: HashMap<String, AppTarget>,
    launches: Vec<LaunchSpec>,
    blocked_hotkeys: Vec<String>,
    hotkey: Option<Arc<dyn Fn() + Send + Sync>>,
    chord_hook: Option<ChordHook>,
    launch_hook: Option<LaunchHook>,
    click_hook: Option<ClickHook>,
    popup_delay: u32,
    menu_delay: u32,
    doc_rects: HashMap<(WindowRef, usize), (Rect, Rect)>,
    chromium_empty: Vec<WindowRef>,
    wakes: Vec<WindowRef>,
    utc_offset: Option<i32>,
    inits: u32,
    capabilities: Capabilities,
    maximized: Vec<WindowRef>,
    clients: HashMap<WindowRef, Rect>,
    window_ops: Vec<(WindowRef, WindowOp)>,
    hold_open: Vec<WindowRef>,
    ignore_ops: bool,
    no_restore: bool,
    max_width: Option<i32>,
    folder_windows: Vec<(String, WindowRef)>,
    shell: ShellMode,
    shell_view: String,
    shell_sort: String,
    shell_fail_sort: bool,
    shell_calls: u32,
}

/// How the fake Shell (Explorer's view interface) behaves for
/// [`Backend::set_folder_view`](super::Backend::set_folder_view).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellMode {
    /// It lists the window and applies what is asked.
    #[default]
    Works,
    /// The interface is out of reach (an elevated caller, say): every call errors.
    Unavailable,
    /// The window never shows up in the interface.
    Absent,
}

/// See the [module docs](self).
pub struct FakeBackend {
    state: Mutex<State>,
    excel: Mutex<super::fake_excel::FakeExcel>,
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeBackend {
    /// A desktop with one 1920x1080 display at scale 1, no windows, nothing on the clipboard.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                windows: Vec::new(),
                window_info_reads: 0,
                next_window: 100,
                foreground: None,
                point_override: None,
                roots: HashMap::new(),
                hosted: HashMap::new(),
                window_text: HashMap::new(),
                displays: vec![Display {
                    id: 1,
                    name: "fake".into(),
                    rect: Rect::from_xywh(0, 0, 1920, 1080),
                    scale: 1.0,
                    primary: true,
                }],
                screens: VecDeque::new(),
                last_screen: None,
                captures: 0,
                shrink_previews: false,
                previews: 0,
                background_settle: false,
                capture_log: Vec::new(),
                trees: HashMap::new(),
                performed: Vec::new(),
                direct_insert: false,
                inserted: Vec::new(),
                focused: None,
                documents: HashMap::new(),
                line_at: HashMap::new(),
                caret: None,
                has_caret: false,
                window_carets: HashMap::new(),
                focus_token: 0,
                popups: Vec::new(),
                menu_items: Vec::new(),
                file_fields: HashMap::new(),
                cursor: Point::default(),
                events: Vec::new(),
                batches: 0,
                idle: Duration::from_secs(3600),
                idle_reads: 0,
                clipboard: ClipboardContent::Empty,
                clipboard_writes: Vec::new(),
                clip_seq: 0,
                ocr: OcrResult::default(),
                ocr_available: true,
                ocr_calls: 0,
                processes: Vec::new(),
                peers: HashMap::new(),
                apps: HashMap::new(),
                launches: Vec::new(),
                blocked_hotkeys: Vec::new(),
                hotkey: None,
                chord_hook: None,
                launch_hook: None,
                click_hook: None,
                popup_delay: 0,
                menu_delay: 0,
                doc_rects: HashMap::new(),
                chromium_empty: Vec::new(),
                wakes: Vec::new(),
                utc_offset: None,
                inits: 0,
                capabilities: Capabilities::all(),
                maximized: Vec::new(),
                clients: HashMap::new(),
                window_ops: Vec::new(),
                hold_open: Vec::new(),
                ignore_ops: false,
                no_restore: false,
                max_width: None,
                folder_windows: Vec::new(),
                shell: ShellMode::Works,
                shell_view: String::new(),
                shell_sort: String::new(),
                shell_fail_sort: false,
                shell_calls: 0,
            }),
            excel: Mutex::new(super::fake_excel::FakeExcel::new()),
        }
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("fake backend state poisoned")
    }

    fn xl(&self) -> MutexGuard<'_, super::fake_excel::FakeExcel> {
        self.excel.lock().expect("fake excel poisoned")
    }

    // ------------------------------------------------------------------ scripting: excel

    /// Pretend Excel is (not) installed.
    pub fn set_excel_available(&self, available: bool) {
        self.xl().available = available;
    }

    /// Every request the `excel` channel got, in order.
    pub fn excel_requests(&self) -> Vec<ExcelRequest> {
        self.xl().requests.clone()
    }

    /// How many times the owned instance was quit.
    pub fn excel_quits(&self) -> u32 {
        self.xl().quits
    }

    /// Is the owned instance running?
    pub fn excel_running(&self) -> bool {
        self.xl().running
    }

    /// The path a workbook was last saved to.
    pub fn excel_saved_as(&self, handle: &str) -> Option<String> {
        self.xl().saved_as(handle)
    }

    // ------------------------------------------------------------------ scripting: windows

    /// Full metadata reads, excluding cheap owner-PID checks.
    pub fn window_info_reads(&self) -> u64 {
        self.st().window_info_reads
    }

    /// Add a window on top of the z-order and return its handle. A `window` of 0 gets a fresh one.
    pub fn add_window(&self, mut info: WindowInfo) -> WindowRef {
        let mut s = self.st();
        if info.window.raw() == 0 {
            s.next_window += 1;
            info.window = WindowRef::from_raw(s.next_window);
        }
        let w = info.window;
        s.windows.insert(0, info);
        w
    }

    /// Remove a window (it closed).
    pub fn close_window(&self, window: WindowRef) {
        let mut s = self.st();
        s.windows.retain(|w| w.window != window);
        if s.foreground == Some(window) {
            s.foreground = None;
        }
    }

    /// Put a window in front (or none).
    pub fn set_foreground(&self, window: Option<WindowRef>) {
        self.st().foreground = window;
    }

    /// Make `window_at_point` answer `value` for every point, whatever the z-order says.
    pub fn set_window_at_point(&self, value: Option<WindowRef>) {
        self.st().point_override = Some(value);
    }

    /// Go back to answering `window_at_point` from the z-order.
    pub fn clear_window_at_point(&self) {
        self.st().point_override = None;
    }

    /// Declare `child` to live inside the top-level window `root`.
    pub fn set_root_window(&self, child: WindowRef, root: WindowRef) {
        self.st().roots.insert(child, root);
    }

    /// Declare that `window` is a frame hosting the app named `app`.
    pub fn set_hosted_app(&self, window: WindowRef, app: &str) {
        self.st().hosted.insert(window, app.to_string());
    }

    /// Script `window_text`.
    pub fn set_window_text(&self, window: WindowRef, text: &str) {
        self.st().window_text.insert(window, text.to_string());
    }

    /// Mutate a window's facts in place.
    pub fn update_window(&self, window: WindowRef, f: impl FnOnce(&mut WindowInfo)) {
        if let Some(w) = self.st().windows.iter_mut().find(|w| w.window == window) {
            f(w);
        }
    }

    // ------------------------------------------------------------------ scripting: screen

    /// Replace the displays.
    pub fn set_displays(&self, displays: Vec<Display>) {
        self.st().displays = displays;
    }

    /// Every capture returns (a crop of) this image, placed at the virtual desktop origin.
    pub fn set_screen(&self, image: Image) {
        self.st().last_screen = Some(image);
    }

    /// Forget the scripted screen: every capture is a flat grey picture of the size asked for.
    pub fn clear_screen(&self) {
        let mut s = self.st();
        s.last_screen = None;
        s.screens.clear();
    }

    /// The next captures return these images in order; the last one then repeats. Used to script
    /// a screen that changes under a settle loop.
    pub fn queue_screens(&self, images: Vec<Image>) {
        let mut s = self.st();
        s.screens = images.into();
    }

    /// The rectangle of every capture taken so far, oldest first (what a crop was asked to cover).
    pub fn captured_rects(&self) -> Vec<Rect> {
        self.st().capture_log.clone()
    }

    /// Make `capture_preview` return shrunk looks (counted by [`Self::preview_count`], not as
    /// captures), as the Windows backend does.
    pub fn set_shrink_previews(&self, on: bool) {
        self.st().shrink_previews = on;
    }

    /// Let a blind call finish its trailing settle on a background thread, as Windows does.
    pub fn set_settle_in_background(&self, on: bool) {
        self.st().background_settle = on;
    }

    /// How many shrunk previews have been taken.
    pub fn preview_count(&self) -> u64 {
        self.st().previews
    }

    /// How many captures have been taken.
    pub fn capture_count(&self) -> u64 {
        self.st().captures
    }

    // ------------------------------------------------------------------ scripting: accessibility

    /// Give a window an accessibility tree (the root is the window element).
    pub fn set_tree(&self, window: WindowRef, root: Element) {
        self.st().trees.insert(window, root);
    }

    /// Script the focused element.
    pub fn set_focused(&self, element: Option<Element>) {
        let mut s = self.st();
        s.focused = element;
        s.focus_token += 1;
    }

    /// Everything `perform` was asked to do.
    pub fn performed(&self) -> Vec<(ElementId, ElementAction)> {
        self.st().performed.clone()
    }

    /// Script whether the focused control takes text directly (`insert_text`).
    pub fn set_direct_insert(&self, value: bool) {
        self.st().direct_insert = value;
    }

    /// Texts written through `insert_text`.
    pub fn inserted(&self) -> Vec<String> {
        self.st().inserted.clone()
    }

    /// Give a window document text.
    pub fn set_document(&self, window: WindowRef, text: &str) {
        self.st().documents.insert(window, text.to_string());
    }

    /// Script `document_line_at` for one point.
    pub fn set_document_line_at(&self, window: WindowRef, p: Point, line: &str) {
        self.st()
            .line_at
            .insert((window, p.x, p.y), line.to_string());
    }

    /// Script the text caret (and so `has_caret`).
    pub fn set_caret(&self, caret: Option<Caret>) {
        let mut s = self.st();
        s.has_caret = caret.is_some();
        s.caret = caret;
    }

    /// Script whether one particular window's GUI thread shows a caret (a read of that window wins
    /// over the general answer).
    pub fn set_window_caret(&self, window: WindowRef, value: bool) {
        self.st().window_carets.insert(window, value);
    }

    /// Script `has_caret` without a position.
    pub fn set_has_caret(&self, value: bool) {
        self.st().has_caret = value;
    }

    /// Make `focus_token` change, as if focus moved.
    pub fn bump_focus(&self) {
        self.st().focus_token += 1;
    }

    /// Script an open context menu: its popup windows and its items.
    pub fn set_popup_menu(&self, popups: Vec<WindowRef>, items: Vec<MenuItem>) {
        let mut s = self.st();
        s.popups = popups;
        s.menu_items = items;
    }

    /// Script the filename field of a file dialog window.
    pub fn set_file_dialog_field(&self, dialog: WindowRef, field: ElementId) {
        self.st().file_fields.insert(dialog, field);
    }

    // ------------------------------------------------------------------ scripting: input and system

    /// Everything injected so far.
    pub fn events(&self) -> Vec<InputEvent> {
        self.st().events.clone()
    }

    /// Forget the input log (the batch counter keeps counting).
    pub fn clear_events(&self) {
        self.st().events.clear();
    }

    /// Everything `type_text` typed, concatenated.
    pub fn typed_text(&self) -> String {
        self.st()
            .events
            .iter()
            .filter_map(|e| {
                if let InputEvent::Type(c) = e {
                    Some(*c)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Script `human_input_idle`.
    pub fn set_human_idle(&self, idle: Duration) {
        self.st().idle = idle;
    }

    /// How many times `human_input_idle` was read.
    pub fn human_idle_reads(&self) -> u64 {
        self.st().idle_reads
    }

    /// Every text the code under test put on the clipboard, oldest first.
    pub fn clipboard_history(&self) -> Vec<String> {
        self.st().clipboard_writes.clone()
    }

    /// Put something on the clipboard (bumps the sequence number).
    pub fn set_clipboard(&self, content: ClipboardContent) {
        let mut s = self.st();
        s.clipboard = content;
        s.clip_seq += 1;
    }

    /// Script what OCR returns for every image.
    pub fn set_ocr(&self, result: OcrResult) {
        self.st().ocr = result;
    }

    /// Make OCR report that no engine exists.
    pub fn set_ocr_available(&self, available: bool) {
        self.st().ocr_available = available;
    }

    /// How many images were OCR'd.
    pub fn ocr_calls(&self) -> u64 {
        self.st().ocr_calls
    }

    /// Script the process table.
    pub fn set_processes(&self, processes: Vec<ProcessInfo>) {
        self.st().processes = processes;
    }

    /// Script the TCP table: the client's local port and the daemon's port map to a pid.
    pub fn set_peer(&self, peer_port: u16, server_port: u16, pid: u32) {
        self.st().peers.insert((peer_port, server_port), pid);
    }

    /// Script what `resolve_app` finds for a (lowercased) name.
    pub fn set_app(&self, name: &str, target: AppTarget) {
        self.st().apps.insert(name.to_lowercase(), target);
    }

    /// Everything `launch` was asked to start.
    pub fn launches(&self) -> Vec<LaunchSpec> {
        self.st().launches.clone()
    }

    /// Make the OS refuse these chord names, as if another app owned them.
    pub fn block_hotkeys(&self, names: &[&str]) {
        self.st().blocked_hotkeys = names.iter().map(|n| n.to_string()).collect();
    }

    /// Press the registered kill hotkey. Returns false when none is registered.
    pub fn fire_kill_hotkey(&self) -> bool {
        let callback = self.st().hotkey.clone();
        match callback {
            Some(cb) => {
                cb();
                true
            }
            None => false,
        }
    }

    /// Run `hook` after every key chord, with the keys that were pressed.
    pub fn set_chord_hook(&self, hook: impl Fn(&FakeBackend, &[Key]) + Send + Sync + 'static) {
        self.st().chord_hook = Some(Arc::new(hook));
    }

    /// Run `hook` after every launch is recorded.
    pub fn set_launch_hook(
        &self,
        hook: impl Fn(&FakeBackend, &LaunchSpec) + Send + Sync + 'static,
    ) {
        self.st().launch_hook = Some(Arc::new(hook));
    }

    /// Script what a click does: `hook` runs after each recorded click.
    pub fn set_click_hook(&self, hook: impl Fn(&FakeBackend, Point) + Send + Sync + 'static) {
        self.st().click_hook = Some(Arc::new(hook));
    }

    /// Make `popup_menus` answer empty for its next `polls` calls, as a menu that opens late.
    pub fn set_popup_delay(&self, polls: u32) {
        self.st().popup_delay = polls;
    }

    /// Make `menu_items` answer empty for its next `reads` calls, as a menu that builds late.
    pub fn set_menu_delay(&self, reads: u32) {
        self.st().menu_delay = reads;
    }

    /// Say where line `line_index` (0-based) of a window's document is on screen: the match's own
    /// box and the whole line's box. Without it a document hit reads as scrolled out of view.
    pub fn set_document_rects(
        &self,
        window: WindowRef,
        line_index: usize,
        match_rect: Rect,
        line_rect: Rect,
    ) {
        self.st()
            .doc_rects
            .insert((window, line_index), (match_rect, line_rect));
    }

    /// Make a window read as an empty Chromium tree until a read allows waking it.
    pub fn set_chromium_empty(&self, window: WindowRef) {
        self.st().chromium_empty.push(window);
    }

    /// The windows `element_tree` woke by raising them.
    pub fn wakes(&self) -> Vec<WindowRef> {
        self.st().wakes.clone()
    }

    /// Change the value of an element that is in a scripted tree.
    pub fn set_element_value(&self, id: &ElementId, value: &str) {
        let mut s = self.st();
        for tree in s.trees.values_mut() {
            if let Some(el) = find_element_mut(tree, id) {
                el.value = Some(value.to_string());
            }
        }
    }

    /// Script `local_utc_offset` (seconds east of UTC). Unset, it is unsupported.
    pub fn set_utc_offset(&self, seconds: i32) {
        self.st().utc_offset = Some(seconds);
    }

    // ------------------------------------------------------------------ scripting: shortcuts

    /// Start a window maximised.
    pub fn set_maximized(&self, window: WindowRef, value: bool) {
        let mut s = self.st();
        s.maximized.retain(|w| *w != window);
        if value {
            s.maximized.push(window);
        }
    }

    /// Script a window's client area (screen coordinates). Unset, it has none.
    pub fn set_client_rect(&self, window: WindowRef, rect: Rect) {
        self.st().clients.insert(window, rect);
    }

    /// A window that stays open when asked to close (a "save changes?" box is up).
    pub fn hold_open_on_close(&self, window: WindowRef) {
        self.st().hold_open.push(window);
    }

    /// Make every `window_op` a no-op that still succeeds (an app that ignores the request).
    pub fn ignore_window_ops(&self, value: bool) {
        self.st().ignore_ops = value;
    }

    /// Make `restore_window` do nothing: a minimised window that stays minimised.
    pub fn refuse_restore(&self, value: bool) {
        self.st().no_restore = value;
    }

    /// Clamp a moved window's width to at most this (an app that limits its own size).
    pub fn limit_window_width(&self, width: i32) {
        self.st().max_width = Some(width);
    }

    /// Every `window_op` the fake was asked for, in order.
    pub fn window_ops(&self) -> Vec<(WindowRef, WindowOp)> {
        self.st().window_ops.clone()
    }

    /// Script `folder_window`: an open window already showing this folder.
    pub fn set_folder_window(&self, folder: &str, window: WindowRef) {
        self.st().folder_windows.push((folder.to_string(), window));
    }

    /// Script how the fake Shell answers `set_folder_view`.
    pub fn set_shell_mode(&self, mode: ShellMode) {
        self.st().shell = mode;
    }

    /// Make the fake Shell refuse to set a sort (the view still applies).
    pub fn fail_shell_sort(&self, value: bool) {
        self.st().shell_fail_sort = value;
    }

    /// The view and sort the fake Shell holds now (`details`, `date_modified desc`), empty if unset.
    pub fn shell_state(&self) -> (String, String) {
        let s = self.st();
        (s.shell_view.clone(), s.shell_sort.clone())
    }

    /// How many `set_folder_view` calls reached the fake Shell.
    pub fn shell_calls(&self) -> u32 {
        self.st().shell_calls
    }

    /// Script `capabilities`.
    pub fn set_capabilities(&self, capabilities: Capabilities) {
        self.st().capabilities = capabilities;
    }

    /// How many times `init` ran.
    pub fn init_count(&self) -> u32 {
        self.st().inits
    }
}

fn find_element<'a>(root: &'a Element, id: &ElementId) -> Option<&'a Element> {
    if &root.id == id {
        return Some(root);
    }
    root.children.iter().find_map(|c| find_element(c, id))
}

fn find_element_mut<'a>(root: &'a mut Element, id: &ElementId) -> Option<&'a mut Element> {
    if &root.id == id {
        return Some(root);
    }
    root.children
        .iter_mut()
        .find_map(|c| find_element_mut(c, id))
}

/// The deepest element containing `p`, with its ancestors nearest first.
fn deepest_at<'a>(
    node: &'a Element,
    p: Point,
    path: &mut Vec<&'a Element>,
) -> Option<(&'a Element, Vec<&'a Element>)> {
    path.push(node);
    for child in &node.children {
        if let Some(hit) = deepest_at(child, p, path) {
            path.pop();
            return Some(hit);
        }
    }
    path.pop();
    if node.rect.is_some_and(|r| r.contains(p)) {
        let ancestors = path.iter().rev().copied().collect();
        return Some((node, ancestors));
    }
    None
}

fn without_children(e: &Element) -> Element {
    Element {
        children: Vec::new(),
        ..e.clone()
    }
}

fn limit_walk(
    node: &Element,
    depth: u32,
    limits: &TreeLimits,
    kept: &mut Vec<Element>,
    truncated: &mut bool,
) {
    for child in &node.children {
        if depth + 1 > limits.max_depth {
            *truncated = true;
            return;
        }
        let keep = match limits.filter {
            ElementFilter::All => true,
            ElementFilter::Interactive => child.is_interactive(),
            ElementFilter::Named => {
                !child.name.trim().is_empty() && child.rect.is_some_and(|r| !r.is_empty())
            }
        };
        if keep {
            if kept.len() >= limits.max_items {
                *truncated = true;
                return;
            }
            kept.push(without_children(child));
        }
        limit_walk(child, depth + 1, limits, kept, truncated);
        if *truncated {
            return;
        }
    }
}

fn prune(
    node: &Element,
    depth: u32,
    max_depth: u32,
    budget: &mut usize,
    truncated: &mut bool,
) -> Element {
    let mut out = without_children(node);
    for child in &node.children {
        if depth + 1 > max_depth || *budget == 0 {
            *truncated = true;
            break;
        }
        *budget -= 1;
        out.children
            .push(prune(child, depth + 1, max_depth, budget, truncated));
    }
    out
}

impl Backend for FakeBackend {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn excel_available(&self) -> bool {
        self.xl().available
    }

    fn excel(&self, req: &ExcelRequest) -> BackendResult<ExcelReply> {
        self.xl().run(req)
    }

    fn excel_shutdown(&self) {
        self.xl().shutdown();
    }

    fn capabilities(&self) -> Capabilities {
        self.st().capabilities
    }

    fn init(&self) -> BackendResult<()> {
        self.st().inits += 1;
        Ok(())
    }

    fn displays(&self) -> BackendResult<Vec<Display>> {
        Ok(self.st().displays.clone())
    }

    fn virtual_screen(&self) -> BackendResult<Rect> {
        let s = self.st();
        let mut it = s.displays.iter().map(|d| d.rect);
        let first = it
            .next()
            .ok_or_else(|| BackendError::Os("no display".into()))?;
        Ok(it.fold(first, |a, b| {
            Rect::new(
                a.left.min(b.left),
                a.top.min(b.top),
                a.right.max(b.right),
                a.bottom.max(b.bottom),
            )
        }))
    }

    fn settle_in_background(&self) -> bool {
        self.st().background_settle
    }

    fn capture_preview(&self, region: Rect, width: u32, height: u32) -> BackendResult<Image> {
        let full = self.capture(CaptureTarget::Region(region))?;
        let mut s = self.st();
        if !s.shrink_previews {
            return Ok(full);
        }
        s.captures -= 1;
        s.capture_log.pop();
        s.previews += 1;
        drop(s);
        crate::imaging::resize(&full, width, height).map_err(|e| BackendError::Os(e.to_string()))
    }

    fn capture(&self, target: CaptureTarget) -> BackendResult<Image> {
        let vs = self.virtual_screen()?;
        let (rect, scale) = {
            let s = self.st();
            match target {
                CaptureTarget::VirtualDesktop => (vs, 1.0),
                CaptureTarget::Display(id) => {
                    let d = s
                        .displays
                        .iter()
                        .find(|d| d.id == id)
                        .ok_or_else(|| BackendError::Os(format!("no display {id}")))?;
                    (d.rect, d.scale)
                }
                CaptureTarget::Region(r) => (r, 1.0),
                CaptureTarget::Window(w) => {
                    let info = s
                        .windows
                        .iter()
                        .find(|i| i.window == w)
                        .ok_or(BackendError::WindowGone)?;
                    (info.rect, 1.0)
                }
            }
        };
        let mut s = self.st();
        s.captures += 1;
        s.capture_log.push(rect);
        let source = if s.screens.len() > 1 {
            s.screens.pop_front()
        } else if let Some(last) = s.screens.front() {
            Some(last.clone())
        } else {
            s.last_screen.clone()
        };
        let width = rect.width().max(1) as u32;
        let height = rect.height().max(1) as u32;
        let image = match source {
            Some(src) => {
                let x = (rect.left - vs.left).max(0) as u32;
                let y = (rect.top - vs.top).max(0) as u32;
                src.crop(x, y, width, height)
                    .unwrap_or_else(|| Image::solid(width, height, [128, 128, 128, 255]))
            }
            None => Image::solid(width, height, [128, 128, 128, 255]),
        };
        Ok(image.with_scale(scale).with_origin(Point {
            x: rect.left,
            y: rect.top,
        }))
    }

    fn list_windows(&self, include_hidden: bool) -> BackendResult<Vec<WindowInfo>> {
        let s = self.st();
        Ok(s.windows
            .iter()
            .filter(|w| include_hidden || !w.hidden)
            .map(|w| WindowInfo {
                foreground: s.foreground == Some(w.window),
                ..w.clone()
            })
            .collect())
    }

    fn window_info(&self, window: WindowRef) -> BackendResult<WindowInfo> {
        let mut s = self.st();
        s.window_info_reads += 1;
        s.windows
            .iter()
            .find(|w| w.window == window)
            .map(|w| WindowInfo {
                foreground: s.foreground == Some(window),
                ..w.clone()
            })
            .ok_or(BackendError::WindowGone)
    }

    fn window_pid(&self, window: WindowRef) -> BackendResult<u32> {
        self.st()
            .windows
            .iter()
            .find(|item| item.window == window)
            .map(|item| item.pid)
            .filter(|pid| *pid != 0)
            .ok_or(BackendError::WindowGone)
    }

    fn foreground_window(&self) -> BackendResult<Option<WindowRef>> {
        Ok(self.st().foreground)
    }

    fn window_at_point(&self, p: Point) -> BackendResult<Option<WindowRef>> {
        let s = self.st();
        if let Some(forced) = s.point_override {
            return Ok(forced);
        }
        Ok(s.windows
            .iter()
            .find(|w| !w.minimized && !w.hidden && w.rect.contains(p))
            .map(|w| w.window))
    }

    fn root_window(&self, window: WindowRef) -> BackendResult<WindowRef> {
        Ok(self.st().roots.get(&window).copied().unwrap_or(window))
    }

    fn owner_chain(&self, window: WindowRef) -> BackendResult<Vec<WindowRef>> {
        let s = self.st();
        let mut chain = Vec::new();
        let mut current = window;
        for _ in 0..8 {
            let owner = s
                .windows
                .iter()
                .find(|w| w.window == current)
                .and_then(|w| w.owner);
            match owner {
                Some(o) if o != current && !chain.contains(&o) => {
                    chain.push(o);
                    current = o;
                }
                _ => break,
            }
        }
        Ok(chain)
    }

    fn hosted_app(&self, window: WindowRef) -> BackendResult<Option<String>> {
        Ok(self.st().hosted.get(&window).cloned())
    }

    fn window_text(&self, window: WindowRef) -> BackendResult<String> {
        let s = self.st();
        if let Some(t) = s.window_text.get(&window) {
            return Ok(t.clone());
        }
        s.windows
            .iter()
            .find(|w| w.window == window)
            .map(|w| w.title.clone())
            .ok_or(BackendError::WindowGone)
    }

    fn bring_to_foreground(&self, window: WindowRef) -> BackendResult<()> {
        let mut s = self.st();
        let idx = s
            .windows
            .iter()
            .position(|w| w.window == window)
            .ok_or(BackendError::WindowGone)?;
        let mut info = s.windows.remove(idx);
        info.minimized = false;
        s.windows.insert(0, info);
        s.foreground = Some(window);
        s.focus_token += 1;
        Ok(())
    }

    fn restore_window(&self, window: WindowRef, _timeout: Duration) -> BackendResult<()> {
        let mut s = self.st();
        if s.no_restore {
            return Ok(());
        }
        let info = s
            .windows
            .iter_mut()
            .find(|w| w.window == window)
            .ok_or(BackendError::WindowGone)?;
        info.minimized = false;
        Ok(())
    }

    fn wait_window_ready(&self, window: WindowRef, _timeout: Duration) -> BackendResult<()> {
        self.window_info(window).map(|_| ())
    }

    fn popup_menus(&self) -> BackendResult<Vec<WindowRef>> {
        let mut s = self.st();
        if s.popup_delay > 0 {
            s.popup_delay -= 1;
            return Ok(Vec::new());
        }
        Ok(s.popups.clone())
    }

    fn menu_items(&self, _budget: Duration) -> BackendResult<Vec<MenuItem>> {
        let mut s = self.st();
        if s.menu_delay > 0 {
            s.menu_delay -= 1;
            return Ok(Vec::new());
        }
        Ok(s.menu_items.clone())
    }

    fn element_tree(&self, window: WindowRef, limits: &TreeLimits) -> BackendResult<TreeSnapshot> {
        let mut s = self.st();
        let info = s
            .windows
            .iter()
            .find(|w| w.window == window)
            .ok_or(BackendError::WindowGone)?;
        let title = info.title.clone();
        if s.chromium_empty.contains(&window) {
            // An empty Chromium tree: only a raise wakes it; the tree then stays built.
            if limits.wake == WakePolicy::Never {
                return Ok(TreeSnapshot {
                    window,
                    title: title.clone(),
                    root: Element::new(&format!("win-{}", window.raw()), Role::Window, &title),
                    truncated: false,
                    wake_hint: true,
                });
            }
            s.chromium_empty.retain(|w| *w != window);
            s.wakes.push(window);
        }
        let tree = s.trees.get(&window).cloned().unwrap_or_else(|| {
            Element::new(&format!("win-{}", window.raw()), Role::Window, &title)
        });
        let mut truncated = false;
        let root = if limits.flatten {
            let mut kept = Vec::new();
            limit_walk(&tree, 0, limits, &mut kept, &mut truncated);
            Element {
                children: kept,
                ..without_children(&tree)
            }
        } else {
            let mut budget = limits.max_items;
            prune(&tree, 0, limits.max_depth, &mut budget, &mut truncated)
        };
        Ok(TreeSnapshot {
            window,
            title,
            root,
            truncated,
            wake_hint: false,
        })
    }

    fn element(&self, id: &ElementId) -> BackendResult<Element> {
        let s = self.st();
        s.trees
            .values()
            .find_map(|t| find_element(t, id))
            .or_else(|| s.focused.as_ref().filter(|f| &f.id == id))
            .map(without_children)
            .ok_or(BackendError::ElementGone)
    }

    fn element_window(&self, id: &ElementId) -> BackendResult<WindowRef> {
        let s = self.st();
        s.trees
            .iter()
            .find(|(_, t)| find_element(t, id).is_some())
            .map(|(w, _)| *w)
            .ok_or(BackendError::ElementGone)
    }

    fn element_at(
        &self,
        p: Point,
        max_ancestors: usize,
        _budget: Duration,
    ) -> BackendResult<Option<ElementHit>> {
        let s = self.st();
        for tree in s.trees.values() {
            let mut path = Vec::new();
            if let Some((hit, ancestors)) = deepest_at(tree, p, &mut path) {
                return Ok(Some(ElementHit {
                    element: without_children(hit),
                    ancestors: ancestors
                        .into_iter()
                        .take(max_ancestors)
                        .map(without_children)
                        .collect(),
                }));
            }
        }
        Ok(None)
    }

    fn focused_element(&self) -> BackendResult<Option<Element>> {
        Ok(self.st().focused.clone().map(|mut e| {
            if e.password {
                e.value = None;
            }
            e
        }))
    }

    fn perform(&self, id: &ElementId, action: ElementAction) -> BackendResult<bool> {
        let mut s = self.st();
        s.performed.push((id.clone(), action.clone()));
        let tree = s
            .trees
            .values_mut()
            .find(|t| find_element(t, id).is_some())
            .ok_or(BackendError::ElementGone)?;
        let el = find_element_mut(tree, id).ok_or(BackendError::ElementGone)?;
        match action {
            ElementAction::Press => Ok(el.affordances.iter().any(|a| {
                matches!(
                    a,
                    Affordance::Invoke
                        | Affordance::Toggle
                        | Affordance::Select
                        | Affordance::ExpandCollapse
                )
            })),
            ElementAction::Focus => {
                el.focused = true;
                Ok(true)
            }
            ElementAction::SetValue(v) => {
                if el.affordances.contains(&Affordance::Value) {
                    el.value = Some(v);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }

    fn find_file_dialog_field(&self, window: WindowRef) -> BackendResult<Option<ElementId>> {
        Ok(self.st().file_fields.get(&window).cloned())
    }

    fn document_text(&self, window: WindowRef, max_chars: usize) -> BackendResult<Option<String>> {
        Ok(self
            .st()
            .documents
            .get(&window)
            .map(|t| t.chars().take(max_chars).collect()))
    }

    fn find_in_documents(
        &self,
        window: WindowRef,
        query: &str,
        max_hits: usize,
        _budget: Duration,
    ) -> BackendResult<Vec<DocumentHit>> {
        let s = self.st();
        let Some(text) = s.documents.get(&window) else {
            return Ok(Vec::new());
        };
        let needle = query.to_lowercase();
        Ok(text
            .lines()
            .enumerate()
            .filter(|(_, l)| l.to_lowercase().contains(&needle))
            .take(max_hits)
            .map(|(i, l)| {
                let rects = s.doc_rects.get(&(window, i));
                DocumentHit {
                    line: l.split_whitespace().collect::<Vec<_>>().join(" "),
                    match_rect: rects.map(|r| r.0),
                    line_rect: rects.map(|r| r.1),
                }
            })
            .collect())
    }

    fn document_line_at(&self, window: WindowRef, p: Point) -> BackendResult<Option<String>> {
        Ok(self.st().line_at.get(&(window, p.x, p.y)).cloned())
    }

    fn caret(&self) -> BackendResult<Option<Caret>> {
        Ok(self.st().caret)
    }

    fn has_caret(&self, window: Option<WindowRef>) -> BackendResult<bool> {
        let s = self.st();
        Ok(window
            .and_then(|w| s.window_carets.get(&w).copied())
            .unwrap_or(s.has_caret))
    }

    fn focus_token(&self) -> BackendResult<u64> {
        Ok(self.st().focus_token)
    }

    fn cursor_position(&self) -> BackendResult<Point> {
        Ok(self.st().cursor)
    }

    fn mouse_move(&self, to: Point) -> BackendResult<()> {
        let mut s = self.st();
        s.cursor = to;
        s.batches += 1;
        s.events.push(InputEvent::MouseMove(to));
        Ok(())
    }

    fn mouse_button(&self, button: MouseButton, down: bool) -> BackendResult<()> {
        let mut s = self.st();
        s.batches += 1;
        s.events.push(InputEvent::MouseButton { button, down });
        Ok(())
    }

    fn mouse_click(
        &self,
        at: Point,
        button: MouseButton,
        count: u8,
        guard: InputGuard<'_>,
    ) -> BackendResult<()> {
        guard()?;
        let hook = {
            let mut s = self.st();
            s.cursor = at;
            s.batches += 1 + 2 * u64::from(count);
            s.events.push(InputEvent::Click { at, button, count });
            s.click_hook.clone()
        };
        if let Some(hook) = hook {
            hook(self, at);
        }
        Ok(())
    }

    fn mouse_scroll(
        &self,
        at: Point,
        direction: ScrollDirection,
        notches: u32,
        guard: InputGuard<'_>,
    ) -> BackendResult<()> {
        guard()?;
        let mut s = self.st();
        s.cursor = at;
        s.batches += 2;
        s.events.push(InputEvent::Scroll {
            at,
            direction,
            notches,
        });
        Ok(())
    }

    fn mouse_drag(&self, from: Point, to: Point, guard: InputGuard<'_>) -> BackendResult<()> {
        guard()?;
        let mut s = self.st();
        s.cursor = to;
        s.batches += 11;
        s.events.push(InputEvent::Drag { from, to });
        Ok(())
    }

    fn key_down(&self, key: Key) -> BackendResult<()> {
        let mut s = self.st();
        s.batches += 1;
        s.events.push(InputEvent::KeyDown(key));
        Ok(())
    }

    fn key_up(&self, key: Key) -> BackendResult<()> {
        let mut s = self.st();
        s.batches += 1;
        s.events.push(InputEvent::KeyUp(key));
        Ok(())
    }

    fn key_chord(&self, keys: &[Key], repeat: u32, guard: InputGuard<'_>) -> BackendResult<()> {
        for _ in 0..repeat.max(1) {
            guard()?;
            for key in keys {
                self.key_down(*key)?;
            }
            for key in keys.iter().rev() {
                self.key_up(*key)?;
            }
            let hook = self.st().chord_hook.clone();
            if let Some(hook) = hook {
                hook(self, keys);
            }
        }
        Ok(())
    }

    fn type_text(&self, text: &str, guard: InputGuard<'_>) -> BackendResult<()> {
        for ch in text.chars() {
            guard()?;
            let mut s = self.st();
            s.batches += ch.len_utf16() as u64;
            s.events.push(InputEvent::Type(ch));
        }
        Ok(())
    }

    fn insert_text(&self, text: &str) -> BackendResult<bool> {
        let mut s = self.st();
        if !s.direct_insert {
            return Ok(false);
        }
        s.batches += 1;
        s.inserted.push(text.to_string());
        Ok(true)
    }

    fn input_events(&self) -> u64 {
        self.st().batches
    }

    fn human_input_idle(&self) -> BackendResult<Duration> {
        let mut s = self.st();
        s.idle_reads += 1;
        Ok(s.idle)
    }

    fn clipboard(&self) -> BackendResult<ClipboardContent> {
        Ok(self.st().clipboard.clone())
    }

    fn set_clipboard_text(&self, text: &str) -> BackendResult<()> {
        let mut s = self.st();
        s.clipboard = ClipboardContent::Text(text.to_string());
        s.clipboard_writes.push(text.to_string());
        s.clip_seq += 1;
        Ok(())
    }

    fn clipboard_sequence(&self) -> BackendResult<Option<u64>> {
        Ok(Some(self.st().clip_seq))
    }

    fn ocr(&self, _image: &Image) -> BackendResult<OcrResult> {
        let mut s = self.st();
        if !s.ocr_available {
            return Err(BackendError::Unsupported("no OCR engine".into()));
        }
        s.ocr_calls += 1;
        Ok(s.ocr.clone())
    }

    fn ocr_engine_name(&self) -> String {
        if self.st().ocr_available {
            "fake".into()
        } else {
            "untried".into()
        }
    }

    fn process_table(&self) -> BackendResult<Vec<ProcessInfo>> {
        Ok(self.st().processes.clone())
    }

    fn process_info(&self, pid: u32) -> BackendResult<Option<ProcessInfo>> {
        Ok(self.st().processes.iter().find(|p| p.pid == pid).cloned())
    }

    fn peer_pid(&self, peer_port: u16, server_port: u16) -> BackendResult<Option<u32>> {
        Ok(self.st().peers.get(&(peer_port, server_port)).copied())
    }

    fn resolve_app(&self, name: &str) -> BackendResult<Option<AppTarget>> {
        Ok(self.st().apps.get(&name.to_lowercase()).cloned())
    }

    fn launch(&self, spec: &LaunchSpec) -> BackendResult<LaunchOutcome> {
        let (pid, hook) = {
            let mut s = self.st();
            s.launches.push(spec.clone());
            s.next_window += 1;
            (s.next_window as u32, s.launch_hook.clone())
        };
        if let Some(hook) = hook {
            hook(self, spec);
        }
        Ok(LaunchOutcome { pid: Some(pid) })
    }

    fn window_geometry(&self, window: WindowRef) -> BackendResult<WindowGeometry> {
        let info = self.window_info(window)?;
        let s = self.st();
        let state = if info.minimized {
            WindowState::Minimized
        } else if s.maximized.contains(&window) {
            WindowState::Maximized
        } else {
            WindowState::Normal
        };
        Ok(WindowGeometry {
            frame: info.rect,
            visible: info.rect,
            client: s.clients.get(&window).copied(),
            state,
        })
    }

    fn window_op(&self, window: WindowRef, op: WindowOp) -> BackendResult<()> {
        let mut s = self.st();
        if !s.windows.iter().any(|w| w.window == window) {
            return Err(BackendError::WindowGone);
        }
        s.window_ops.push((window, op));
        if s.ignore_ops {
            return Ok(());
        }
        match op {
            WindowOp::Close => {
                if !s.hold_open.contains(&window) {
                    s.windows.retain(|w| w.window != window);
                    if s.foreground == Some(window) {
                        s.foreground = None;
                    }
                }
            }
            WindowOp::Minimize | WindowOp::Maximize | WindowOp::Restore => {
                s.maximized.retain(|w| *w != window);
                if op == WindowOp::Maximize {
                    s.maximized.push(window);
                }
                if let Some(w) = s.windows.iter_mut().find(|w| w.window == window) {
                    w.minimized = op == WindowOp::Minimize;
                }
            }
            WindowOp::Move(mut rect) => {
                if let Some(max) = s.max_width {
                    rect.right = rect.left + rect.width().min(max);
                }
                if let Some(w) = s.windows.iter_mut().find(|w| w.window == window) {
                    w.rect = rect;
                }
            }
        }
        Ok(())
    }

    fn folder_window(&self, folder: &str) -> BackendResult<Option<WindowRef>> {
        let s = self.st();
        if s.shell == ShellMode::Unavailable {
            return Err(BackendError::Os("the Shell is out of reach".into()));
        }
        Ok(s.folder_windows
            .iter()
            .find(|(f, _)| f.eq_ignore_ascii_case(folder))
            .map(|(_, w)| *w))
    }

    fn set_folder_view(
        &self,
        _window: WindowRef,
        _folder: &str,
        view: Option<FolderView>,
        sort: Option<FolderSort>,
        _timeout: Duration,
    ) -> BackendResult<Option<FolderViewState>> {
        let mut s = self.st();
        s.shell_calls += 1;
        match s.shell {
            ShellMode::Unavailable => Err(BackendError::Os("E_ACCESSDENIED".into())),
            ShellMode::Absent => Ok(None),
            ShellMode::Works => {
                let mut state = FolderViewState::default();
                if let Some(v) = view {
                    s.shell_view = v.name().to_string();
                    state.view = s.shell_view.clone();
                }
                if let Some(o) = sort {
                    if s.shell_fail_sort {
                        state.errors.push("sort: E_FAIL".into());
                    } else {
                        s.shell_sort = o.describe();
                        state.sort = s.shell_sort.clone();
                    }
                }
                Ok(Some(state))
            }
        }
    }

    fn local_utc_offset(&self, _unix_secs: i64) -> BackendResult<i32> {
        self.st()
            .utc_offset
            .ok_or_else(|| BackendError::Unsupported("local time zone".into()))
    }

    fn register_kill_hotkey(
        &self,
        candidates: &[HotkeyChord],
        on_press: Arc<dyn Fn() + Send + Sync>,
    ) -> BackendResult<Option<String>> {
        let mut s = self.st();
        for chord in candidates {
            if !s.blocked_hotkeys.contains(&chord.name) {
                s.hotkey = Some(on_press);
                return Ok(Some(chord.name.clone()));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{InputGuard, no_guard};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn win(title: &str, app: &str, rect: Rect) -> WindowInfo {
        WindowInfo::basic(title, app, 100, rect)
    }

    #[test]
    fn the_backend_is_object_safe_and_thread_safe() {
        fn needs_send_sync<T: Send + Sync + ?Sized>(_: &T) {}
        let b: Arc<dyn Backend> = Arc::new(FakeBackend::new());
        needs_send_sync(&*b);
        assert_eq!(b.name(), "fake");
        assert!(b.capabilities().ocr);
        b.init().unwrap();
        b.init().unwrap();
    }

    #[test]
    fn windows_have_a_z_order_and_a_foreground() {
        let b = FakeBackend::new();
        let back = b.add_window(win("Back", "a.exe", Rect::new(0, 0, 800, 600)));
        let front = b.add_window(win("Front", "b.exe", Rect::new(100, 100, 500, 400)));
        assert_ne!(back, front);
        assert_eq!(
            b.window_at_point(Point::new(200, 200)).unwrap(),
            Some(front)
        );
        assert_eq!(b.window_at_point(Point::new(10, 10)).unwrap(), Some(back));
        assert_eq!(b.window_at_point(Point::new(1000, 1000)).unwrap(), None);
        b.bring_to_foreground(back).unwrap();
        assert_eq!(b.foreground_window().unwrap(), Some(back));
        assert_eq!(
            b.window_at_point(Point::new(200, 200)).unwrap(),
            Some(back),
            "raised to the top"
        );
        let listed = b.list_windows(false).unwrap();
        assert_eq!(listed.iter().filter(|w| w.foreground).count(), 1);
        assert_eq!(listed[0].title, "Back");
        b.close_window(back);
        assert_eq!(b.window_info(back), Err(BackendError::WindowGone));
        assert!(!b.window_exists(back));
        assert_eq!(b.foreground_window().unwrap(), None);
    }

    #[test]
    fn hidden_windows_are_listed_only_on_request_and_minimised_ones_do_not_catch_clicks() {
        let b = FakeBackend::new();
        let mut hidden = win("Ghost", "uwp.exe", Rect::new(0, 0, 100, 100));
        hidden.hidden = true;
        b.add_window(hidden);
        let mut min = win("Min", "m.exe", Rect::new(0, 0, 100, 100));
        min.minimized = true;
        b.add_window(min);
        assert_eq!(b.list_windows(false).unwrap().len(), 1);
        assert_eq!(b.list_windows(true).unwrap().len(), 2);
        assert_eq!(b.window_at_point(Point::new(5, 5)).unwrap(), None);
    }

    #[test]
    fn restoring_unminimises_and_the_override_beats_the_z_order() {
        let b = FakeBackend::new();
        let mut m = win("Min", "m.exe", Rect::new(0, 0, 100, 100));
        m.minimized = true;
        let w = b.add_window(m);
        b.restore_window(w, Duration::from_secs(1)).unwrap();
        assert!(!b.window_info(w).unwrap().minimized);
        b.set_window_at_point(None);
        assert_eq!(b.window_at_point(Point::new(5, 5)).unwrap(), None);
        b.clear_window_at_point();
        assert_eq!(b.window_at_point(Point::new(5, 5)).unwrap(), Some(w));
    }

    #[test]
    fn owner_chains_roots_and_same_window() {
        let b = FakeBackend::new();
        let main = b.add_window(win("Main", "app.exe", Rect::new(0, 0, 10, 10)));
        let mut dlg = win("Dlg", "app.exe", Rect::new(0, 0, 10, 10));
        dlg.owner = Some(main);
        let dlg = b.add_window(dlg);
        assert_eq!(b.owner_chain(dlg).unwrap(), [main]);
        assert!(b.owner_chain(main).unwrap().is_empty());
        let child = WindowRef::from_raw(999);
        b.set_root_window(child, main);
        assert!(b.same_window(child, main));
        assert!(!b.same_window(dlg, main));
        assert!(b.same_window(main, main));
    }

    #[test]
    fn visible_fraction_samples_the_z_order() {
        let b = FakeBackend::new();
        let back = b.add_window(win("Back", "a.exe", Rect::new(0, 0, 100, 100)));
        assert_eq!(
            b.visible_fraction(back, Rect::new(0, 0, 100, 100), 5)
                .unwrap(),
            1.0
        );
        b.add_window(win("Cover", "b.exe", Rect::new(0, 0, 50, 100)));
        let f = b
            .visible_fraction(back, Rect::new(0, 0, 100, 100), 4)
            .unwrap();
        assert!((f - 0.5).abs() < 1e-9, "{f}");
        assert_eq!(
            b.visible_fraction(back, Rect::new(5, 5, 5, 5), 5).unwrap(),
            0.0
        );
    }

    #[test]
    fn displays_and_the_virtual_screen() {
        let b = FakeBackend::new();
        assert!(b.point_on_screen(Point::new(1919, 1079)).unwrap());
        assert!(!b.point_on_screen(Point::new(1920, 0)).unwrap());
        b.set_displays(vec![
            Display {
                id: 1,
                name: String::new(),
                rect: Rect::from_xywh(0, 0, 1000, 800),
                scale: 1.0,
                primary: true,
            },
            Display {
                id: 2,
                name: String::new(),
                rect: Rect::from_xywh(-500, 0, 500, 800),
                scale: 2.0,
                primary: false,
            },
        ]);
        assert_eq!(b.virtual_screen().unwrap(), Rect::new(-500, 0, 1000, 800));
        assert!(b.point_on_screen(Point::new(-1, 0)).unwrap());
    }

    #[test]
    fn capture_crops_the_scripted_screen_and_reports_origin_and_scale() {
        let b = FakeBackend::new();
        b.set_displays(vec![Display {
            id: 1,
            name: String::new(),
            rect: Rect::from_xywh(0, 0, 4, 4),
            scale: 1.0,
            primary: true,
        }]);
        let mut screen = Image::solid(4, 4, [0, 0, 0, 255]);
        screen.put_pixel(2, 1, [10, 20, 30, 255]);
        b.set_screen(screen);
        let img = b
            .capture(CaptureTarget::Region(Rect::new(1, 1, 4, 3)))
            .unwrap();
        assert_eq!((img.width(), img.height()), (3, 2));
        assert_eq!(img.pixel(1, 0), Some([10, 20, 30, 255]));
        assert_eq!(img.origin(), Point::new(1, 1));
        assert_eq!(b.capture_count(), 1);
        let full = b.capture(CaptureTarget::VirtualDesktop).unwrap();
        assert_eq!((full.width(), full.height()), (4, 4));
        assert!(b.capture(CaptureTarget::Display(9)).is_err());
        assert!(
            b.capture(CaptureTarget::Window(WindowRef::from_raw(1)))
                .is_err()
        );
    }

    #[test]
    fn queued_screens_play_in_order_and_the_last_repeats() {
        let b = FakeBackend::new();
        b.set_displays(vec![Display {
            id: 1,
            name: String::new(),
            rect: Rect::from_xywh(0, 0, 2, 2),
            scale: 1.0,
            primary: true,
        }]);
        let a = Image::solid(2, 2, [1, 1, 1, 255]);
        let c = Image::solid(2, 2, [2, 2, 2, 255]);
        b.queue_screens(vec![a, c]);
        let px = |b: &FakeBackend| {
            b.capture(CaptureTarget::VirtualDesktop)
                .unwrap()
                .pixel(0, 0)
                .unwrap()[0]
        };
        assert_eq!([px(&b), px(&b), px(&b), px(&b)], [1, 2, 2, 2]);
    }

    #[test]
    fn an_unscripted_screen_is_grey_and_window_capture_uses_its_rect() {
        let b = FakeBackend::new();
        let w = b.add_window(win("W", "w.exe", Rect::new(10, 10, 30, 20)));
        let img = b.capture(CaptureTarget::Window(w)).unwrap();
        assert_eq!((img.width(), img.height()), (20, 10));
        assert_eq!(img.pixel(0, 0), Some([128, 128, 128, 255]));
        assert_eq!(img.origin(), Point::new(10, 10));
    }

    fn sample_tree() -> Element {
        let mut ok = Element::new("ok", Role::Button, "OK").at(Rect::new(10, 10, 50, 30));
        ok.affordances.push(Affordance::Invoke);
        let mut name = Element::new("name", Role::Edit, "Name")
            .at(Rect::new(10, 40, 200, 60))
            .valued("old");
        name.affordances.push(Affordance::Value);
        let label = Element::new("label", Role::Text, "Hello").at(Rect::new(0, 0, 5, 5));
        let silent = Element::new("silent", Role::Pane, "").at(Rect::new(0, 0, 5, 5));
        Element::new("root", Role::Window, "Win")
            .at(Rect::new(0, 0, 300, 300))
            .with_child(
                Element::new("pane", Role::Pane, "")
                    .at(Rect::new(0, 0, 300, 100))
                    .with_child(ok)
                    .with_child(name),
            )
            .with_child(label)
            .with_child(silent)
    }

    #[test]
    fn element_trees_honour_filter_flatten_and_limits() {
        let b = FakeBackend::new();
        let w = b.add_window(win("Win", "w.exe", Rect::new(0, 0, 300, 300)));
        b.set_tree(w, sample_tree());

        let snap = b.element_tree(w, &TreeLimits::snapshot(10)).unwrap();
        let ids: Vec<&str> = snap.root.children.iter().map(|e| e.id.0.as_str()).collect();
        assert_eq!(
            ids,
            ["ok", "name"],
            "interactive only, flat, document order"
        );
        assert!(!snap.truncated);
        assert!(snap.root.children.iter().all(|c| c.children.is_empty()));

        let capped = b.element_tree(w, &TreeLimits::snapshot(1)).unwrap();
        assert_eq!(capped.root.children.len(), 1);
        assert!(capped.truncated);

        let named = b
            .element_tree(w, &TreeLimits::named(Duration::from_millis(300)))
            .unwrap();
        let ids: Vec<&str> = named
            .root
            .children
            .iter()
            .map(|e| e.id.0.as_str())
            .collect();
        assert_eq!(
            ids,
            ["ok", "name", "label"],
            "named elements with a real rect; the nameless pane is dropped"
        );

        let mut shallow = TreeLimits::snapshot(100);
        shallow.max_depth = 1;
        let cut = b.element_tree(w, &shallow).unwrap();
        assert!(
            cut.root.children.is_empty() && cut.truncated,
            "buttons live at depth 2"
        );

        let mut nested = TreeLimits::snapshot(100);
        nested.flatten = false;
        nested.filter = ElementFilter::All;
        let tree = b.element_tree(w, &nested).unwrap();
        assert_eq!(tree.root.children.len(), 3);
        assert_eq!(tree.root.children[0].children.len(), 2);

        assert!(b.element_tree(WindowRef::from_raw(5), &nested).is_err());
    }

    #[test]
    fn a_window_with_no_tree_reads_as_an_empty_root() {
        let b = FakeBackend::new();
        let w = b.add_window(win("Empty", "w.exe", Rect::new(0, 0, 10, 10)));
        let snap = b.element_tree(w, &TreeLimits::snapshot(10)).unwrap();
        assert!(snap.root.children.is_empty() && snap.title == "Empty");
    }

    #[test]
    fn elements_are_found_by_id_point_and_window() {
        let b = FakeBackend::new();
        let w = b.add_window(win("Win", "w.exe", Rect::new(0, 0, 300, 300)));
        b.set_tree(w, sample_tree());
        let ok = b.element(&ElementId("ok".into())).unwrap();
        assert_eq!(ok.name, "OK");
        assert!(ok.children.is_empty());
        assert_eq!(
            b.element(&ElementId("nope".into())),
            Err(BackendError::ElementGone)
        );
        assert_eq!(b.element_window(&ElementId("name".into())).unwrap(), w);
        let hit = b
            .element_at(Point::new(20, 20), 3, Duration::from_millis(250))
            .unwrap()
            .unwrap();
        assert_eq!(hit.element.id.0, "ok");
        let ancestors: Vec<&str> = hit.ancestors.iter().map(|e| e.id.0.as_str()).collect();
        assert_eq!(ancestors, ["pane", "root"], "nearest first");
        assert!(
            b.element_at(Point::new(900, 900), 3, Duration::from_millis(250))
                .unwrap()
                .is_none()
        );
        let one = b
            .element_at(Point::new(20, 20), 1, Duration::from_millis(250))
            .unwrap()
            .unwrap();
        assert_eq!(one.ancestors.len(), 1);
    }

    #[test]
    fn perform_records_and_reports_whether_it_could() {
        let b = FakeBackend::new();
        let w = b.add_window(win("Win", "w.exe", Rect::new(0, 0, 300, 300)));
        b.set_tree(w, sample_tree());
        assert_eq!(
            b.perform(&ElementId("ok".into()), ElementAction::Press),
            Ok(true)
        );
        assert_eq!(
            b.perform(&ElementId("label".into()), ElementAction::Press),
            Ok(false)
        );
        assert_eq!(
            b.perform(
                &ElementId("name".into()),
                ElementAction::SetValue("new".into())
            ),
            Ok(true)
        );
        assert_eq!(
            b.element(&ElementId("name".into()))
                .unwrap()
                .value
                .as_deref(),
            Some("new")
        );
        assert_eq!(
            b.perform(&ElementId("ok".into()), ElementAction::Focus),
            Ok(true)
        );
        assert!(b.element(&ElementId("ok".into())).unwrap().focused);
        assert_eq!(
            b.perform(&ElementId("zzz".into()), ElementAction::Press),
            Err(BackendError::ElementGone)
        );
        assert_eq!(b.performed().len(), 5);
    }

    #[test]
    fn a_password_field_never_returns_its_value() {
        let b = FakeBackend::new();
        let mut pw = Element::new("pw", Role::Edit, "Password").valued("hunter2");
        pw.password = true;
        b.set_focused(Some(pw));
        let got = b.focused_element().unwrap().unwrap();
        assert!(got.password && got.value.is_none());
    }

    #[test]
    fn document_text_search_and_menus_and_dialogs() {
        let b = FakeBackend::new();
        let w = b.add_window(win("Doc", "w.exe", Rect::new(0, 0, 10, 10)));
        assert_eq!(b.document_text(w, 100).unwrap(), None);
        b.set_document(w, "alpha beta\nPARAGRAPH  140:  the   reference\nlast");
        assert_eq!(b.document_text(w, 5).unwrap().as_deref(), Some("alpha"));
        let hits = b
            .find_in_documents(w, "reference", 6, Duration::from_secs(1))
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, "PARAGRAPH 140: the reference");
        assert!(hits[0].match_rect.is_none());
        assert_eq!(
            b.find_in_documents(w, "a", 1, Duration::from_secs(1))
                .unwrap()
                .len(),
            1
        );
        b.set_document_line_at(w, Point::new(3, 4), "exact line");
        assert_eq!(
            b.document_line_at(w, Point::new(3, 4)).unwrap().as_deref(),
            Some("exact line")
        );
        assert_eq!(b.document_line_at(w, Point::new(0, 0)).unwrap(), None);

        assert!(b.popup_menus().unwrap().is_empty());
        b.set_popup_menu(
            vec![w],
            vec![MenuItem {
                name: "Copy".into(),
                rect: Rect::new(0, 0, 10, 10),
            }],
        );
        assert_eq!(b.popup_menus().unwrap(), [w]);
        assert_eq!(
            b.menu_items(Duration::from_millis(100)).unwrap()[0].name,
            "Copy"
        );

        assert_eq!(b.find_file_dialog_field(w).unwrap(), None);
        b.set_file_dialog_field(w, ElementId("fn".into()));
        assert_eq!(
            b.find_file_dialog_field(w).unwrap(),
            Some(ElementId("fn".into()))
        );
    }

    #[test]
    fn caret_focus_and_text_state() {
        let b = FakeBackend::new();
        assert!(!b.has_caret(None).unwrap());
        b.set_caret(Some(Caret {
            x: 1,
            y: 2,
            height: 16,
        }));
        assert!(b.has_caret(None).unwrap());
        assert_eq!(
            b.caret().unwrap(),
            Some(Caret {
                x: 1,
                y: 2,
                height: 16
            })
        );
        let t0 = b.focus_token().unwrap();
        b.bump_focus();
        assert_ne!(b.focus_token().unwrap(), t0);
    }

    #[test]
    fn input_is_logged_in_order_and_counted() {
        let b = FakeBackend::new();
        let before = b.input_events();
        b.mouse_move(Point::new(5, 6)).unwrap();
        b.mouse_click(Point::new(7, 8), MouseButton::Right, 2, &no_guard)
            .unwrap();
        b.mouse_scroll(Point::new(1, 1), ScrollDirection::Down, 3, &no_guard)
            .unwrap();
        b.mouse_drag(Point::new(0, 0), Point::new(9, 9), &no_guard)
            .unwrap();
        b.mouse_button(MouseButton::Left, true).unwrap();
        b.key_chord(&[Key::Ctrl, Key::Char('a')], 1, &no_guard)
            .unwrap();
        b.type_text("hi", &no_guard).unwrap();
        assert!(b.input_events() > before);
        assert_eq!(b.cursor_position().unwrap(), Point::new(9, 9));
        let events = b.events();
        assert_eq!(events[0], InputEvent::MouseMove(Point::new(5, 6)));
        assert_eq!(
            events[1],
            InputEvent::Click {
                at: Point::new(7, 8),
                button: MouseButton::Right,
                count: 2
            }
        );
        assert!(events.contains(&InputEvent::KeyDown(Key::Ctrl)));
        let chord_end = events
            .iter()
            .position(|e| *e == InputEvent::KeyUp(Key::Ctrl))
            .unwrap();
        assert_eq!(
            events[chord_end - 1],
            InputEvent::KeyUp(Key::Char('a')),
            "released in reverse order"
        );
        assert_eq!(b.typed_text(), "hi");
        b.clear_events();
        assert!(b.events().is_empty());
    }

    #[test]
    fn chords_press_in_order_release_in_reverse_and_repeat() {
        let b = FakeBackend::new();
        b.key_chord(&[Key::Ctrl, Key::Shift, Key::Char('s')], 2, &no_guard)
            .unwrap();
        let events = b.events();
        assert_eq!(events.len(), 12);
        assert_eq!(
            &events[..6],
            &[
                InputEvent::KeyDown(Key::Ctrl),
                InputEvent::KeyDown(Key::Shift),
                InputEvent::KeyDown(Key::Char('s')),
                InputEvent::KeyUp(Key::Char('s')),
                InputEvent::KeyUp(Key::Shift),
                InputEvent::KeyUp(Key::Ctrl),
            ]
        );
    }

    #[test]
    fn a_guard_that_fails_stops_the_input() {
        let b = FakeBackend::new();
        let calls = AtomicU32::new(0);
        let guard = || {
            if calls.fetch_add(1, Ordering::SeqCst) >= 2 {
                Err(BackendError::Denied("focus changed".into()))
            } else {
                Ok(())
            }
        };
        let guard: InputGuard<'_> = &guard;
        assert!(b.type_text("abcdef", guard).is_err());
        assert_eq!(
            b.typed_text(),
            "ab",
            "the third guard call refused the third character"
        );
        assert!(
            b.mouse_click(Point::new(1, 1), MouseButton::Left, 1, guard)
                .is_err()
        );
        assert!(
            b.events()
                .iter()
                .all(|e| !matches!(e, InputEvent::Click { .. }))
        );
        assert!(b.key_chord(&[Key::Enter], 1, guard).is_err());
    }

    #[test]
    fn human_idle_is_scriptable() {
        let b = FakeBackend::new();
        assert!(b.human_input_idle().unwrap() >= Duration::from_secs(60));
        b.set_human_idle(Duration::from_millis(40));
        assert_eq!(b.human_input_idle().unwrap(), Duration::from_millis(40));
    }

    #[test]
    fn the_clipboard_tracks_content_and_a_change_counter() {
        let b = FakeBackend::new();
        assert_eq!(b.clipboard().unwrap(), ClipboardContent::Empty);
        assert_eq!(b.clipboard_text().unwrap(), "");
        let s0 = b.clipboard_sequence().unwrap();
        b.set_clipboard_text("héllo").unwrap();
        assert_eq!(b.clipboard_text().unwrap(), "héllo");
        assert_ne!(b.clipboard_sequence().unwrap(), s0);
        b.set_clipboard(ClipboardContent::Files(vec!["C:\\a.txt".into()]));
        assert_eq!(b.clipboard_text().unwrap(), "", "files are not text");
    }

    #[test]
    fn ocr_returns_the_script_counts_calls_and_can_be_missing() {
        let b = FakeBackend::new();
        let line = OcrLine {
            text: "Save".into(),
            rect: Rect::new(0, 0, 40, 10),
            words: vec![OcrWord {
                text: "Save".into(),
                rect: Rect::new(0, 0, 40, 10),
                confidence: 1.0,
            }],
            confidence: 1.0,
        };
        b.set_ocr(OcrResult {
            lines: vec![line],
            engine: "fake".into(),
        });
        let (result, origin, scale) = b.ocr_region(Rect::new(5, 6, 105, 56)).unwrap();
        assert_eq!(result.lines[0].text, "Save");
        assert_eq!((origin, scale), (Point::new(5, 6), 1.0));
        assert_eq!(b.ocr_calls(), 1);
        b.set_ocr_available(false);
        assert!(matches!(
            b.ocr(&Image::solid(1, 1, [0; 4])),
            Err(BackendError::Unsupported(_))
        ));
        assert_eq!(b.ocr_engine_name(), "untried");
        b.warm_ocr().unwrap();
    }

    #[test]
    fn processes_peers_and_launches() {
        let b = FakeBackend::new();
        b.set_processes(vec![
            ProcessInfo {
                pid: 10,
                parent_pid: 4,
                exe_name: "python.exe".into(),
                exe_path: None,
                started_at: Some(5),
                elevated: false,
            },
            ProcessInfo {
                pid: 4,
                parent_pid: 0,
                exe_name: "claude.exe".into(),
                exe_path: None,
                started_at: Some(1),
                elevated: true,
            },
        ]);
        assert_eq!(b.process_table().unwrap().len(), 2);
        assert_eq!(b.process_info(4).unwrap().unwrap().exe_name, "claude.exe");
        assert_eq!(b.process_info(77).unwrap(), None);
        b.set_peer(50123, 47811, 10);
        assert_eq!(b.peer_pid(50123, 47811).unwrap(), Some(10));
        assert_eq!(b.peer_pid(1, 2).unwrap(), None);
        assert_eq!(b.own_process().unwrap(), None);

        assert_eq!(b.resolve_app("Notepad").unwrap(), None);
        b.set_app(
            "Notepad",
            AppTarget::Executable("C:\\Windows\\notepad.exe".into()),
        );
        let target = b.resolve_app("notepad").unwrap().unwrap();
        let spec = LaunchSpec {
            target,
            args: String::new(),
            deelevate: true,
        };
        assert!(b.launch(&spec).unwrap().pid.is_some());
        assert_eq!(b.launches(), [spec]);
    }

    #[test]
    fn the_kill_hotkey_takes_the_first_chord_the_os_allows() {
        let b = FakeBackend::new();
        let chords = vec![
            HotkeyChord {
                name: "ctrl+alt+shift+f12".into(),
                keys: vec![Key::Ctrl, Key::Alt, Key::Shift, Key::F(12)],
            },
            HotkeyChord {
                name: "ctrl+alt+shift+pause".into(),
                keys: vec![Key::Ctrl, Key::Alt, Key::Shift, Key::Pause],
            },
        ];
        let hits = Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        b.block_hotkeys(&["ctrl+alt+shift+f12"]);
        let chosen = b
            .register_kill_hotkey(
                &chords,
                Arc::new(move || {
                    h.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .unwrap();
        assert_eq!(chosen.as_deref(), Some("ctrl+alt+shift+pause"));
        assert!(b.fire_kill_hotkey());
        assert!(b.fire_kill_hotkey());
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        let none = FakeBackend::new();
        none.block_hotkeys(&["ctrl+alt+shift+f12", "ctrl+alt+shift+pause"]);
        assert_eq!(
            none.register_kill_hotkey(&chords, Arc::new(|| {})).unwrap(),
            None
        );
        assert!(!none.fire_kill_hotkey());
    }

    #[test]
    fn hosted_apps_and_window_text() {
        let b = FakeBackend::new();
        let w = b.add_window(win(
            "Calculator",
            "applicationframehost.exe",
            Rect::new(0, 0, 10, 10),
        ));
        assert_eq!(b.hosted_app(w).unwrap(), None);
        b.set_hosted_app(w, "calculatorapp.exe");
        assert_eq!(
            b.hosted_app(w).unwrap().as_deref(),
            Some("calculatorapp.exe")
        );
        assert_eq!(b.window_text(w).unwrap(), "Calculator");
        b.set_window_text(w, "Confirm Save As / x already exists");
        assert_eq!(
            b.window_text(w).unwrap(),
            "Confirm Save As / x already exists"
        );
        assert!(b.window_text(WindowRef::from_raw(1)).is_err());
        b.update_window(w, |i| i.title = "Renamed".into());
        assert_eq!(b.window_info(w).unwrap().title, "Renamed");
    }
}
