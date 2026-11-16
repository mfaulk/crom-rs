//! Per-client limiting.

use std::collections::HashMap;

use bucket::{Take, TokenBucket};
use clock::Clock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow { remaining: u32 },
    Limit { retry_after_ms: u64 },
}

impl Decision {
    /// `Retry-After` header value in whole seconds, rounded up.
    pub fn retry_after_header(&self) -> Option<String> {
        match self {
            Decision::Allow { .. } => None,
            Decision::Limit { retry_after_ms } => Some(retry_after_ms.div_ceil(1000).to_string()),
        }
    }
}

pub struct Gateway<C: Clock> {
    clock: C,
    capacity: u32,
    interval_ms: u64,
    buckets: HashMap<String, (TokenBucket, u64)>,
}

impl<C: Clock> Gateway<C> {
    pub fn new(clock: C, capacity: u32, interval_ms: u64) -> Self {
        Gateway {
            clock,
            capacity,
            interval_ms,
            buckets: HashMap::new(),
        }
    }

    /// Decide whether `client` may make a request now.
    pub fn check(&mut self, client: &str) -> Decision {
        let now = self.clock.now_ms();
        let (capacity, interval) = (self.capacity, self.interval_ms);
        let (bucket, last_seen) = self
            .buckets
            .entry(client.to_string())
            .or_insert_with(|| (TokenBucket::new(capacity, interval, now), now));
        *last_seen = now;
        match bucket.try_take(now) {
            Take::Granted { remaining } => Decision::Allow { remaining },
            Take::Denied { retry_after_ms } => Decision::Limit { retry_after_ms },
        }
    }

    /// Forget clients not seen for at least `idle_ms`. Returns how many were
    /// removed.
    pub fn evict_idle(&mut self, idle_ms: u64) -> usize {
        let now = self.clock.now_ms();
        let before = self.buckets.len();
        self.buckets
            .retain(|_, (_, last_seen)| now.saturating_sub(*last_seen) < idle_ms);
        before - self.buckets.len()
    }

    pub fn tracked_clients(&self) -> usize {
        self.buckets.len()
    }
}
