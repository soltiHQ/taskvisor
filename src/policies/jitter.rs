//! # Jitter policy for retry delays.
//!
//! [`JitterPolicy`] adds randomness to backoff delays to prevent thundering herd effects when multiple tasks retry simultaneously.
//! - [`JitterPolicy::Equal`] delay = backoff_delay/2 + random[0, backoff_delay/2] (balanced)
//! - [`JitterPolicy::Decorrelated`] stateful jitter based on previous delay (sophisticated)
//! - [`JitterPolicy::Full`] random delay in [0, backoff_delay] (most aggressive)
//! - [`JitterPolicy::None`] no randomization, predictable delays

use std::time::Duration;

/// Policy controlling randomization of retry delays.
///
/// Prevents synchronized retries across multiple tasks by adding controlled randomness.
///
/// ## Trade-offs
/// - **None**: Predictable, but risks thundering herd
/// - **Equal**: Balanced (recommended for most use cases)
/// - **Full**: Maximum randomness, aggressive load spreading
/// - **Decorrelated**: Stateful, prevents retry correlation (requires previous delay)
///
/// # Also
///
/// - [`BackoffPolicy`](crate::BackoffPolicy) - uses `JitterPolicy` to randomize computed delays
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    /// Decorrelated jitter: delay = random[base, prev_delay * 3], capped at `max`.
    ///
    /// More sophisticated, considers the previous delay and grows independently.
    /// Requires context via [`apply_decorrelated`](Self::apply_decorrelated).
    Decorrelated,
}

impl Default for JitterPolicy {
    /// Returns [`JitterPolicy::None`] as default (backwards compatible).
    fn default() -> Self {
        JitterPolicy::None
    }
}

impl JitterPolicy {
    /// Applies jitter to the given delay.
    ///
    /// For `Decorrelated`, this method returns the input **unchanged** because decorrelated jitter requires additional context.
    /// Use [`apply_decorrelated`](Self::apply_decorrelated) instead.
    pub fn apply(&self, delay: Duration) -> Duration {
        match self {
            JitterPolicy::None | JitterPolicy::Decorrelated => delay,
            JitterPolicy::Equal => self.equal_jitter(delay),
            JitterPolicy::Full => self.full_jitter(delay),
        }
    }

    /// Applies decorrelated jitter with full context.
    /// - `base`: minimal delay (usually the initial backoff)
    /// - `prev`: previous actual delay
    /// - `max`: maximum cap
    ///
    /// ### Note
    ///
    /// If called on a non-`Decorrelated` policy, falls back to `apply(base)`.
    pub fn apply_decorrelated(&self, base: Duration, prev: Duration, max: Duration) -> Duration {
        if !matches!(self, JitterPolicy::Decorrelated) {
            return self.apply(base);
        }

        let base_ms = (base.as_millis().min(u128::from(u64::MAX))) as u64;
        let prev_ms = (prev.as_millis().min(u128::from(u64::MAX))) as u64;
        let max_ms = (max.as_millis().min(u128::from(u64::MAX))) as u64;

        let upper_bound = prev_ms.saturating_mul(3).min(max_ms);
        let clamped_upper = upper_bound.max(base_ms);
        if base_ms >= clamped_upper {
            return base;
        }

        let jittered_ms = fastrand::u64(base_ms..=clamped_upper);
        Duration::from_millis(jittered_ms)
    }

    /// Full jitter: random in [0, delay].
    fn full_jitter(&self, delay: Duration) -> Duration {
        let ms = (delay.as_millis().min(u128::from(u64::MAX))) as u64;
        if ms == 0 {
            return Duration::ZERO;
        }
        Duration::from_millis(fastrand::u64(0..=ms))
    }

    /// Equal jitter: delay/2 + random[0, delay/2].
    fn equal_jitter(&self, delay: Duration) -> Duration {
        let ms = (delay.as_millis().min(u128::from(u64::MAX))) as u64;
        if ms == 0 {
            return Duration::ZERO;
        }
        let half = ms / 2;
        let jitter = if half == 0 {
            0
        } else {
            fastrand::u64(0..=half)
        };
        Duration::from_millis(half + jitter)
    }
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
    fn decorrelated_apply_returns_input_unchanged() {
        let delay = Duration::from_millis(500);
        assert_eq!(JitterPolicy::Decorrelated.apply(delay), delay);
    }

    #[test]
    fn apply_decorrelated_on_full_falls_back_to_apply() {
        let base = Duration::from_millis(100);
        let prev = Duration::from_millis(300);
        let max = Duration::from_secs(10);

        for _ in 0..100 {
            let result = JitterPolicy::Full.apply_decorrelated(base, prev, max);
            assert!(result <= base, "Full fallback should be in [0, base]");
        }
    }

    #[test]
    fn apply_decorrelated_base_ge_max_returns_base() {
        let base = Duration::from_secs(10);
        let prev = Duration::from_millis(50);
        let max = Duration::from_secs(5);

        let result = JitterPolicy::Decorrelated.apply_decorrelated(base, prev, max);
        assert_eq!(result, base);
    }
}
