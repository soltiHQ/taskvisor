//! # Jitter policy for retry delays.
//!
//! [`JitterPolicy`] adds random spread to retry delays. This helps avoid many tasks retrying at the same moment.
//!
//! | Policy                                           | Range when used by [`BackoffPolicy`](crate::BackoffPolicy) | Use when                       |
//! |--------------------------------------------------|------------------------------------------------------------|--------------------------------|
//! | [`None`](JitterPolicy::None)                     | exact base delay                                           | Predictable timing or tests    |
//! | [`Equal`](JitterPolicy::Equal)                   | `[base / 2, base]`                                         | Balanced spread, good default  |
//! | [`Full`](JitterPolicy::Full)                     | `[0, base]`                                                | Maximum spread, shorter delays |
//! | [`RandomizedBand`](JitterPolicy::RandomizedBand) | `[first, min(base * 3, max)]`                              | Widest spread                  |
//!
//! [`RandomizedBand`](JitterPolicy::RandomizedBand) is memoryless.
//! It uses the current retry base delay, not the previous actual delay.

use std::time::Duration;

/// Policy controlling randomization of retry delays.
///
/// Most users configure this through [`BackoffPolicy`](crate::BackoffPolicy).
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
    /// No jitter: use exact backoff delay.
    ///
    /// Use when:
    /// - Only one task retrying (no herd risk)
    /// - Predictable timing required
    /// - Testing/debugging
    None,

    /// Full jitter: random delay in [0, backoff_delay].
    ///
    /// Most aggressive jitter, can significantly reduce delay.
    /// Use when maximum load spreading is needed.
    Full,

    /// Equal jitter: delay/2 + random[0, delay/2].
    ///
    /// Balances predictability with randomness (recommended default).
    /// Preserves ~75% of the original backoff on average.
    Equal,

    /// Randomized wideband.
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
    /// [`BackoffPolicy::default`](crate::BackoffPolicy::default) selects
    /// [`JitterPolicy::Equal`] explicitly; constructing a jitter policy on its
    /// own remains deterministic.
    fn default() -> Self {
        JitterPolicy::None
    }
}

impl JitterPolicy {
    /// Applies context-free jitter to `delay`.
    ///
    /// [`RandomizedBand`](Self::RandomizedBand) needs `first` and `max` to build its full band.
    /// This method does not have that context, so `RandomizedBand` falls back to [`Full`](Self::Full)-style jitter: `[0, delay]`.
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

    /// Draws a uniform random delay over the band `[lower, min(upper_seed × 3, max)]`.
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
    /// Nanosecond-faithful: a sub-millisecond `delay` is **not** truncated to zero.
    fn full_jitter(&self, delay: Duration) -> Duration {
        let ns = delay.as_nanos();
        if ns == 0 {
            return Duration::ZERO;
        }
        duration_from_nanos(fastrand::u128(0..=ns))
    }

    /// Equal jitter: `delay/2 + random[0, delay/2]`.
    ///
    /// Nanosecond-faithful: a sub-millisecond `delay` is **not** truncated to zero.
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
    fn full_jitter_zero_duration_returns_zero() {
        assert_eq!(JitterPolicy::Full.apply(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn equal_jitter_zero_duration_returns_zero() {
        assert_eq!(JitterPolicy::Equal.apply(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn randomized_band_apply_falls_back_to_full_jitter() {
        let delay = Duration::from_millis(500);
        for _ in 0..100 {
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

        for _ in 0..100 {
            let result = JitterPolicy::Full.apply_randomized_band(lower, upper_seed, max);
            assert!(result <= lower, "Full fallback should be in [0, lower]");
        }
    }

    #[test]
    fn apply_randomized_band_clamps_lower_to_max() {
        // A direct caller passing lower > max must still not exceed the cap: lower is
        // clamped to max, so the result is max (never the out-of-band lower).
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
        // Lower bound is the constant `lower` (first); upper is min(seed*3, max).
        let lower = Duration::from_millis(100);
        let upper_seed = Duration::from_secs(1);
        let max = Duration::from_secs(30);
        let upper = Duration::from_secs(3); // seed*3 = 3s < max
        for _ in 0..500 {
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

        for _ in 0..500 {
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

        for _ in 0..100 {
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

        for _ in 0..100 {
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
        let mut equal_nonzero = false;
        for _ in 0..1000 {
            let f = JitterPolicy::Full.apply(d);
            assert!(f <= d, "full jitter must stay within [0, delay]");
            if f > Duration::ZERO {
                full_nonzero = true;
            }

            let e = JitterPolicy::Equal.apply(d);
            assert!(
                e >= d / 2 && e <= d,
                "equal jitter must stay within [delay/2, delay]"
            );
            if e > Duration::ZERO {
                equal_nonzero = true;
            }
        }
        assert!(
            full_nonzero,
            "sub-ms full jitter must not be systematically zero"
        );
        assert!(
            equal_nonzero,
            "sub-ms equal jitter must not be systematically zero"
        );
    }
}
