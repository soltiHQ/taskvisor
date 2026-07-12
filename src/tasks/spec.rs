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
/// - optional timeout,
/// - the task itself,
/// - restart policy,
/// - backoff policy,
/// - retry limit.
///
/// Use:
/// - [`restartable`](Self::restartable) for tasks that restart after failure.
/// - [`periodic`](Self::periodic) for tasks that repeat on a fixed interval.
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
/// let task: TaskRef = TaskFn::arc("demo", |_ctx| async move {
///     Ok(())
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
    /// Creates a spec with explicit restart, backoff, and timeout settings.
    ///
    /// Prefer the named constructors for common cases:
    /// [`once`](Self::once), [`restartable`](Self::restartable), [`periodic`](Self::periodic).
    ///
    /// A `Some(Duration::ZERO)` timeout is stored as `None` (no timeout).
    /// The retry limit starts unlimited.
    /// Change it with [`with_max_retries`](Self::with_max_retries).
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
    ///
    /// The task runs a single attempt.
    /// It does not restart, even after a failure.
    ///
    /// There is no timeout by default.
    /// Add one with [`with_timeout`](Self::with_timeout).
    pub fn once(task: TaskRef) -> Self {
        Self {
            backoff: BackoffPolicy::default(),
            restart: RestartPolicy::Never,
            max_retries: None,
            timeout: None,
            task,
        }
    }

    /// Restartable: restart on failure, stop on success.
    ///
    /// On failure the task restarts after the default [`BackoffPolicy`] delay.
    /// Retries are unlimited by default.
    /// Limit them with [`with_max_retries`](Self::with_max_retries).
    pub fn restartable(task: TaskRef) -> Self {
        Self {
            restart: RestartPolicy::OnFailure,
            backoff: BackoffPolicy::default(),
            timeout: None,

            task,

            max_retries: None,
        }
    }

    /// Periodic: run, wait `every`, run again. Forever.
    ///
    /// The task restarts after both success and failure.
    /// On failure the default [`BackoffPolicy`] delay applies first.
    /// A zero `every` means restart immediately.
    ///
    /// The interval starts after the task completes.
    /// This is not a wall-clock schedule (no "daily at 03:00").
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef, TaskSpec};
    ///
    /// let tick: TaskRef = TaskFn::arc("tick", |_ctx| async move {
    ///     println!("tick");
    ///     Ok(())
    /// });
    ///
    /// // Runs every 30 seconds until shutdown.
    /// let spec = TaskSpec::periodic(tick, Duration::from_secs(30));
    /// ```
    pub fn periodic(task: TaskRef, every: Duration) -> Self {
        Self {
            restart: RestartPolicy::Always {
                interval: Some(every).filter(|d| !d.is_zero()),
            },
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

    /// Builder: sets the backoff policy.
    ///
    /// Backoff controls the delay before a failed attempt restarts.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Builder: sets the restart policy.
    ///
    /// Restart controls whether the task runs again after it exits.
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
    fn periodic_sets_always_restart_with_interval() {
        let every = Duration::from_secs(30);
        let spec = TaskSpec::periodic(task("tick"), every);

        assert!(
            matches!(spec.restart(), RestartPolicy::Always { interval: Some(d) } if d == every),
            "periodic must set RestartPolicy::Always with the given interval, got {:?}",
            spec.restart()
        );
        assert_eq!(spec.timeout(), None, "periodic must not set a timeout");
        assert_eq!(
            spec.max_retries(),
            None,
            "periodic must not limit retries by default"
        );
        assert_eq!(spec.backoff().first(), Duration::from_millis(200));
        assert_eq!(spec.backoff().factor(), 2.0);
        assert_eq!(spec.backoff().jitter(), crate::JitterPolicy::Equal);
    }

    #[test]
    fn named_constructors_use_safe_backoff_defaults() {
        let once = TaskSpec::once(task("once"));
        let restartable = TaskSpec::restartable(task("restartable"));

        assert!(matches!(once.restart(), RestartPolicy::Never));
        assert!(matches!(restartable.restart(), RestartPolicy::OnFailure));
        for spec in [once, restartable] {
            assert_eq!(spec.backoff().first(), Duration::from_millis(200));
            assert_eq!(spec.backoff().factor(), 2.0);
            assert_eq!(spec.backoff().max(), Duration::from_secs(30));
            assert_eq!(spec.backoff().jitter(), crate::JitterPolicy::Equal);
            assert_eq!(spec.timeout(), None);
            assert_eq!(spec.max_retries(), None);
        }
    }

    #[test]
    fn periodic_zero_interval_normalizes_to_immediate_restart() {
        let spec = TaskSpec::periodic(task("tick"), Duration::ZERO);

        assert!(
            matches!(spec.restart(), RestartPolicy::Always { interval: None }),
            "a zero interval must normalize to None (immediate restart), got {:?}",
            spec.restart()
        );
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
