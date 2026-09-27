//! Per-client request throttling with the semantics of `express-rate-limit@5`'s memory
//! store: a fixed window shared by all clients, after which every counter resets.

use std::collections::HashMap;
use std::sync::Mutex;

pub struct Throttle {
    max_requests: u64,
    window_ms: i64,
    state: Mutex<Window>,
}

struct Window {
    reset_at: i64,
    hits: HashMap<String, u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub limit: u64,
    pub remaining: u64,
    /// Window reset time, in seconds since the Unix epoch (rounded up).
    pub reset_secs: i64,
    /// Seconds a throttled client should wait (the window length, rounded up).
    pub retry_after_secs: i64,
    pub exceeded: bool,
    /// This is the first rejected request of the client in the current window.
    pub first_exceeded: bool,
}

impl Throttle {
    /// Returns `None` when throttling is disabled (`maxRequests` of 0).
    pub fn new(max_requests: u64, window_ms: u64) -> Option<Self> {
        (max_requests > 0).then(|| Self {
            max_requests,
            window_ms: i64::try_from(window_ms.max(1)).unwrap_or(i64::MAX),
            state: Mutex::new(Window { reset_at: 0, hits: HashMap::new() }),
        })
    }

    /// Records a request from `key` at `now` (ms since epoch).
    pub fn hit(&self, key: &str, now: i64) -> Hit {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if now >= state.reset_at {
            state.hits.clear();
            state.reset_at = now.saturating_add(self.window_ms);
        }
        let count = state.hits.entry(key.to_owned()).or_insert(0);
        *count += 1;
        let current = *count;
        Hit {
            limit: self.max_requests,
            remaining: self.max_requests.saturating_sub(current),
            reset_secs: ceil_div(state.reset_at, 1000),
            retry_after_secs: ceil_div(self.window_ms, 1000),
            exceeded: current > self.max_requests,
            first_exceeded: current == self.max_requests + 1,
        }
    }
}

fn ceil_div(value: i64, by: i64) -> i64 {
    (value + by - 1).div_euclid(by)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_max_is_zero() {
        assert!(Throttle::new(0, 1000).is_none());
    }

    #[test]
    fn limits_per_key_within_window_and_resets() {
        let throttle = Throttle::new(2, 1000).unwrap();
        assert!(!throttle.hit("a", 0).exceeded);
        let second = throttle.hit("a", 10);
        assert!(!second.exceeded);
        assert_eq!(second.remaining, 0);
        let third = throttle.hit("a", 20);
        assert!(third.exceeded && third.first_exceeded);
        assert!(!throttle.hit("a", 25).first_exceeded);
        assert!(!throttle.hit("b", 30).exceeded);
        let after_reset = throttle.hit("a", 1000);
        assert!(!after_reset.exceeded);
        assert_eq!(after_reset.remaining, 1);
        assert_eq!(after_reset.reset_secs, 2);
    }
}
