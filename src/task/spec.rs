//! # Task specification.
//!
//! This module defines [`TaskSpec`] — a configuration bundle that
//! describes how a task should be executed under supervision
//! (restart policy, backoff, timeout).

use std::fmt;
use std::time::Duration;

use crate::task::task::TaskRef;
use crate::{config::Config, policy::RestartPolicy, strategy::BackoffStrategy};

/// # Specification for running a task under supervision.
///
/// A [`TaskSpec`] bundles:
/// - the task itself ([`TaskRef`])
/// - restart policy ([`RestartPolicy`])
/// - backoff strategy ([`BackoffStrategy`])
/// - optional execution timeout
///
/// It can be created manually with [`TaskSpec::new`] or derived from a
/// global [`Config`] via [`TaskSpec::from_task`].
///
/// # Example
/// ```
/// use tokio_util::sync::CancellationToken;
/// use taskvisor::{TaskSpec, TaskFn, Config, RestartPolicy, BackoffStrategy, TaskRef};
///
/// let demo: TaskRef = TaskFn::arc("demo", |_ctx: CancellationToken| async move { Ok(()) });
///
/// // Build spec explicitly:
/// let spec = TaskSpec::new(
///     demo.clone(),
///     RestartPolicy::Never,
///     BackoffStrategy::default(),
///     None,
/// );
/// assert!(spec.timeout.is_none());
///
/// // Or derive from global config:
/// let cfg = Config::default();
/// let spec2 = TaskSpec::from_task(demo, &cfg);
/// ```
#[derive(Clone)]
pub struct TaskSpec {
    /// Reference to the task to be executed.
    pub task: TaskRef,
    /// Policy controlling if/when the task should be restarted.
    pub restart: RestartPolicy,
    /// Strategy controlling delays between restarts.
    pub backoff: BackoffStrategy,
    /// Optional timeout for the task execution.
    pub timeout: Option<Duration>,
}

impl TaskSpec {
    /// Creates a new task specification with explicit parameters.
    pub fn new(
        task: TaskRef,
        restart: RestartPolicy,
        backoff: BackoffStrategy,
        timeout: Option<Duration>,
    ) -> Self {
        Self {
            task,
            restart,
            backoff,
            timeout,
        }
    }

    /// Builds a task specification from a [`Config`], inheriting restart,
    /// backoff, and timeout settings from global configuration.
    pub fn from_task(task: TaskRef, cfg: &Config) -> Self {
        Self {
            task,
            restart: cfg.restart,
            backoff: cfg.backoff,
            timeout: (!cfg.timeout.is_zero()).then_some(cfg.timeout),
        }
    }
}

impl fmt::Debug for TaskSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskSpec")
            .field("task", &self.task.name())
            .field("restart", &self.restart)
            .field("backoff", &self.backoff)
            .field("timeout", &self.timeout)
            .finish()
    }
}
