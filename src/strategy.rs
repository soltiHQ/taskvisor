//! # Backoff strategy for retrying tasks.
//!
//! [`BackoffStrategy`] controls how retry delays grow after repeated failures.
//! It is parameterized by:
//! - [`BackoffStrategy::factor`] — the multiplicative growth factor;
//! - [`BackoffStrategy::first`] — the initial delay;
//! - [`BackoffStrategy::max`] — the maximum delay cap.
//!
//! # Example
//! ```
//! use std::time::Duration;
//! use taskvisor::BackoffStrategy;
//!
//! let backoff = BackoffStrategy {
//!     first: Duration::from_millis(100),
//!     max: Duration::from_secs(10),
//!     factor: 2.0,
//! };
//!
//! assert_eq!(backoff.next(None), Duration::from_millis(100));
//! assert_eq!(backoff.next(Some(Duration::from_millis(100))), Duration::from_millis(200));
//! assert_eq!(backoff.next(Some(Duration::from_secs(20))), Duration::from_secs(10));
//! ```

use std::time::Duration;

/// Retry backoff policy.
///
/// Encapsulates parameters that determine how retry delays grow:
/// - [`factor`] — multiplicative growth factor;
/// - [`first`] — the initial delay;
/// - [`max`] — the maximum delay cap.
#[derive(Clone, Copy, Debug)]
pub struct BackoffStrategy {
    /// Initial delay before the first retry.
    pub first: Duration,
    /// Maximum delay cap for retries.
    pub max: Duration,
    /// Multiplicative growth factor (`>= 1.0` recommended).
    pub factor: f64,
}

impl Default for BackoffStrategy {
    /// Returns a strategy with:
    /// - `factor = 1.0` (constant delay);
    /// - `first = 100ms`;
    /// - `max = 30s`.
    fn default() -> Self {
        Self {
            first: Duration::from_millis(100),
            max: Duration::from_secs(30),
            factor: 1.0,
        }
    }
}

impl BackoffStrategy {
    /// Computes the next delay based on the previous one.
    ///
    /// - If `prev` is `None`, returns [`BackoffStrategy::first`].
    /// - Otherwise multiplies the previous delay by [`BackoffStrategy::factor`], and caps it at [`BackoffStrategy::max`].
    pub fn next(&self, prev: Option<Duration>) -> Duration {
        match prev {
            None => self.first,
            Some(d) => {
                let next = (d.as_secs_f64() * self.factor).min(self.max.as_secs_f64());
                Duration::from_secs_f64(next)
            }
        }
    }
}
