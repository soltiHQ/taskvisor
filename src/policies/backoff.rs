//! # Backoff policy for retrying tasks.
//!
//! [`BackoffPolicy`] controls how retry delays grow after repeated failures.
//!
//! It is parameterized by:
//! - [`BackoffPolicy::factor`] the multiplicative growth factor.
//! - [`BackoffPolicy::max`] the maximum delay cap.
//! - [`BackoffPolicy::first`] the initial delay.
//!
//! The delay for attempt `n` is computed as `first × factor^n`, clamped to `max`, then jitter is applied.
//!
//! # Example
//! ```rust
//! use std::time::Duration;
//! use taskvisor::{BackoffPolicy, JitterPolicy};
//!
//! let backoff = BackoffPolicy::new(
//!     Duration::from_millis(100),
//!     Duration::from_secs(10),
//!     2.0,
//!     JitterPolicy::None,
//! )
//! .unwrap();
//!
//! // Attempt 0 - uses 'first' (100ms), clamped to max
//! assert_eq!(backoff.next(0), Duration::from_millis(100));
//!
//! // Attempt 1 - first × factor^1 = 200ms
//! assert_eq!(backoff.next(1), Duration::from_millis(200));
//!
//! // Attempt 10 - 100ms × 2^10 = 102_400ms → capped at max=10s
//! assert_eq!(backoff.next(10), Duration::from_secs(10));
//! ```

use std::time::Duration;

use thiserror::Error;

use crate::policies::jitter::JitterPolicy;

/// Error returned by [`BackoffPolicy::new`] when parameters violate the policy invariants.
#[derive(Debug, Clone, Copy, PartialEq, Error)]
#[non_exhaustive]
pub enum BackoffError {
    /// `factor` was not finite or was below `1.0` (a backoff factor must not shrink delays).
    #[error("backoff factor must be finite and >= 1.0, got {0}")]
    InvalidFactor(f64),
    /// `first` exceeded `max` (the initial delay cannot be larger than the cap).
    #[error("backoff first delay {first:?} exceeds max {max:?}")]
    FirstExceedsMax {
        /// The offending initial delay.
        first: Duration,
        /// The configured cap.
        max: Duration,
    },
}

/// Retry backoff policy.
///
/// See the module-level documentation for formula, parameters, and examples.
///
/// # Also
///
/// - [`TaskSpec`](crate::TaskSpec) - wires restart + backoff + timeout together
/// - [`RestartPolicy`](crate::RestartPolicy) - whether to restart at all
/// - [`JitterPolicy`] - randomization applied to computed delay
#[derive(Clone, Copy, Debug)]
pub struct BackoffPolicy {
    jitter: JitterPolicy,
    first: Duration,
    floor: Duration,
    max: Duration,
    factor: f64,
}

impl Default for BackoffPolicy {
    /// Returns a strategy with:
    /// - `factor = 1.0` (constant delay);
    /// - `first = 100ms`;
    /// - `max = 30s`;
    /// - `floor = 0` (no floor).
    fn default() -> Self {
        Self {
            first: Duration::from_millis(100),
            max: Duration::from_secs(30),
            jitter: JitterPolicy::None,
            factor: 1.0,
            floor: Duration::ZERO,
        }
    }
}

impl BackoffPolicy {
    /// Creates a validated backoff policy.
    ///
    /// # Errors
    /// - [`BackoffError::InvalidFactor`] if `factor` is not finite or `< 1.0`.
    /// - [`BackoffError::FirstExceedsMax`] if `first > max`.
    ///
    /// The delay floor defaults to `0`; set one with [`with_floor`](Self::with_floor).
    pub fn new(
        first: Duration,
        max: Duration,
        factor: f64,
        jitter: JitterPolicy,
    ) -> Result<Self, BackoffError> {
        if !factor.is_finite() || factor < 1.0 {
            return Err(BackoffError::InvalidFactor(factor));
        }
        if first > max {
            return Err(BackoffError::FirstExceedsMax { first, max });
        }
        Ok(Self {
            first,
            max,
            factor,
            jitter,
            floor: Duration::ZERO,
        })
    }

    /// Sets a minimum delay floor, applied to every computed delay (after jitter).
    ///
    /// Useful with [`JitterPolicy::Full`], which can otherwise return near-zero delays.
    /// A floor above `max` is clamped to `max`.
    #[must_use]
    pub fn with_floor(mut self, floor: Duration) -> Self {
        self.floor = floor.min(self.max);
        self
    }

    /// Initial delay before the first retry.
    #[must_use]
    pub fn first(&self) -> Duration {
        self.first
    }

    /// Maximum delay cap for retries.
    #[must_use]
    pub fn max(&self) -> Duration {
        self.max
    }

    /// Multiplicative growth factor (always finite and `>= 1.0`).
    #[must_use]
    pub fn factor(&self) -> f64 {
        self.factor
    }

    /// Jitter policy applied to computed delays.
    #[must_use]
    pub fn jitter(&self) -> JitterPolicy {
        self.jitter
    }

    /// Minimum delay floor (`0` if unset).
    #[must_use]
    pub fn floor(&self) -> Duration {
        self.floor
    }

    /// Computes the delay for the given attempt number (0-indexed).
    ///
    /// The base delay is `first × factor^attempt`, clamped to [`BackoffPolicy::max`].
    /// Jitter is applied to the clamped base, but the result is **never** fed back into subsequent calculations.
    /// Each attempt derives its base independently.
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

        let delay = match self.jitter {
            JitterPolicy::Decorrelated => {
                self.jitter
                    .apply_decorrelated(self.first.min(self.max), base, self.max)
            }
            _ => self.jitter.apply(base),
        };
        delay.max(self.floor)
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
            floor: Duration::ZERO,
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
            floor: Duration::ZERO,
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
            floor: Duration::ZERO,
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
            floor: Duration::ZERO,
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
            floor: Duration::ZERO,
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
            floor: Duration::ZERO,
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
            floor: Duration::ZERO,
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
            floor: Duration::ZERO,
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
            floor: Duration::ZERO,
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
            floor: Duration::ZERO,
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
            floor: Duration::ZERO,
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
            floor: Duration::ZERO,
        };
        assert_eq!(policy.next(u32::MAX), Duration::from_secs(10));
    }

    #[test]
    fn new_rejects_non_finite_or_subunit_factor() {
        let bad = [f64::NAN, f64::INFINITY, 0.5, 0.0, -1.0];
        for f in bad {
            assert!(
                matches!(
                    BackoffPolicy::new(
                        Duration::from_millis(100),
                        Duration::from_secs(30),
                        f,
                        JitterPolicy::None
                    ),
                    Err(BackoffError::InvalidFactor(_))
                ),
                "factor {f} must be rejected"
            );
        }
    }

    #[test]
    fn new_rejects_first_exceeding_max() {
        let res = BackoffPolicy::new(
            Duration::from_secs(10),
            Duration::from_secs(5),
            2.0,
            JitterPolicy::None,
        );
        assert!(matches!(res, Err(BackoffError::FirstExceedsMax { .. })));
    }

    #[test]
    fn new_accepts_valid_policy() {
        let p = BackoffPolicy::new(
            Duration::from_millis(100),
            Duration::from_secs(30),
            2.0,
            JitterPolicy::None,
        )
        .expect("valid");
        assert_eq!(p.next(1), Duration::from_millis(200));
    }

    #[test]
    fn floor_raises_jittered_delays() {
        let p = BackoffPolicy::new(
            Duration::from_millis(100),
            Duration::from_secs(30),
            2.0,
            JitterPolicy::Full,
        )
        .expect("valid")
        .with_floor(Duration::from_millis(50));

        for attempt in 0..12 {
            assert!(
                p.next(attempt) >= Duration::from_millis(50),
                "attempt {attempt}: below floor"
            );
        }
    }

    #[test]
    fn floor_is_clamped_to_max() {
        let p = BackoffPolicy::new(
            Duration::from_millis(100),
            Duration::from_secs(5),
            1.0,
            JitterPolicy::None,
        )
        .expect("valid")
        .with_floor(Duration::from_secs(999));
        assert!(p.next(0) <= Duration::from_secs(5));
    }
}
