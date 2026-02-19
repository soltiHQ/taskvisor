//! # Backoff policy for retrying tasks.
//!
//! [`BackoffPolicy`] controls how retry delays grow after repeated failures.
//! It is parameterized by:
//! - [`BackoffPolicy::factor`] the multiplicative growth factor;
//! - [`BackoffPolicy::first`] the initial delay;
//! - [`BackoffPolicy::max`] the maximum delay cap.
//!
//! The delay for attempt `n` is computed as `first × factor^n`, clamped to `max`,
//! then jitter is applied. Because the base delay is derived purely from the attempt
//! number, jitter output never feeds back into subsequent calculations — this prevents
//! the negative feedback loop that causes delays to shrink over time.
//!
//! # Example
//! ```rust
//! use std::time::Duration;
//! use taskvisor::{BackoffPolicy, JitterPolicy};
//!
//! let backoff = BackoffPolicy {
//!     first: Duration::from_millis(100),
//!     max: Duration::from_secs(10),
//!     factor: 2.0,
//!     jitter: JitterPolicy::None,
//! };
//!
//! // Attempt 0 — uses 'first' (100ms), clamped to max
//! assert_eq!(backoff.next(0), Duration::from_millis(100));
//!
//! // Attempt 1 — first × factor^1 = 200ms
//! assert_eq!(backoff.next(1), Duration::from_millis(200));
//!
//! // Attempt 10 — 100ms × 2^10 = 102_400ms → capped at max=10s
//! assert_eq!(backoff.next(10), Duration::from_secs(10));
//! ```

use std::time::Duration;

use crate::policies::jitter::JitterPolicy;

/// Retry backoff policy.
///
/// Encapsulates parameters that determine how retry delays grow:
/// - [`BackoffPolicy::factor`] — multiplicative growth factor;
/// - [`BackoffPolicy::first`] — the initial delay;
/// - [`BackoffPolicy::max`] — the maximum delay cap.
#[derive(Clone, Copy, Debug)]
pub struct BackoffPolicy {
    /// Initial delay before the first retry.
    pub first: Duration,
    /// Maximum delay cap for retries.
    pub max: Duration,
    /// Multiplicative growth factor (`>= 1.0` recommended).
    pub factor: f64,
    /// Jitter policy to prevent thundering herd.
    pub jitter: JitterPolicy,
}

impl Default for BackoffPolicy {
    /// Returns a strategy with:
    /// - `factor = 1.0` (constant delay);
    /// - `first = 100ms`;
    /// - `max = 30s`.
    fn default() -> Self {
        Self {
            first: Duration::from_millis(100),
            max: Duration::from_secs(30),
            jitter: JitterPolicy::None,
            factor: 1.0,
        }
    }
}

impl BackoffPolicy {
    /// Computes the delay for the given attempt number (0-indexed).
    ///
    /// The base delay is `first × factor^attempt`, clamped to [`BackoffPolicy::max`].
    /// Jitter is applied to the clamped base, but the result is **never** fed back
    /// into subsequent calculations — each attempt derives its base independently.
    ///
    /// # Notes
    /// - If `factor` is less than 1.0, delays decrease with higher attempts (not typical).
    /// - If `factor` equals 1.0, delay remains constant at `first` (up to `max`).
    /// - If `factor` is greater than 1.0, delays grow exponentially up to `max`.
    pub fn next(&self, attempt: u32) -> Duration {
        let max_secs = self.max.as_secs_f64();
        let clamped_exp = attempt.min(i32::MAX as u32) as i32;
        let unclamped_secs = self.first.as_secs_f64() * self.factor.powi(clamped_exp);

        let base =
            if !unclamped_secs.is_finite() || unclamped_secs < 0.0 || unclamped_secs > max_secs {
                self.max
            } else {
                Duration::from_secs_f64(unclamped_secs)
            };

        match self.jitter {
            JitterPolicy::Decorrelated => {
                self.jitter
                    .apply_decorrelated(self.first.min(self.max), base, self.max)
            }
            _ => self.jitter.apply(base),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_attempt_zero_returns_first() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(100),
            max: Duration::from_secs(30),
            factor: 2.0,
            jitter: JitterPolicy::None,
        };
        assert_eq!(policy.next(0), Duration::from_millis(100));
    }

    #[test]
    fn test_exponential_growth_no_jitter() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(100),
            max: Duration::from_secs(30),
            factor: 2.0,
            jitter: JitterPolicy::None,
        };

        assert_eq!(policy.next(0), Duration::from_millis(100));
        assert_eq!(policy.next(1), Duration::from_millis(200));
        assert_eq!(policy.next(2), Duration::from_millis(400));
        assert_eq!(policy.next(3), Duration::from_millis(800));
        assert_eq!(policy.next(4), Duration::from_millis(1600));
    }

    #[test]
    fn test_constant_factor() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(500),
            max: Duration::from_secs(30),
            factor: 1.0,
            jitter: JitterPolicy::None,
        };
        for attempt in 0..10 {
            assert_eq!(
                policy.next(attempt),
                Duration::from_millis(500),
                "attempt {} should be constant at 500ms",
                attempt
            );
        }
    }

    #[test]
    fn test_clamped_to_max() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(100),
            max: Duration::from_secs(1),
            factor: 2.0,
            jitter: JitterPolicy::None,
        };
        assert_eq!(policy.next(10), Duration::from_secs(1));
    }

    #[test]
    fn test_first_exceeds_max() {
        let policy = BackoffPolicy {
            first: Duration::from_secs(10),
            max: Duration::from_secs(5),
            factor: 2.0,
            jitter: JitterPolicy::None,
        };
        assert_eq!(policy.next(0), Duration::from_secs(5));
    }

    #[test]
    fn test_full_jitter_no_negative_feedback() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(100),
            max: Duration::from_secs(30),
            factor: 2.0,
            jitter: JitterPolicy::Full,
        };

        for attempt in 5..15 {
            let base_ms = 100.0 * 2.0f64.powi(attempt as i32);
            let delay = policy.next(attempt);
            assert!(
                delay <= Duration::from_millis(base_ms as u64),
                "attempt {}: delay {:?} exceeds base {}ms",
                attempt,
                delay,
                base_ms
            );
        }
    }

    #[test]
    fn test_equal_jitter_no_negative_feedback() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(100),
            max: Duration::from_secs(30),
            factor: 2.0,
            jitter: JitterPolicy::Equal,
        };

        for attempt in 0..15 {
            let base_ms = (100.0 * 2.0f64.powi(attempt as i32)).min(30_000.0);
            let half = base_ms / 2.0;
            let delay = policy.next(attempt);
            assert!(
                delay >= Duration::from_millis(half as u64),
                "attempt {}: delay {:?} < half of base {}ms",
                attempt,
                delay,
                base_ms
            );
            assert!(
                delay <= Duration::from_millis(base_ms as u64),
                "attempt {}: delay {:?} > base {}ms",
                attempt,
                delay,
                base_ms
            );
        }
    }

    #[test]
    fn test_decorrelated_jitter_grows_with_attempts() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(100),
            max: Duration::from_secs(30),
            factor: 2.0,
            jitter: JitterPolicy::Decorrelated,
        };

        let mut min_late = Duration::from_secs(999);
        let mut max_late = Duration::ZERO;
        for _ in 0..100 {
            let delay = policy.next(8);
            min_late = min_late.min(delay);
            max_late = max_late.max(delay);
        }

        assert!(
            min_late >= Duration::from_millis(100),
            "min_late {:?} below floor",
            min_late
        );
        assert!(
            max_late >= Duration::from_secs(5),
            "max_late {:?} suspiciously low, range too narrow",
            max_late
        );
    }

    #[test]
    fn test_full_jitter_bounds() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(1000),
            max: Duration::from_secs(30),
            factor: 1.0,
            jitter: JitterPolicy::Full,
        };
        for attempt in 0..50 {
            let delay = policy.next(attempt);
            assert!(delay <= Duration::from_millis(1000));
        }
    }

    #[test]
    fn test_equal_jitter_bounds() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(1000),
            max: Duration::from_secs(30),
            factor: 1.0,
            jitter: JitterPolicy::Equal,
        };
        for attempt in 0..50 {
            let delay = policy.next(attempt);
            assert!(delay >= Duration::from_millis(500));
            assert!(delay <= Duration::from_millis(1000));
        }
    }

    #[test]
    fn test_huge_attempt_clamps_to_max() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(100),
            max: Duration::from_secs(60),
            factor: 2.0,
            jitter: JitterPolicy::None,
        };
        assert_eq!(policy.next(100), Duration::from_secs(60));
    }

    #[test]
    fn test_non_finite_overflow_clamps_to_max() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(100),
            max: Duration::from_secs(10),
            factor: 2.0,
            jitter: JitterPolicy::None,
        };
        assert_eq!(policy.next(u32::MAX), Duration::from_secs(10));
    }
}
