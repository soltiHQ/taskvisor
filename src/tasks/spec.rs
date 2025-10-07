//! # Task specification for supervised execution.
//!
//! Defines [`TaskSpec`] a configuration bundle that describes how a task
//! should be executed under supervision (restart policy, backoff, timeout).
//!
//! A spec can be created:
//! - **Explicitly** with [`TaskSpec::new`] (full control)
//! - **From config** with [`TaskSpec::with_defaults`] (inherit defaults)
//!
//! ## Rules
//! - The spec is then passed to [`Supervisor::run`](crate::Supervisor::run) for execution.

use std::time::Duration;

use crate::{
    config::Config, policies::BackoffPolicy, policies::RestartPolicy, tasks::task::TaskRef,
};

/// Specification for running a task under supervision.
///
/// Bundles together:
/// - The task itself ([`TaskRef`])
/// - Restart policy ([`RestartPolicy`])
/// - Backoff policy ([`BackoffPolicy`])
/// - Optional execution timeout
///
/// It can be created manually with [`TaskSpec::new`] or derived from a
/// global [`Config`] via [`TaskSpec::with_defaults`].
///
/// ## Example
/// ```rust
/// use tokio_util::sync::CancellationToken;
/// use taskvisor::{TaskSpec, TaskFn, Config, RestartPolicy, BackoffPolicy, TaskRef, TaskError};
/// use std::time::Duration;
///
/// let demo: TaskRef = TaskFn::arc("demo", |_ctx: CancellationToken| async move {
///     Ok::<(), TaskError>(())
/// });
///
/// // Explicit configuration:
/// let spec = TaskSpec::new(
///     demo.clone(),
///     RestartPolicy::Never,
///     BackoffPolicy::default(),
///     None,
/// );
/// assert!(spec.timeout().is_none());
///
/// // Inherit from global config:
/// let cfg = Config::default();
/// let spec2 = TaskSpec::with_defaults(demo, &cfg);
/// // `cfg.timeout = 0s` is treated as `None`
/// ```
#[derive(Clone)]
pub struct TaskSpec {
    task: TaskRef,
    restart: RestartPolicy,
    backoff: BackoffPolicy,
    timeout: Option<Duration>,
}

impl TaskSpec {
    /// Creates a new task specification with explicit parameters.
    ///
    /// ### Parameters
    /// - `task`: Task to execute
    /// - `restart`: When to restart (never/always/on-failure)
    /// - `backoff`: How to delay between retries
    /// - `timeout`: Optional per-attempt timeout (`None` = no timeout)
    pub fn new(
        task: TaskRef,
        restart: RestartPolicy,
        backoff: BackoffPolicy,
        timeout: Option<Duration>,
    ) -> Self {
        Self {
            task,
            restart,
            backoff,
            timeout,
        }
    }

    /// Creates a task specification inheriting defaults from global config.
    ///
    /// Uses `Config::default_timeout()` so that `0s` in config is treated as `None`.
    ///
    /// ### Parameters
    /// - `task`: Task to execute
    /// - `cfg`: Config to inherit restart/backoff/timeout from
    pub fn with_defaults(task: TaskRef, cfg: &Config) -> Self {
        Self {
            task,
            restart: cfg.restart,
            backoff: cfg.backoff,
            timeout: cfg.default_timeout(),
        }
    }

    /// Returns reference to the task.
    pub fn task(&self) -> &TaskRef {
        &self.task
    }

    /// Convenience: returns the task name.
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

    /// Returns a new spec with updated timeout.
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Returns a new spec with updated backoff.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Returns a new spec with updated restart policy.
    pub fn with_restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }
}
