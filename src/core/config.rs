//! # Supervisor configuration.
//!
//! [`SupervisorConfig`] stores runtime defaults and limits.
//!
//! It is used in two places:
//! - to build a [`Supervisor`](crate::Supervisor),
//! - to create [`TaskSpec`] values with [`task_spec`](SupervisorConfig::task_spec).
//!
//! ## Optional Fields
//!
//! - `max_concurrent: None` means no global concurrency limit.
//! - `timeout: None` means no default per-attempt timeout.
//! - `max_retries: None` means unlimited failure retries.
//!
//! ## Normalized Fields
//!
//! `bus_capacity` and `registry_queue_capacity` are normalized to at least `1`
//! before their channels are created.

use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use crate::policies::{BackoffPolicy, RestartPolicy};
use crate::tasks::{TaskRef, TaskSpec};

/// Global configuration for the supervisor runtime.
///
/// The task defaults are applied only when a task spec is created through [`task_spec`](Self::task_spec).
/// A manually built [`TaskSpec`] keeps its own values.
#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    /// Maximum time to wait for tasks to stop during graceful shutdown.
    ///
    /// During shutdown, tasks receive cancellation through their [`TaskContext`](crate::TaskContext).
    /// The supervisor waits up to `grace` for them to exit.
    ///
    /// If some tasks still do not stop, shutdown returns [`RuntimeError::GraceExceeded`](crate::RuntimeError::GraceExceeded).
    ///
    /// `Duration::ZERO` means there is no graceful wait.
    pub grace: Duration,

    /// Global limit for concurrently running task attempts.
    ///
    /// - `None` means unlimited concurrency.
    /// - `Some(n)` means at most `n` attempts may run at the same time.
    ///
    /// `NonZeroUsize` makes a zero-permit semaphore impossible to represent.
    pub max_concurrent: Option<NonZeroUsize>,

    /// Capacity of the runtime event bus.
    ///
    /// The bus uses a broadcast ring buffer.
    /// Slow receivers that fall behind by more than this capacity may skip older events and observe lag.
    ///
    /// Use [`bus_capacity_clamped`](Self::bus_capacity_clamped) when creating the bus; the effective capacity is at least `1`.
    pub bus_capacity: usize,

    /// Capacity of the registry management command queue.
    ///
    /// This bounds pending add and remove commands.
    /// The registry may process one additional command while this many commands are buffered.
    ///
    /// Management methods that do not wait for queue capacity fail with [`RuntimeError::CommandQueueFull`](crate::RuntimeError::CommandQueueFull) when the queue has no capacity.
    ///
    /// Use [`registry_queue_capacity_clamped`](Self::registry_queue_capacity_clamped) when creating the queue; the effective capacity is at least `1`.
    pub registry_queue_capacity: usize,

    /// Default restart policy for tasks created with [`task_spec`](Self::task_spec).
    ///
    /// Individual [`TaskSpec`] values may override this.
    pub restart: RestartPolicy,

    /// Default backoff policy for retryable failures.
    ///
    /// Individual [`TaskSpec`] values may override this.
    pub backoff: BackoffPolicy,

    /// Default timeout for one task attempt.
    ///
    /// - `None` means no default timeout.
    /// - `Some(d)` means each attempt may run for at most `d`.
    ///
    /// When converted through [`task_spec`](Self::task_spec), `Some(Duration::ZERO)` is normalized to `None`.
    pub timeout: Option<Duration>,

    /// Default failure-retry limit.
    ///
    /// - `None` means unlimited retries.
    /// - `Some(n)` means at most `n` retries after the first failed attempt.
    ///
    /// This counts only failure-driven retries.
    /// Successful restarts from [`RestartPolicy::Always`] do not consume this budget.
    pub max_retries: Option<NonZeroU32>,
}

impl SupervisorConfig {
    /// Returns the effective bus capacity.
    ///
    /// The returned value is always at least `1`.
    #[inline]
    #[must_use]
    pub fn bus_capacity_clamped(&self) -> usize {
        self.bus_capacity.max(1)
    }

    /// Returns the effective registry command queue capacity.
    ///
    /// The returned value is always at least `1`.
    #[inline]
    #[must_use]
    pub fn registry_queue_capacity_clamped(&self) -> usize {
        self.registry_queue_capacity.max(1)
    }

    /// Builds a [`TaskSpec`] that inherits this config's task defaults.
    ///
    /// The created spec receives:
    /// - [`restart`](Self::restart),
    /// - [`backoff`](Self::backoff),
    /// - [`timeout`](Self::timeout),
    /// - [`max_retries`](Self::max_retries).
    ///
    /// Per-task overrides still compose:
    ///
    /// ```rust
    /// # use std::time::Duration;
    /// # use taskvisor::{SupervisorConfig, TaskContext, TaskFn, TaskRef, TaskError};
    /// let task: TaskRef = TaskFn::arc("worker", |_ctx| async {
    ///     Ok(())
    /// });
    ///
    /// let spec = SupervisorConfig::default()
    ///     .task_spec(task)
    ///     .with_timeout(Some(Duration::from_secs(10)));
    /// ```
    pub fn task_spec(&self, task: TaskRef) -> TaskSpec {
        TaskSpec::new(task, self.restart, self.backoff, self.timeout)
            .with_max_retries(self.max_retries)
    }
}

impl Default for SupervisorConfig {
    /// Returns the default runtime configuration.
    ///
    /// Defaults:
    /// - `grace = 60s`
    /// - `max_concurrent = None`
    /// - `bus_capacity = 1024`
    /// - `registry_queue_capacity = 1024`
    /// - `restart = RestartPolicy::OnFailure`
    /// - `backoff = BackoffPolicy::default()`
    /// - `timeout = None`
    /// - `max_retries = None`
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(60),
            max_concurrent: None,
            bus_capacity: 1024,
            registry_queue_capacity: 1024,
            timeout: None,
            restart: RestartPolicy::default(),
            backoff: BackoffPolicy::default(),
            max_retries: None,
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
        assert_eq!(cfg.registry_queue_capacity, 1024);
    }

    #[test]
    fn optional_fields_round_trip() {
        let cfg = SupervisorConfig {
            max_concurrent: NonZeroUsize::new(4),
            timeout: Some(Duration::from_secs(30)),
            max_retries: NonZeroU32::new(5),
            ..Default::default()
        };
        assert_eq!(cfg.max_concurrent.map(NonZeroUsize::get), Some(4));
        assert_eq!(cfg.timeout, Some(Duration::from_secs(30)));
        assert_eq!(cfg.max_retries.map(NonZeroU32::get), Some(5));
    }

    #[test]
    fn bus_capacity_clamped_never_zero() {
        let cfg = SupervisorConfig {
            bus_capacity: 0,
            ..Default::default()
        };
        assert_eq!(cfg.bus_capacity_clamped(), 1);
    }

    #[test]
    fn registry_queue_capacity_clamped_never_zero() {
        let cfg = SupervisorConfig {
            registry_queue_capacity: 0,
            ..Default::default()
        };
        assert_eq!(cfg.registry_queue_capacity_clamped(), 1);
    }

    #[test]
    fn task_spec_carries_config_defaults() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let task: TaskRef = TaskFn::arc("bridge", |_ctx: TaskContext| async { Ok(()) });
        let cfg = SupervisorConfig {
            max_retries: NonZeroU32::new(7),
            timeout: Some(Duration::from_secs(3)),
            ..Default::default()
        };

        let spec = cfg.task_spec(task);
        assert_eq!(
            spec.max_retries(),
            NonZeroU32::new(7),
            "task_spec must bridge max_retries from config into the spec"
        );
        assert_eq!(
            spec.timeout(),
            Some(Duration::from_secs(3)),
            "task_spec must bridge timeout from config into the spec"
        );
    }
}
