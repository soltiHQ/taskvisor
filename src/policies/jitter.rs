//! # Jitter policy for retry delays.
//!
//! [`JitterPolicy`] adds randomness to backoff delays to prevent thundering herd effects
//! when multiple tasks retry simultaneously.
//!
//! - [`JitterPolicy::None`] no randomization, predictable delays
//! - [`JitterPolicy::Full`] random delay in [0, backoff_delay] (most aggressive)
//! - [`JitterPolicy::Equal`] delay = backoff_delay/2 + random[0, backoff_delay/2] (balanced)
//! - [`JitterPolicy::Decorrelated`] stateful jitter based on previous delay (sophisticated)

use rand::Rng;
use std::time::Duration;

/// Policy controlling randomization of retry delays.
///
/// Prevents synchronized retries across multiple tasks by adding controlled randomness.
///
/// ## Trade-offs
/// - **None**: Predictable, but risks thundering herd
/// - **Full**: Maximum randomness, aggressive load spreading
/// - **Equal**: Balanced (recommended for most use cases)
/// - **Decorrelated**: Stateful, prevents retry correlation (requires previous delay)
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
    /// ### Note
    /// For `Decorrelated`, this method returns the input unchanged.
    /// Use [`apply_decorrelated`](Self::apply_decorrelated) instead,
    /// as it requires additional context (previous delay, base, max).
    pub fn apply(&self, delay: Duration) -> Duration {
        match self {
            JitterPolicy::None => delay,
            JitterPolicy::Full => self.full_jitter(delay),
            JitterPolicy::Equal => self.equal_jitter(delay),
            JitterPolicy::Decorrelated => delay,
        }
    }

    /// Applies decorrelated jitter with full context.
    ///
    /// - `base`: minimal delay (usually the initial backoff)
    /// - `prev`: previous actual delay
    /// - `max`: maximum cap
    ///
    /// ### Note
    /// If called on a non-`Decorrelated` policy, falls back to `apply(base)`.
    pub fn apply_decorrelated(&self, base: Duration, prev: Duration, max: Duration) -> Duration {
        if !matches!(self, JitterPolicy::Decorrelated) {
            return self.apply(base);
        }

        let mut rng = rand::rng();
        let base_ms = (base.as_millis().min(u128::from(u64::MAX))) as u64;
        let prev_ms = (prev.as_millis().min(u128::from(u64::MAX))) as u64;
        let max_ms = (max.as_millis().min(u128::from(u64::MAX))) as u64;

        // Upper bound is min(prev*3, max), but never below base.
        let upper_bound = prev_ms.saturating_mul(3).min(max_ms);
        let clamped_upper = upper_bound.max(base_ms);
        if base_ms >= clamped_upper {
            return base;
        }

        let jittered_ms = rng.random_range(base_ms..=clamped_upper);
        Duration::from_millis(jittered_ms)
    }

    /// Full jitter: random in [0, delay].
    fn full_jitter(&self, delay: Duration) -> Duration {
        let mut rng = rand::rng();
        let ms = (delay.as_millis().min(u128::from(u64::MAX))) as u64;
        if ms == 0 {
            return Duration::ZERO;
        }
        Duration::from_millis(rng.random_range(0..=ms))
    }

    /// Equal jitter: delay/2 + random[0, delay/2].
    fn equal_jitter(&self, delay: Duration) -> Duration {
        let mut rng = rand::rng();
        let ms = (delay.as_millis().min(u128::from(u64::MAX))) as u64;
        if ms == 0 {
            return Duration::ZERO;
        }
        let half = ms / 2;
        let jitter = if half == 0 {
            0
        } else {
            rng.random_range(0..=half)
        };
        Duration::from_millis(half + jitter)
    }
}
