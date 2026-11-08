//! Time behind a trait so timing code (settle loops, the human-input wait, idle shutdown, the rate
//! cap) runs in tests without waiting. Python tests swapped `time` for a `_Clock` whose `sleep`
//! advanced `monotonic`; [`FakeClock`] is that.

use std::time::{Duration, Instant};

/// A monotonic clock that can also sleep.
pub trait Clock: Send + Sync {
    /// Time since an arbitrary fixed point. Only differences matter.
    fn now(&self) -> Duration;

    /// Wait for `d`. A fake clock advances instead.
    fn sleep(&self, d: Duration);
}

/// The real clock.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    /// A clock whose zero is now.
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// A clock that only moves when told to, or when something sleeps on it.
#[cfg(any(test, feature = "testing"))]
#[derive(Debug, Default)]
pub struct FakeClock {
    now: std::sync::Mutex<Duration>,
}

#[cfg(any(test, feature = "testing"))]
impl FakeClock {
    /// A clock at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Move time forward.
    pub fn advance(&self, d: Duration) {
        *self.now.lock().expect("fake clock") += d;
    }
}

#[cfg(any(test, feature = "testing"))]
impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *self.now.lock().expect("fake clock")
    }

    fn sleep(&self, d: Duration) {
        self.advance(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleeping_on_a_fake_clock_advances_it_without_waiting() {
        let clock = FakeClock::new();
        let real = Instant::now();
        clock.sleep(Duration::from_secs(3600));
        assert_eq!(clock.now(), Duration::from_secs(3600));
        assert!(real.elapsed() < Duration::from_secs(5));
        clock.advance(Duration::from_millis(500));
        assert_eq!(clock.now(), Duration::from_millis(3_600_500));
    }

    #[test]
    fn the_system_clock_moves_forward() {
        let clock = SystemClock::new();
        let a = clock.now();
        clock.sleep(Duration::from_millis(5));
        assert!(clock.now() > a);
    }
}
