//! # Backoff policy for retrying tasks.
//!
//! [`BackoffPolicy`] computes the delay before the next retry after a retryable task failure.
//!
//! | Field                              | Role                                      |
//! |------------------------------------|-------------------------------------------|
//! | [`first`](BackoffPolicy::first)    | Delay before the first retry              |
//! | [`factor`](BackoffPolicy::factor)  | Growth multiplier (`1.0` = constant)      |
//! | [`jitter`](BackoffPolicy::jitter)  | Random spread applied to the base delay   |
//! | [`floor`](BackoffPolicy::floor)    | User minimum delay after jitter           |
//! | [`max`](BackoffPolicy::max)        | Maximum delay cap                         |
//!
//! ## Formula
//!
//! For retry index `n` (`0` = delay before the first retry, after 1-based event attempt `1`):
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
//! use taskvisor::BackoffPolicy;
//!
//! let backoff = BackoffPolicy::exponential(Duration::from_millis(100))
//!     .with_max(Duration::from_secs(10));
//!
//! // Retry index 0 - uses 'first' (100ms)
//! assert_eq!(backoff.delay_for_retry(0), Duration::from_millis(100));
//!
//! // Retry index 1 - first × factor^1 = 200ms
//! assert_eq!(backoff.delay_for_retry(1), Duration::from_millis(200));
//!
//! // Retry index 10 - 100ms × 2^10 = 102_400ms → capped at max=10s
//! assert_eq!(backoff.delay_for_retry(10), Duration::from_secs(10));
//! ```
//!
//! Named constructors: [`constant`](BackoffPolicy::constant) and [`exponential`](BackoffPolicy::exponential).
//! For a custom growth factor use [`new`](BackoffPolicy::new).

use std::time::Duration;

use thiserror::Error;

use crate::policies::jitter::JitterPolicy;

/// Error returned by [`BackoffPolicy::new`] when parameters violate the policy invariants.
///
/// Match with a wildcard arm because this enum is non-exhaustive. Its data-carrying
/// variants are also non-exhaustive, so include `..` when matching their payloads.
#[derive(Debug, Clone, Copy, PartialEq, Error)]
#[non_exhaustive]
pub enum BackoffError {
    /// `factor` was not finite or was below `1.0` (a backoff factor must not shrink delays).
    #[error("backoff factor must be finite and >= 1.0, got {0}")]
    #[non_exhaustive]
    InvalidFactor(f64),
    /// `first` exceeded `max` (the initial delay cannot be larger than the cap).
    #[error("backoff first delay {first:?} exceeds max {max:?}")]
    #[non_exhaustive]
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
/// - [`TaskSpec`](crate::TaskSpec) - inherits or overrides task execution settings
/// - [`TaskDefaults`](crate::TaskDefaults) - supervisor-wide task settings
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

/// Default delay cap shared by [`Default`] and the named constructors.
const DEFAULT_MAX: Duration = Duration::from_secs(30);

impl Default for BackoffPolicy {
    /// Returns the production default retry strategy:
    /// - `factor = 2.0` (exponential delay);
    /// - `first = 200ms`;
    /// - `max = 30s`;
    /// - `jitter = Equal` (each delay stays in `[base / 2, base]`);
    /// - `floor = 0` (no user floor; the implicit 1ms non-zero-base safety floor still applies).
    ///
    /// The first actual delay is therefore in `[100ms, 200ms]`. It is never
    /// faster than the pre-0.6 constant `100ms` default, while repeated failures
    /// back off and spread retries across time.
    fn default() -> Self {
        Self {
            first: Duration::from_millis(200),
            max: DEFAULT_MAX,
            jitter: JitterPolicy::Equal,
            factor: 2.0,
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
    /// (A separate implicit 1ms safety floor still applies to a non-zero base - see [`delay_for_retry`](Self::delay_for_retry).)
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

    /// Constant backoff: the same `delay` before every retry.
    ///
    /// No jitter by default.
    /// Add it with [`with_jitter`](Self::with_jitter).
    /// The cap starts at 30 seconds, or at `delay` if that is larger.
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use taskvisor::BackoffPolicy;
    ///
    /// let backoff = BackoffPolicy::constant(Duration::from_millis(500));
    /// assert_eq!(backoff.delay_for_retry(0), Duration::from_millis(500));
    /// assert_eq!(backoff.delay_for_retry(9), Duration::from_millis(500));
    /// ```
    #[must_use]
    pub fn constant(delay: Duration) -> Self {
        Self {
            first: delay,
            max: delay.max(DEFAULT_MAX),
            factor: 1.0,
            jitter: JitterPolicy::None,
            floor: Duration::ZERO,
        }
    }

    /// Exponential backoff: the delay doubles after every retry.
    ///
    /// Starts at `first` with `factor = 2.0`.
    /// The cap starts at 30 seconds, or at `first` if that is larger.
    /// Change the cap with [`with_max`](Self::with_max).
    /// No jitter by default.
    /// Add it with [`with_jitter`](Self::with_jitter).
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use taskvisor::{BackoffPolicy, JitterPolicy};
    ///
    /// let backoff = BackoffPolicy::exponential(Duration::from_millis(100))
    ///     .with_max(Duration::from_secs(10))
    ///     .with_jitter(JitterPolicy::Equal);
    ///
    /// // Base delays: 100ms, 200ms, 400ms, ... capped at 10s.
    /// // Equal jitter keeps each delay within [base/2, base].
    /// assert!(backoff.delay_for_retry(0) <= Duration::from_millis(100));
    /// ```
    #[must_use]
    pub fn exponential(first: Duration) -> Self {
        Self {
            first,
            max: first.max(DEFAULT_MAX),
            factor: 2.0,
            jitter: JitterPolicy::None,
            floor: Duration::ZERO,
        }
    }

    /// Builder: sets the maximum delay cap.
    ///
    /// If `max` is below the current `first`, `first` is lowered to `max`.
    /// A previously set floor is re-clamped to the new cap.
    /// The policy stays valid without a `Result`.
    #[must_use]
    pub fn with_max(mut self, max: Duration) -> Self {
        self.max = max;
        self.first = self.first.min(max);
        self.floor = self.floor.min(max);
        self
    }

    /// Builder: sets the jitter policy.
    ///
    /// Jitter spreads retry delays in time.
    /// It helps when many tasks fail at the same moment.
    #[must_use]
    pub fn with_jitter(mut self, jitter: JitterPolicy) -> Self {
        self.jitter = jitter;
        self
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

    /// Computes the delay for a retry index (`0` = first retry after attempt 1).
    ///
    /// Runtime event attempt numbers are 1-based; this retry index is 0-based.
    /// For example, the delay after event attempt `1` uses `retry_index = 0`.
    ///
    /// The base delay is `first × factor^retry_index`, capped at [`Self::max`].
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
    #[must_use]
    pub fn delay_for_retry(&self, retry_index: u32) -> Duration {
        let clamped_exp = retry_index.min(i32::MAX as u32) as i32;
        let unclamped_secs = self.first.as_secs_f64() * self.factor.powi(clamped_exp);

        // `Duration::MAX.as_secs_f64()` rounds up to a value that cannot be
        // converted back to `Duration`. Keep the conversion fallible, then
        // enforce the configured cap after conversion as well.
        let base = if self.first.is_zero() {
            Duration::ZERO
        } else {
            Duration::try_from_secs_f64(unclamped_secs)
                .unwrap_or(self.max)
                .min(self.max)
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

    /// Compatibility alias for [`delay_for_retry`](Self::delay_for_retry).
    ///
    /// `retry_index` is 0-based: `0` computes the delay before the first retry.
    #[inline]
    #[must_use]
    pub fn next(&self, retry_index: u32) -> Duration {
        self.delay_for_retry(retry_index)
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
    fn default_is_exponential_with_equal_jitter() {
        let p = BackoffPolicy::default();

        assert_eq!(p.first(), Duration::from_millis(200));
        assert_eq!(p.max(), Duration::from_secs(30));
        assert_eq!(p.factor(), 2.0);
        assert_eq!(p.jitter(), JitterPolicy::Equal);
        assert_eq!(p.floor(), Duration::ZERO);

        for (attempt, lower, upper) in [
            (0, 100, 200),
            (1, 200, 400),
            (2, 400, 800),
            (20, 15_000, 30_000),
        ] {
            for _ in 0..8 {
                let delay = p.delay_for_retry(attempt);
                assert!(
                    delay >= Duration::from_millis(lower) && delay <= Duration::from_millis(upper),
                    "attempt {attempt}: {delay:?} outside [{lower}ms, {upper}ms]"
                );
            }
        }
    }

    #[test]
    fn next_is_a_compatible_alias_for_delay_for_retry() {
        let p = BackoffPolicy::exponential(Duration::from_millis(100));

        for retry_index in [0, 1, 8, u32::MAX] {
            assert_eq!(p.next(retry_index), p.delay_for_retry(retry_index));
        }
    }

    #[test]
    fn large_attempt_and_overflow_clamp_to_max() {
        let p = policy(
            Duration::from_millis(100),
            Duration::from_secs(60),
            2.0,
            JitterPolicy::None,
        );
        assert_eq!(p.delay_for_retry(100), Duration::from_secs(60));
        assert_eq!(p.delay_for_retry(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn duration_max_randomized_band_does_not_panic() {
        let p = BackoffPolicy::new(
            Duration::MAX,
            Duration::MAX,
            1.0,
            JitterPolicy::RandomizedBand,
        )
        .expect("Duration::MAX is a valid delay when max matches first");

        assert_eq!(p.delay_for_retry(0), Duration::MAX);
    }

    #[test]
    fn randomized_band_uses_the_grown_base_through_delay_for_retry() {
        let p = policy(
            Duration::from_millis(100),
            Duration::from_secs(30),
            2.0,
            JitterPolicy::RandomizedBand,
        );
        let retry_index = 8;
        let grown_base = Duration::from_millis(25_600);
        let previous_seed = fastrand::get_seed();

        fastrand::seed(0x5eed);
        let expected =
            JitterPolicy::RandomizedBand.apply_randomized_band(p.first(), grown_base, p.max());
        fastrand::seed(0x5eed);
        let actual = p.delay_for_retry(retry_index);
        fastrand::seed(previous_seed);

        assert!(
            expected > p.first().saturating_mul(3),
            "the seeded draw must distinguish the grown band from the initial band"
        );
        assert_eq!(
            actual, expected,
            "delay_for_retry must pass the grown exponential base into RandomizedBand"
        );
    }

    #[test]
    fn full_jitter_never_exceeds_base_as_it_grows() {
        let p = policy(
            Duration::from_millis(100),
            Duration::from_secs(30),
            2.0,
            JitterPolicy::Full,
        );
        for attempt in [5, 8, 14] {
            let base_ms = (100.0 * 2.0f64.powi(attempt as i32)).min(30_000.0);
            assert!(
                p.delay_for_retry(attempt) <= Duration::from_millis(base_ms as u64),
                "attempt {attempt}: exceeds base {base_ms}ms"
            );
        }
    }

    #[test]
    fn constant_preset_yields_flat_delays() {
        let p = BackoffPolicy::constant(Duration::from_millis(500));

        assert_eq!(p.factor(), 1.0, "constant preset must use factor 1.0");
        assert!(
            matches!(p.jitter(), JitterPolicy::None),
            "constant preset must have no jitter by default"
        );
        for attempt in [0, 1, 9] {
            assert_eq!(
                p.delay_for_retry(attempt),
                Duration::from_millis(500),
                "attempt {attempt}: constant delay must not change"
            );
        }
    }

    #[test]
    fn constant_preset_allows_delay_above_default_cap() {
        let p = BackoffPolicy::constant(Duration::from_secs(60));

        assert!(
            p.first() <= p.max(),
            "invariant first <= max must hold for any delay"
        );
        assert_eq!(
            p.delay_for_retry(0),
            Duration::from_secs(60),
            "a delay above the default cap must be preserved, not clamped"
        );
    }

    #[test]
    fn exponential_preset_doubles_and_caps_at_default_max() {
        let p = BackoffPolicy::exponential(Duration::from_millis(100));

        assert_eq!(p.factor(), 2.0, "exponential preset must use factor 2.0");
        assert_eq!(p.delay_for_retry(0), Duration::from_millis(100));
        assert_eq!(p.delay_for_retry(1), Duration::from_millis(200));
        assert_eq!(p.delay_for_retry(2), Duration::from_millis(400));
        assert_eq!(
            p.delay_for_retry(20),
            Duration::from_secs(30),
            "growth must cap at the default 30s max"
        );
    }

    #[test]
    fn exponential_preset_with_large_first_keeps_invariant() {
        let p = BackoffPolicy::exponential(Duration::from_secs(60));

        assert!(
            p.first() <= p.max(),
            "invariant first <= max must hold when first exceeds the default cap"
        );
        assert_eq!(p.delay_for_retry(0), Duration::from_secs(60));
    }

    #[test]
    fn with_max_sets_the_cap() {
        let p =
            BackoffPolicy::exponential(Duration::from_millis(100)).with_max(Duration::from_secs(1));

        assert_eq!(
            p.delay_for_retry(10),
            Duration::from_secs(1),
            "with_max must cap the grown delay"
        );
    }

    #[test]
    fn with_max_below_first_clamps_first_down() {
        let p = BackoffPolicy::constant(Duration::from_secs(10)).with_max(Duration::from_secs(5));

        assert_eq!(
            p.first(),
            Duration::from_secs(5),
            "with_max below first must lower first to max (invariant by construction)"
        );
        assert_eq!(p.delay_for_retry(0), Duration::from_secs(5));
    }

    #[test]
    fn with_max_reclamps_existing_floor() {
        let p = BackoffPolicy::constant(Duration::from_millis(100))
            .with_floor(Duration::from_secs(5))
            .with_max(Duration::from_secs(1));

        assert!(
            p.floor() <= p.max(),
            "with_max must re-clamp a previously set floor"
        );
        for attempt in [0, 4, 9] {
            assert!(
                p.delay_for_retry(attempt) <= Duration::from_secs(1),
                "attempt {attempt}: delay must never exceed the new max"
            );
        }
    }

    #[test]
    fn with_jitter_sets_policy_and_keeps_bounds() {
        let p =
            BackoffPolicy::exponential(Duration::from_millis(100)).with_jitter(JitterPolicy::Equal);

        assert!(
            matches!(p.jitter(), JitterPolicy::Equal),
            "with_jitter must store the given policy"
        );
        for attempt in [0, 3, 9] {
            let base_ms = (100.0 * 2.0f64.powi(attempt as i32)).min(30_000.0);
            let delay = p.delay_for_retry(attempt);
            assert!(
                delay >= Duration::from_millis((base_ms / 2.0) as u64)
                    && delay <= Duration::from_millis(base_ms as u64),
                "attempt {attempt}: Equal jitter must stay within [base/2, base]"
            );
        }
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
        assert_eq!(p.delay_for_retry(1), Duration::from_millis(200));
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
        .with_floor(Duration::from_millis(100));

        assert_eq!(p.delay_for_retry(0), Duration::from_millis(100));
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
        assert_eq!(p.floor(), Duration::from_secs(5));
        assert_eq!(p.delay_for_retry(0), Duration::from_secs(5));
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
        for attempt in [0, 1, u32::MAX] {
            assert_eq!(
                p.delay_for_retry(attempt),
                Duration::from_millis(1),
                "non-zero sub-ms backoff must use the 1ms hot-spin floor"
            );
        }
    }

    #[test]
    fn zero_first_opts_out_of_the_floor() {
        let p = BackoffPolicy::new(
            Duration::ZERO,
            Duration::from_secs(1),
            2.0,
            JitterPolicy::None,
        )
        .expect("valid");
        assert_eq!(
            p.delay_for_retry(0),
            Duration::ZERO,
            "an explicit zero `first` must stay zero (no implicit floor)"
        );
        assert_eq!(
            p.delay_for_retry(u32::MAX),
            Duration::ZERO,
            "zero multiplied by an overflowing exponential factor is still zero"
        );
    }

    #[test]
    fn delay_for_retry_never_exceeds_a_sub_ms_max() {
        let p = BackoffPolicy::new(
            Duration::from_micros(500),
            Duration::from_micros(500),
            1.0,
            JitterPolicy::Full,
        )
        .expect("valid");
        for attempt in [0, 1, u32::MAX] {
            assert_eq!(
                p.delay_for_retry(attempt),
                Duration::from_micros(500),
                "the implicit floor must be capped when max is below 1ms"
            );
        }
    }
}
