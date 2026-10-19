//! Token-bucket rate limiter (one per tenant).

use std::sync::Mutex;
use std::time::Instant;

pub struct RateLimiter {
    /// `None` when unlimited.
    state: Option<Mutex<Bucket>>,
}

struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(per_sec: u32, burst: u32) -> Self {
        if per_sec == 0 {
            return Self { state: None };
        }
        let burst = if burst == 0 { per_sec } else { burst } as f64;
        Self { state: Some(Mutex::new(Bucket { rate: per_sec as f64, burst, tokens: burst, last: Instant::now() })) }
    }

    pub fn unlimited() -> Self {
        Self { state: None }
    }

    pub fn try_acquire(&self) -> bool {
        self.try_acquire_at(Instant::now())
    }

    fn try_acquire_at(&self, now: Instant) -> bool {
        let Some(m) = &self.state else { return true };
        let mut b = m.lock().unwrap_or_else(|p| p.into_inner());
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * b.rate).min(b.burst);
        b.last = now;
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
    use std::time::Duration;

    #[test]
    fn unlimited_always_allows() {
        let l = RateLimiter::unlimited();
        assert!((0..10_000).all(|_| l.try_acquire()));
    }

    #[test]
    fn burst_then_refill() {
        let l = RateLimiter::new(10, 3);
        let t0 = Instant::now();
        assert!(l.try_acquire_at(t0));
        assert!(l.try_acquire_at(t0));
        assert!(l.try_acquire_at(t0));
        assert!(!l.try_acquire_at(t0), "burst exhausted");
        assert!(!l.try_acquire_at(t0 + Duration::from_millis(50)));
        assert!(l.try_acquire_at(t0 + Duration::from_millis(150)), "0.15 s * 10/s refills a token");
    }

    #[test]
    fn refill_never_exceeds_burst() {
        let l = RateLimiter::new(100, 2);
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(60);
        assert!(l.try_acquire_at(later));
        assert!(l.try_acquire_at(later));
        assert!(!l.try_acquire_at(later));
    }
}
