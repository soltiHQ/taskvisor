//! # Jitter policy for retry delays.
//!
//! [`JitterPolicy`] adds randomness to backoff delays to prevent thundering herd effects
//! when multiple tasks retry simultaneously.
//!
//! # Variants
//! - [`JitterPolicy::None`] — no jitter, use exact backoff delay
//! - [`JitterPolicy::Full`] — random delay in range [0, backoff_delay]
//! - [`JitterPolicy::Equal`] — delay = backoff_delay/2 + random[0, backoff_delay/2]
//! - [`JitterPolicy::Decorrelated`] — delay = random[base, prev_delay * 3], capped at max
//!
//! # Example
//! ```rust
//! use std::time::Duration;
//! use taskvisor::JitterPolicy;
//!
//! let jitter = JitterPolicy::Equal;
//! let base_delay = Duration::from_secs(2);
//!
//! // Equal jitter: delay will be in range [1s, 2s]
//! let actual_delay = jitter.apply(base_delay);
//! assert!(actual_delay >= Duration::from_secs(1));
//! assert!(actual_delay <= Duration::from_secs(2));
//! ```

use rand::Rng;
use std::time::Duration;

/// Policy controlling randomization of retry delays.
///
/// Prevents synchronized retries across multiple tasks by adding controlled randomness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JitterPolicy {
    /// No jitter: use exact backoff delay.
    None,

    /// Full jitter: random delay in [0, backoff_delay].
    /// Most aggressive jitter, can significantly reduce delay.
    Full,

    /// Equal jitter: delay = backoff_delay/2 + random[0, backoff_delay/2].
    /// Balances predictability with randomness (recommended default).
    Equal,

    /// Decorrelated jitter: delay = random[base, prev_delay * 3], capped at max.
    /// More sophisticated, considers previous delay and grows independently.
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
    /// # Parameters
    /// - `delay`: base delay to apply jitter to
    ///
    /// # Returns
    /// Modified delay with jitter applied according to the policy.
    ///
    /// # Note
    /// For `Decorrelated`, you should use [`apply_decorrelated`] instead,
    /// as it requires additional context (previous delay, base, max).
    pub fn apply(&self, delay: Duration) -> Duration {
        match self {
            JitterPolicy::None => delay,
            JitterPolicy::Full => self.full_jitter(delay),
            JitterPolicy::Equal => self.equal_jitter(delay),
            JitterPolicy::Decorrelated => {
                // Fallback to Full if called without context
                self.full_jitter(delay)
            }
        }
    }

    /// Applies decorrelated jitter with full context.
    ///
    /// # Parameters
    /// - `base`: minimum delay (usually BackoffPolicy::first)
    /// - `prev`: previous actual delay used
    /// - `max`: maximum allowed delay (usually BackoffPolicy::max)
    ///
    /// # Returns
    /// New delay in range [base, min(prev * 3, max)]
    pub fn apply_decorrelated(&self, base: Duration, prev: Duration, max: Duration) -> Duration {
        if !matches!(self, JitterPolicy::Decorrelated) {
            return self.apply(prev);
        }

        let mut rng = rand::rng();
        let base_ms = base.as_millis() as u64;
        let prev_ms = prev.as_millis() as u64;
        let max_ms = max.as_millis() as u64;

        let upper_bound = (prev_ms.saturating_mul(3)).min(max_ms);
        let clamped_upper = upper_bound.max(base_ms);

        if base_ms >= clamped_upper {
            return base;
        }

        let jittered_ms = rng.random_range(base_ms..=clamped_upper);
        Duration::from_millis(jittered_ms)
    }

    /// Full jitter: random[0, delay]
    fn full_jitter(&self, delay: Duration) -> Duration {
        let mut rng = rand::rng();
        let ms = delay.as_millis() as u64;
        if ms == 0 {
            return Duration::ZERO;
        }
        Duration::from_millis(rng.random_range(0..=ms))
    }

    /// Equal jitter: delay/2 + random[0, delay/2]
    fn equal_jitter(&self, delay: Duration) -> Duration {
        let mut rng = rand::rng();
        let ms = delay.as_millis() as u64;
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
