//! # Backoff policy for retrying tasks.
//!
//! [`BackoffPolicy`] controls how retry delays grow after repeated failures.
//! It is parameterized by:
//! - [`BackoffPolicy::factor`] the multiplicative growth factor;
//! - [`BackoffPolicy::first`] the initial delay;
//! - [`BackoffPolicy::max`] the maximum delay cap.
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
//! // First attempt - uses 'first' (clamped to max)
//! assert_eq!(backoff.next(None), Duration::from_millis(100));
//!
//! // Second attempt - multiplied by factor (100ms * 2.0 = 200ms)
//! assert_eq!(backoff.next(Some(Duration::from_millis(100))), Duration::from_millis(200));
//!
//! // When previous delay exceeds max, result is capped at max
//! // (20s * 2.0 = 40s, but capped at max=10s)
//! assert_eq!(backoff.next(Some(Duration::from_secs(20))), Duration::from_secs(10));
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
    /// Computes the next delay based on the previous one.
    ///
    /// - If `prev` is `None`, returns `first` **clamped to `max`**.
    /// - Otherwise multiplies the previous delay by [`BackoffPolicy::factor`], and caps it at [`BackoffPolicy::max`].
    ///
    /// # Notes
    /// - If `factor` is less than 1.0, delays decrease over time (not typical).
    /// - If `factor` equals 1.0, delay remains constant at `first` (up to `max`).
    /// - If `factor` is greater than 1.0, delays grow exponentially.
    pub fn next(&self, prev: Option<Duration>) -> Duration {
        let unclamped = match prev {
            None => self.first,
            Some(d) => {
                let mul = d.as_secs_f64() * self.factor;
                if !mul.is_finite() {
                    self.max
                } else {
                    d.mul_f64(self.factor)
                }
            }
        };

        let base = if unclamped > self.max {
            self.max
        } else {
            unclamped
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
    fn test_first_delay_no_jitter() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(100),
            max: Duration::from_secs(30),
            factor: 2.0,
            jitter: JitterPolicy::None,
        };
        assert_eq!(policy.next(None), Duration::from_millis(100));
    }

    #[test]
    fn test_exponential_growth_no_jitter() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(100),
            max: Duration::from_secs(30),
            factor: 2.0,
            jitter: JitterPolicy::None,
        };

        let d1 = policy.next(None);
        assert_eq!(d1, Duration::from_millis(100));

        let d2 = policy.next(Some(d1));
        assert_eq!(d2, Duration::from_millis(200));

        let d3 = policy.next(Some(d2));
        assert_eq!(d3, Duration::from_millis(400));

        let d4 = policy.next(Some(d3));
        assert_eq!(d4, Duration::from_millis(800));
    }

    #[test]
    fn test_first_exceeds_max() {
        let policy = BackoffPolicy {
            first: Duration::from_secs(10),
            max: Duration::from_secs(5),
            factor: 2.0,
            jitter: JitterPolicy::None,
        };
        let d1 = policy.next(None);
        assert_eq!(d1, Duration::from_secs(5));
    }

    #[test]
    fn test_monotonic_growth_with_equal_jitter() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(100),
            max: Duration::from_secs(30),
            factor: 2.0,
            jitter: JitterPolicy::Equal,
        };

        let mut prev = None;
        let mut prev_delay = Duration::ZERO;

        for i in 0..20 {
            let delay = policy.next(prev);
            if i > 5 {
                assert!(
                    delay >= Duration::from_millis(10),
                    "iteration {}: delay {:?} is suspiciously low (prev: {:?})",
                    i,
                    delay,
                    prev_delay
                );
            }
            prev_delay = delay;
            prev = Some(delay);
        }
    }

    #[test]
    fn test_decorrelated_jitter_no_negative_feedback() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(100),
            max: Duration::from_secs(30),
            factor: 2.0,
            jitter: JitterPolicy::Decorrelated,
        };

        let mut prev = None;
        let mut min_seen = Duration::from_secs(999);
        let mut max_seen = Duration::ZERO;
        for i in 0..100 {
            let delay = policy.next(prev);

            min_seen = min_seen.min(delay);
            max_seen = max_seen.max(delay);

            assert!(
                delay >= Duration::from_millis(50), // с запасом на jitter
                "iteration {}: delay {:?} too low (min_seen: {:?})",
                i,
                delay,
                min_seen
            );
            if i > 10 {
                assert!(
                    delay >= Duration::from_millis(200),
                    "iteration {}: delay {:?} suspiciously low after warmup",
                    i,
                    delay
                );
            }
            prev = Some(delay);
        }
        println!("Decorrelated stats: min={:?}, max={:?}", min_seen, max_seen);
        assert!(
            max_seen > min_seen * 3,
            "Range too narrow for decorrelated jitter"
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
        for _ in 0..50 {
            let delay = policy.next(Some(Duration::from_millis(1000)));
            assert!(delay <= Duration::from_millis(1000));
        }
    }

    #[test]
    fn test_equal_jitter_bounds() {
        let policy = BackoffPolicy {
            first: Duration::from_millis(1000),
            max: Duration::from_secs(30),
            factor: 1.0, // constant base
            jitter: JitterPolicy::Equal,
        };
        for _ in 0..50 {
            let delay = policy.next(Some(Duration::from_millis(1000)));
            assert!(delay >= Duration::from_millis(500));
            assert!(delay <= Duration::from_millis(1000));
        }
    }
}
