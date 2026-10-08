//! Do not fight a person for the mouse. Port of `computer._wait_for_human` and `_mark_ours`.
//!
//! Input newer than our own last injection came from a human. Before an acting step, wait until they
//! have been still for `quiet` (1.2 s), up to `max_wait` (6 s), then refuse. Found 2026-09-20: the
//! owner used the PC during a run and clicks landed in the wrong window.
//!
//! The OS reports "time since the last input from anyone" ([`Backend::human_input_idle`]). Our own
//! injections count too, so the gate remembers when it last sent input itself: input is the person's
//! only when it is newer than that, by more than 150 ms of slack.

use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use crate::backend::Backend;
use crate::clock::Clock;

/// How long a person must be still before an action goes ahead (the Python default, 1200 ms).
pub const DEFAULT_QUIET: Duration = Duration::from_millis(1200);

/// The longest an action waits for them (6 s).
pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(6);

/// Input within this long after our own last injection is still ours (key repeat, OS echo).
const OURS_SLACK: Duration = Duration::from_millis(150);

/// How often the gate looks again while someone is active.
const POLL: Duration = Duration::from_millis(200);

/// A person was still using the input devices when the wait ran out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanActive;

impl fmt::Display for HumanActive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "a person is using the mouse or keyboard right now; waited 6 s for them to stop. \
             Try again shortly",
        )
    }
}

impl std::error::Error for HumanActive {}

/// Tells our own input from a person's.
#[derive(Debug, Default)]
pub struct HumanGate {
    last_ours: Mutex<Option<Duration>>,
}

impl HumanGate {
    /// A gate that has not injected anything yet. Until it does, any input counts as a person's.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that we just injected input. Python `_mark_ours`.
    pub fn mark_ours(&self, clock: &dyn Clock) {
        *self
            .last_ours
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(clock.now());
    }

    /// Wait for a person who is using the mouse or keyboard to stop, or refuse. With `enabled` false
    /// (`human_pause=0`: nobody is at this machine and something else, such as a benchmark harness,
    /// is injecting input) it returns at once without looking at the input state at all. A backend
    /// that cannot say when input last happened never blocks an action.
    pub fn wait(
        &self,
        backend: &dyn Backend,
        clock: &dyn Clock,
        enabled: bool,
        quiet: Duration,
        max_wait: Duration,
    ) -> Result<(), HumanActive> {
        if !enabled {
            return Ok(());
        }
        let deadline = clock.now() + max_wait;
        loop {
            let Ok(idle) = backend.human_input_idle() else {
                return Ok(());
            };
            let ours = *self
                .last_ours
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // last input = now - idle; ours = now - ours_age; "newer than ours + slack" is
            // idle + slack < ours_age.
            let human = match ours {
                Some(at) => idle + OURS_SLACK < clock.now().saturating_sub(at),
                None => true,
            };
            if !human || idle >= quiet {
                return Ok(());
            }
            if clock.now() > deadline {
                return Err(HumanActive);
            }
            clock.sleep(POLL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeBackend;
    use crate::clock::FakeClock;

    /// A clock where a person keeps typing for `until` and is then still: `idle` follows the clock.
    struct PersonStopsAt<'a> {
        clock: FakeClock,
        backend: &'a FakeBackend,
        until: Duration,
    }

    impl Clock for PersonStopsAt<'_> {
        fn now(&self) -> Duration {
            self.clock.now()
        }
        fn sleep(&self, d: Duration) {
            self.clock.sleep(d);
            let now = self.clock.now();
            let idle = if now >= self.until {
                now - self.until
            } else {
                Duration::ZERO
            };
            self.backend.set_human_idle(idle);
        }
    }

    // Ported from tests/test_config_and_idle.py::test_the_human_pause_call_site_honours_the_switch:
    // with the pause on, a fresh burst of "someone else's" input makes it raise; with it off, it
    // returns at once and never looks at the input state.
    #[test]
    fn the_human_pause_call_site_honours_the_switch() {
        let backend = FakeBackend::new();
        backend.set_human_idle(Duration::ZERO); // input that just happened...
        let gate = HumanGate::new(); // ...and was not ours
        let clock = FakeClock::new();
        let err = gate
            .wait(&backend, &clock, true, DEFAULT_QUIET, Duration::ZERO)
            .unwrap_err();
        assert_eq!(err, HumanActive);
        assert!(backend.human_idle_reads() > 0);

        let before = backend.human_idle_reads();
        gate.wait(&backend, &clock, false, DEFAULT_QUIET, Duration::ZERO)
            .unwrap();
        assert_eq!(
            backend.human_idle_reads(),
            before,
            "never looked at the input tick"
        );
    }

    #[test]
    fn a_person_who_stops_lets_the_action_through_after_the_quiet_period() {
        let backend = FakeBackend::new();
        backend.set_human_idle(Duration::ZERO);
        let clock = PersonStopsAt {
            clock: FakeClock::new(),
            backend: &backend,
            until: Duration::from_secs(2),
        };
        let gate = HumanGate::new();
        gate.wait(&backend, &clock, true, DEFAULT_QUIET, DEFAULT_MAX_WAIT)
            .unwrap();
        let waited = clock.now();
        assert!(
            waited >= Duration::from_secs(2) + DEFAULT_QUIET - POLL,
            "{waited:?}"
        );
        assert!(waited < DEFAULT_MAX_WAIT, "{waited:?}");
    }

    #[test]
    fn a_person_who_never_stops_gets_refused_after_six_seconds() {
        let backend = FakeBackend::new();
        backend.set_human_idle(Duration::from_millis(50));
        let clock = FakeClock::new();
        let gate = HumanGate::new();
        let err = gate
            .wait(&backend, &clock, true, DEFAULT_QUIET, DEFAULT_MAX_WAIT)
            .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("a person is using the mouse or keyboard right now; waited 6 s")
        );
        assert!(clock.now() >= DEFAULT_MAX_WAIT);
    }

    #[test]
    fn an_idle_machine_goes_straight_through() {
        let backend = FakeBackend::new();
        let clock = FakeClock::new();
        HumanGate::new()
            .wait(&backend, &clock, true, DEFAULT_QUIET, DEFAULT_MAX_WAIT)
            .unwrap();
        assert_eq!(clock.now(), Duration::ZERO, "no waiting at all");
    }

    #[test]
    fn our_own_input_is_not_a_person() {
        let backend = FakeBackend::new();
        let clock = FakeClock::new();
        clock.advance(Duration::from_secs(100));
        let gate = HumanGate::new();
        gate.mark_ours(&clock);
        // the OS says input happened 10 ms ago: that is our injection, not a person
        backend.set_human_idle(Duration::from_millis(10));
        gate.wait(&backend, &clock, true, DEFAULT_QUIET, Duration::ZERO)
            .unwrap();
        // half a second later the OS says input happened 100 ms ago: newer than ours, a person
        clock.advance(Duration::from_millis(500));
        backend.set_human_idle(Duration::from_millis(100));
        assert!(
            gate.wait(&backend, &clock, true, DEFAULT_QUIET, Duration::ZERO)
                .is_err()
        );
    }
}
