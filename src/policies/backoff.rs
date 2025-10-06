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

use crate::policies::jitter::JitterPolicy;
use std::time::Duration;

/// Retry backoff policy.
///
/// Encapsulates parameters that determine how retry delays grow:
/// - [`factor`] multiplicative growth factor;
/// - [`first`] the initial delay;
/// - [`max`] the maximum delay cap.
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
                let prev_for_jitter = prev.unwrap_or(self.first.min(self.max));
                self.jitter
                    .apply_decorrelated(self.first.min(self.max), prev_for_jitter, self.max)
            }
            _ => self.jitter.apply(base),
        }
    }
}
