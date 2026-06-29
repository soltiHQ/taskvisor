//! Task execution specification.

use std::num::NonZeroU32;
use std::time::Duration;

use crate::{policies::BackoffPolicy, policies::RestartPolicy, tasks::task::TaskRef};

/// Converts `Some(Duration::ZERO)` to `None`.
#[inline]
fn normalize_timeout(timeout: Option<Duration>) -> Option<Duration> {
    timeout.filter(|d| !d.is_zero())
}

/// Describes how a [`Task`](crate::Task) should run.
///
/// A `TaskSpec` combines:
/// - the task itself,
/// - restart policy,
/// - backoff policy,
/// - optional timeout,
/// - retry limit.
///
/// Use:
/// - [`restartable`](Self::restartable) for tasks that restart after failure.
/// - [`new`](Self::new) when you want to set all main options directly.
/// - [`once`](Self::once) for tasks that run once and do not restart.
///
/// ## Creating a spec
/// ```rust
/// use taskvisor::TaskContext;
/// use taskvisor::{TaskSpec, TaskFn, SupervisorConfig, RestartPolicy, BackoffPolicy, TaskRef, TaskError};
/// use std::num::NonZeroU32;
/// use std::time::Duration;
///
/// let task: TaskRef = TaskFn::arc("demo", |_ctx: TaskContext| async move {
///     Ok::<(), TaskError>(())
/// });
///
/// // One-shot (most common):
/// let spec = TaskSpec::once(task.clone());
///
/// // Restartable with builder chain:
/// let spec = TaskSpec::restartable(task.clone())
///     .with_timeout(Some(Duration::from_secs(30)))
///     .with_max_retries(NonZeroU32::new(5).unwrap());
///
/// // Inherit from global config:
/// let cfg = SupervisorConfig::default();
/// let spec = cfg.task_spec(task);
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

    max_retries: Option<NonZeroU32>,
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
    /// Creates a spec with custom restart, backoff, and timeout settings.
    pub fn new(
        task: TaskRef,
        restart: RestartPolicy,
        backoff: BackoffPolicy,
        timeout: Option<Duration>,
    ) -> Self {
        Self {
            restart,
            backoff,
            task,

            max_retries: None,
            timeout: normalize_timeout(timeout),
        }
    }

    /// One-shot: run once, never restart.
    pub fn once(task: TaskRef) -> Self {
        Self {
            backoff: BackoffPolicy::default(),
            restart: RestartPolicy::Never,
            max_retries: None,
            timeout: None,
            task,
        }
    }

    /// Restartable: restart on failure with default backoff.
    pub fn restartable(task: TaskRef) -> Self {
        Self {
            restart: RestartPolicy::OnFailure,
            backoff: BackoffPolicy::default(),
            timeout: None,

            task,

            max_retries: None,
        }
    }

    /// Returns the task handle.
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

    /// Returns the failure-retry limit (`None` = unlimited).
    pub fn max_retries(&self) -> Option<NonZeroU32> {
        self.max_retries
    }

    /// Builder: sets the timeout.
    ///
    /// - Stored `Some(d)` is always a positive duration.
    /// - `Some(Duration::ZERO)` is normalized to `None`.
    /// - `None` means no timeout.
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = normalize_timeout(timeout);
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

    /// Builder: set the failure-retry limit (`None` = unlimited).
    ///
    /// Accepts a `NonZeroU32` (a limit) or an `Option<NonZeroU32>` (`None` = unlimited).
    pub fn with_max_retries(mut self, max_retries: impl Into<Option<NonZeroU32>>) -> Self {
        self.max_retries = max_retries.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TaskContext, TaskFn};

    fn task(name: &str) -> TaskRef {
        TaskFn::arc(name, |_ctx: TaskContext| async { Ok(()) })
    }

    #[test]
    fn zero_timeout_normalizes_to_none() {
        let via_builder = TaskSpec::once(task("z")).with_timeout(Some(Duration::ZERO));
        assert_eq!(
            via_builder.timeout(),
            None,
            "with_timeout(Some(ZERO)) must normalize to None"
        );

        let via_new = TaskSpec::new(
            task("z"),
            RestartPolicy::Never,
            BackoffPolicy::default(),
            Some(Duration::ZERO),
        );
        assert_eq!(
            via_new.timeout(),
            None,
            "new(.., Some(ZERO)) must normalize to None"
        );

        let positive = TaskSpec::once(task("p")).with_timeout(Some(Duration::from_secs(1)));
        assert_eq!(
            positive.timeout(),
            Some(Duration::from_secs(1)),
            "a positive timeout must be preserved"
        );
    }
}
