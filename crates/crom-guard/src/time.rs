use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Milliseconds since the Unix epoch on Guard's clock. Grant validity is
/// always judged against Guard's clock, never a client-supplied time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(u64);

impl Timestamp {
    pub const fn from_unix_millis(ms: u64) -> Self {
        Self(ms)
    }

    pub const fn as_unix_millis(self) -> u64 {
        self.0
    }

    pub const fn saturating_add_secs(self, secs: u64) -> Self {
        Self(self.0.saturating_add(secs.saturating_mul(1000)))
    }
}

/// Source of "now". Injected so expiry is testable without sleeping.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// The system wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        Timestamp(ms)
    }
}
