//! # Global runtime configuration.
//!
//! [`SupervisorConfig`] provides centralized settings for the supervisor runtime.
//!
//! Config is used in two ways:
//! 1. **Supervisor creation**: `Supervisor::new(config, subscribers)`
//! 2. **TaskSpec defaults**: [`config.task_spec(task)`](SupervisorConfig::task_spec)
//!
//! ## Optional / normalized fields
//!
//! - `bus_capacity` is normalized to a minimum of `1` by [`bus_capacity_clamped`](SupervisorConfig::bus_capacity_clamped).
//! - `max_retries = 0` still means "unlimited" (a sentinel that a future change will turn into `Option`).
//! - `max_concurrent: None` → unlimited (no global semaphore); `Some(n)` → `n` permits.
//! - `timeout: None` → no per-task timeout; `Some(d)` → applied per attempt.

use std::num::NonZeroUsize;
use std::time::Duration;

use crate::policies::{BackoffPolicy, RestartPolicy};
use crate::tasks::{TaskRef, TaskSpec};

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
/// - `max_concurrent`: Task concurrency limit (`None` = unlimited)
/// - `timeout`: Default per-task timeout (`None` = no timeout)
/// - `max_retries`: Default retry limit (`0` = unlimited)
///
/// # Also
///
/// - [`SupervisorBuilder`](crate::SupervisorBuilder) - consumes config to build a [`Supervisor`](crate::Supervisor)
/// - [`TaskSpec`](crate::TaskSpec) - inherits defaults from config via [`task_spec`](SupervisorConfig::task_spec)
#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    /// Maximum time to wait for graceful shutdown before force-terminating.
    ///
    /// When a shutdown signal is received:
    /// - Tasks are cancelled via their `TaskContext`
    /// - Supervisor waits up to `grace` for tasks to exit
    /// - If timeout exceeds, returns `RuntimeError::GraceExceeded`
    pub grace: Duration,

    /// Global concurrency limit, applied across all tasks.
    ///
    /// - `None` = unlimited (no semaphore)
    /// - `Some(n)` = at most `n` tasks run simultaneously (`NonZeroUsize` makes a `0`-permit semaphore unrepresentable)
    pub max_concurrent: Option<NonZeroUsize>,

    /// Capacity of the event bus broadcast channel ring buffer.
    ///
    /// Slow subscribers that lag behind more than `bus_capacity` messages will receive `Lagged` and skip older items.
    /// Minimum value is 1 (enforced by Bus).
    pub bus_capacity: usize,

    /// Default restart policy for tasks.
    ///
    /// Used by [`task_spec`](SupervisorConfig::task_spec). Can be overridden per-task.
    pub restart: RestartPolicy,

    /// Default backoff policy for retries.
    ///
    /// Used by [`task_spec`](SupervisorConfig::task_spec). Can be overridden per-task.
    pub backoff: BackoffPolicy,

    /// Default per-task timeout.
    /// - `None` = no timeout (task runs until completion)
    /// - `Some(d)` with `d > 0` = timeout applied per attempt
    ///
    /// Note: `Some(Duration::ZERO)` is also treated as "no timeout" by the runner.
    ///
    /// Used by [`task_spec`](SupervisorConfig::task_spec). Can be overridden per-task.
    pub timeout: Option<Duration>,

    /// Default maximum number of retry attempts after failure.
    /// - `0` = unlimited retries (default)
    /// - `n > 0` = at most `n` retries after the initial failure
    ///
    /// Only counts failure-driven retries, not success-driven restarts
    /// (e.g., `RestartPolicy::Always` after success does not consume retries).
    ///
    /// Used by [`task_spec`](SupervisorConfig::task_spec). Can be overridden per-task.
    pub max_retries: u32,
}

impl SupervisorConfig {
    /// Returns a bus capacity clamped to a minimum of 1.
    ///
    /// The `Bus` should use this value to avoid constructing an invalid channel.
    #[inline]
    #[must_use]
    pub fn bus_capacity_clamped(&self) -> usize {
        self.bus_capacity.max(1)
    }

    /// Builds a [`TaskSpec`] for `task` that inherits this config's restart policy,
    /// backoff, timeout, and max-retries defaults.
    ///
    /// Lives on the config (not `TaskSpec`) so the `tasks` layer stays free of any
    /// dependency on the runtime `core` layer. Per-task overrides still compose:
    /// `cfg.task_spec(task).with_timeout(..)`.
    pub fn task_spec(&self, task: TaskRef) -> TaskSpec {
        TaskSpec::new(task, self.restart, self.backoff, self.timeout)
            .with_max_retries(self.max_retries)
    }
}

impl Default for SupervisorConfig {
    /// Default configuration:
    /// - `grace = 60s` (reasonable graceful shutdown window)
    /// - `max_concurrent = None` (unlimited)
    /// - `bus_capacity = 1024` (a reasonable default)
    /// - `timeout = None` (no timeout)
    /// - `restart = RestartPolicy::OnFailure` (restart on errors only)
    /// - `backoff = BackoffPolicy::default()` (constant 100ms, see [`BackoffPolicy`])
    /// - `max_retries = 0` (unlimited)
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(60),
            max_concurrent: None,
            bus_capacity: 1024,
            timeout: None,
            restart: RestartPolicy::default(),
            backoff: BackoffPolicy::default(),
            max_retries: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_no_concurrency_limit_or_timeout() {
        let cfg = SupervisorConfig::default();
        assert_eq!(cfg.max_concurrent, None, "default is unlimited concurrency");
        assert_eq!(cfg.timeout, None, "default has no per-task timeout");
    }

    #[test]
    fn optional_fields_round_trip() {
        let cfg = SupervisorConfig {
            max_concurrent: NonZeroUsize::new(4),
            timeout: Some(Duration::from_secs(30)),
            ..Default::default()
        };
        assert_eq!(cfg.max_concurrent.map(NonZeroUsize::get), Some(4));
        assert_eq!(cfg.timeout, Some(Duration::from_secs(30)));
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
