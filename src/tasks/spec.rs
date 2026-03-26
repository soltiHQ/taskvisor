//! # Task specification for supervised execution.
//!
//! Defines [`TaskSpec`] a configuration bundle that describes how a task
//! should be executed under supervision (restart policy, backoff, timeout).
//!
//! A spec can be created:
//! - **Quick**: [`TaskSpec::once`] (run once, never restart) or [`TaskSpec::restartable`] (restart on failure)
//! - **Explicit**: [`TaskSpec::new`] (full control over all parameters)
//! - **From config**: [`TaskSpec::with_defaults`] (inherit defaults from [`SupervisorConfig`])
//!
//! ## Rules
//! - The spec is then passed to [`Supervisor::run`](crate::Supervisor::run) for execution.

use std::time::Duration;

use crate::{
    core::SupervisorConfig, policies::BackoffPolicy, policies::RestartPolicy, tasks::task::TaskRef,
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
/// global [`SupervisorConfig`] via [`TaskSpec::with_defaults`].
///
/// ## Example
/// ```rust
/// use tokio_util::sync::CancellationToken;
/// use taskvisor::{TaskSpec, TaskFn, SupervisorConfig, RestartPolicy, BackoffPolicy, TaskRef, TaskError};
/// use std::time::Duration;
///
/// let demo: TaskRef = TaskFn::arc("demo", |_ctx: CancellationToken| async move {
///     Ok::<(), TaskError>(())
/// });
///
/// // Quick one-shot (most common):
/// let spec = TaskSpec::once(demo.clone());
///
/// // Restartable with defaults:
/// let spec = TaskSpec::restartable(demo.clone());
///
/// // Restartable with custom timeout (builder chain):
/// let spec = TaskSpec::restartable(demo.clone())
///     .with_timeout(Some(Duration::from_secs(30)));
///
/// // Full control:
/// let spec = TaskSpec::new(
///     demo.clone(),
///     RestartPolicy::Always { interval: None },
///     BackoffPolicy::default(),
///     Some(Duration::from_secs(5)),
/// );
///
/// // Inherit from global config:
/// let cfg = SupervisorConfig::default();
/// let spec = TaskSpec::with_defaults(demo, &cfg);
/// ```
#[derive(Clone)]
pub struct TaskSpec {
    task: TaskRef,
    restart: RestartPolicy,
    backoff: BackoffPolicy,
    timeout: Option<Duration>,
}

impl std::fmt::Debug for TaskSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskSpec")
            .field("task", &self.task.name())
            .field("restart", &self.restart)
            .field("backoff", &self.backoff)
            .field("timeout", &self.timeout)
            .finish()
    }
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

    /// Creates a one-shot task that runs once and never restarts.
    ///
    /// Equivalent to `TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)`.
    ///
    /// Use builder methods (`.with_timeout()`, `.with_backoff()`) to customize further.
    pub fn once(task: TaskRef) -> Self {
        Self {
            task,
            restart: RestartPolicy::Never,
            backoff: BackoffPolicy::default(),
            timeout: None,
        }
    }

    /// Creates a restartable task that restarts on failure with default backoff.
    ///
    /// Equivalent to `TaskSpec::new(task, RestartPolicy::OnFailure, BackoffPolicy::default(), None)`.
    ///
    /// Use builder methods (`.with_timeout()`, `.with_backoff()`, `.with_restart()`) to customize further.
    pub fn restartable(task: TaskRef) -> Self {
        Self {
            task,
            restart: RestartPolicy::OnFailure,
            backoff: BackoffPolicy::default(),
            timeout: None,
        }
    }

    /// Creates a task specification inheriting defaults from global config.
    ///
    /// Uses `SupervisorConfig::default_timeout()` so that `0s` in config is treated as `None`.
    ///
    /// ### Parameters
    /// - `task`: Task to execute
    /// - `cfg`: Config to inherit restart/backoff/timeout from
    pub fn with_defaults(task: TaskRef, cfg: &SupervisorConfig) -> Self {
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
