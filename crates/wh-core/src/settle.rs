//! Waiting for the screen to stop moving. Port of `computer.settle`, `_settle_floor_s`,
//! `_slow_ui_window`, `_is_fullscreen` and `_exposes_uia`.
//!
//! After an acting call the daemon watches small grayscale thumbnails of the display until fewer than
//! 2% of the pixels change between two looks, instead of sleeping a fixed time. A static app is ready
//! in about 60 ms; an animation gets up to `settle_ms` (1500 ms).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::backend::{
    Backend, BackendError, BackendResult, ElementFilter, Image, Rect, TreeLimits, WakePolicy,
    WindowRef,
};
use crate::clock::Clock;
use crate::config::Config;
use crate::imaging::{self, BLANK_STD};
use crate::scale::display_containing;

/// The floor that fullscreen windows and windows with no UIA elements keep (a game's menu fade
/// starts a beat late and a screenshot taken at the first calm read was half dark):
/// `computer.LONG_SETTLE_FLOOR_S`.
pub const LONG_SETTLE_FLOOR: Duration = Duration::from_millis(450);

/// The title-bar buttons alone are 3 elements, so a window needs at least this many interactive
/// elements to count as exposing UIA (`computer._UIA_RICH_MIN`).
pub const UIA_RICH_MIN: usize = 4;

/// How long a window's "exposes UIA" answer is trusted (`computer._UIA_RICH_TTL_S`).
pub const UIA_RICH_TTL: Duration = Duration::from_secs(10);

/// Pause before the first look. The gap between looks is the backend's
/// [`Backend::settle_look_interval`].
const FIRST_LOOK_DELAY: Duration = Duration::from_millis(40);

/// How the loop behaves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SettleParams {
    /// Give up after this long (`settle_ms`).
    pub max: Duration,
    /// Keep watching at least this long even when calm (the settle floor after an acting call).
    /// Some UIs start their transition a beat late (a map node animates, then the fight loads).
    pub min: Duration,
    /// Calm looks in a row that end the wait: 2 for the trailing settle (a slow fade changes little
    /// per frame), 1 for the mid-batch step settle where the next step acts anyway.
    pub calm_needed: u32,
    /// A blank (black or white) screen is steady but not ready: it gets this long before giving up
    /// (`loading_ms`).
    pub loading_max: Duration,
}

impl SettleParams {
    /// The trailing settle with the configured limits: `settle_ms`, two calm looks, `loading_ms`.
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            max: Duration::from_secs_f64(cfg.settle_max_s().max(0.0)),
            min: Duration::ZERO,
            calm_needed: 2,
            loading_max: Duration::from_secs_f64(cfg.loading_max_s().max(0.0)),
        }
    }

    /// Keep watching at least `min`.
    pub fn with_min(mut self, min: Duration) -> Self {
        self.min = min;
        self
    }

    /// End after `n` calm looks in a row.
    pub fn with_calm_needed(mut self, n: u32) -> Self {
        self.calm_needed = n;
        self
    }

    /// Give up after `max`.
    pub fn with_max(mut self, max: Duration) -> Self {
        self.max = max;
        self
    }
}

/// Wait until the screen stops changing. Returns how long it watched. Python `settle()`.
///
/// Compares small grayscale thumbnails of `region` (the display the model sees). "Calm" means fewer
/// than 2% of the pixels changed visibly between two looks; an animated background (waterfall,
/// water, idle sprites) keeps the average difference up forever, so the share is what counts.
pub fn settle(
    backend: &dyn Backend,
    clock: &dyn Clock,
    region: Rect,
    params: &SettleParams,
) -> BackendResult<Duration> {
    settle_with_frame(backend, clock, region, params, &|| Ok(())).map(|s| s.elapsed)
}

/// Size of each settle look. The backend may shrink the screen to this while copying it, which is
/// far cheaper than a full copy (2026-10-28: 25-30 ms for 2560x1440 by `BitBlt`).
pub const PREVIEW_SIZE: (u32, u32) = (320, 180);

/// Last frame used to establish readiness. Reusing it avoids a duplicate final capture.
pub struct SettledFrame {
    /// Time spent observing and waiting.
    pub elapsed: Duration,
    /// Most recent captured pixels: full size when the backend could not shrink the looks, else a
    /// [`PREVIEW_SIZE`] preview that only a change check should use (see [`SettledFrame::full`]).
    pub image: Image,
    region: Rect,
}

impl SettledFrame {
    /// The last look when it is a full-size capture of the region, else `None` (capture again).
    pub fn full(self) -> Option<Image> {
        let full = i64::from(self.image.width()) == i64::from(self.region.width())
            && i64::from(self.image.height()) == i64::from(self.region.height());
        full.then_some(self.image)
    }
}

fn checked_sleep(
    clock: &dyn Clock,
    duration: Duration,
    guard: &dyn Fn() -> BackendResult<()>,
) -> BackendResult<()> {
    let end = clock.now().saturating_add(duration);
    loop {
        guard()?;
        let left = end.saturating_sub(clock.now());
        if left.is_zero() {
            return Ok(());
        }
        clock.sleep(left.min(Duration::from_millis(25)));
    }
}

/// Cancellable readiness wait that retains its last image for the response.
pub fn settle_with_frame(
    backend: &dyn Backend,
    clock: &dyn Clock,
    region: Rect,
    params: &SettleParams,
    guard: &dyn Fn() -> BackendResult<()>,
) -> BackendResult<SettledFrame> {
    let _p = crate::prof::span("settle");
    let calm_needed = params.calm_needed.max(1);
    let interval = backend.settle_look_interval();
    let start = clock.now();
    let look = || backend.capture_preview(region, PREVIEW_SIZE.0, PREVIEW_SIZE.1);
    checked_sleep(clock, FIRST_LOOK_DELAY, guard)?;
    let mut image = look()?;
    guard()?;
    let mut prev = imaging::thumbnail(&image).map_err(|e| BackendError::Os(e.to_string()))?;
    let mut calm = 0u32;
    let mut looked = clock.now();
    while clock.now().saturating_sub(start) < params.max {
        // 2026-10-11 (macOS): a capture alone takes ~50 ms, so the gap counts from the last look's
        // start, not its end; sleeping the full interval after it made every settle ~120 ms a look.
        let gap = interval.saturating_sub(clock.now().saturating_sub(looked));
        checked_sleep(
            clock,
            gap.min(params.max.saturating_sub(clock.now().saturating_sub(start))),
            guard,
        )?;
        looked = clock.now();
        image = look()?;
        guard()?;
        let cur = imaging::thumbnail(&image).map_err(|e| BackendError::Os(e.to_string()))?;
        // Two calm reads in a row: a slow fade changes little per frame and fooled a single check
        // into returning a half-dark screen (Slay the Spire's menu fade, 2026-09-20).
        calm = if imaging::is_calm(&prev, &cur) {
            calm + 1
        } else {
            0
        };
        let elapsed = clock.now().saturating_sub(start);
        if calm >= calm_needed && elapsed < params.min {
            // Grace after an action: keep watching a little before calling it done.
            calm = calm_needed - 1;
        } else if calm >= calm_needed {
            if cur.std_dev() < BLANK_STD && elapsed < params.loading_max {
                // A blank screen is steady but not ready: a loading screen.
                calm = 0;
            } else {
                break;
            }
        }
        prev = cur;
    }
    Ok(SettledFrame {
        elapsed: clock.now().saturating_sub(start),
        image,
        region,
    })
}

/// Does the window cover the whole of the monitor it is on (a fullscreen game or video)?
/// A borderless window that overshoots the monitor counts; a maximised one with the taskbar showing
/// does not. Python `_is_fullscreen`.
pub fn is_fullscreen(window: Rect, monitor: Rect) -> bool {
    window.covers(&monitor)
}

/// Remembers, per window, whether it exposes real accessibility elements, for a few seconds: the
/// query walks the whole tree and a window rarely changes its mind. Python `_UIA_RICH_CACHE`.
#[derive(Default)]
pub struct UiaProbe {
    cache: Mutex<HashMap<WindowRef, (Duration, bool)>>,
}

impl UiaProbe {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Does `window` expose more than its title-bar buttons? A failed query counts as "no": keep the
    /// long floor. Python `_exposes_uia`.
    pub fn exposes_uia(&self, backend: &dyn Backend, clock: &dyn Clock, window: WindowRef) -> bool {
        let now = clock.now();
        if let Some((at, rich)) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&window)
            .copied()
            && now.saturating_sub(at) < UIA_RICH_TTL
        {
            return rich;
        }
        let limits = TreeLimits {
            max_items: UIA_RICH_MIN,
            max_depth: 25,
            deadline: Duration::from_millis(2500),
            filter: ElementFilter::Interactive,
            flatten: true,
            wake: WakePolicy::Never,
        };
        let rich = backend
            .element_tree(window, &limits)
            .map(|t| t.root.children.len() >= UIA_RICH_MIN)
            .unwrap_or(false);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() > 32 {
            cache.clear();
        }
        cache.insert(window, (now, rich));
        rich
    }

    /// Forget everything.
    pub fn clear(&self) {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

/// The front window needs the long settle floor: nothing in front, fullscreen, or no UIA tree.
/// `fallback_monitor` is the display the model sees, used when the window is on no known monitor.
/// Python `_slow_ui_window`.
pub fn slow_ui_window(
    backend: &dyn Backend,
    clock: &dyn Clock,
    probe: &UiaProbe,
    fallback_monitor: Rect,
) -> BackendResult<bool> {
    let Some(front) = backend.foreground_window()? else {
        return Ok(true);
    };
    let root = backend.root_window(front)?;
    let info = backend.window_info(root)?;
    let c = info.rect.center();
    let monitor = display_containing(&backend.displays()?, c.x, c.y).unwrap_or(fallback_monitor);
    Ok(is_fullscreen(info.rect, monitor) || !probe.exposes_uia(backend, clock, root))
}

/// Minimum watch time of the trailing settle after an acting call: the configured floor (130 ms by
/// default), raised to 450 ms where late fades happen (a fullscreen window, a window with no UIA
/// elements), and 450 ms whenever that cannot be told. A configured floor of 450 ms or more is
/// honoured as is. Python `_settle_floor_s`.
pub fn settle_floor(
    cfg: &Config,
    backend: &dyn Backend,
    clock: &dyn Clock,
    probe: &UiaProbe,
    fallback_monitor: Rect,
) -> Duration {
    let _p = crate::prof::span("settle");
    let floor = Duration::from_secs_f64(cfg.settle_floor_s());
    if floor >= LONG_SETTLE_FLOOR {
        return floor;
    }
    match slow_ui_window(backend, clock, probe, fallback_monitor) {
        Ok(false) => floor,
        Ok(true) | Err(_) => LONG_SETTLE_FLOOR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeBackend;
    use crate::backend::{Affordance, Display, Element, Image, Role, WindowInfo};
    use crate::clock::FakeClock;
    use crate::env::MapEnv;
    use crate::paths::Paths;
    use crate::testutil::TempDir;

    // ---- ported from _settle_env: a static, non-blank screen with a fake clock ----

    fn static_screen() -> (FakeBackend, Rect) {
        let backend = FakeBackend::new();
        let rect = Rect::from_xywh(0, 0, 320, 180);
        backend.set_displays(vec![Display {
            id: 1,
            name: String::new(),
            rect,
            scale: 1.0,
            primary: true,
        }]);
        let mut bytes = Vec::new();
        for i in 0..(320 * 180u32) {
            let v = ((i / 2) * 7 % 256) as u8;
            bytes.extend_from_slice(&[v, v, v, 255]);
        }
        backend.set_screen(Image::new(320, 180, bytes).unwrap());
        (backend, rect)
    }

    fn params(max_ms: u64) -> SettleParams {
        SettleParams {
            max: Duration::from_millis(max_ms),
            min: Duration::ZERO,
            calm_needed: 2,
            loading_max: Duration::from_secs(6),
        }
    }

    #[test]
    fn settle_needs_two_calm_reads_by_default() {
        let (backend, rect) = static_screen();
        let clock = FakeClock::new();
        settle(&backend, &clock, rect, &params(1500)).unwrap();
        assert_eq!(backend.capture_count(), 3); // the first look, then two calm reads
    }

    #[test]
    fn settle_with_one_calm_read() {
        let (backend, rect) = static_screen();
        let clock = FakeClock::new();
        let took = settle(&backend, &clock, rect, &params(1500).with_calm_needed(1)).unwrap();
        assert_eq!(backend.capture_count(), 2);
        assert!(took < Duration::from_millis(200), "{took:?}");
    }

    #[test]
    fn settle_min_grace_still_applies_with_one_calm_read() {
        let (backend, rect) = static_screen();
        let clock = FakeClock::new();
        let p = params(1500)
            .with_calm_needed(1)
            .with_min(Duration::from_millis(400));
        let took = settle(&backend, &clock, rect, &p).unwrap();
        assert!(took >= Duration::from_millis(400), "{took:?}");
    }

    #[test]
    fn a_busy_screen_gets_the_whole_budget() {
        // Every look differs from the last, so it never calms and the budget ends the wait.
        let (backend, rect) = static_screen();
        let frames: Vec<Image> = (0..60u8)
            .map(|i| Image::solid(320, 180, [if i % 2 == 0 { 20 } else { 230 }, 100, 100, 255]))
            .collect();
        backend.queue_screens(frames);
        let clock = FakeClock::new();
        let took = settle(&backend, &clock, rect, &params(500)).unwrap();
        assert!(
            took >= Duration::from_millis(500) && took < Duration::from_millis(700),
            "{took:?}"
        );
    }

    #[test]
    fn a_blank_screen_is_a_loading_screen_until_loading_ms() {
        let backend = FakeBackend::new();
        let rect = Rect::from_xywh(0, 0, 320, 180);
        backend.set_displays(vec![Display {
            id: 1,
            name: String::new(),
            rect,
            scale: 1.0,
            primary: true,
        }]);
        backend.set_screen(Image::solid(320, 180, [0, 0, 0, 255]));
        let clock = FakeClock::new();
        let p = SettleParams {
            loading_max: Duration::from_secs(2),
            ..params(10_000)
        };
        let took = settle(&backend, &clock, rect, &p).unwrap();
        assert!(
            took >= Duration::from_secs(2),
            "waited out the loading screen: {took:?}"
        );
        assert!(took < Duration::from_secs(3), "{took:?}");
    }

    #[test]
    fn a_capture_failure_is_reported() {
        let backend = FakeBackend::new();
        let clock = FakeClock::new();
        let bad = Rect::from_xywh(0, 0, 10, 10);
        backend.set_displays(Vec::new());
        assert!(settle(&backend, &clock, bad, &params(100)).is_err());
    }

    #[test]
    fn settle_params_come_from_config() {
        let dir = TempDir::new("settle-cfg");
        let cfg = Config::new(
            Paths::in_dir(dir.path()),
            MapEnv::new().with("WINHANDS_SETTLE_MS", "900"),
        );
        let p = SettleParams::from_config(&cfg);
        assert_eq!(p.max, Duration::from_millis(900));
        assert_eq!(p.calm_needed, 2);
        assert_eq!(p.loading_max, Duration::from_secs(6));
        assert_eq!(p.min, Duration::ZERO);
    }

    // ---- ported from tests/test_20261008_speed_fixes.py: the floor ----

    /// A config whose directory does not exist, so only the environment and defaults count.
    fn no_file_config(env: MapEnv) -> Config {
        Config::new(
            Paths::in_dir(std::env::temp_dir().join("wh-core-no-such-dir")),
            env,
        )
    }

    fn floor_env(
        fullscreen: bool,
        rich: bool,
        front: bool,
    ) -> (FakeBackend, Option<WindowRef>, Config) {
        let backend = FakeBackend::new();
        let rect = if fullscreen {
            Rect::from_xywh(0, 0, 1920, 1080)
        } else {
            Rect::from_xywh(100, 100, 800, 600)
        };
        let w = backend.add_window(WindowInfo::basic("App", "app.exe", 5, rect));
        let n = if rich { 6 } else { 3 };
        let mut root = Element::new("root", Role::Window, "App");
        for i in 0..n {
            let mut b = Element::new(&format!("b{i}"), Role::Button, "b");
            b.affordances.push(Affordance::Invoke);
            root = root.with_child(b);
        }
        backend.set_tree(w, root);
        backend.set_foreground(front.then_some(w));
        (backend, front.then_some(w), no_file_config(MapEnv::new()))
    }

    fn floor_of(backend: &FakeBackend, cfg: &Config) -> Duration {
        let monitor = Rect::from_xywh(0, 0, 1920, 1080);
        settle_floor(cfg, backend, &FakeClock::new(), &UiaProbe::new(), monitor)
    }

    #[test]
    fn floor_is_130ms_for_an_ordinary_window() {
        let (backend, _, cfg) = floor_env(false, true, true);
        assert_eq!(floor_of(&backend, &cfg), Duration::from_millis(130));
    }

    #[test]
    fn floor_stays_450ms_for_a_fullscreen_window() {
        let (backend, _, cfg) = floor_env(true, true, true);
        assert_eq!(floor_of(&backend, &cfg), Duration::from_millis(450));
    }

    #[test]
    fn floor_stays_450ms_when_the_window_exposes_no_uia() {
        let (backend, _, cfg) = floor_env(false, false, true);
        assert_eq!(floor_of(&backend, &cfg), Duration::from_millis(450));
    }

    #[test]
    fn floor_stays_450ms_with_no_front_window_or_on_error() {
        let (backend, _, cfg) = floor_env(false, true, false);
        assert_eq!(floor_of(&backend, &cfg), Duration::from_millis(450));
        // the foreground window vanished underneath us: cannot tell, keep the safe floor
        let (backend, front, cfg) = floor_env(false, true, true);
        backend.close_window(front.unwrap());
        backend.set_foreground(front);
        assert_eq!(floor_of(&backend, &cfg), Duration::from_millis(450));
    }

    #[test]
    fn a_configured_floor_above_450_is_honoured() {
        let (backend, _, _) = floor_env(false, true, true);
        let cfg = no_file_config(MapEnv::new().with("WINHANDS_SETTLE_FLOOR_MS", "700"));
        assert_eq!(floor_of(&backend, &cfg), Duration::from_millis(700));
    }

    #[test]
    fn is_fullscreen_compares_window_to_monitor() {
        let monitor = Rect::new(0, 0, 1920, 1080);
        assert!(is_fullscreen(Rect::new(0, 0, 1920, 1080), monitor));
        // maximised with the taskbar showing
        assert!(!is_fullscreen(Rect::new(0, 0, 1920, 1040), monitor));
        // borderless with the usual overshoot
        assert!(is_fullscreen(Rect::new(-8, -8, 1928, 1088), monitor));
    }

    #[test]
    fn exposes_uia_threshold_and_cache() {
        let backend = FakeBackend::new();
        let tree = |n: usize| {
            let mut root = Element::new("r", Role::Window, "w");
            for i in 0..n {
                root = root.with_child(Element::new(&format!("b{i}"), Role::Button, "b"));
            }
            root
        };
        let one = backend.add_window(WindowInfo::basic(
            "one",
            "a.exe",
            1,
            Rect::new(0, 0, 10, 10),
        ));
        let two = backend.add_window(WindowInfo::basic(
            "two",
            "a.exe",
            1,
            Rect::new(0, 0, 10, 10),
        ));
        let three = WindowRef::from_raw(3); // the query fails: no such window
        backend.set_tree(one, tree(3)); // only the min/max/close buttons
        backend.set_tree(two, tree(6));
        let clock = FakeClock::new();
        let probe = UiaProbe::new();
        assert!(!probe.exposes_uia(&backend, &clock, one));
        assert!(probe.exposes_uia(&backend, &clock, two));
        assert!(
            !probe.exposes_uia(&backend, &clock, three),
            "a failed query keeps the long floor"
        );
        // the second look is cached: change the tree, the answer stays until the TTL passes
        backend.set_tree(two, tree(0));
        assert!(probe.exposes_uia(&backend, &clock, two));
        clock.advance(UIA_RICH_TTL + Duration::from_millis(1));
        assert!(!probe.exposes_uia(&backend, &clock, two));
        probe.clear();
    }

    #[test]
    fn the_probe_forgets_everything_when_the_cache_grows_past_32() {
        let backend = FakeBackend::new();
        let clock = FakeClock::new();
        let probe = UiaProbe::new();
        for raw in 1..=40u64 {
            let _ = probe.exposes_uia(&backend, &clock, WindowRef::from_raw(raw));
        }
        assert!(probe.cache.lock().unwrap().len() <= 33);
    }
}
