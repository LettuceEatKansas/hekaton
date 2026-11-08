//! Request deadlines propagated across HTTP, legacy routes and chained actions.

use std::cell::Cell;
use std::time::{Duration, Instant};

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Time left for the current request, including time already spent in its queue.
pub fn remaining() -> Option<Duration> {
    DEADLINE.with(|d| {
        d.get()
            .map(|at| at.saturating_duration_since(Instant::now()))
    })
}

/// Run within a deadline. Nested work may shorten, but cannot extend, its parent's deadline.
pub fn within<T>(deadline: Instant, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<Instant>);
    impl Drop for Restore {
        fn drop(&mut self) {
            DEADLINE.with(|d| d.set(self.0));
        }
    }
    let previous = DEADLINE.with(|d| {
        let old = d.get();
        d.set(Some(old.map_or(deadline, |parent| parent.min(deadline))));
        old
    });
    let _restore = Restore(previous);
    work()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_deadline_cannot_extend_parent_and_restores_on_unwind() {
        let expired = Instant::now() - Duration::from_secs(1);
        within(expired, || {
            within(Instant::now() + Duration::from_secs(60), || {
                assert_eq!(remaining(), Some(Duration::ZERO));
            });
        });
        assert!(remaining().is_none());
        let _ = std::panic::catch_unwind(|| within(expired, || panic!("test")));
        assert!(remaining().is_none());
    }
}
