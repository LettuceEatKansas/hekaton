//! The daemon's run-time state that is not a setting: the pause flag, the idle-shutdown countdown and
//! the rate cap. Port of `daemon.State`, `daemon._idle_expired` and `State.check_rate`. The HTTP
//! layer that owns one of these lands in a later phase; the logic lives here so it is tested on
//! every OS.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::clock::Clock;

/// Actions per second the daemon accepts (daemon.py `RATE_CAP`).
pub const RATE_CAP: usize = 50;

/// The sliding window the rate cap counts over.
pub const RATE_WINDOW: Duration = Duration::from_secs(1);

/// How often the idle thread looks (daemon.py `_idle_shutdown_thread`: `time.sleep(15)`).
pub const IDLE_POLL: Duration = Duration::from_secs(15);

/// The pause flag the kill hotkey and the tray toggle. While set, everything except the read-only
/// routes is refused ("paused by user hotkey").
#[derive(Debug, Default)]
pub struct PauseFlag(AtomicBool);

impl PauseFlag {
    /// Not paused.
    pub fn new() -> Self {
        Self::default()
    }

    /// Is the daemon paused?
    pub fn is_paused(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Set the flag.
    pub fn set(&self, paused: bool) {
        self.0.store(paused, Ordering::SeqCst);
    }

    /// Flip the flag (the hotkey) and return the new value.
    pub fn toggle(&self) -> bool {
        !self.0.fetch_xor(true, Ordering::SeqCst)
    }
}

/// The idle-shutdown countdown: `touch()` on every handled call, `expired()` once nothing has
/// arrived for the timeout.
pub struct IdleTimer {
    timeout: Mutex<Duration>,
    disabled: AtomicBool,
    last_activity: Mutex<Duration>,
}

impl IdleTimer {
    /// A timer of `timeout_seconds` starting now. Zero or negative disables it (never expires).
    pub fn new(timeout_seconds: i64, clock: &dyn Clock) -> Self {
        let timer = Self {
            timeout: Mutex::new(Duration::ZERO),
            disabled: AtomicBool::new(true),
            last_activity: Mutex::new(clock.now()),
        };
        timer.set_timeout(timeout_seconds);
        timer
    }

    /// Change the timeout. Zero or negative disables the timer.
    pub fn set_timeout(&self, seconds: i64) {
        *self
            .timeout
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Duration::from_secs(seconds.max(0) as u64);
        self.disabled.store(seconds <= 0, Ordering::SeqCst);
    }

    /// Any handled call resets the countdown. Python `State.touch`.
    pub fn touch(&self, clock: &dyn Clock) {
        *self
            .last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = clock.now();
    }

    /// Seconds left before shutdown, 0.0 once passed, `-1.0` when idle shutdown is disabled.
    /// Python `State.idle_remaining`.
    pub fn remaining(&self, clock: &dyn Clock) -> f64 {
        if self.disabled.load(Ordering::SeqCst) {
            return -1.0;
        }
        let timeout = *self
            .timeout
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let last = *self
            .last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let idle = clock.now().saturating_sub(last);
        timeout.saturating_sub(idle).as_secs_f64()
    }

    /// Has the timeout run out? Python `_idle_expired`:
    /// `state.idle_timeout > 0 and state.idle_remaining() <= 0`.
    pub fn expired(&self, clock: &dyn Clock) -> bool {
        !self.disabled.load(Ordering::SeqCst) && self.remaining(clock) <= 0.0
    }
}

/// At most [`RATE_CAP`] actions per second, counted over a sliding window. Python
/// `State.check_rate`.
pub struct RateLimiter {
    cap: usize,
    window: Duration,
    times: Mutex<VecDeque<Duration>>,
}

impl RateLimiter {
    /// The daemon's cap.
    pub fn new() -> Self {
        Self::with_cap(RATE_CAP, RATE_WINDOW)
    }

    /// A custom cap and window.
    pub fn with_cap(cap: usize, window: Duration) -> Self {
        Self {
            cap,
            window,
            times: Mutex::new(VecDeque::new()),
        }
    }

    /// Record one action and say whether it is within the cap. A refused action still counts, as
    /// in Python, so a flood does not slip through by being refused.
    pub fn check(&self, clock: &dyn Clock) -> bool {
        let now = clock.now();
        let mut times = self
            .times
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        times.push_back(now);
        while times
            .front()
            .is_some_and(|t| now.saturating_sub(*t) > self.window)
        {
            times.pop_front();
        }
        times.len() <= self.cap
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;

    // ---- ported from tests/test_config_and_idle.py (the _FakeState tests) ----

    #[test]
    fn idle_remaining_counts_down_from_touch() {
        let clock = FakeClock::new();
        let timer = IdleTimer::new(10, &clock);
        let before = timer.remaining(&clock);
        assert!((9.5..=10.0).contains(&before), "{before}");
        clock.advance(Duration::from_secs(4)); // simulate 4 seconds of inactivity
        let after = timer.remaining(&clock);
        assert!((5.5..=6.0).contains(&after), "{after}");
    }

    #[test]
    fn idle_remaining_hits_zero_after_timeout() {
        let clock = FakeClock::new();
        let timer = IdleTimer::new(1, &clock);
        clock.advance(Duration::from_secs(2)); // already past the timeout
        assert_eq!(timer.remaining(&clock), 0.0);
    }

    #[test]
    fn idle_disabled_when_timeout_zero_or_negative() {
        let clock = FakeClock::new();
        assert_eq!(IdleTimer::new(0, &clock).remaining(&clock), -1.0);
        assert_eq!(IdleTimer::new(-5, &clock).remaining(&clock), -1.0);
    }

    #[test]
    fn touch_resets_the_countdown() {
        let clock = FakeClock::new();
        let timer = IdleTimer::new(5, &clock);
        clock.advance(Duration::from_secs(4));
        assert!(timer.remaining(&clock) < 2.0);
        timer.touch(&clock);
        assert!(timer.remaining(&clock) > 4.5);
    }

    // ---- ported from tests/test_shutdown_hygiene.py ----

    #[test]
    fn idle_check_shuts_down_only_after_the_timeout() {
        let clock = FakeClock::new();
        clock.advance(Duration::from_secs(1000));
        let timer = IdleTimer::new(900, &clock);
        timer.touch(&clock);
        clock.advance(Duration::from_secs(899));
        assert!(!timer.expired(&clock));
        clock.advance(Duration::from_secs(2));
        assert!(timer.expired(&clock));
        timer.touch(&clock); // a request arrives: the countdown starts over
        assert!(!timer.expired(&clock));
        timer.set_timeout(0);
        clock.advance(Duration::from_secs(10_000));
        assert!(!timer.expired(&clock), "0 disables");
    }

    // ---- the rate cap and the pause flag ----

    #[test]
    fn fifty_a_second_pass_and_the_fifty_first_is_refused() {
        let clock = FakeClock::new();
        let limiter = RateLimiter::new();
        for i in 0..RATE_CAP {
            assert!(limiter.check(&clock), "call {i}");
        }
        assert!(!limiter.check(&clock), "the 51st in the same second");
        clock.advance(Duration::from_millis(1100));
        assert!(limiter.check(&clock), "the window slid past the old calls");
    }

    #[test]
    fn refused_calls_still_count_against_the_window() {
        let clock = FakeClock::new();
        let limiter = RateLimiter::with_cap(2, Duration::from_secs(1));
        assert!(limiter.check(&clock) && limiter.check(&clock));
        for _ in 0..5 {
            assert!(!limiter.check(&clock));
        }
        clock.advance(Duration::from_millis(900));
        assert!(!limiter.check(&clock), "still crowded by the refused ones");
        clock.advance(Duration::from_millis(200));
        assert!(limiter.check(&clock));
    }

    #[test]
    fn the_pause_flag_toggles() {
        let flag = PauseFlag::new();
        assert!(!flag.is_paused());
        assert!(flag.toggle());
        assert!(flag.is_paused());
        assert!(!flag.toggle());
        flag.set(true);
        assert!(flag.is_paused());
    }
}
