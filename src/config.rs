//! # Global runtime configuration.
//!
//! Provides [`Config`] — centralized settings for the supervisor runtime.
//!
//! Config is used in two ways:
//! 1. **Supervisor creation**: `Supervisor::new(config, subscribers)`
//! 2. **TaskSpec defaults**: `TaskSpec::with_defaults(task, &config)`

use std::time::Duration;

use crate::policies::{BackoffPolicy, RestartPolicy};

/// Global configuration for the supervisor runtime.
///
/// Defines:
/// - **Shutdown behavior**: grace period for graceful termination
/// - **Concurrency limits**: max simultaneous tasks
/// - **Event system**: bus capacity for event delivery
/// - **Task defaults**: restart policy, backoff strategy, timeout
///
/// ## Field semantics
/// - `grace`: Maximum wait for tasks to stop gracefully (0 = no wait, force immediately)
/// - `max_concurrent`: Task concurrency limit (0 = unlimited)
/// - `bus_capacity`: Event bus ring buffer size (min 1, enforced by Bus)
/// - `timeout`: Default per-task timeout (0 = no timeout)
/// - `restart`: Default restart policy (can be overridden per-task)
/// - `backoff`: Default backoff strategy (can be overridden per-task)
///
/// ## Notes
/// All fields are public for flexibility. Ensure:
/// - `grace > Duration::ZERO` for graceful shutdown (0 = force kill)
/// - `bus_capacity >= 1` (enforced by Bus constructor)
/// - `timeout = Duration::ZERO` means no timeout (checked at runtime)
#[derive(Clone, Debug)]
pub struct Config {
    /// Maximum time to wait for graceful shutdown before force-terminating.
    ///
    /// When shutdown signal received:
    /// - Tasks are cancelled via CancellationToken
    /// - Supervisor waits up to `grace` for tasks to exit
    /// - If timeout exceeded, returns `RuntimeError::GraceExceeded`
    pub grace: Duration,

    /// Maximum number of tasks to run concurrently.
    ///
    /// - `0` = unlimited (no semaphore)
    /// - `n > 0` = at most n tasks run simultaneously
    ///
    /// Applied globally across all tasks in the supervisor.
    pub max_concurrent: usize,

    /// Capacity of the event bus broadcast channel.
    ///
    /// When capacity exceeded, oldest events are dropped for slow subscribers.
    /// Minimum value is 1 (enforced by Bus).
    ///
    /// Applied globally across all tasks in the supervisor.
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
    ///
    /// - `Duration::ZERO` = no timeout (task runs until completion)
    /// - `> 0` = timeout applied per task attempt
    ///
    /// Used by `TaskSpec::with_defaults()`. Can be overridden per-task.
    pub timeout: Duration,
}

impl Default for Config {
    /// Provides a default configuration:
    ///
    /// - `grace = 30s` (reasonable graceful shutdown window)
    /// - `max_concurrent = 0` (unlimited)
    /// - `bus_capacity = 1024` (sufficient for most use cases)
    /// - `timeout = 0s` (no timeout)
    /// - `restart = RestartPolicy::OnFailure` (restart on errors only)
    /// - `backoff = BackoffPolicy::default()` (exponential backoff)
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(30),
            max_concurrent: 0,
            bus_capacity: 1024,
            timeout: Duration::from_secs(0),
            restart: RestartPolicy::default(),
            backoff: BackoffPolicy::default(),
        }
    }
}
