//! A clock that tests can replace.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// Tells the time on a monotonic clock.
pub trait Clock {
    fn now(&self) -> Duration;
}

/// `CLOCK_MONOTONIC`, the clock KMS timestamps use.
#[derive(Debug, Clone, Copy, Default)]
pub struct MonotonicClock;

impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        crate::monotonic_now()
    }
}

/// A clock that only moves when told to. Clones share the same time.
#[derive(Debug, Clone, Default)]
pub struct ManualClock(Rc<Cell<Duration>>);

impl ManualClock {
    pub fn new(start: Duration) -> Self {
        Self(Rc::new(Cell::new(start)))
    }

    pub fn advance(&self, by: Duration) {
        self.0.set(self.0.get() + by);
    }

    pub fn set(&self, time: Duration) {
        self.0.set(time);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        self.0.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_is_shared_between_clones() {
        let clock = ManualClock::new(Duration::from_secs(10));
        let other = clock.clone();
        clock.advance(Duration::from_millis(16));
        assert_eq!(other.now(), Duration::from_millis(10_016));
        other.set(Duration::from_secs(1));
        assert_eq!(clock.now(), Duration::from_secs(1));
    }

    #[test]
    fn monotonic_clock_does_not_go_backwards() {
        let clock = MonotonicClock;
        let a = clock.now();
        let b = clock.now();
        assert!(b >= a);
    }
}
