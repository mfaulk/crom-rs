//! Millisecond clocks.

use std::cell::Cell;
use std::rc::Rc;

pub trait Clock {
    /// Milliseconds since an arbitrary, fixed origin. Never decreases.
    fn now_ms(&self) -> u64;
}

/// A clock advanced by hand. Clones share the same time.
#[derive(Debug, Clone, Default)]
pub struct ManualClock {
    now: Rc<Cell<u64>>,
}

impl ManualClock {
    pub fn new(start_ms: u64) -> Self {
        ManualClock {
            now: Rc::new(Cell::new(start_ms)),
        }
    }

    pub fn advance(&self, ms: u64) {
        self.now.set(self.now.get() + ms);
    }

    pub fn set(&self, ms: u64) {
        assert!(ms >= self.now.get(), "clock must not go backwards");
        self.now.set(ms);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }
}

/// Wall-clock time since the Unix epoch.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_shared() {
        let a = ManualClock::new(10);
        let b = a.clone();
        a.advance(5);
        assert_eq!(b.now_ms(), 15);
    }
}
