//! Role-neutral data types the [`Backend`](super::Backend) trait speaks. No OS types leak through
//! here: a Windows `HWND`, a UIA control type or a macOS `AXUIElement` is translated at the
//! backend boundary.
//!
//! Coordinates are **input coordinates**: the space `mouse_move` and `window.rect` use. On Windows
//! that is physical pixels (the process is per-monitor-v2 DPI aware). On macOS it is points;
//! [`Display::scale`] and [`Image::scale`] carry the pixels-per-point factor so a screenshot (pixels)
//! can be mapped back to input coordinates (points).

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

// ------------------------------------------------------------------------------------ geometry

/// A point in input coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Point {
    /// Horizontal, growing to the right.
    pub x: i32,
    /// Vertical, growing downwards.
    pub y: i32,
}

impl Point {
    /// A point.
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A rectangle as `(left, top, right, bottom)`, right and bottom exclusive. This is the tuple shape
/// every Python rect used (`BoundingRectangle`, `GetWindowRect`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    /// Left edge.
    pub left: i32,
    /// Top edge.
    pub top: i32,
    /// Right edge, exclusive.
    pub right: i32,
    /// Bottom edge, exclusive.
    pub bottom: i32,
}

impl Rect {
    /// From edges.
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// From a corner and a size, the `(left, top, width, height)` shape `display_rect()` used.
    pub const fn from_xywh(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        }
    }

    /// Width (may be zero or negative for a degenerate rect).
    pub const fn width(&self) -> i32 {
        self.right - self.left
    }

    /// Height (may be zero or negative for a degenerate rect).
    pub const fn height(&self) -> i32 {
        self.bottom - self.top
    }

    /// No area.
    pub const fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    /// Integer centre, rounding towards the top left like Python's `//`.
    pub const fn center(&self) -> Point {
        Point {
            x: (self.left + self.right).div_euclid(2),
            y: (self.top + self.bottom).div_euclid(2),
        }
    }

    /// Is `p` inside (left/top inclusive, right/bottom exclusive)?
    pub const fn contains(&self, p: Point) -> bool {
        self.left <= p.x && p.x < self.right && self.top <= p.y && p.y < self.bottom
    }

    /// The overlap of two rects, or `None` when they do not overlap.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let r = Rect {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        };
        if r.is_empty() { None } else { Some(r) }
    }

    /// Does this rect cover all of `other`?
    pub const fn covers(&self, other: &Rect) -> bool {
        self.left <= other.left
            && self.top <= other.top
            && self.right >= other.right
            && self.bottom >= other.bottom
    }

    /// Shift by `(dx, dy)`.
    pub const fn translate(&self, dx: i32, dy: i32) -> Rect {
        Rect {
            left: self.left + dx,
            top: self.top + dy,
            right: self.right + dx,
            bottom: self.bottom + dy,
        }
    }
}

// ------------------------------------------------------------------------------------ errors

/// What a backend call can fail with. The core turns these into the Python-compatible messages
/// (`TargetError` and friends); a backend should keep them short and factual.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// This backend cannot do that on this OS (see [`Capabilities`]).
    Unsupported(String),
    /// The window no longer exists, or never did.
    WindowGone,
    /// The element is gone, or its window is; a fresh tree read is needed.
    ElementGone,
    /// The OS refused (missing Accessibility or Screen Recording grant, UAC boundary, secure input).
    Denied(String),
    /// A deadline passed before the OS answered.
    Timeout,
    /// Any other OS failure.
    Os(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::Unsupported(what) => write!(f, "not supported on this OS: {what}"),
            BackendError::WindowGone => f.write_str("window unavailable; refresh windows"),
            BackendError::ElementGone => f.write_str("element is gone; re-snapshot"),
            BackendError::Denied(why) => write!(f, "the OS refused: {why}"),
            BackendError::Timeout => f.write_str("the OS did not answer in time"),
            BackendError::Os(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for BackendError {}

/// Result of a backend call.
pub type BackendResult<T> = Result<T, BackendError>;

// ------------------------------------------------------------------------------------ windows

/// An opaque, stable handle to a top-level window. Stable means: the same window keeps the same
/// value for as long as it lives, and a value is never reused for another window while the first
/// is alive. Core code never looks inside it.
///
/// Windows builds it from the `HWND`; macOS from the owning pid plus the AX window id
/// ([`WindowRef::from_parts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowRef(u64);

impl WindowRef {
    /// Wrap a backend-defined 64-bit value (an `HWND` on Windows).
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The backend-defined value. Only the backend that made it should interpret it.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Pack a pair (macOS: `pid`, AX window id) into one handle.
    pub const fn from_parts(hi: u32, lo: u32) -> Self {
        Self(((hi as u64) << 32) | lo as u64)
    }

    /// The pair [`WindowRef::from_parts`] packed.
    pub const fn parts(self) -> (u32, u32) {
        ((self.0 >> 32) as u32, self.0 as u32)
    }

    /// The `hwnd:N` window spec the Python build accepts and prints. `prepare_window` pins a
    /// request to the window it resolved with this form so later lookups hit the same window.
    pub fn spec(self) -> String {
        format!("hwnd:{}", self.0)
    }

    /// Parse a spec made by [`WindowRef::spec`] (case-insensitive prefix, like Python).
    pub fn parse_spec(spec: &str) -> Option<Self> {
        let spec = spec.trim();
        let head = spec.get(..5)?;
        if !head.eq_ignore_ascii_case("hwnd:") {
            return None;
        }
        spec[5..].trim().parse::<u64>().ok().map(Self)
    }
}

impl fmt::Display for WindowRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What kind of surface a window is, for the guards that single out the desktop, the taskbar, the
/// Run dialog or a file manager. Python matched Win32 class names (`progman`, `Shell_TrayWnd`,
/// `#32770`, `CabinetWClass`, `#32768`); the backend maps its own class to one of these and also
/// keeps the raw class in [`WindowInfo::class`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WindowKind {
    /// An ordinary application window.
    #[default]
    Normal,
    /// A dialog box (Win32 `#32770`; a macOS sheet or `AXDialog`).
    Dialog,
    /// A popup or context menu window (Win32 `#32768` or a XAML popup host).
    PopupMenu,
    /// The desktop surface (`Progman`, `WorkerW`).
    Desktop,
    /// The taskbar or Dock (`Shell_TrayWnd`).
    Taskbar,
    /// A file-manager window (Explorer's `CabinetWClass`; Finder).
    FileManager,
}

/// One surface on screen, for hiding apps from pictures ([`super::Backend::screen_layers`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenLayer {
    /// Where it is drawn, in input coordinates.
    pub rect: Rect,
    /// The owning application, as in [`WindowInfo::app`] ("" when the OS does not say).
    pub app: String,
    /// Its title, "" when it has none.
    pub title: String,
}

/// One top-level window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// Its stable handle.
    pub window: WindowRef,
    /// The title, as the OS reports it.
    pub title: String,
    /// The owning application: the executable file name on Windows (`notepad.exe`, any case), the
    /// bundle id on macOS. Compare case-insensitively.
    pub app: String,
    /// The owning process id.
    pub pid: u32,
    /// Where it is, in input coordinates.
    pub rect: Rect,
    /// Minimised (Win32 `IsIconic`).
    pub minimized: bool,
    /// Covers the whole monitor it is on (a fullscreen game or video).
    pub fullscreen: bool,
    /// The window in front (`GetForegroundWindow`).
    pub foreground: bool,
    /// Present but not composited on any desktop: DWM-cloaked on Windows (a UWP app keeps one per
    /// process), on another Space or hidden on macOS. Listed only on request, never an action target.
    pub hidden: bool,
    /// The owning process runs elevated (Windows); always false elsewhere.
    pub elevated: bool,
    /// What kind of surface this is.
    pub kind: WindowKind,
    /// The raw platform class (`Notepad`, `CabinetWClass`, `AXWindow`), for the guards that need it.
    pub class: String,
    /// The window that owns this one (a popup's or dialog's owner), when there is one.
    pub owner: Option<WindowRef>,
}

impl WindowInfo {
    /// A visible, ordinary, non-foreground window with the given facts. Handy for backends that
    /// fill in the rest, and for tests.
    pub fn basic(title: &str, app: &str, pid: u32, rect: Rect) -> Self {
        Self {
            window: WindowRef::from_raw(0),
            title: title.to_string(),
            app: app.to_string(),
            pid,
            rect,
            minimized: false,
            fullscreen: false,
            foreground: false,
            hidden: false,
            elevated: false,
            kind: WindowKind::Normal,
            class: String::new(),
            owner: None,
        }
    }

    /// The application name lowercased, the form the guards compare (Python `window_process`).
    pub fn app_lower(&self) -> String {
        self.app.to_lowercase()
    }
}

/// A display (monitor).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Display {
    /// A stable id for this display within a session (1-based, primary first).
    pub id: u32,
    /// A human name, possibly empty.
    pub name: String,
    /// Its rect in input coordinates (the virtual desktop).
    pub rect: Rect,
    /// Backing pixels per input unit: 1.0 on Windows, 2.0 on a Retina Mac in points.
    pub scale: f64,
    /// The primary display (the one whose origin is the desktop origin).
    pub primary: bool,
}

impl Display {
    /// Its size in physical pixels, the size of a capture of the whole display.
    pub fn pixel_size(&self) -> (u32, u32) {
        let w = (f64::from(self.rect.width()) * self.scale).round().max(0.0);
        let h = (f64::from(self.rect.height()) * self.scale)
            .round()
            .max(0.0);
        (w as u32, h as u32)
    }
}

// ------------------------------------------------------------------------------------ images

/// What to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTarget {
    /// The whole virtual desktop, every display (Python `grab(None)`).
    VirtualDesktop,
    /// One display (Python `mss` monitor).
    Display(u32),
    /// A rectangle of the virtual desktop in input coordinates (Python `grab(box)`).
    Region(Rect),
    /// One window's pixels. Backends that cannot capture a covered window may fall back to its rect.
    Window(WindowRef),
}

/// Decoded pixels: row-major RGBA, 8 bits per channel, `width * height * 4` bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    scale: f64,
    origin: Point,
}

impl Image {
    /// Build an image from RGBA bytes. The byte count must be `width * height * 4`.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> BackendResult<Self> {
        let want = (width as usize) * (height as usize) * 4;
        if rgba.len() != want {
            return Err(BackendError::Os(format!(
                "image buffer is {} bytes, expected {want} for {width}x{height} RGBA",
                rgba.len()
            )));
        }
        Ok(Self {
            width,
            height,
            rgba,
            scale: 1.0,
            origin: Point::default(),
        })
    }

    /// A single-colour image.
    pub fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Self {
        let mut bytes = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for _ in 0..(width as usize) * (height as usize) {
            bytes.extend_from_slice(&rgba);
        }
        Self {
            width,
            height,
            rgba: bytes,
            scale: 1.0,
            origin: Point::default(),
        }
    }

    /// Set the pixels-per-input-unit factor of the capture ([`Display::scale`]).
    pub fn with_scale(mut self, scale: f64) -> Self {
        self.scale = scale;
        self
    }

    /// Set where the top left pixel sits in input coordinates.
    pub fn with_origin(mut self, origin: Point) -> Self {
        self.origin = origin;
        self
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The RGBA bytes.
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// Take the RGBA bytes.
    pub fn into_rgba(self) -> Vec<u8> {
        self.rgba
    }

    /// Pixels per input unit (1.0 on Windows; 2.0 for Retina in points).
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// The input-coordinate position of the top left pixel.
    pub fn origin(&self) -> Point {
        self.origin
    }

    /// The pixel at `(x, y)`, or `None` outside the image.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = ((y as usize) * (self.width as usize) + x as usize) * 4;
        Some([
            self.rgba[i],
            self.rgba[i + 1],
            self.rgba[i + 2],
            self.rgba[i + 3],
        ])
    }

    /// Overwrite the pixel at `(x, y)`; outside the image is ignored.
    pub fn put_pixel(&mut self, x: u32, y: u32, rgba: [u8; 4]) {
        if x < self.width && y < self.height {
            let i = ((y as usize) * (self.width as usize) + x as usize) * 4;
            self.rgba[i..i + 4].copy_from_slice(&rgba);
        }
    }

    /// Copy out the pixel rectangle `[x, x+w) x [y, y+h)`, clamped to the image. `None` when empty.
    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Option<Image> {
        let x1 = x.saturating_add(w).min(self.width);
        let y1 = y.saturating_add(h).min(self.height);
        if x >= x1 || y >= y1 {
            return None;
        }
        let (cw, ch) = (x1 - x, y1 - y);
        let mut out = Vec::with_capacity((cw as usize) * (ch as usize) * 4);
        for row in y..y1 {
            let start = ((row as usize) * (self.width as usize) + x as usize) * 4;
            out.extend_from_slice(&self.rgba[start..start + (cw as usize) * 4]);
        }
        Some(Image {
            width: cw,
            height: ch,
            rgba: out,
            scale: self.scale,
            origin: Point {
                x: self.origin.x + (f64::from(x) / self.scale).round() as i32,
                y: self.origin.y + (f64::from(y) / self.scale).round() as i32,
            },
        })
    }
}

// ------------------------------------------------------------------------------------ elements

/// A stable reference to one accessibility element, minted by the backend and valid until the
/// element's window closes or the element disappears. It must survive being used from another
/// thread than the one that read the tree (Python lost `snapshot` refs between daemon threads; see
/// `core.py:_refind_element`): a backend that cannot keep a live object re-finds it by name, role
/// and nearest position.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ElementId(pub String);

impl fmt::Display for ElementId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A role-neutral element kind. The Python code switched on UIA control type names; every one it
/// names is here. Anything else a platform reports lands in [`Role::Other`] with its native name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    /// A push button.
    Button,
    /// A text field.
    Edit,
    /// A check box.
    CheckBox,
    /// A radio button.
    RadioButton,
    /// A combo box or pop-up button.
    ComboBox,
    /// A row of a list.
    ListItem,
    /// A menu entry.
    MenuItem,
    /// A tab header.
    TabItem,
    /// A link.
    Hyperlink,
    /// A node of a tree.
    TreeItem,
    /// A slider.
    Slider,
    /// A button with a drop-down part.
    SplitButton,
    /// A cell or row of a data grid.
    DataItem,
    /// A document or web page.
    Document,
    /// Static text.
    Text,
    /// A list container.
    List,
    /// A tree container.
    Tree,
    /// A data grid container.
    DataGrid,
    /// A table container.
    Table,
    /// A menu container.
    Menu,
    /// A generic pane or group.
    Pane,
    /// A window.
    Window,
    /// A role the neutral set has no name for, with the platform's own name.
    Other(String),
}

impl Role {
    /// Interactive in the sense of `core.INTERACTIVE_CONTROL_TYPES`: the kinds a model can act on.
    pub fn is_interactive(&self) -> bool {
        matches!(
            self,
            Role::Button
                | Role::Edit
                | Role::CheckBox
                | Role::RadioButton
                | Role::ComboBox
                | Role::ListItem
                | Role::MenuItem
                | Role::TabItem
                | Role::Hyperlink
                | Role::TreeItem
                | Role::Slider
                | Role::SplitButton
                | Role::DataItem
        )
    }

    /// A row of a list-like container (`computer._ITEM_KINDS`).
    pub fn is_item(&self) -> bool {
        matches!(
            self,
            Role::ListItem | Role::TreeItem | Role::DataItem | Role::MenuItem | Role::TabItem
        )
    }

    /// A list-like container (`computer._CONTAINER_KINDS`).
    pub fn is_container(&self) -> bool {
        matches!(self, Role::List | Role::Tree | Role::DataGrid | Role::Table)
    }

    /// Roles whose state a click changes, so a snapshot prints `[on]`, `[selected]`, `[expanded]`
    /// for them (`core._STATEFUL`).
    pub fn is_stateful(&self) -> bool {
        matches!(
            self,
            Role::CheckBox
                | Role::RadioButton
                | Role::ListItem
                | Role::TabItem
                | Role::TreeItem
                | Role::MenuItem
                | Role::ComboBox
                | Role::SplitButton
        )
    }

    /// The short name a compact snapshot prints (`Button`, `Edit`, `CheckBox`).
    pub fn short_name(&self) -> String {
        match self {
            Role::Button => "Button".into(),
            Role::Edit => "Edit".into(),
            Role::CheckBox => "CheckBox".into(),
            Role::RadioButton => "RadioButton".into(),
            Role::ComboBox => "ComboBox".into(),
            Role::ListItem => "ListItem".into(),
            Role::MenuItem => "MenuItem".into(),
            Role::TabItem => "TabItem".into(),
            Role::Hyperlink => "Hyperlink".into(),
            Role::TreeItem => "TreeItem".into(),
            Role::Slider => "Slider".into(),
            Role::SplitButton => "SplitButton".into(),
            Role::DataItem => "DataItem".into(),
            Role::Document => "Document".into(),
            Role::Text => "Text".into(),
            Role::List => "List".into(),
            Role::Tree => "Tree".into(),
            Role::DataGrid => "DataGrid".into(),
            Role::Table => "Table".into(),
            Role::Menu => "Menu".into(),
            Role::Pane => "Pane".into(),
            Role::Window => "Window".into(),
            Role::Other(name) => name.clone(),
        }
    }

    /// Lowercase words for prose: `ListItem` reads `list item` (`computer._describe_point`).
    pub fn words(&self) -> String {
        let name = self.short_name();
        let mut out = String::new();
        for (i, ch) in name.chars().enumerate() {
            if i > 0 && ch.is_uppercase() {
                out.push(' ');
            }
            out.extend(ch.to_lowercase());
        }
        out
    }
}

/// A toggle's state (UIA `TogglePattern`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Toggle {
    /// Unchecked.
    Off,
    /// Checked.
    On,
    /// Indeterminate.
    Mixed,
}

/// An expand/collapse state. A leaf is `None` in [`ElementState::expanded`], never printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Expand {
    /// Collapsed.
    Collapsed,
    /// Expanded.
    Expanded,
    /// Partially expanded.
    Partial,
}

/// What a click changes on an element: checked, selected, expanded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElementState {
    /// Toggle state, when the element toggles.
    pub toggle: Option<Toggle>,
    /// Selected, when it is a selectable item.
    pub selected: bool,
    /// Expand state, when it expands.
    pub expanded: Option<Expand>,
}

impl ElementState {
    /// The words a snapshot prints in brackets, in Python's order: toggle, selected, expanded.
    pub fn flags(&self) -> Vec<&'static str> {
        let mut flags = Vec::new();
        if let Some(t) = self.toggle {
            flags.push(match t {
                Toggle::Off => "off",
                Toggle::On => "on",
                Toggle::Mixed => "mixed",
            });
        }
        if self.selected {
            flags.push("selected");
        }
        if let Some(e) = self.expanded {
            flags.push(match e {
                Expand::Collapsed => "collapsed",
                Expand::Expanded => "expanded",
                Expand::Partial => "partial",
            });
        }
        flags
    }
}

/// What an element supports (UIA patterns, AX actions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Affordance {
    /// Can be invoked or pressed.
    Invoke,
    /// Can be toggled.
    Toggle,
    /// Can be selected.
    Select,
    /// Can expand and collapse.
    ExpandCollapse,
    /// Exposes a settable value.
    Value,
    /// Exposes text content (documents).
    Text,
}

/// One accessibility element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Element {
    /// Stable id, see [`ElementId`].
    pub id: ElementId,
    /// Neutral role.
    pub role: Role,
    /// The platform's own role name (`ButtonControl`, `AXButton`), kept for `coords=true` output.
    pub native_role: String,
    /// Accessible name, "" when it has none.
    pub name: String,
    /// Current value (edit text, slider position), when it has one.
    pub value: Option<String>,
    /// Bounds in input coordinates, `None` when the platform reports none.
    pub rect: Option<Rect>,
    /// Enabled.
    pub enabled: bool,
    /// Has keyboard focus.
    pub focused: bool,
    /// Scrolled or clipped out of view (UIA `IsOffscreen`).
    pub offscreen: bool,
    /// A password field: its value must never be read back.
    pub password: bool,
    /// Checked / selected / expanded.
    pub state: ElementState,
    /// What it supports.
    pub affordances: Vec<Affordance>,
    /// Children, empty for a leaf or when the tree was flattened.
    pub children: Vec<Element>,
}

impl Element {
    /// An enabled, visible element with no state, value, rect or children.
    pub fn new(id: &str, role: Role, name: &str) -> Self {
        Self {
            id: ElementId(id.to_string()),
            native_role: format!("{}Control", role.short_name()),
            role,
            name: name.to_string(),
            value: None,
            rect: None,
            enabled: true,
            focused: false,
            offscreen: false,
            password: false,
            state: ElementState::default(),
            affordances: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Builder: set the bounds.
    pub fn at(mut self, rect: Rect) -> Self {
        self.rect = Some(rect);
        self
    }

    /// Builder: set the value.
    pub fn valued(mut self, value: &str) -> Self {
        self.value = Some(value.to_string());
        self
    }

    /// Builder: add a child.
    pub fn with_child(mut self, child: Element) -> Self {
        self.children.push(child);
        self
    }

    /// Does the element carry any of the affordances, or have an interactive role? This is the
    /// test `core._is_interactive` / `_snapshot_batched` applied to decide a snapshot row.
    pub fn is_interactive(&self) -> bool {
        self.role.is_interactive()
            || self.affordances.iter().any(|a| {
                matches!(
                    a,
                    Affordance::Invoke
                        | Affordance::Toggle
                        | Affordance::Select
                        | Affordance::ExpandCollapse
                        | Affordance::Value
                )
            })
            || (self.role == Role::Document && self.affordances.contains(&Affordance::Text))
    }

    /// Walk this element and its descendants in document order.
    pub fn walk<'a>(&'a self, out: &mut Vec<&'a Element>) {
        out.push(self);
        for child in &self.children {
            child.walk(out);
        }
    }

    /// Count this element and its descendants.
    pub fn count(&self) -> usize {
        1 + self.children.iter().map(Element::count).sum::<usize>()
    }
}

/// Which elements a tree read keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElementFilter {
    /// Everything.
    All,
    /// Interactive elements only (`core.snapshot`).
    Interactive,
    /// Elements with a non-empty name and a non-empty rect (`computer._uia_elements`).
    Named,
}

/// Whether an empty tree may be woken by raising the window (Chromium builds its accessibility tree
/// only after a client has walked it and the window has been in front; `core._wake_chromium_by_raise`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WakePolicy {
    /// Never touch focus or z-order; report [`TreeSnapshot::wake_hint`] instead.
    Never,
    /// Raise the window briefly when its tree looks empty, then hand focus back.
    IfEmpty,
}

/// Limits for an element tree read. A read stops at the first limit it hits and says so in
/// [`TreeSnapshot::truncated`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TreeLimits {
    /// At most this many elements are returned (after filtering).
    pub max_items: usize,
    /// Deepest level visited; the root is depth 0.
    pub max_depth: u32,
    /// Wall-clock budget for the whole read.
    pub deadline: Duration,
    /// Which elements to keep.
    pub filter: ElementFilter,
    /// Return the kept elements as the root's direct children in document order, with no nesting.
    pub flatten: bool,
    /// May an empty tree be woken by raising the window?
    pub wake: WakePolicy,
}

impl TreeLimits {
    /// The `snapshot` defaults: interactive elements, flat, 2.5 s, 25 levels
    /// (`core.snapshot(max_items=120, time_box_s=2.5)`).
    pub fn snapshot(max_items: usize) -> Self {
        Self {
            max_items,
            max_depth: 25,
            deadline: Duration::from_millis(2500),
            filter: ElementFilter::Interactive,
            flatten: true,
            wake: WakePolicy::IfEmpty,
        }
    }

    /// The text-lookup defaults: named elements, flat, 12 levels, `deadline` (default `uia_ms`)
    /// (`computer._uia_elements`).
    pub fn named(deadline: Duration) -> Self {
        Self {
            max_items: usize::MAX,
            max_depth: 12,
            deadline,
            filter: ElementFilter::Named,
            flatten: true,
            wake: WakePolicy::Never,
        }
    }
}

/// The result of an element tree read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreeSnapshot {
    /// The window that was read.
    pub window: WindowRef,
    /// Its title.
    pub title: String,
    /// The root element (the window itself). With `flatten` its children are the kept elements.
    pub root: Element,
    /// A limit cut the read short.
    pub truncated: bool,
    /// The window looked like an empty Chromium tree that [`WakePolicy::Never`] left alone, and
    /// raising it would help (`core.WAKE_HINT`).
    pub wake_hint: bool,
}

/// An element found at a screen point, with its ancestors (nearest first).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElementHit {
    /// The deepest element at the point.
    pub element: Element,
    /// Its parents, nearest first, no children attached.
    pub ancestors: Vec<Element>,
}

/// Something to do to an element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElementAction {
    /// Do the element's primary action: invoke, else toggle, else select, else expand
    /// (`core._try_patterns`).
    Press,
    /// Give the element keyboard focus.
    Focus,
    /// Replace its value (a settable text field).
    SetValue(String),
}

// ------------------------------------------------------------------------------------ text

/// A match found by the backend's document text search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentHit {
    /// The whole line the match sits on, whitespace collapsed.
    pub line: String,
    /// Where the match is on screen; `None` when it is scrolled out of view (the text is still exact).
    pub match_rect: Option<Rect>,
    /// Where the line is on screen; `None` when scrolled out of view.
    pub line_rect: Option<Rect>,
}

/// The text cursor on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caret {
    /// Left edge, input coordinates.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Height of the caret.
    pub height: i32,
}

/// An item of an open context menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuItem {
    /// Label, possibly with a tab-separated shortcut (`Undo Rename\tCtrl+Z`).
    pub name: String,
    /// Its bounds.
    pub rect: Rect,
}

// ------------------------------------------------------------------------------------ OCR

/// One recognised word.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrWord {
    /// The text.
    pub text: String,
    /// Its box in pixels of the OCR'd image (not screen coordinates).
    pub rect: Rect,
    /// Confidence 0..1; 1.0 where the engine reports none (Windows.Media.Ocr).
    pub confidence: f32,
}

/// One recognised line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrLine {
    /// The words joined with single spaces.
    pub text: String,
    /// The line's box, in image pixels.
    pub rect: Rect,
    /// The words of the line, left to right.
    pub words: Vec<OcrWord>,
    /// Confidence 0..1.
    pub confidence: f32,
}

/// OCR output for one image.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OcrResult {
    /// Lines in reading order.
    pub lines: Vec<OcrLine>,
    /// Which engine answered (`winrt`, `vision`).
    pub engine: String,
}

// ------------------------------------------------------------------------------------ input

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    /// Primary.
    Left,
    /// Secondary.
    Right,
    /// Wheel button.
    Middle,
}

/// A scroll direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScrollDirection {
    /// Content moves down (wheel away from you).
    Up,
    /// Content moves up.
    Down,
    /// Content moves right.
    Left,
    /// Content moves left.
    Right,
}

impl ScrollDirection {
    /// Parse `up`/`down`/`left`/`right`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            _ => None,
        }
    }
}

// ------------------------------------------------------------------------------------ clipboard

/// What the clipboard holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardContent {
    /// Nothing.
    Empty,
    /// Text.
    Text(String),
    /// Files copied in a file manager, as full paths.
    Files(Vec<String>),
    /// Something else (an image, rich data).
    Other,
}

// ------------------------------------------------------------------------------------ processes

/// A process as the caller guard and the shim reaper need it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    /// Process id.
    pub pid: u32,
    /// Parent process id, 0 when unknown.
    pub parent_pid: u32,
    /// Executable file name only (`explorer.exe`), as the OS reports it.
    pub exe_name: String,
    /// Full path when the OS lets us read it.
    pub exe_path: Option<String>,
    /// Creation time in a backend-defined unit. Only the ordering matters: a "parent" created after
    /// its child is a recycled pid, not a real ancestor (`caller.py:process_chain`).
    pub started_at: Option<u64>,
    /// Running with an elevated token (Windows); false elsewhere.
    pub elevated: bool,
}

// ------------------------------------------------------------------------------------ launching

/// What a launch name resolved to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppTarget {
    /// A program file.
    Executable(String),
    /// A shortcut (`.lnk`, `.app` alias) to open through the shell.
    Shortcut(String),
    /// A packaged / Store app by its app id (`shell:AppsFolder\<id>`).
    Packaged(String),
    /// Anything the shell can open (a URI, a document).
    Other(String),
}

/// A request to start something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSpec {
    /// What to start.
    pub target: AppTarget,
    /// Arguments, one string as typed.
    pub args: String,
    /// Start it at the interactive user's normal integrity level even when winhands runs elevated,
    /// so launched apps do not inherit the daemon's token (`core.launch_deelevated`).
    pub deelevate: bool,
}

/// What a launch produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LaunchOutcome {
    /// The new process id when the backend could tell.
    pub pid: Option<u32>,
}

// ------------------------------------------------------------------------------------ hotkey

/// A global key chord with a name (`ctrl+alt+shift+f12`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyChord {
    /// The name reported when this chord is the one registered.
    pub name: String,
    /// The keys: modifiers first, then one key.
    pub keys: Vec<crate::keys::Key>,
}

// ------------------------------------------------------------------------------------ shortcuts

/// A change to a whole window, for the `window` action. A backend does the OS call and returns; the
/// core polls [`WindowGeometry::state`] to find out whether the app honoured it.
///
/// Python: `shortcuts._post_close`, `_show_async`, `_set_pos`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowOp {
    /// Ask the window to close the way its close button does. The process is never killed: an app
    /// with unsaved work asks first, and the core reports that instead of answering the question.
    Close,
    /// Minimise (Win32 `SW_MINIMIZE`, AX `AXMinimized`). Must not block on a hung window.
    Minimize,
    /// Maximise (Win32 `SW_MAXIMIZE`, macOS zoom or full screen).
    Maximize,
    /// Undo a minimise or a maximise (Win32 `SW_RESTORE`).
    Restore,
    /// Place the window's outer frame at this rect, in input coordinates, without changing the
    /// stacking order or taking focus. The core restores a minimised or maximised window first.
    Move(Rect),
}

/// Whether a window is minimised, maximised or neither.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowState {
    /// Neither (Win32 `!IsIconic && !IsZoomed`).
    #[default]
    Normal,
    /// Minimised (Win32 `IsIconic`).
    Minimized,
    /// Maximised (Win32 `IsZoomed`; macOS full screen).
    Maximized,
}

impl WindowState {
    /// The word a result line uses: `minimized`, `maximized`, `normal`.
    pub fn word(self) -> &'static str {
        match self {
            WindowState::Normal => "normal",
            WindowState::Minimized => "minimized",
            WindowState::Maximized => "maximized",
        }
    }
}

/// Everything about a window's shape that [`WindowInfo::rect`] alone does not say.
///
/// Python: `shortcuts._window_rect`, `_client_rect`, `_visible_rect`, `_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowGeometry {
    /// The whole window frame, including any invisible resize border (Win32 `GetWindowRect`).
    pub frame: Rect,
    /// The bounds a person sees: DWM's extended frame on Windows (no invisible border). Equal to
    /// `frame` where the OS draws no such border.
    pub visible: Rect,
    /// The part inside the frame and title bar, in screen coordinates, when the OS can say.
    pub client: Option<Rect>,
    /// Minimised, maximised or neither.
    pub state: WindowState,
}

/// An Explorer or Finder view mode, for the `folder` action. The names are what a model sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FolderView {
    /// Details: a table with column headers.
    Details,
    /// List.
    List,
    /// Tiles.
    Tiles,
    /// Content.
    Content,
    /// Medium icons.
    Icons,
    /// Large icons.
    Large,
    /// Extra large icons.
    XLarge,
    /// Small icons.
    Small,
}

impl FolderView {
    /// Every view, in the order the error message lists them.
    pub const ALL: [FolderView; 8] = [
        FolderView::Details,
        FolderView::List,
        FolderView::Tiles,
        FolderView::Content,
        FolderView::Icons,
        FolderView::Large,
        FolderView::XLarge,
        FolderView::Small,
    ];

    /// The name a model sends and a result prints.
    pub fn name(self) -> &'static str {
        match self {
            FolderView::Details => "details",
            FolderView::List => "list",
            FolderView::Tiles => "tiles",
            FolderView::Content => "content",
            FolderView::Icons => "icons",
            FolderView::Large => "large",
            FolderView::XLarge => "xlarge",
            FolderView::Small => "small",
        }
    }

    /// The digit of Explorer's own `Ctrl+Shift+<digit>` view shortcut.
    pub fn shortcut_digit(self) -> char {
        match self {
            FolderView::Details => '6',
            FolderView::List => '5',
            FolderView::Tiles => '7',
            FolderView::Content => '8',
            FolderView::Icons => '3',
            FolderView::Large => '2',
            FolderView::XLarge => '1',
            FolderView::Small => '4',
        }
    }
}

/// A column a folder can be sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortColumn {
    /// File name (natural order: `b2` before `b10`).
    Name,
    /// Date modified.
    DateModified,
    /// Date created.
    DateCreated,
    /// Size.
    Size,
    /// Type (the extension; a folder has none).
    Type,
}

impl SortColumn {
    /// Every column, in the order the error message lists them.
    pub const ALL: [SortColumn; 5] = [
        SortColumn::Name,
        SortColumn::DateModified,
        SortColumn::DateCreated,
        SortColumn::Size,
        SortColumn::Type,
    ];

    /// The name a model sends and a result prints.
    pub fn name(self) -> &'static str {
        match self {
            SortColumn::Name => "name",
            SortColumn::DateModified => "date_modified",
            SortColumn::DateCreated => "date_created",
            SortColumn::Size => "size",
            SortColumn::Type => "type",
        }
    }

    /// The Windows property the Shell sorts on (`System.DateModified`).
    pub fn property(self) -> &'static str {
        match self {
            SortColumn::Name => "System.ItemNameDisplay",
            SortColumn::DateModified => "System.DateModified",
            SortColumn::DateCreated => "System.DateCreated",
            SortColumn::Size => "System.Size",
            SortColumn::Type => "System.ItemTypeText",
        }
    }

    /// The column header Explorer's Details view shows.
    pub fn header(self) -> &'static str {
        match self {
            SortColumn::Name => "Name",
            SortColumn::DateModified => "Date modified",
            SortColumn::DateCreated => "Date created",
            SortColumn::Size => "Size",
            SortColumn::Type => "Type",
        }
    }

    /// The direction a person expects with none given: names and types A to Z, dates newest first,
    /// sizes largest first.
    pub fn natural_descending(self) -> bool {
        matches!(
            self,
            SortColumn::DateModified | SortColumn::DateCreated | SortColumn::Size
        )
    }
}

/// A sort order for a folder view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FolderSort {
    /// The column.
    pub column: SortColumn,
    /// Largest, newest or Z first.
    pub descending: bool,
}

impl FolderSort {
    /// `date_modified desc`, the form a result prints and a read-back returns.
    pub fn describe(self) -> String {
        format!(
            "{} {}",
            self.column.name(),
            if self.descending { "desc" } else { "asc" }
        )
    }

    /// The Shell's `SortColumns` text: `prop:-System.DateModified;` (a leading minus is descending).
    pub fn shell_spec(self) -> String {
        format!(
            "prop:{}{};",
            if self.descending { "-" } else { "" },
            self.column.property()
        )
    }

    /// Read the first column of a Shell `SortColumns` text back (`prop:-System.DateModified;`).
    /// `None` when it names no column winhands knows.
    pub fn from_shell_spec(text: &str) -> Option<Self> {
        let rest = text.split("prop:").nth(1)?;
        let (descending, rest) = match rest.chars().next()? {
            '-' => (true, &rest[1..]),
            '+' => (false, &rest[1..]),
            _ => (false, rest),
        };
        let prop: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.')
            .collect();
        SortColumn::ALL
            .into_iter()
            .find(|c| c.property().eq_ignore_ascii_case(&prop))
            .map(|column| FolderSort { column, descending })
    }
}

/// What a file manager's window shows after [`Backend::set_folder_view`](super::Backend::set_folder_view),
/// read back from the window itself. An aspect that could not be set or read is left empty and its
/// error is in `errors`; the core then falls back to the keyboard (view) or a header click (sort).
///
/// Python: the `state, errors` pair `shortcuts._com_apply` returns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderViewState {
    /// The view name read back (`details`), or empty.
    pub view: String,
    /// The sort read back (`date_modified desc`), or empty.
    pub sort: String,
    /// One entry per aspect that failed, worded `view: ...` or `sort: ...`.
    pub errors: Vec<String>,
}

// ------------------------------------------------------------------------------------ permissions

/// One OS permission a backend needs, as `winhands doctor` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionStatus {
    /// The permission's name as the OS shows it (`Accessibility`, `Screen Recording`).
    pub name: String,
    /// Granted to this process right now.
    pub granted: bool,
    /// What stops working without it.
    pub needed_for: String,
    /// Where to switch it on (`System Settings > Privacy & Security > Accessibility`).
    pub fix: String,
}

// ------------------------------------------------------------------------------------ capabilities

/// What this backend can do on this OS, so the core can degrade instead of failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    /// [`Backend::element_tree`](super::Backend::element_tree) and friends work.
    pub accessibility_tree: bool,
    /// The document text queries work.
    pub document_text: bool,
    /// [`Backend::ocr`](super::Backend::ocr) works.
    pub ocr: bool,
    /// Context menus can be read.
    pub popup_menus: bool,
    /// The file dialog filename field can be found.
    pub file_dialog: bool,
    /// The text caret can be located.
    pub caret: bool,
    /// A global kill hotkey can be registered.
    pub kill_hotkey: bool,
    /// The clipboard can report copied files.
    pub clipboard_files: bool,
    /// The OS has a UAC-style elevation boundary (`elevated` is meaningful).
    pub elevation: bool,
}

impl Capabilities {
    /// Everything on. What a complete backend, and the fake, report.
    pub const fn all() -> Self {
        Self {
            accessibility_tree: true,
            document_text: true,
            ocr: true,
            popup_menus: true,
            file_dialog: true,
            caret: true,
            kill_hotkey: true,
            clipboard_files: true,
            elevation: true,
        }
    }
}

// ------------------------------------------------------------------------------------ excel

/// One cell crossing the Excel channel. In JSON it is the plain value: `null`, a string, a number
/// or a bool. A string that starts with `=` is a formula when written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExcelCell {
    /// An empty cell.
    Empty,
    /// Text, or a formula when it starts with `=`.
    Text(String),
    /// A number.
    Number(f64),
    /// TRUE or FALSE.
    Bool(bool),
}

/// What [`Backend::excel`](super::Backend::excel) is asked to do. The backend owns one hidden Excel
/// instance and the workbook handles (`x1`, `x2`, ...) it hands out; it never attaches to an Excel
/// the person is using.
#[derive(Debug, Clone, PartialEq)]
pub enum ExcelRequest {
    /// Start the owned instance if needed and add an empty workbook.
    New,
    /// Open a workbook file in the owned instance.
    Open {
        /// Absolute path of the file.
        path: String,
    },
    /// Set a block of cells in one call, `range` being its top-left cell.
    Write {
        /// Handle from `New` or `Open`.
        workbook: String,
        /// Sheet name; the first sheet when `None`.
        sheet: Option<String>,
        /// Top-left cell, `A1` style.
        range: String,
        /// Rows of equal length.
        values: Vec<Vec<ExcelCell>>,
    },
    /// Read a block of cells.
    Read {
        /// Handle from `New` or `Open`.
        workbook: String,
        /// Sheet name; the first sheet when `None`.
        sheet: Option<String>,
        /// `A1:C5` style.
        range: String,
        /// Also return the formulas.
        formulas: bool,
    },
    /// Add a sheet after the last one.
    AddSheet {
        /// Handle from `New` or `Open`.
        workbook: String,
        /// Its name.
        name: String,
    },
    /// Recalculate every open workbook.
    Calc,
    /// Save as `.xlsx`.
    SaveAs {
        /// Handle from `New` or `Open`.
        workbook: String,
        /// Absolute path ending in `.xlsx`; the core has already checked it.
        path: String,
    },
    /// Close without saving; the instance quits with its last workbook.
    Close {
        /// Handle from `New` or `Open`.
        workbook: String,
    },
    /// Add a conditional format to a block of cells.
    FormatConditional {
        /// Handle from `New` or `Open`.
        workbook: String,
        /// Sheet name; the first sheet when `None`.
        sheet: Option<String>,
        /// `D2:D100001` style.
        range: String,
        /// What to highlight.
        rule: ExcelRule,
    },
    /// Add a chart of a block of cells (header row included) to the sheet.
    Chart {
        /// Handle from `New` or `Open`.
        workbook: String,
        /// Sheet name; the first sheet when `None`.
        sheet: Option<String>,
        /// The source data, header row included.
        range: String,
        /// The chart type.
        kind: ExcelChartKind,
        /// Title text, none for Excel's own default.
        title: Option<String>,
        /// Top-left cell for the chart; a few columns right of the data when `None`.
        at: Option<String>,
    },
    /// Build a pivot table of a block of cells on a new sheet.
    Pivot {
        /// Handle from `New` or `Open`.
        workbook: String,
        /// Sheet holding the source data; the first sheet when `None`.
        sheet: Option<String>,
        /// The source data, header row included.
        range: String,
        /// Header of the row field.
        rows: String,
        /// Header of the field to sum.
        sum_field: String,
        /// Header of the column field, if any.
        columns: Option<String>,
        /// Name of the new sheet the pivot goes to.
        name: String,
    },
}

/// A conditional-format rule. Colors are Excel color values (`R + G * 256 + B * 65536`).
#[derive(Debug, Clone, PartialEq)]
pub enum ExcelRule {
    /// Cell value greater than `value`.
    GreaterThan {
        /// The threshold.
        value: f64,
        /// Fill color.
        color: u32,
    },
    /// Cell value less than `value`.
    LessThan {
        /// The threshold.
        value: f64,
        /// Fill color.
        color: u32,
    },
    /// Cell value between `low` and `high`, ends included.
    Between {
        /// Lower end.
        low: f64,
        /// Upper end.
        high: f64,
        /// Fill color.
        color: u32,
    },
    /// Excel's default three-color scale.
    ColorScale,
}

/// The chart types `Chart` can draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExcelChartKind {
    /// Clustered columns.
    Column,
    /// Lines.
    Line,
    /// Clustered bars.
    Bar,
    /// A pie.
    Pie,
}

impl ExcelChartKind {
    /// The `XlChartType` constant Excel wants.
    pub fn xl_type(self) -> i32 {
        match self {
            Self::Column => 51,
            Self::Line => 4,
            Self::Bar => 57,
            Self::Pie => 5,
        }
    }
}

/// What the owned Excel answered. Only the fields an operation fills are set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExcelReply {
    /// The workbook handle (`New`, `Open`).
    pub workbook: Option<String>,
    /// Sheet names (`New`, `Open`, `AddSheet`).
    pub sheets: Vec<String>,
    /// Values read, row by row (`Read`).
    pub values: Vec<Vec<ExcelCell>>,
    /// Formulas read, row by row (`Read` with `formulas`).
    pub formulas: Vec<Vec<ExcelCell>>,
    /// The instance quit because its last workbook closed (`Close`).
    pub instance_quit: bool,
    /// A name Excel gave: the chart (`Chart`) or the sheet the pivot went to (`Pivot`).
    pub name: Option<String>,
    /// An address, without `$`: the pivot's output range (`Pivot`) or the chart's top-left cell
    /// (`Chart`).
    pub address: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_geometry() {
        let r = Rect::from_xywh(10, 20, 100, 50);
        assert_eq!((r.width(), r.height()), (100, 50));
        assert_eq!(r.center(), Point::new(60, 45));
        assert!(r.contains(Point::new(10, 20)) && !r.contains(Point::new(110, 20)));
        assert!(!r.contains(Point::new(109, 70)));
        assert!(Rect::new(0, 0, 0, 5).is_empty());
        assert_eq!(
            r.intersect(&Rect::new(0, 0, 30, 30)),
            Some(Rect::new(10, 20, 30, 30))
        );
        assert_eq!(r.intersect(&Rect::new(200, 0, 300, 30)), None);
        assert!(Rect::new(0, 0, 500, 500).covers(&r));
        assert_eq!(r.translate(5, -5), Rect::new(15, 15, 115, 65));
        // Python's // floors toward negative infinity, and so does center().
        assert_eq!(Rect::new(-3, -3, 0, 0).center(), Point::new(-2, -2));
    }

    #[test]
    fn window_refs_round_trip_through_parts_and_specs() {
        let w = WindowRef::from_parts(4242, 7);
        assert_eq!(w.parts(), (4242, 7));
        assert_eq!(WindowRef::from_raw(w.raw()), w);
        assert_eq!(
            WindowRef::parse_spec(&WindowRef::from_raw(197).spec()),
            Some(WindowRef::from_raw(197))
        );
        assert_eq!(
            WindowRef::parse_spec("HWND: 12"),
            Some(WindowRef::from_raw(12))
        );
        assert_eq!(WindowRef::parse_spec("w3"), None);
        assert_eq!(WindowRef::parse_spec("hwnd:x"), None);
        assert_eq!(WindowRef::parse_spec("hwnd:"), None);
        assert_eq!(WindowRef::from_raw(9).to_string(), "9");
    }

    #[test]
    fn images_check_their_size_and_crop() {
        assert!(Image::new(2, 2, vec![0; 15]).is_err());
        let mut img = Image::new(4, 3, vec![0; 48]).unwrap();
        img.put_pixel(2, 1, [1, 2, 3, 4]);
        assert_eq!(img.pixel(2, 1), Some([1, 2, 3, 4]));
        assert_eq!(img.pixel(4, 0), None);
        img.put_pixel(99, 99, [9; 4]);
        let cut = img.crop(1, 1, 3, 5).unwrap();
        assert_eq!((cut.width(), cut.height()), (3, 2));
        assert_eq!(cut.pixel(1, 0), Some([1, 2, 3, 4]));
        assert_eq!(cut.origin(), Point::new(1, 1));
        assert!(img.crop(4, 0, 1, 1).is_none());
        let solid = Image::solid(2, 2, [9, 8, 7, 255])
            .with_scale(2.0)
            .with_origin(Point::new(5, 6));
        assert_eq!((solid.scale(), solid.origin()), (2.0, Point::new(5, 6)));
        assert_eq!(solid.pixel(1, 1), Some([9, 8, 7, 255]));
    }

    #[test]
    fn display_pixel_size_follows_scale() {
        let d = Display {
            id: 1,
            name: String::new(),
            rect: Rect::from_xywh(0, 0, 1440, 900),
            scale: 2.0,
            primary: true,
        };
        assert_eq!(d.pixel_size(), (2880, 1800));
    }

    #[test]
    fn roles_know_what_python_counted_as_interactive() {
        for r in [
            Role::Button,
            Role::Edit,
            Role::CheckBox,
            Role::RadioButton,
            Role::ComboBox,
            Role::ListItem,
            Role::MenuItem,
            Role::TabItem,
            Role::Hyperlink,
            Role::TreeItem,
            Role::Slider,
            Role::SplitButton,
            Role::DataItem,
        ] {
            assert!(r.is_interactive(), "{r:?}");
        }
        assert!(!Role::Pane.is_interactive() && !Role::Document.is_interactive());
        assert!(Role::ListItem.is_item() && Role::List.is_container() && !Role::Button.is_item());
        assert!(Role::CheckBox.is_stateful() && !Role::Button.is_stateful());
        assert_eq!(Role::ListItem.words(), "list item");
        assert_eq!(Role::CheckBox.words(), "check box");
        assert_eq!(Role::Other("Thumb".into()).words(), "thumb");
        assert_eq!(Role::Other("Thumb".into()).short_name(), "Thumb");
    }

    #[test]
    fn element_state_flags_follow_the_python_order() {
        let s = ElementState {
            toggle: Some(Toggle::On),
            selected: true,
            expanded: Some(Expand::Collapsed),
        };
        assert_eq!(s.flags(), ["on", "selected", "collapsed"]);
        assert!(ElementState::default().flags().is_empty());
        let mixed = ElementState {
            toggle: Some(Toggle::Mixed),
            ..Default::default()
        };
        assert_eq!(mixed.flags(), ["mixed"]);
    }

    #[test]
    fn interactive_elements_by_role_or_affordance() {
        assert!(Element::new("1", Role::Button, "OK").is_interactive());
        let mut pane = Element::new("2", Role::Pane, "");
        assert!(!pane.is_interactive());
        pane.affordances.push(Affordance::Invoke);
        assert!(pane.is_interactive());
        let mut doc = Element::new("3", Role::Document, "page");
        assert!(!doc.is_interactive());
        doc.affordances.push(Affordance::Text);
        assert!(doc.is_interactive());
    }

    #[test]
    fn element_trees_walk_in_document_order() {
        let tree = Element::new("r", Role::Window, "w")
            .with_child(Element::new("a", Role::Pane, "").with_child(Element::new(
                "a1",
                Role::Button,
                "b",
            )))
            .with_child(Element::new("b", Role::Button, "c"));
        let mut out = Vec::new();
        tree.walk(&mut out);
        let ids: Vec<&str> = out.iter().map(|e| e.id.0.as_str()).collect();
        assert_eq!(ids, ["r", "a", "a1", "b"]);
        assert_eq!(tree.count(), 4);
    }

    #[test]
    fn tree_limit_presets_match_the_python_defaults() {
        let s = TreeLimits::snapshot(120);
        assert_eq!((s.max_items, s.max_depth), (120, 25));
        assert_eq!(s.deadline, Duration::from_millis(2500));
        assert_eq!(s.filter, ElementFilter::Interactive);
        let n = TreeLimits::named(Duration::from_millis(300));
        assert_eq!(
            (n.max_depth, n.filter, n.wake),
            (12, ElementFilter::Named, WakePolicy::Never)
        );
    }

    #[test]
    fn scroll_directions_parse() {
        assert_eq!(ScrollDirection::parse("up"), Some(ScrollDirection::Up));
        assert_eq!(ScrollDirection::parse("sideways"), None);
    }

    #[test]
    fn errors_read_like_the_python_messages() {
        assert_eq!(
            BackendError::WindowGone.to_string(),
            "window unavailable; refresh windows"
        );
        assert_eq!(
            BackendError::ElementGone.to_string(),
            "element is gone; re-snapshot"
        );
    }
}
