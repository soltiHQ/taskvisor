//! # Random spread for retry delays
//!
//! [`JitterPolicy`] changes a backoff delay by a random amount.
//! This helps when many tasks fail together: their retries no longer happen at the same moment.
//!
//! | Policy                                           | Raw range before backoff floors    | Use when                       |
//! |--------------------------------------------------|------------------------------------|--------------------------------|
//! | [`None`](JitterPolicy::None)                     | exact base delay                   | Predictable timing or tests    |
//! | [`Equal`](JitterPolicy::Equal)                   | `[base / 2, base]`                 | Balanced spread, good default  |
//! | [`Full`](JitterPolicy::Full)                     | `[0, base]`                        | Maximum spread, shorter delays |
//! | [`RandomizedBand`](JitterPolicy::RandomizedBand) | `[first, min(base * 3, max)]`      | Spread that may exceed base    |
//!
//! All policies have no memory.
//! A result from one retry does not affect the next retry.
//! [`RandomizedBand`](JitterPolicy::RandomizedBand) uses the current base delay, not the previous random result.
//! [`BackoffPolicy`](crate::BackoffPolicy) applies its user floor and safety floor after jitter; the final delay can be above the raw lower bound.

use std::time::Duration;

/// Controls random spread of retry delays.
///
/// Most users configure this through [`BackoffPolicy`](crate::BackoffPolicy).
/// Include a wildcard arm when matching because new policies may be added.
///
/// ## Trade-offs
///
/// - [`RandomizedBand`](Self::RandomizedBand): can exceed the base delay when used by [`BackoffPolicy`](crate::BackoffPolicy).
/// - [`Equal`](Self::Equal): balanced; keeps about 75% of the base delay on average.
/// - [`None`](Self::None): predictable, but retries can line up.
/// - [`Full`](Self::Full): widest spread below the base delay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum JitterPolicy {
    /// Uses the exact backoff delay.
    ///
    /// Use this for predictable timing and tests, or when retries cannot create a large load spike.
    None,

    /// Chooses a random delay in `[0, base]`.
    ///
    /// This gives the largest spread below the base, but it can also retry very quickly.
    /// Use [`BackoffPolicy::with_floor`](crate::BackoffPolicy::with_floor) when you need a strict minimum.
    Full,

    /// Chooses a random delay in `[base / 2, base]`.
    ///
    /// The average is about 75% of the base delay.
    /// This is the jitter used by [`BackoffPolicy::default`](crate::BackoffPolicy::default).
    Equal,

    /// Chooses a wider random band that may be above the base delay.
    ///
    /// When used by [`BackoffPolicy::delay_for_retry`](crate::BackoffPolicy::delay_for_retry), the delay is drawn from:
    /// ```text
    /// [first, min(base * 3, max)]
    /// ```
    ///
    /// Here `base` is the current retry delay before jitter:
    /// `first * factor^retry_index`, capped at `max`, where `retry_index` is 0-based.
    RandomizedBand,
}

impl Default for JitterPolicy {
    /// Returns [`JitterPolicy::None`].
    ///
    /// [`BackoffPolicy::default`](crate::BackoffPolicy::default) selects [`JitterPolicy::Equal`] explicitly;
    /// constructing a jitter policy on its own remains deterministic.
    fn default() -> Self {
        JitterPolicy::None
    }
}

impl JitterPolicy {
    /// Applies jitter using only `delay`.
    ///
    /// [`RandomizedBand`](Self::RandomizedBand) needs `first` and `max` for its normal range.
    /// This method does not have those values; it uses the same `[0, delay]` range as [`Full`](Self::Full).
    ///
    /// For full band behavior, use [`Self::apply_randomized_band`].
    #[must_use]
    pub fn apply(&self, delay: Duration) -> Duration {
        match self {
            JitterPolicy::None => delay,
            JitterPolicy::RandomizedBand | JitterPolicy::Full => self.full_jitter(delay),
            JitterPolicy::Equal => self.equal_jitter(delay),
        }
    }

    /// Chooses a uniform random delay in `[lower, min(upper_seed × 3, max)]` for [`RandomizedBand`](Self::RandomizedBand).
    ///
    /// This method first clamps `lower` to `max`.
    /// For any other policy, it applies that policy to the clamped `lower`; `upper_seed` is unused.
    #[must_use]
    pub fn apply_randomized_band(
        &self,
        lower: Duration,
        upper_seed: Duration,
        max: Duration,
    ) -> Duration {
        let lower = lower.min(max);
        if !matches!(self, JitterPolicy::RandomizedBand) {
            return self.apply(lower);
        }

        let upper = upper_seed.saturating_mul(3).min(max).max(lower);
        if lower >= upper {
            return lower;
        }

        let nanos = fastrand::u128(lower.as_nanos()..=upper.as_nanos());
        duration_from_nanos(nanos)
    }

    /// Full jitter: random in `[0, delay]`.
    ///
    /// Keeps sub-millisecond precision instead of rounding down to zero.
    fn full_jitter(&self, delay: Duration) -> Duration {
        let ns = delay.as_nanos();
        if ns == 0 {
            return Duration::ZERO;
        }
        duration_from_nanos(fastrand::u128(0..=ns))
    }

    /// Equal jitter: `delay/2 + random[0, delay/2]`.
    ///
    /// Keeps sub-millisecond precision instead of rounding down to zero.
    fn equal_jitter(&self, delay: Duration) -> Duration {
        let ns = delay.as_nanos();
        if ns == 0 {
            return Duration::ZERO;
        }
        let half = ns / 2;
        let jitter = fastrand::u128(0..=(ns - half));
        duration_from_nanos(half + jitter)
    }
}

/// Converts a total nanosecond count known to fit in [`Duration`].
fn duration_from_nanos(nanos: u128) -> Duration {
    const NANOS_PER_SECOND: u128 = 1_000_000_000;

    let seconds = (nanos / NANOS_PER_SECOND) as u64;
    let subsec_nanos = (nanos % NANOS_PER_SECOND) as u32;
    Duration::new(seconds, subsec_nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn jitter_policies_preserve_zero_duration() {
        for policy in [
            JitterPolicy::None,
            JitterPolicy::Full,
            JitterPolicy::Equal,
            JitterPolicy::RandomizedBand,
        ] {
            assert_eq!(policy.apply(Duration::ZERO), Duration::ZERO, "{policy:?}");
        }
    }

    #[test]
    fn randomized_band_apply_falls_back_to_full_jitter() {
        let delay = Duration::from_millis(500);
        for _ in 0..16 {
            let d = JitterPolicy::RandomizedBand.apply(delay);
            assert!(
                d <= delay,
                "context-free RandomizedBand apply() must be ≤ delay"
            );
        }
    }

    #[test]
    fn apply_randomized_band_on_full_falls_back_to_apply() {
        let lower = Duration::from_millis(100);
        let upper_seed = Duration::from_millis(300);
        let max = Duration::from_secs(10);

        for _ in 0..16 {
            let result = JitterPolicy::Full.apply_randomized_band(lower, upper_seed, max);
            assert!(result <= lower, "Full fallback should be in [0, lower]");
        }
    }

    #[test]
    fn apply_randomized_band_clamps_lower_to_max() {
        let lower = Duration::from_secs(10);
        let upper_seed = Duration::from_millis(50);
        let max = Duration::from_secs(5);

        let result = JitterPolicy::RandomizedBand.apply_randomized_band(lower, upper_seed, max);
        assert_eq!(
            result, max,
            "result must be capped at max, not the out-of-band lower"
        );
    }

    #[test]
    fn apply_randomized_band_draws_within_first_to_3x_band() {
        let lower = Duration::from_millis(100);
        let upper_seed = Duration::from_secs(1);
        let max = Duration::from_secs(30);
        let upper = Duration::from_secs(3); // seed*3 = 3s < max
        for _ in 0..32 {
            let d = JitterPolicy::RandomizedBand.apply_randomized_band(lower, upper_seed, max);
            assert!(
                d >= lower && d <= upper,
                "draw {d:?} outside [{lower:?}, {upper:?}]"
            );
        }
    }

    #[test]
    fn randomized_band_preserves_sub_millisecond_bounds_and_entropy() {
        let lower = Duration::from_nanos(123_457);
        let upper_seed = Duration::from_nanos(321_987);
        let upper = upper_seed.saturating_mul(3);
        let mut saw_value_above_lower = false;

        for _ in 0..16 {
            let result = JitterPolicy::RandomizedBand.apply_randomized_band(
                lower,
                upper_seed,
                Duration::from_millis(2),
            );
            assert!(
                result >= lower && result <= upper,
                "sub-ms draw {result:?} outside [{lower:?}, {upper:?}]"
            );
            saw_value_above_lower |= result > lower;
        }

        assert!(
            saw_value_above_lower,
            "a non-degenerate sub-ms band must not collapse to its lower bound"
        );
    }

    #[test]
    fn randomized_band_handles_duration_max_without_overflow() {
        let lower = Duration::MAX.saturating_sub(Duration::from_nanos(32));

        for _ in 0..8 {
            let result = JitterPolicy::RandomizedBand.apply_randomized_band(
                lower,
                Duration::MAX,
                Duration::MAX,
            );
            assert!(result >= lower && result <= Duration::MAX);
        }

        assert_eq!(
            duration_from_nanos(Duration::MAX.as_nanos()),
            Duration::MAX,
            "nanosecond conversion must preserve the largest Duration"
        );
    }

    #[test]
    fn full_and_equal_jitter_preserve_duration_max_bounds() {
        let equal_lower = duration_from_nanos(Duration::MAX.as_nanos() / 2);
        let legacy_u64_ceiling = Duration::from_nanos(u64::MAX);
        let mut full_used_the_wide_range = false;

        for _ in 0..8 {
            let full = JitterPolicy::Full.apply(Duration::MAX);
            assert!(full <= Duration::MAX);
            full_used_the_wide_range |= full > legacy_u64_ceiling;

            let equal = JitterPolicy::Equal.apply(Duration::MAX);
            assert!(
                equal >= equal_lower && equal <= Duration::MAX,
                "equal jitter {equal:?} outside [{equal_lower:?}, {:?}]",
                Duration::MAX
            );
        }

        assert!(
            full_used_the_wide_range,
            "full jitter must not truncate its nanosecond range to u64::MAX"
        );
    }

    #[test]
    fn full_and_equal_jitter_preserve_sub_millisecond_delays() {
        let d = Duration::from_micros(500);
        let mut full_nonzero = false;
        for _ in 0..16 {
            let f = JitterPolicy::Full.apply(d);
            assert!(f <= d, "full jitter must stay within [0, delay]");
            full_nonzero |= f > Duration::ZERO;

            let e = JitterPolicy::Equal.apply(d);
            assert!(
                e >= d / 2 && e <= d,
                "equal jitter must stay within [delay/2, delay]"
            );
        }
        assert!(
            full_nonzero,
            "sub-ms full jitter must not be systematically zero"
        );
    }
}
