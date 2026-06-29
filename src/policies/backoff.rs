//! # Backoff policy for retrying tasks.
//!
//! [`BackoffPolicy`] computes the delay before the next retry after a retryable task failure.
//!
//! | Field                              | Role                                      |
//! |------------------------------------|-------------------------------------------|
//! | [`first`](BackoffPolicy::first)    | Delay before the first retry              |
//! | [`factor`](BackoffPolicy::factor)  | Growth multiplier (`1.0` = constant)      |
//! | [`max`](BackoffPolicy::max)        | Maximum delay cap                         |
//! | [`jitter`](BackoffPolicy::jitter)  | Random spread applied to the base delay   |
//! | [`floor`](BackoffPolicy::floor)    | User minimum delay after jitter           |
//!
//! ## Formula
//!
//! For retry attempt `n` (`0` = first retry):
//!
//! ```text
//! base = min(first * factor^n, max)
//! delay = jitter(base)
//! delay = max(delay, user_floor)
//! delay = max(delay, 1ms) for non-zero base, capped at max
//! ```
//!
//! The implicit `1ms` floor prevents hot retry loops when jitter produces a near-zero delay.
//! `first = 0` opts out of this implicit floor, but a user floor set with [`with_floor`](BackoffPolicy::with_floor) still applies.
//!
//! ## Example
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
    /// - `floor = 0` (no user floor; the implicit 1ms non-zero-base safety floor still applies).
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
    /// The user delay floor defaults to `0`; set one with [`with_floor`](Self::with_floor).
    /// (A separate implicit 1ms safety floor still applies to a non-zero base - see [`next`](Self::next).)
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

    /// User-configured minimum delay floor (`0` = none; the implicit 1ms safety floor is separate).
    #[must_use]
    pub fn floor(&self) -> Duration {
        self.floor
    }

    /// Computes the delay for a retry attempt (`0` = first retry).
    ///
    /// The base delay is `first × factor^attempt`, capped at [`Self::max`].
    /// Jitter is then applied.
    ///
    /// For [`JitterPolicy::None`], [`JitterPolicy::Full`], and [`JitterPolicy::Equal`], the jittered delay never exceeds the base.
    /// [`JitterPolicy::RandomizedBand`] uses a wider band and may return a delay larger than the base.
    ///
    /// This method is stateless: the result is never fed back into later calls.
    ///
    /// After jitter, the user floor from [`with_floor`](Self::with_floor) is applied.
    /// For a non-zero base, an extra `1ms` safety floor is also applied, capped at `max`, to avoid zero-delay hot loops.
    /// `first = 0` disables only this implicit safety floor.
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
            JitterPolicy::RandomizedBand => {
                self.jitter
                    .apply_randomized_band(self.first.min(self.max), base, self.max)
            }
            _ => self.jitter.apply(base),
        };

        const MIN_NONZERO_DELAY: Duration = Duration::from_millis(1);
        let floored = delay.max(self.floor);
        if base.is_zero() {
            floored
        } else {
            floored.max(MIN_NONZERO_DELAY.min(self.max))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn policy(first: Duration, max: Duration, factor: f64, jitter: JitterPolicy) -> BackoffPolicy {
        BackoffPolicy {
            first,
            max,
            factor,
            jitter,
            floor: Duration::ZERO,
        }
    }

    #[test]
    fn exponential_growth_no_jitter() {
        let p = policy(
            Duration::from_millis(100),
            Duration::from_secs(30),
            2.0,
            JitterPolicy::None,
        );
        assert_eq!(p.next(0), Duration::from_millis(100));
        assert_eq!(p.next(1), Duration::from_millis(200));
        assert_eq!(p.next(2), Duration::from_millis(400));
        assert_eq!(p.next(3), Duration::from_millis(800));
        assert_eq!(p.next(4), Duration::from_millis(1600));
    }

    #[test]
    fn constant_factor_is_flat() {
        let p = policy(
            Duration::from_millis(500),
            Duration::from_secs(30),
            1.0,
            JitterPolicy::None,
        );
        for attempt in 0..10 {
            assert_eq!(
                p.next(attempt),
                Duration::from_millis(500),
                "attempt {attempt}"
            );
        }
    }

    #[test]
    fn first_exceeding_max_is_clamped_by_next() {
        let p = policy(
            Duration::from_secs(10),
            Duration::from_secs(5),
            2.0,
            JitterPolicy::None,
        );
        assert_eq!(p.next(0), Duration::from_secs(5));
    }

    #[test]
    fn large_attempt_and_overflow_clamp_to_max() {
        let p = policy(
            Duration::from_millis(100),
            Duration::from_secs(60),
            2.0,
            JitterPolicy::None,
        );
        assert_eq!(p.next(100), Duration::from_secs(60));
        assert_eq!(p.next(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn full_jitter_never_exceeds_base_as_it_grows() {
        let p = policy(
            Duration::from_millis(100),
            Duration::from_secs(30),
            2.0,
            JitterPolicy::Full,
        );
        for attempt in 5..15 {
            let base_ms = 100.0 * 2.0f64.powi(attempt as i32);
            assert!(
                p.next(attempt) <= Duration::from_millis(base_ms as u64),
                "attempt {attempt}: exceeds base {base_ms}ms"
            );
        }
    }

    #[test]
    fn equal_jitter_stays_within_half_to_full_base() {
        let p = policy(
            Duration::from_millis(100),
            Duration::from_secs(30),
            2.0,
            JitterPolicy::Equal,
        );
        for attempt in 0..15 {
            let base_ms = (100.0 * 2.0f64.powi(attempt as i32)).min(30_000.0);
            let delay = p.next(attempt);
            assert!(
                delay >= Duration::from_millis((base_ms / 2.0) as u64),
                "attempt {attempt}: below half of {base_ms}ms"
            );
            assert!(
                delay <= Duration::from_millis(base_ms as u64),
                "attempt {attempt}: above base {base_ms}ms"
            );
        }
    }

    #[test]
    fn randomized_band_jitter_grows_with_attempts() {
        let p = policy(
            Duration::from_millis(100),
            Duration::from_secs(30),
            2.0,
            JitterPolicy::RandomizedBand,
        );
        let mut lo = Duration::from_secs(999);
        let mut hi = Duration::ZERO;
        for _ in 0..100 {
            let delay = p.next(8);
            lo = lo.min(delay);
            hi = hi.max(delay);
        }
        assert!(
            lo >= Duration::from_millis(100),
            "lower bound {lo:?} below first"
        );
        assert!(
            hi >= Duration::from_secs(5),
            "upper bound {hi:?}; band too narrow"
        );
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

    #[test]
    fn sub_ms_nonzero_base_is_floored_to_at_least_one_ms() {
        let p = BackoffPolicy::new(
            Duration::from_micros(500),
            Duration::from_secs(1),
            1.0,
            JitterPolicy::Full,
        )
        .expect("valid");
        for attempt in 0..100 {
            assert!(
                p.next(attempt) >= Duration::from_millis(1),
                "non-zero sub-ms backoff must floor to >= 1ms (never a zero-delay hot-spin)"
            );
        }
    }

    #[test]
    fn zero_first_opts_out_of_the_floor() {
        let p = BackoffPolicy::new(
            Duration::ZERO,
            Duration::from_secs(1),
            1.0,
            JitterPolicy::None,
        )
        .expect("valid");
        assert_eq!(
            p.next(0),
            Duration::ZERO,
            "an explicit zero `first` must stay zero (no implicit floor)"
        );
    }

    #[test]
    fn next_never_exceeds_a_sub_ms_max() {
        let p = BackoffPolicy::new(
            Duration::from_micros(500),
            Duration::from_micros(500),
            1.0,
            JitterPolicy::Full,
        )
        .expect("valid");
        for attempt in 0..100 {
            assert!(
                p.next(attempt) <= Duration::from_micros(500),
                "next() must never exceed max, even when max < 1ms"
            );
        }
    }
}
