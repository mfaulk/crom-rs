use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use crate::net::Peer;

/// Peers tracked at once. Past this, full buckets are dropped; if still
/// full, unknown peers share one bucket, so memory stays bounded.
const MAX_PEERS: usize = 4096;

/// Per-peer token bucket charged only for denied agent requests. Allowed
/// calls never consume it, so a legitimate agent is not throttled by its
/// own successes.
pub(crate) struct DenyLimiter {
    burst: f64,
    per_second: f64,
    buckets: Mutex<HashMap<Option<Peer>, (f64, Instant)>>,
}

impl DenyLimiter {
    pub(crate) fn new(burst: u32, per_second: f64) -> Self {
        Self {
            burst: f64::from(burst),
            per_second,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn with_bucket<R>(&self, peer: Peer, now: Instant, f: impl FnOnce(&mut f64) -> R) -> R {
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let mut key = Some(peer);
        if !map.contains_key(&key) && map.len() >= MAX_PEERS {
            let (burst, rate) = (self.burst, self.per_second);
            map.retain(|_, (t, at)| *t + rate * now.duration_since(*at).as_secs_f64() < burst);
            if map.len() >= MAX_PEERS {
                key = None;
            }
        }
        let (tokens, at) = map.entry(key).or_insert((self.burst, now));
        *tokens =
            (*tokens + self.per_second * now.duration_since(*at).as_secs_f64()).min(self.burst);
        *at = now;
        f(tokens)
    }

    /// False when `peer` has used up its deny budget.
    pub(crate) fn permits(&self, peer: Peer) -> bool {
        self.with_bucket(peer, Instant::now(), |t| *t >= 1.0)
    }

    pub(crate) fn charge(&self, peer: Peer) {
        self.with_bucket(peer, Instant::now(), |t| *t = (*t - 1.0).max(0.0));
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use super::*;

    fn ip(n: u8) -> Peer {
        Peer::Tcp(IpAddr::V4(Ipv4Addr::new(127, 0, 0, n)))
    }

    /// A peer is refused after `burst` charges, and other peers are unaffected.
    #[test]
    fn burst_is_per_peer() {
        let l = DenyLimiter::new(2, 0.0001);
        assert!(l.permits(ip(1)));
        l.charge(ip(1));
        l.charge(ip(1));
        assert!(!l.permits(ip(1)));
        assert!(l.permits(ip(2)));
    }

    /// The bucket refills over time.
    #[test]
    fn bucket_refills() {
        let l = DenyLimiter::new(1, 1000.0);
        l.charge(ip(1));
        std::thread::sleep(Duration::from_millis(5));
        assert!(l.permits(ip(1)));
    }

    /// The peer table stays bounded; overflow peers share one bucket.
    #[test]
    fn peer_table_is_bounded() {
        let l = DenyLimiter::new(1, 0.0001);
        for i in 0..(MAX_PEERS as u32 + 10) {
            let p = Peer::Tcp(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + i)));
            l.charge(p);
        }
        assert!(l.buckets.lock().unwrap().len() <= MAX_PEERS + 1);
    }
}
