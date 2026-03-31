//! Task execution specification.

use std::time::Duration;

use crate::{
    core::SupervisorConfig, policies::BackoffPolicy, policies::RestartPolicy, tasks::task::TaskRef,
};

/// Describes *how* a [`Task`](crate::Task) should run under supervision.
///
/// Bundles restart policy, backoff strategy, timeout, and max retries.
///
/// - [`once`](Self::once) for fire-and-forget tasks.
/// - [`restartable`](Self::restartable) for long-lived workers.
/// - [`new`](Self::new) for full control over all parameters.
///
/// ## Creating a spec
/// ```rust
/// use tokio_util::sync::CancellationToken;
/// use taskvisor::{TaskSpec, TaskFn, SupervisorConfig, RestartPolicy, BackoffPolicy, TaskRef, TaskError};
/// use std::time::Duration;
///
/// let task: TaskRef = TaskFn::arc("demo", |_ctx: CancellationToken| async move {
///     Ok::<(), TaskError>(())
/// });
///
/// // One-shot (most common):
/// let spec = TaskSpec::once(task.clone());
///
/// // Restartable with builder chain:
/// let spec = TaskSpec::restartable(task.clone())
///     .with_timeout(Some(Duration::from_secs(30)))
///     .with_max_retries(5);
///
/// // Inherit from global config:
/// let cfg = SupervisorConfig::default();
/// let spec = TaskSpec::with_defaults(task, &cfg);
/// ```
///
/// ## Also
///
/// - See [`Task`](crate::Task) for the execution contract and cancellation semantics.
/// - For the closure-based implementation see [`TaskFn`](crate::TaskFn).
#[derive(Clone)]
#[must_use]
pub struct TaskSpec {
    timeout: Option<Duration>,
    restart: RestartPolicy,
    backoff: BackoffPolicy,

    task: TaskRef,

    max_retries: u32,
}

impl std::fmt::Debug for TaskSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskSpec")
            .field("restart", &self.restart)
            .field("backoff", &self.backoff)
            .field("timeout", &self.timeout)
            .field("task", &self.task.name())
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

impl TaskSpec {
    /// Creates a spec with core parameters.
    ///
    /// Sets `max_retries` to `0` (unlimited).
    /// Use [`.with_max_retries()`](Self::with_max_retries) to limit retry attempts.
    pub fn new(
        task: TaskRef,
        restart: RestartPolicy,
        backoff: BackoffPolicy,
        timeout: Option<Duration>,
    ) -> Self {
        Self {
            restart,
            backoff,
            timeout,

            task,

            max_retries: 0,
        }
    }

    /// One-shot: run once, never restart.
    pub fn once(task: TaskRef) -> Self {
        Self {
            restart: RestartPolicy::Never,
            backoff: BackoffPolicy::default(),
            timeout: None,

            task,

            max_retries: 0,
        }
    }

    /// Restartable: restart on failure with default backoff.
    pub fn restartable(task: TaskRef) -> Self {
        Self {
            restart: RestartPolicy::OnFailure,
            backoff: BackoffPolicy::default(),
            timeout: None,

            task,

            max_retries: 0,
        }
    }

    /// Inherit restart, backoff, timeout, and max_retries from global config.
    ///
    /// A config `timeout` of [`Duration::ZERO`] is treated as "no timeout" (`None`).
    pub fn with_defaults(task: TaskRef, cfg: &SupervisorConfig) -> Self {
        Self {
            restart: cfg.restart,
            backoff: cfg.backoff,
            timeout: cfg.default_timeout(),

            task,

            max_retries: cfg.max_retries,
        }
    }

    /// Returns reference to the task.
    pub fn task(&self) -> &TaskRef {
        &self.task
    }

    /// Returns the task name.
    pub fn name(&self) -> &str {
        self.task.name()
    }

    /// Returns the restart policy.
    pub fn restart(&self) -> RestartPolicy {
        self.restart
    }

    /// Returns the backoff policy.
    pub fn backoff(&self) -> BackoffPolicy {
        self.backoff
    }

    /// Returns the timeout, if configured.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Returns the maximum retry attempts (`0` = unlimited).
    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// Builder: set timeout.
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Builder: set backoff policy.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Builder: set restart policy.
    pub fn with_restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }

    /// Builder: set max retries (`0` = unlimited).
    ///
    /// Only counts failure-driven retries, not success-driven restarts.
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }
}
