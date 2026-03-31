//! # Global runtime configuration.
//!
//! [`SupervisorConfig`] provides centralized settings for the supervisor runtime.
//!
//! Config is used in two ways:
//! 1. **Supervisor creation**: `Supervisor::new(config, subscribers)`
//! 2. **TaskSpec defaults**: `TaskSpec::with_defaults(task, &config)`
//!
//! ## Sentinel values
//!
//! - `max_concurrent = 0` → unlimited (no global semaphore created)
//! - `timeout = 0s` → no timeout (treated as `None` by `TaskSpec::with_defaults`)

use std::time::Duration;

use crate::policies::{BackoffPolicy, RestartPolicy};

/// Global configuration for the supervisor runtime.
///
/// Defines:
/// - **Concurrency limits**: max simultaneous tasks
/// - **Event system**: bus capacity for event delivery
/// - **Shutdown behavior**: grace period for graceful termination
/// - **Task defaults**: restart policy, backoff strategy, timeout
///
/// ## Field semantics
///
/// - `grace`: Maximum wait for tasks to stop gracefully (`0s` = no wait, force immediately)
/// - `bus_capacity`: Event bus ring buffer size (min 1; clamped by Bus)
/// - `backoff`: Default backoff strategy (can be overridden per-task)
/// - `restart`: Default restart policy (can be overridden per-task)
/// - `max_concurrent`: Task concurrency limit (`0` = unlimited)
/// - `timeout`: Default per-task timeout (`0s` = no timeout)
/// - `max_retries`: Default retry limit (`0` = unlimited)
///
/// ## Notes
///
/// All fields are public for flexibility.
/// Prefer using helper accessors to avoid sprinkling sentinel checks (`0`) across the codebase.
///
/// # Also
///
/// - `SupervisorBuilder` - consumes config to build a [`Supervisor`](crate::Supervisor)
/// - [`TaskSpec`](crate::TaskSpec) - inherits defaults from config via `with_defaults`
#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    /// Maximum time to wait for graceful shutdown before force-terminating.
    ///
    /// When a shutdown signal is received:
    /// - Tasks are cancelled via `CancellationToken`
    /// - Supervisor waits up to `grace` for tasks to exit
    /// - If timeout exceeds, returns `RuntimeError::GraceExceeded`
    pub grace: Duration,

    /// Maximum number of tasks to run concurrently.
    ///
    /// - `0` = unlimited (no semaphore)
    /// - `n > 0` = at most `n` tasks run simultaneously
    ///
    /// Applied globally across all tasks in the supervisor.
    pub max_concurrent: usize,

    /// Capacity of the event bus broadcast channel ring buffer.
    ///
    /// Slow subscribers that lag behind more than `bus_capacity` messages will receive `Lagged` and skip older items.
    /// Minimum value is 1 (enforced by Bus).
    pub bus_capacity: usize,

    /// Default restart policy for tasks.
    ///
    /// Used by `TaskSpec::with_defaults()`. Can be overridden per-task.
    pub restart: RestartPolicy,

    /// Default backoff policy for retries.
    ///
    /// Used by `TaskSpec::with_defaults()`. Can be overridden per-task.
    pub backoff: BackoffPolicy,

    /// Default task timeout.
    /// - `Duration::ZERO` = no timeout (task runs until completion)
    /// - `> 0` = timeout applied per task attempt
    ///
    /// Used by `TaskSpec::with_defaults()`. Can be overridden per-task.
    pub timeout: Duration,

    /// Default maximum number of retry attempts after failure.
    /// - `0` = unlimited retries (default)
    /// - `n > 0` = at most `n` retries after the initial failure
    ///
    /// Only counts failure-driven retries, not success-driven restarts
    /// *(e.g., `RestartPolicy::Always` after success does not consume retries)*.
    ///
    /// Used by `TaskSpec::with_defaults()`. Can be overridden per-task.
    pub max_retries: u32,
}

impl SupervisorConfig {
    /// Validates configuration parameters.
    ///
    /// Checks:
    /// - `bus_capacity > 0`
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.bus_capacity == 0 {
            return Err("bus_capacity must be > 0");
        }
        Ok(())
    }

    /// Returns the global concurrency limit as an `Option`.
    /// - `None` → unlimited (no semaphore)
    /// - `Some(n)` → at most `n` concurrent tasks
    #[inline]
    pub fn concurrency_limit(&self) -> Option<usize> {
        if self.max_concurrent == 0 {
            None
        } else {
            Some(self.max_concurrent)
        }
    }

    /// Returns the default per-task timeout as an `Option`.
    /// - `None` → no timeout
    /// - `Some(d)` → timeout applied per attempt
    #[inline]
    pub fn default_timeout(&self) -> Option<Duration> {
        if self.timeout == Duration::ZERO {
            None
        } else {
            Some(self.timeout)
        }
    }

    /// Returns a bus capacity clamped to a minimum of 1.
    ///
    /// The `Bus` should use this value to avoid constructing an invalid channel.
    #[inline]
    pub fn bus_capacity_clamped(&self) -> usize {
        self.bus_capacity.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_zero_bus_capacity() {
        let mut cfg = SupervisorConfig::default();
        cfg.bus_capacity = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_accepts_default() {
        assert!(SupervisorConfig::default().validate().is_ok());
    }

    #[test]
    fn concurrency_limit_zero_means_unlimited() {
        let cfg = SupervisorConfig {
            max_concurrent: 0,
            ..Default::default()
        };
        assert_eq!(cfg.concurrency_limit(), None);
    }

    #[test]
    fn concurrency_limit_nonzero() {
        let cfg = SupervisorConfig {
            max_concurrent: 4,
            ..Default::default()
        };
        assert_eq!(cfg.concurrency_limit(), Some(4));
    }

    #[test]
    fn default_timeout_zero_means_none() {
        let cfg = SupervisorConfig::default();
        assert_eq!(cfg.default_timeout(), None);
    }

    #[test]
    fn default_timeout_nonzero() {
        let cfg = SupervisorConfig {
            timeout: Duration::from_secs(30),
            ..Default::default()
        };
        assert_eq!(cfg.default_timeout(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn bus_capacity_clamped_never_zero() {
        let cfg = SupervisorConfig {
            bus_capacity: 0,
            ..Default::default()
        };
        assert_eq!(cfg.bus_capacity_clamped(), 1);
    }
}

impl Default for SupervisorConfig {
    /// Default configuration:
    /// - `grace = 60s` (reasonable graceful shutdown window)
    /// - `max_concurrent = 0` (unlimited)
    /// - `bus_capacity = 1024` (good baseline)
    /// - `timeout = 0s` (no timeout)
    /// - `restart = RestartPolicy::OnFailure` (restart on errors only)
    /// - `backoff = BackoffPolicy::default()` (constant 100ms, see [`BackoffPolicy`])
    /// - `max_retries = 0` (unlimited)
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(60),
            max_concurrent: 0,
            bus_capacity: 1024,
            timeout: Duration::from_secs(0),
            restart: RestartPolicy::default(),
            backoff: BackoffPolicy::default(),
            max_retries: 0,
        }
    }
}
