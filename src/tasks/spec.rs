//! Per-task execution settings and default resolution.

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

/// A task and the rules used to run it.
///
/// A spec can set a value or inherit it from [`TaskDefaults`].
/// The supervisor resolves all inherited values when it accepts the task.
/// A `with_*` method always sets an explicit value and wins over the default.
///
/// ```text
/// Resolve each field at registry admission:
///
/// explicit value
/// TaskSpec.restart     = Never     ─────────────────────────────────► restart = Never
/// TaskDefaults.restart = OnFailure ─────────────────────────────────► not selected
///
/// inherited value
/// TaskSpec.timeout     = inherit   ──► TaskDefaults.timeout = 30s ──► timeout = 30s
///
/// ResolvedTaskSpec
/// ├── restart = Never
/// └── timeout = 30s
/// ```
///
/// | Constructor                            | Restart setting | Other settings                  |
/// |----------------------------------------|-----------------|---------------------------------|
/// | [`once`](Self::once)                   | Never           | Inherited                       |
/// | [`restartable`](Self::restartable)     | On failure      | Inherited                       |
/// | [`periodic`](Self::periodic)           | Always          | Inherited                       |
/// | [`from_defaults`](Self::from_defaults) | Inherited       | Inherited                       |
/// | [`new`](Self::new)                     | Explicit        | Explicit; retries are unlimited |
///
/// ## Example
///
/// ```rust
/// use std::num::NonZeroU32;
/// use std::time::Duration;
/// use taskvisor::{TaskFn, TaskRef, TaskSpec};
///
/// let task: TaskRef = TaskFn::arc("worker", |_ctx| async { Ok(()) });
///
/// let spec = TaskSpec::restartable(task)
///     .with_timeout(Duration::from_secs(30))
///     .with_max_retries(NonZeroU32::new(5).unwrap());
/// ```
///
/// > `max_retries = 5` allows the first failed attempt plus five retries.
///
/// A successful attempt resets this count; an `Always` task may still have more than six attempts over its full lifetime.
///
/// ## See Also
///
/// - See [`Task`](crate::Task) for the execution contract and cancellation semantics.
/// - For the closure-based implementation see [`TaskFn`](crate::TaskFn).
#[derive(Clone)]
#[must_use]
pub struct TaskSpec {
    /// Restart policy selected explicitly or inherited from [`TaskDefaults`].
    restart: TaskSetting<RestartPolicy>,
    /// Backoff policy selected explicitly or inherited from [`TaskDefaults`].
    backoff: TaskSetting<BackoffPolicy>,
    /// Per-attempt timeout; `Explicit(None)` disables an inherited timeout.
    timeout: TaskSetting<Option<Duration>>,
    /// Retry limit; `Explicit(None)` selects unlimited retries.
    max_retries: TaskSetting<Option<NonZeroU32>>,

    /// Task object reused across all attempts started from this spec.
    task: TaskRef,
}

/// Whether a [`TaskSpec`] setting is inherited or explicitly selected.
///
/// Optional settings use `TaskSetting<Option<T>>`. This keeps inherited and
/// explicitly disabled values distinct without exposing `Option<Option<T>>`:
///
/// ```rust
/// use taskvisor::TaskSetting;
///
/// let inherited: TaskSetting<Option<u32>> = TaskSetting::Inherit;
/// let disabled = TaskSetting::Explicit(None);
/// let limited = TaskSetting::Explicit(Some(3));
///
/// assert_ne!(inherited, disabled);
/// assert_ne!(disabled, limited);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskSetting<T> {
    /// Resolve the field from [`TaskDefaults`] at registry admission.
    Inherit,
    /// Use this explicit value instead of the corresponding default.
    Explicit(T),
}

impl<T: Copy> TaskSetting<T> {
    #[inline]
    fn value(self) -> Option<T> {
        match self {
            Self::Inherit => None,
            Self::Explicit(value) => Some(value),
        }
    }

    #[inline]
    fn resolve(self, default: T) -> T {
        match self {
            Self::Inherit => default,
            Self::Explicit(value) => value,
        }
    }
}

/// A task specification after default resolution.
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
    /// Creates a spec that inherits every execution setting.
    ///
    /// The supervisor resolves restart, backoff, timeout, and retry limit from its [`TaskDefaults`] when it accepts the task.
    /// > A later `with_*` call sets that one field explicitly.
    pub fn from_defaults(task: TaskRef) -> Self {
        Self {
            restart: TaskSetting::Inherit,
            backoff: TaskSetting::Inherit,
            timeout: TaskSetting::Inherit,
            max_retries: TaskSetting::Inherit,
            task,
        }
    }

    /// Creates a spec with explicit main settings.
    ///
    /// Prefer the named constructors for common cases: [`once`](Self::once), [`restartable`](Self::restartable), [`periodic`](Self::periodic).
    ///
    /// `timeout` accepts a [`Duration`] or `Option<Duration>`.
    /// `None` and zero disable the attempt timeout.
    ///
    /// The retry limit is set to unlimited.
    /// > _change it with [`with_max_retries`](Self::with_max_retries)_.
    pub fn new(
        task: TaskRef,
        restart: RestartPolicy,
        backoff: BackoffPolicy,
        timeout: impl Into<Option<Duration>>,
    ) -> Self {
        Self {
            restart: TaskSetting::Explicit(restart),
            backoff: TaskSetting::Explicit(backoff),
            timeout: TaskSetting::Explicit(normalize_timeout(timeout.into())),
            max_retries: TaskSetting::Explicit(None),
            task,
        }
    }

    /// Creates a one-shot task that never restarts.
    ///
    /// Backoff, timeout, and retry limit are inherited from [`TaskDefaults`].
    /// > Override them with the matching `with_*` methods.
    pub fn once(task: TaskRef) -> Self {
        Self {
            restart: TaskSetting::Explicit(RestartPolicy::Never),
            backoff: TaskSetting::Inherit,
            timeout: TaskSetting::Inherit,
            max_retries: TaskSetting::Inherit,
            task,
        }
    }

    /// Creates a task that restarts after retryable failures.
    ///
    /// Success, fatal failure, and cancellation stop the task.
    /// > Backoff, timeout, and retry limit are inherited from [`TaskDefaults`].
    pub fn restartable(task: TaskRef) -> Self {
        Self {
            restart: TaskSetting::Explicit(RestartPolicy::OnFailure),
            backoff: TaskSetting::Inherit,
            timeout: TaskSetting::Inherit,
            max_retries: TaskSetting::Inherit,
            task,
        }
    }

    /// Creates a task that runs again after each success.
    ///
    /// After success, the supervisor waits `every` before the next attempt.
    /// A zero value means no configured interval;
    /// > _a small internal guard still prevents an instant task from creating a hot loop._
    ///
    /// Retryable failures use the backoff policy, not `every`.
    /// A retry limit can stop the task after repeated failures.
    /// Fatal failure and cancellation always stop it.
    ///
    /// The interval starts after an attempt completes.
    /// > *This is not a wall-clock schedule such as "daily at 03:00".*
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use taskvisor::{TaskFn, TaskRef, TaskSpec};
    ///
    /// let tick: TaskRef = TaskFn::arc("tick", |_ctx| async move {
    ///     println!("tick");
    ///     Ok(())
    /// });
    ///
    /// // Starts the next successful cycle 30 seconds after this one ends.
    /// let spec = TaskSpec::periodic(tick, Duration::from_secs(30));
    /// ```
    #[doc(alias = "interval")]
    #[doc(alias = "fixed delay")]
    pub fn periodic(task: TaskRef, every: Duration) -> Self {
        Self {
            restart: TaskSetting::Explicit(RestartPolicy::Always {
                interval: Some(every).filter(|d| !d.is_zero()),
            }),
            backoff: TaskSetting::Inherit,
            timeout: TaskSetting::Inherit,
            max_retries: TaskSetting::Inherit,
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

    /// Returns the explicit restart policy, or `None` if it is inherited.
    #[must_use]
    pub fn restart_override(&self) -> Option<RestartPolicy> {
        self.restart.value()
    }

    /// Returns the explicit backoff policy, or `None` if it is inherited.
    #[must_use]
    pub fn backoff_override(&self) -> Option<BackoffPolicy> {
        self.backoff.value()
    }

    /// Returns how this spec overrides the attempt timeout.
    ///
    /// - [`TaskSetting::Inherit`] means inherit the default.
    /// - `TaskSetting::Explicit(None)` explicitly disables the timeout.
    /// - `TaskSetting::Explicit(Some(duration))` selects that timeout.
    #[must_use]
    pub fn timeout_override(&self) -> TaskSetting<Option<Duration>> {
        self.timeout
    }

    /// Returns how this spec overrides the retry limit.
    ///
    /// - [`TaskSetting::Inherit`] means inherit the default.
    /// - `TaskSetting::Explicit(None)` explicitly allows unlimited retries.
    /// - `TaskSetting::Explicit(Some(limit))` selects that retry limit.
    #[must_use]
    pub fn max_retries_override(&self) -> TaskSetting<Option<NonZeroU32>> {
        self.max_retries
    }

    /// Sets the timeout for each attempt.
    ///
    /// Pass a `Duration` to enable it.
    /// Pass `None` or zero to disable it, including a timeout inherited from [`TaskDefaults`].
    #[doc(alias = "watchdog")]
    #[doc(alias = "attempt deadline")]
    pub fn with_timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.timeout = TaskSetting::Explicit(normalize_timeout(timeout.into()));
        self
    }

    /// Sets the delay policy for retryable failures.
    ///
    /// This value overrides the supervisor default.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = TaskSetting::Explicit(backoff);
        self
    }

    /// Sets when the task may run another attempt.
    ///
    /// This value overrides the supervisor default.
    pub fn with_restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = TaskSetting::Explicit(restart);
        self
    }

    /// Sets the number of retries after the first failed attempt in a failure streak.
    ///
    /// Pass a [`NonZeroU32`] to set a limit.
    /// Pass `None` for unlimited retries, including when [`TaskDefaults`] has a limit.
    ///
    /// A success resets the count.
    #[doc(alias = "retry limit")]
    #[doc(alias = "retry budget")]
    pub fn with_max_retries(mut self, max_retries: impl Into<Option<NonZeroU32>>) -> Self {
        self.max_retries = TaskSetting::Explicit(max_retries.into());
        self
    }

    /// Sets a retry limit from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_retries` is zero.
    /// > Use [`with_max_retries`](Self::with_max_retries) with `None` for unlimited retries.
    pub fn try_with_max_retries(self, max_retries: u32) -> Result<Self, ConfigError> {
        let max_retries = NonZeroU32::new(max_retries).ok_or(ConfigError::Zero {
            field: "max_retries",
        })?;
        Ok(self.with_max_retries(max_retries))
    }

    /// Applies inherited defaults at registry admission.
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
        assert_eq!(spec.timeout_override(), TaskSetting::Inherit);
        assert_eq!(spec.max_retries_override(), TaskSetting::Inherit);
    }

    fn assert_explicit_timeout(spec: TaskSpec, expected: Option<Duration>, case: &str) {
        assert_eq!(
            spec.timeout_override(),
            TaskSetting::Explicit(expected),
            "{case}"
        );
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
        assert_eq!(
            spec.timeout_override(),
            TaskSetting::Explicit(Some(timeout))
        );
        assert_eq!(spec.max_retries_override(), TaskSetting::Explicit(None));
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

        assert_eq!(spec.timeout_override(), TaskSetting::Explicit(None));
        assert_eq!(spec.max_retries_override(), TaskSetting::Explicit(None));

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
        assert!(matches!(
            spec.max_retries_override(),
            TaskSetting::Explicit(Some(limit)) if limit.get() == 3
        ));

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
