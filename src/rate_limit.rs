//! Simple process-wide token-bucket rate limiter for the HTTP API.
//!
//! Disabled when `BROWSER_HEADLESS_RATE_LIMIT_RPS` is 0 (the default). When
//! enabled, every `/summary*` and `/jobs*` request consumes one token; if the
//! bucket is empty the request is rejected with 429 before any capture work
//! starts. Health probes and (optionally-protected) metrics are not gated
//! here — the HTTP layer only calls this for capture routes.

use std::sync::Mutex;
use std::time::Instant;

use crate::config;

/// Token bucket shared across the process. `None` means rate limiting is off.
pub(crate) struct RateLimiter {
    inner: Option<Mutex<Bucket>>,
}

struct Bucket {
    tokens: f64,
    capacity: f64,
    /// Tokens added per second.
    refill_per_sec: f64,
    last: Instant,
}

impl RateLimiter {
    pub(crate) fn from_env() -> Self {
        let rps = config::rate_limit_rps();
        if rps == 0 {
            return Self { inner: None };
        }
        let capacity = config::rate_limit_burst().max(1) as f64;
        let refill = rps as f64;
        tracing::info!(rps, burst = capacity as u32, "HTTP rate limit enabled");
        Self {
            inner: Some(Mutex::new(Bucket {
                tokens: capacity,
                capacity,
                refill_per_sec: refill,
                last: Instant::now(),
            })),
        }
    }

    /// Try to consume one token. Returns `true` if allowed, `false` if the
    /// caller should respond 429.
    pub(crate) fn try_acquire(&self) -> bool {
        let Some(lock) = &self.inner else {
            return true;
        };
        let Ok(mut b) = lock.lock() else {
            // Poisoned mutex — fail open so a panic elsewhere can't DoS the API.
            return true;
        };
        let now = Instant::now();
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.last = now;
        b.tokens = (b.tokens + elapsed * b.refill_per_sec).min(b.capacity);
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_always_allows() {
        let lim = RateLimiter { inner: None };
        assert!(lim.try_acquire());
        assert!(lim.try_acquire());
    }

    #[test]
    fn bucket_exhausts() {
        let lim = RateLimiter {
            inner: Some(Mutex::new(Bucket {
                tokens: 2.0,
                capacity: 2.0,
                refill_per_sec: 0.0, // no refill
                last: Instant::now(),
            })),
        };
        assert!(lim.try_acquire());
        assert!(lim.try_acquire());
        assert!(!lim.try_acquire());
    }
}
