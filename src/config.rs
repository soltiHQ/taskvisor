//! # Global runtime configuration.
//!
//! [`Config`] defines the supervisor’s behavior: shutdown grace period,
//! concurrency limits, bus capacity, retry policy, backoff strategy,
//! and default task timeout.
//!
//! # Example
//! ```
//! use std::time::Duration;
//! use taskvisor::{Config, RestartPolicy, BackoffStrategy};
//!
//! let mut cfg = Config::default();
//! cfg.grace = Duration::from_secs(10);
//! cfg.timeout = Duration::from_secs(5);
//! cfg.backoff = BackoffStrategy::default();
//! cfg.restart = RestartPolicy::Always;
//! cfg.max_concurrent = 4;
//!
//! assert_eq!(cfg.max_concurrent, 4);
//! ```

use std::time::Duration;

use crate::event::strategy::BackoffStrategy;
use crate::policy::RestartPolicy;

/// Global configuration for the runtime and supervisor.
///
/// Controls shutdown grace, concurrency, event bus, restart/backoff behavior, and task timeouts.
#[derive(Clone, Debug)]
pub struct Config {
    /// Maximum time to wait for graceful shutdown before force-terminating.
    pub grace: Duration,
    /// Maximum number of tasks to run concurrently (0 = unlimited).
    pub max_concurrent: usize,
    /// Capacity of the event bus channel.
    pub bus_capacity: usize,
    /// Default restart policy for tasks.
    pub restart: RestartPolicy,
    /// Default backoff strategy for retries.
    pub backoff: BackoffStrategy,
    /// Default task timeout (0 = no timeout).
    pub timeout: Duration,
}

impl Default for Config {
    /// Provides a default configuration:
    /// - `grace = 30s`
    /// - `max_concurrent = 0` (unlimited)
    /// - `bus_capacity = 1024`
    /// - `timeout = 0s` (no timeout)
    /// - `restart = RestartPolicy::OnFailure`
    /// - `backoff = BackoffStrategy::default()`
    fn default() -> Self {
        Self {
            max_concurrent: 0,
            bus_capacity: 1024,
            timeout: Duration::from_secs(0),
            grace: Duration::from_secs(30),
            backoff: BackoffStrategy::default(),
            restart: RestartPolicy::default(),
        }
    }
}
