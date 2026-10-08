//! Window-server facts to [`WindowInfo`]. The platform layer reads each `CGWindowListCopyWindowInfo`
//! dictionary into a [`RawWindow`]; everything after that (which windows count, what kind they are,
//! the stable handle, hit testing) is pure and tested on every OS.

use wh_core::backend::{Point, Rect, WindowInfo, WindowRef};

use crate::roles::window_kind;

/// Smallest window edge, in points, that counts as an application window. Smaller ones are
/// helper surfaces (1x1 input method windows, drag handles).
pub(crate) const MIN_WINDOW_EDGE: i32 = 32;

/// `kCGPopUpMenuWindowLevel`.
pub(crate) const POPUP_MENU_LAYER: i32 = 101;

/// One entry of the window server's list, in front-to-back order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawWindow {
    /// `kCGWindowNumber`.
    pub(crate) id: u32,
    /// `kCGWindowOwnerPID`.
    pub(crate) pid: i32,
    /// `kCGWindowLayer`.
    pub(crate) layer: i32,
    /// `kCGWindowAlpha`.
    pub(crate) alpha: f64,
    /// `kCGWindowBounds` in global points.
    pub(crate) bounds: Rect,
    /// `kCGWindowName`; absent without the Screen Recording grant, or for untitled windows.
    pub(crate) title: Option<String>,
    /// `kCGWindowOwnerName`.
    pub(crate) owner_name: String,
    /// `kCGWindowIsOnscreen`.
    pub(crate) on_screen: bool,
}

/// What the platform layer adds from other sources (AX, NSRunningApplication).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct WindowContext {
    /// The owner's bundle identifier; empty when it has none.
    pub(crate) bundle_id: String,
    /// `AXMinimized`.
    pub(crate) minimized: bool,
    /// This is the frontmost app's focused window.
    pub(crate) foreground: bool,
    /// The window's `AXSubrole`, when known.
    pub(crate) subrole: Option<String>,
    /// A title read through AX when the window server gave none.
    pub(crate) ax_title: Option<String>,
}

/// The stable handle: owning pid plus `CGWindowID`. The window server never reuses an id while the
/// window lives, and the pid disambiguates a reused id after it dies.
pub(crate) fn window_ref(pid: i32, id: u32) -> WindowRef {
    WindowRef::from_parts(pid as u32, id)
}

/// Split a handle back into `(pid, CGWindowID)`.
pub(crate) fn window_parts(window: WindowRef) -> (i32, u32) {
    let (pid, id) = window.parts();
    (pid as i32, id)
}

impl RawWindow {
    /// Visible enough to matter: not fully transparent, with a real size.
    fn has_substance(&self) -> bool {
        self.alpha > 0.01
            && self.bounds.width() >= MIN_WINDOW_EDGE
            && self.bounds.height() >= MIN_WINDOW_EDGE
    }

    /// An ordinary application window (layer 0, the "normal" level).
    pub(crate) fn is_app_window(&self) -> bool {
        self.layer == 0 && self.has_substance()
    }

    /// A pop-up or context menu surface.
    pub(crate) fn is_popup_menu(&self) -> bool {
        self.layer == POPUP_MENU_LAYER
            && self.on_screen
            && self.alpha > 0.01
            && !self.bounds.is_empty()
    }

    /// Something a click can land on, for hit testing. Skips the window server's own overlays
    /// and invisible windows.
    pub(crate) fn is_hit_target(&self) -> bool {
        self.on_screen
            && self.alpha > 0.01
            && !self.bounds.is_empty()
            && self.owner_name != "Window Server"
            && self.layer >= 0
    }
}

/// The topmost hit target under `p`, given windows in front-to-back order.
pub(crate) fn window_at_point(windows: &[RawWindow], p: Point) -> Option<&RawWindow> {
    windows
        .iter()
        .find(|w| w.is_hit_target() && w.bounds.contains(p))
}

/// The frontmost app's frontmost application window, the fallback when its AX focused window
/// cannot be read.
pub(crate) fn frontmost_app_window(windows: &[RawWindow], pid: i32) -> Option<&RawWindow> {
    windows
        .iter()
        .find(|w| w.pid == pid && w.on_screen && w.is_app_window())
}

/// The share of a `grid x grid` sample of `rect` where the window `(pid, id)` is what is on
/// screen, from one snapshot of the window server's list (front to back).
pub(crate) fn visible_fraction(
    windows: &[RawWindow],
    target: (i32, u32),
    rect: Rect,
    grid: u32,
) -> f64 {
    if rect.is_empty() || grid == 0 {
        return 0.0;
    }
    let (mut hits, mut total) = (0u32, 0u32);
    for i in 0..grid {
        for j in 0..grid {
            let x = rect.left
                + (f64::from(rect.width()) * (f64::from(i) + 0.5) / f64::from(grid)) as i32;
            let y = rect.top
                + (f64::from(rect.height()) * (f64::from(j) + 0.5) / f64::from(grid)) as i32;
            total += 1;
            if window_at_point(windows, Point { x, y }).is_some_and(|w| (w.pid, w.id) == target) {
                hits += 1;
            }
        }
    }
    f64::from(hits) / f64::from(total)
}

/// Does the window cover the whole of any one display (native full screen, a game)?
pub(crate) fn is_fullscreen(bounds: Rect, displays: &[Rect]) -> bool {
    !bounds.is_empty() && displays.iter().any(|d| bounds.covers(d))
}

/// Build the neutral record.
pub(crate) fn to_info(raw: &RawWindow, ctx: &WindowContext, displays: &[Rect]) -> WindowInfo {
    let title = raw
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .or_else(|| ctx.ax_title.clone())
        .unwrap_or_default();
    let app = if ctx.bundle_id.is_empty() {
        raw.owner_name.clone()
    } else {
        ctx.bundle_id.clone()
    };
    let class = match &ctx.subrole {
        Some(s) if !s.is_empty() => s.clone(),
        _ if raw.layer == 0 => "AXWindow".to_string(),
        _ => format!("CGLayer{}", raw.layer),
    };
    let kind = window_kind(ctx.subrole.as_deref(), raw.layer, &ctx.bundle_id);
    WindowInfo {
        window: window_ref(raw.pid, raw.id),
        title,
        app,
        pid: raw.pid as u32,
        rect: raw.bounds,
        minimized: ctx.minimized,
        fullscreen: raw.on_screen && raw.layer == 0 && is_fullscreen(raw.bounds, displays),
        foreground: ctx.foreground,
        hidden: !raw.on_screen && !ctx.minimized,
        elevated: false,
        kind,
        class,
        owner: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wh_core::backend::WindowKind;

    fn raw(id: u32, pid: i32, layer: i32, bounds: Rect) -> RawWindow {
        RawWindow {
            id,
            pid,
            layer,
            alpha: 1.0,
            bounds,
            title: Some(format!("w{id}")),
            owner_name: "App".into(),
            on_screen: true,
        }
    }

    #[test]
    fn handles_pack_pid_and_window_id() {
        let w = window_ref(4242, 77);
        assert_eq!(w.parts(), (4242, 77));
        assert_eq!(window_parts(w), (4242, 77));
        assert_ne!(window_ref(1, 77), window_ref(2, 77));
        assert_ne!(window_ref(1, 77), window_ref(1, 78));
    }

    #[test]
    fn app_windows_are_layer_zero_sized_and_visible() {
        let big = Rect::from_xywh(0, 0, 400, 300);
        assert!(raw(1, 1, 0, big).is_app_window());
        assert!(!raw(1, 1, 3, big).is_app_window(), "floating panel");
        assert!(!raw(1, 1, 0, Rect::from_xywh(0, 0, 10, 10)).is_app_window());
        let mut ghost = raw(1, 1, 0, big);
        ghost.alpha = 0.0;
        assert!(!ghost.is_app_window());
    }

    #[test]
    fn popup_menus_sit_on_layer_101() {
        let menu = Rect::from_xywh(10, 10, 200, 120);
        assert!(raw(1, 1, 101, menu).is_popup_menu());
        assert!(!raw(1, 1, 0, menu).is_popup_menu());
        let mut off = raw(1, 1, 101, menu);
        off.on_screen = false;
        assert!(!off.is_popup_menu());
    }

    #[test]
    fn hit_testing_takes_the_first_window_in_z_order() {
        let front = raw(1, 1, 0, Rect::from_xywh(50, 50, 100, 100));
        let back = raw(2, 2, 0, Rect::from_xywh(0, 0, 400, 400));
        let all = [front.clone(), back.clone()];
        assert_eq!(window_at_point(&all, Point::new(60, 60)).unwrap().id, 1);
        assert_eq!(window_at_point(&all, Point::new(10, 10)).unwrap().id, 2);
        assert!(window_at_point(&all, Point::new(900, 900)).is_none());
    }

    #[test]
    fn hit_testing_skips_overlays_and_the_desktop() {
        let mut overlay = raw(1, 9, 25, Rect::from_xywh(0, 0, 800, 600));
        overlay.owner_name = "Window Server".into();
        let wallpaper = raw(2, 9, -2147483624, Rect::from_xywh(0, 0, 800, 600));
        let mut clear = raw(3, 5, 0, Rect::from_xywh(0, 0, 800, 600));
        clear.alpha = 0.0;
        let real = raw(4, 6, 0, Rect::from_xywh(0, 0, 800, 600));
        let all = [overlay, wallpaper, clear, real];
        assert_eq!(window_at_point(&all, Point::new(5, 5)).unwrap().id, 4);
    }

    #[test]
    fn frontmost_window_of_an_app_skips_other_apps_and_panels() {
        let all = [
            raw(1, 7, 0, Rect::from_xywh(0, 0, 400, 300)),
            raw(2, 8, 3, Rect::from_xywh(0, 0, 400, 300)),
            raw(3, 8, 0, Rect::from_xywh(0, 0, 400, 300)),
            raw(4, 8, 0, Rect::from_xywh(0, 0, 400, 300)),
        ];
        assert_eq!(frontmost_app_window(&all, 8).unwrap().id, 3);
        assert!(frontmost_app_window(&all, 99).is_none());
    }

    #[test]
    fn full_screen_means_covering_a_whole_display() {
        let displays = [
            Rect::from_xywh(0, 0, 1440, 900),
            Rect::from_xywh(1440, 0, 1920, 1080),
        ];
        assert!(is_fullscreen(
            Rect::from_xywh(1440, 0, 1920, 1080),
            &displays
        ));
        assert!(!is_fullscreen(Rect::from_xywh(0, 25, 1440, 875), &displays));
        assert!(!is_fullscreen(Rect::default(), &displays));
    }

    #[test]
    fn info_uses_bundle_id_title_and_flags() {
        let r = raw(9, 321, 0, Rect::from_xywh(10, 20, 300, 200));
        let ctx = WindowContext {
            bundle_id: "com.apple.TextEdit".into(),
            foreground: true,
            subrole: Some("AXStandardWindow".into()),
            ..Default::default()
        };
        let info = to_info(&r, &ctx, &[]);
        assert_eq!(info.app, "com.apple.TextEdit");
        assert_eq!(info.title, "w9");
        assert_eq!(info.pid, 321);
        assert_eq!(info.window, window_ref(321, 9));
        assert!(info.foreground && !info.hidden && !info.minimized);
        assert_eq!(info.class, "AXStandardWindow");
        assert_eq!(info.kind, WindowKind::Normal);
        assert!(!info.elevated);
    }

    #[test]
    fn info_falls_back_to_the_owner_name_and_the_ax_title() {
        let mut r = raw(9, 321, 0, Rect::from_xywh(10, 20, 300, 200));
        r.title = None;
        let ctx = WindowContext {
            ax_title: Some("Untitled".into()),
            ..Default::default()
        };
        let info = to_info(&r, &ctx, &[]);
        assert_eq!(info.app, "App");
        assert_eq!(info.title, "Untitled");
        assert_eq!(info.class, "AXWindow");
    }

    #[test]
    fn off_screen_windows_are_minimised_or_hidden() {
        let mut r = raw(9, 321, 0, Rect::from_xywh(10, 20, 300, 200));
        r.on_screen = false;
        let hidden = to_info(&r, &WindowContext::default(), &[]);
        assert!(hidden.hidden && !hidden.minimized);
        let ctx = WindowContext {
            minimized: true,
            ..Default::default()
        };
        let min = to_info(&r, &ctx, &[]);
        assert!(min.minimized && !min.hidden);
    }

    #[test]
    fn popup_and_dialog_kinds_come_through() {
        let menu = raw(1, 1, 101, Rect::from_xywh(0, 0, 100, 100));
        assert_eq!(
            to_info(&menu, &WindowContext::default(), &[]).kind,
            WindowKind::PopupMenu
        );
        assert_eq!(
            to_info(&menu, &WindowContext::default(), &[]).class,
            "CGLayer101"
        );
        let sheet = raw(2, 1, 0, Rect::from_xywh(0, 0, 100, 100));
        let ctx = WindowContext {
            subrole: Some("AXSheet".into()),
            ..Default::default()
        };
        assert_eq!(to_info(&sheet, &ctx, &[]).kind, WindowKind::Dialog);
    }

    #[test]
    fn visible_fraction_counts_samples_where_the_window_is_on_top() {
        let target = raw(1, 1, 0, Rect::from_xywh(0, 0, 100, 100));
        let cover = raw(2, 2, 0, Rect::from_xywh(0, 0, 50, 100));
        let all = [cover, target.clone()];
        let whole = Rect::from_xywh(0, 0, 100, 100);
        let f = visible_fraction(&all, (1, 1), whole, 4);
        assert!((f - 0.5).abs() < 1e-9, "{f}");
        assert_eq!(visible_fraction(&[target], (1, 1), whole, 4), 1.0);
        assert_eq!(visible_fraction(&all, (1, 1), Rect::default(), 4), 0.0);
        assert_eq!(visible_fraction(&all, (9, 9), whole, 4), 0.0);
    }
}
