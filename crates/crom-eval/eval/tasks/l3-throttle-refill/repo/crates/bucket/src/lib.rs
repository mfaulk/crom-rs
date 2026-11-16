//! Token bucket.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// A token was consumed; `remaining` tokens are left.
    Granted { remaining: u32 },
    /// No token available; one will be in `retry_after_ms`.
    Denied { retry_after_ms: u64 },
}

#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: u32,
    interval_ms: u64,
    tokens: u32,
    last_refill_ms: u64,
}

impl TokenBucket {
    /// A full bucket at time `now_ms`.
    pub fn new(capacity: u32, interval_ms: u64, now_ms: u64) -> Self {
        assert!(capacity > 0 && interval_ms > 0);
        TokenBucket {
            capacity,
            interval_ms,
            tokens: capacity,
            last_refill_ms: now_ms,
        }
    }

    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    fn refill(&mut self, now_ms: u64) {
        let elapsed = now_ms.saturating_sub(self.last_refill_ms);
        let new_tokens = elapsed / self.interval_ms;
        if new_tokens > 0 {
            self.tokens = (self.tokens as u64 + new_tokens).min(self.capacity as u64) as u32;
            self.last_refill_ms = now_ms;
        }
    }

    /// Tokens available at `now_ms`.
    pub fn available(&mut self, now_ms: u64) -> u32 {
        self.refill(now_ms);
        self.tokens
    }

    pub fn try_take(&mut self, now_ms: u64) -> Take {
        self.refill(now_ms);
        if self.tokens > 0 {
            self.tokens -= 1;
            Take::Granted {
                remaining: self.tokens,
            }
        } else {
            Take::Denied {
                retry_after_ms: self.interval_ms,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_full_and_drains() {
        let mut b = TokenBucket::new(3, 1000, 0);
        assert_eq!(b.try_take(0), Take::Granted { remaining: 2 });
        assert_eq!(b.try_take(0), Take::Granted { remaining: 1 });
        assert_eq!(b.try_take(0), Take::Granted { remaining: 0 });
        assert!(matches!(b.try_take(0), Take::Denied { .. }));
    }

    #[test]
    fn refills_whole_intervals() {
        let mut b = TokenBucket::new(5, 100, 0);
        for _ in 0..5 {
            b.try_take(0);
        }
        assert_eq!(b.available(300), 3);
        assert_eq!(b.available(10_000), 5);
    }
}
