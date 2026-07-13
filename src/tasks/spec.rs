//! Task execution specification.

use std::num::NonZeroU32;
use std::time::Duration;

use crate::{
    core::{ConfigError, TaskDefaults},
    policies::BackoffPolicy,
    policies::RestartPolicy,
    tasks::task::TaskRef,
};

/// Converts `Some(Duration::ZERO)` to `None`.
#[inline]
fn normalize_timeout(timeout: Option<Duration>) -> Option<Duration> {
    timeout.filter(|d| !d.is_zero())
}

/// Describes how a [`Task`](crate::Task) should run.
///
/// A `TaskSpec` combines a task with explicit settings and settings inherited
/// from [`TaskDefaults`].
///
/// Use:
/// - [`from_defaults`](Self::from_defaults) when all execution settings should come from the supervisor.
/// - [`restartable`](Self::restartable) for tasks that restart after failure.
/// - [`periodic`](Self::periodic) for tasks that repeat on a fixed interval.
/// - [`new`](Self::new) when you want to set all main options directly.
/// - [`once`](Self::once) for tasks that run once and do not restart.
///
/// ## Creating a spec
/// ```rust
/// use taskvisor::TaskContext;
/// use taskvisor::{TaskSpec, TaskFn, RestartPolicy, BackoffPolicy, TaskRef, TaskError};
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
///     .with_timeout(Duration::from_secs(30))
///     .with_max_retries(NonZeroU32::new(5).unwrap());
///
/// // Named constructors inherit settings that they do not set:
/// let spec = TaskSpec::restartable(task);
/// ```
///
/// ## Also
///
/// - See [`Task`](crate::Task) for the execution contract and cancellation semantics.
/// - For the closure-based implementation see [`TaskFn`](crate::TaskFn).
#[derive(Clone)]
#[must_use]
pub struct TaskSpec {
    restart: Override<RestartPolicy>,
    backoff: Override<BackoffPolicy>,
    timeout: Override<Option<Duration>>,
    max_retries: Override<Option<NonZeroU32>>,

    task: TaskRef,
}

#[derive(Clone, Copy, Debug)]
enum Override<T> {
    Inherit,
    Set(T),
}

impl<T: Copy> Override<T> {
    #[inline]
    fn value(self) -> Option<T> {
        match self {
            Self::Inherit => None,
            Self::Set(value) => Some(value),
        }
    }

    #[inline]
    fn resolve(self, default: T) -> T {
        match self {
            Self::Inherit => default,
            Self::Set(value) => value,
        }
    }
}

/// A task specification after all inherited settings have been applied.
#[derive(Clone)]
#[must_use]
pub(crate) struct ResolvedTaskSpec {
    restart: RestartPolicy,
    backoff: BackoffPolicy,
    timeout: Option<Duration>,
    max_retries: Option<NonZeroU32>,
    task: TaskRef,
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

impl std::fmt::Debug for ResolvedTaskSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedTaskSpec")
            .field("restart", &self.restart)
            .field("backoff", &self.backoff)
            .field("timeout", &self.timeout)
            .field("task", &self.task.name())
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

impl TaskSpec {
    /// Creates a task specification that inherits every execution setting.
    ///
    /// The admitting supervisor resolves restart, backoff, timeout, and retry
    /// limit from its [`TaskDefaults`]. Any later `with_*` call overrides one
    /// inherited value.
    pub fn from_defaults(task: TaskRef) -> Self {
        Self {
            restart: Override::Inherit,
            backoff: Override::Inherit,
            timeout: Override::Inherit,
            max_retries: Override::Inherit,
            task,
        }
    }

    /// Creates a spec with explicit restart, backoff, and timeout settings.
    ///
    /// Prefer the named constructors for common cases:
    /// [`once`](Self::once), [`restartable`](Self::restartable), [`periodic`](Self::periodic).
    ///
    /// Accepts either a [`Duration`] or an `Option<Duration>`; pass either form
    /// directly without calling `.into()`.
    /// `Duration::ZERO` and `Some(Duration::ZERO)` are stored as `None` (no timeout).
    /// The retry limit is explicitly set to unlimited. Change it with
    /// [`with_max_retries`](Self::with_max_retries).
    pub fn new(
        task: TaskRef,
        restart: RestartPolicy,
        backoff: BackoffPolicy,
        timeout: impl Into<Option<Duration>>,
    ) -> Self {
        Self {
            restart: Override::Set(restart),
            backoff: Override::Set(backoff),
            timeout: Override::Set(normalize_timeout(timeout.into())),
            max_retries: Override::Set(None),
            task,
        }
    }

    /// One-shot: run once, never restart.
    ///
    /// The task runs a single attempt.
    /// It does not restart, even after a failure.
    ///
    /// Backoff, timeout, and retry limit are inherited from [`TaskDefaults`].
    /// Override them with the matching `with_*` methods.
    pub fn once(task: TaskRef) -> Self {
        Self {
            restart: Override::Set(RestartPolicy::Never),
            backoff: Override::Inherit,
            timeout: Override::Inherit,
            max_retries: Override::Inherit,
            task,
        }
    }

    /// Restartable: restart on failure, stop on success.
    ///
    /// Backoff, timeout, and retry limit are inherited from [`TaskDefaults`].
    /// Override them with the matching `with_*` methods.
    pub fn restartable(task: TaskRef) -> Self {
        Self {
            restart: Override::Set(RestartPolicy::OnFailure),
            backoff: Override::Inherit,
            timeout: Override::Inherit,
            max_retries: Override::Inherit,
            task,
        }
    }

    /// Periodic: run, wait `every`, run again. Forever.
    ///
    /// The task restarts after both success and failure.
    /// On failure the inherited backoff delay applies first.
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
            restart: Override::Set(RestartPolicy::Always {
                interval: Some(every).filter(|d| !d.is_zero()),
            }),
            backoff: Override::Inherit,
            timeout: Override::Inherit,
            max_retries: Override::Inherit,
            task,
        }
    }

    /// Returns the task handle.
    #[must_use]
    pub fn task(&self) -> &TaskRef {
        &self.task
    }

    /// Returns the task name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.task.name()
    }

    /// Returns the explicit restart policy, or `None` when it is inherited.
    #[must_use]
    pub fn restart_override(&self) -> Option<RestartPolicy> {
        self.restart.value()
    }

    /// Returns the explicit backoff policy, or `None` when it is inherited.
    #[must_use]
    pub fn backoff_override(&self) -> Option<BackoffPolicy> {
        self.backoff.value()
    }

    /// Returns the explicit timeout override.
    ///
    /// - `None` means inherit the default.
    /// - `Some(None)` means explicitly disable the timeout.
    /// - `Some(Some(duration))` means use that timeout.
    #[must_use]
    pub fn timeout_override(&self) -> Option<Option<Duration>> {
        self.timeout.value()
    }

    /// Returns the explicit failure-retry limit override.
    ///
    /// - `None` means inherit the default.
    /// - `Some(None)` means explicitly allow unlimited failure retries.
    /// - `Some(Some(limit))` means use that retry limit.
    #[must_use]
    pub fn max_retries_override(&self) -> Option<Option<NonZeroU32>> {
        self.max_retries.value()
    }

    /// Builder: sets the timeout.
    ///
    /// Accepts either a `Duration` or an `Option<Duration>`; pass either form
    /// directly without calling `.into()`.
    /// - Stored `Some(d)` is always a positive duration.
    /// - `Duration::ZERO` and `Some(Duration::ZERO)` are normalized to `None`.
    /// - `None` means explicitly disable an inherited timeout.
    pub fn with_timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.timeout = Override::Set(normalize_timeout(timeout.into()));
        self
    }

    /// Builder: sets the backoff policy.
    ///
    /// Backoff controls the delay before a failed attempt restarts.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = Override::Set(backoff);
        self
    }

    /// Builder: sets the restart policy.
    ///
    /// Restart controls whether the task runs again after it exits.
    pub fn with_restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = Override::Set(restart);
        self
    }

    /// Builder: set the failure-retry limit (`None` = unlimited).
    ///
    /// Accepts a `NonZeroU32` (a limit) or an `Option<NonZeroU32>`; pass either
    /// form directly without calling `.into()`.
    /// `None` explicitly disables an inherited retry limit.
    pub fn with_max_retries(mut self, max_retries: impl Into<Option<NonZeroU32>>) -> Self {
        self.max_retries = Override::Set(max_retries.into());
        self
    }

    /// Convenience setter that validates a raw failure-retry limit.
    ///
    /// # Errors
    /// Returns [`ConfigError::Zero`] when `max_retries` is zero. Use
    /// [`with_max_retries`](Self::with_max_retries) with `None` for unlimited retries.
    pub fn try_with_max_retries(self, max_retries: u32) -> Result<Self, ConfigError> {
        let max_retries = NonZeroU32::new(max_retries).ok_or(ConfigError::Zero {
            field: "max_retries",
        })?;
        Ok(self.with_max_retries(max_retries))
    }

    /// Applies inherited task defaults and returns a concrete specification.
    pub(crate) fn resolve(self, defaults: &TaskDefaults) -> ResolvedTaskSpec {
        ResolvedTaskSpec {
            restart: self.restart.resolve(defaults.restart()),
            backoff: self.backoff.resolve(defaults.backoff()),
            timeout: self.timeout.resolve(defaults.timeout()),
            max_retries: self.max_retries.resolve(defaults.max_retries()),
            task: self.task,
        }
    }
}

impl ResolvedTaskSpec {
    /// Returns the task handle.
    pub(crate) fn task(&self) -> &TaskRef {
        &self.task
    }

    /// Returns the task name.
    #[cfg(test)]
    pub(crate) fn name(&self) -> &str {
        self.task.name()
    }

    /// Returns the resolved restart policy.
    pub(crate) fn restart(&self) -> RestartPolicy {
        self.restart
    }

    /// Returns the resolved backoff policy.
    pub(crate) fn backoff(&self) -> BackoffPolicy {
        self.backoff
    }

    /// Returns the resolved attempt timeout.
    pub(crate) fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Returns the resolved failure-retry limit.
    pub(crate) fn max_retries(&self) -> Option<NonZeroU32> {
        self.max_retries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JitterPolicy, TaskContext, TaskFn};

    fn task(name: &str) -> TaskRef {
        TaskFn::arc(name, |_ctx: TaskContext| async { Ok(()) })
    }

    fn assert_inherits_non_restart_settings(spec: &TaskSpec) {
        assert!(spec.backoff_override().is_none());
        assert!(spec.timeout_override().is_none());
        assert!(spec.max_retries_override().is_none());
    }

    fn assert_explicit_timeout(spec: TaskSpec, expected: Option<Duration>, case: &str) {
        assert_eq!(spec.timeout_override(), Some(expected), "{case}");
    }

    #[test]
    fn named_constructors_set_restart_and_inherit_other_settings() {
        let inherited = TaskSpec::from_defaults(task("inherited"));
        assert!(inherited.restart_override().is_none());
        assert_inherits_non_restart_settings(&inherited);

        let once = TaskSpec::once(task("once"));
        assert!(matches!(
            once.restart_override(),
            Some(RestartPolicy::Never)
        ));
        assert_inherits_non_restart_settings(&once);

        let restartable = TaskSpec::restartable(task("restartable"));
        assert!(matches!(
            restartable.restart_override(),
            Some(RestartPolicy::OnFailure)
        ));
        assert_inherits_non_restart_settings(&restartable);

        let every = Duration::from_secs(30);
        let spec = TaskSpec::periodic(task("tick"), every);
        assert!(
            matches!(spec.restart_override(), Some(RestartPolicy::Always { interval: Some(d) }) if d == every),
            "periodic must set RestartPolicy::Always with the given interval, got {:?}",
            spec.restart_override()
        );
        assert_inherits_non_restart_settings(&spec);

        let immediate = TaskSpec::periodic(task("immediate"), Duration::ZERO);
        assert!(
            matches!(
                immediate.restart_override(),
                Some(RestartPolicy::Always { interval: None })
            ),
            "a zero interval must normalize to None (immediate restart), got {:?}",
            immediate.restart_override()
        );
    }

    #[test]
    fn new_marks_every_setting_as_explicit() {
        let backoff = BackoffPolicy::constant(Duration::from_secs(2));
        let timeout = Duration::from_secs(7);
        let spec = TaskSpec::new(task("explicit"), RestartPolicy::Never, backoff, timeout);

        assert!(matches!(
            spec.restart_override(),
            Some(RestartPolicy::Never)
        ));
        assert_eq!(
            spec.backoff_override().map(|policy| policy.first()),
            Some(Duration::from_secs(2))
        );
        assert_eq!(spec.timeout_override(), Some(Some(timeout)));
        assert_eq!(spec.max_retries_override(), Some(None));
    }

    #[test]
    fn explicit_none_disables_inherited_optional_settings() {
        let retries = NonZeroU32::new(4).unwrap();
        let defaults = TaskDefaults::default()
            .with_timeout(Duration::from_secs(9))
            .with_max_retries(retries);
        let spec = TaskSpec::restartable(task("disabled"))
            .with_timeout(None)
            .with_max_retries(None);

        assert_eq!(spec.timeout_override(), Some(None));
        assert_eq!(spec.max_retries_override(), Some(None));

        let resolved = spec.resolve(&defaults);
        assert_eq!(resolved.timeout(), None);
        assert_eq!(resolved.max_retries(), None);
    }

    #[test]
    fn resolve_applies_defaults_only_to_inherited_settings() {
        let retries = NonZeroU32::new(6).unwrap();
        let defaults = TaskDefaults::default()
            .with_restart(RestartPolicy::Never)
            .with_backoff(BackoffPolicy::constant(Duration::from_secs(3)))
            .with_timeout(Duration::from_secs(12))
            .with_max_retries(retries);
        let spec = TaskSpec::restartable(task("worker"));

        let resolved = spec.resolve(&defaults);

        assert_eq!(resolved.name(), "worker");
        assert_eq!(resolved.task().name(), "worker");
        assert!(matches!(resolved.restart(), RestartPolicy::OnFailure));
        assert_eq!(resolved.backoff().first(), Duration::from_secs(3));
        assert_eq!(resolved.backoff().jitter(), JitterPolicy::None);
        assert_eq!(resolved.timeout(), Some(Duration::from_secs(12)));
        assert_eq!(resolved.max_retries(), Some(retries));
    }

    #[test]
    fn new_does_not_inherit_task_defaults() {
        let defaults = TaskDefaults::default()
            .with_restart(RestartPolicy::OnFailure)
            .with_backoff(BackoffPolicy::constant(Duration::from_secs(8)))
            .with_timeout(Duration::from_secs(9))
            .with_max_retries(NonZeroU32::new(3).unwrap());
        let spec = TaskSpec::new(
            task("explicit"),
            RestartPolicy::Never,
            BackoffPolicy::constant(Duration::from_secs(1)),
            None,
        );

        let resolved = spec.resolve(&defaults);

        assert!(matches!(resolved.restart(), RestartPolicy::Never));
        assert_eq!(resolved.backoff().first(), Duration::from_secs(1));
        assert_eq!(resolved.timeout(), None);
        assert_eq!(resolved.max_retries(), None);
    }

    #[test]
    fn with_timeout_accepts_duration_or_option_and_normalizes_zero() {
        assert_explicit_timeout(
            TaskSpec::once(task("zero-duration")).with_timeout(Duration::ZERO),
            None,
            "with_timeout(ZERO) must normalize to None",
        );
        assert_explicit_timeout(
            TaskSpec::once(task("zero-option")).with_timeout(Some(Duration::ZERO)),
            None,
            "with_timeout(Some(ZERO)) must normalize to None",
        );
        assert_explicit_timeout(
            TaskSpec::new(
                task("z"),
                RestartPolicy::Never,
                BackoffPolicy::default(),
                Some(Duration::ZERO),
            ),
            None,
            "new(.., Some(ZERO)) must normalize to None",
        );

        let duration = Duration::from_secs(1);
        assert_explicit_timeout(
            TaskSpec::once(task("positive-duration")).with_timeout(duration),
            Some(duration),
            "a positive Duration must be preserved",
        );
        assert_explicit_timeout(
            TaskSpec::once(task("positive-option")).with_timeout(Some(duration)),
            Some(duration),
            "a positive Some(Duration) must be preserved",
        );
        assert_explicit_timeout(
            TaskSpec::once(task("none-inference")).with_timeout(None),
            None,
            "None must infer Option<Duration> and explicitly disable the timeout",
        );
    }

    #[test]
    fn raw_retry_limit_is_validated_like_task_defaults() {
        let spec = TaskSpec::once(task("limited"))
            .try_with_max_retries(3)
            .expect("a positive retry limit must be accepted");
        assert_eq!(
            spec.max_retries_override().flatten().map(NonZeroU32::get),
            Some(3)
        );

        assert_eq!(
            TaskSpec::once(task("zero"))
                .try_with_max_retries(0)
                .unwrap_err(),
            ConfigError::Zero {
                field: "max_retries"
            }
        );
    }
}
