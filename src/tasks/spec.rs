//! Configures one named task before it enters the runtime.
//!
//! [`TaskSpec`] is the value an application passes to Taskvisor. It combines a [`TaskRef`] with
//! a registration name, restart behavior, retry timing, an optional attempt timeout, and an optional retry limit.
//! Direct adds send the spec to the registry. Controller submissions first apply slot admission.
//! The registry resolves inherited fields from [`TaskDefaults`] before it starts the task.
//!
//! ```text
//! application ──► TaskSpec
//!                     │ direct add or controller submission
//!                     ▼
//!              registry admission
//!                     ├── name ────────────────────────► identity index
//!                     ├── inherited setting ───────────► TaskDefaults
//!                     └── resolved task and settings ──► TaskActor
//! ```
//!
//! The name is the registry key, not a controller slot. A second registration with the same name
//! is rejected while the first registration exists. After a force-abort, the name remains reserved
//! until Taskvisor has observed the actor's physical exit and collected its terminal state.

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use crate::{
    core::{ConfigError, TaskDefaults},
    policies::BackoffPolicy,
    policies::RestartPolicy,
    tasks::task::TaskRef,
};

/// Treats a zero timeout as disabled.
#[inline]
fn normalize_timeout(timeout: Option<Duration>) -> Option<Duration> {
    timeout.filter(|d| !d.is_zero())
}

/// A ready-to-submit task with its name and execution settings.
///
/// Each setting is either explicit or inherited from [`TaskDefaults`].
/// Resolution happens once during admission. A `with_*` method makes its field explicit for this task only.
///
/// ```text
/// TaskSpec field
///      ├── Explicit(value) ──► value
///      └── Inherit ──────────► matching TaskDefaults value
///                                          ▼
///                               resolved actor settings
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
/// # Examples
///
/// ```rust
/// use std::num::NonZeroU32;
/// use std::time::Duration;
/// use taskvisor::{TaskError, TaskFn, TaskRef, TaskSpec};
///
/// let task: TaskRef = TaskFn::arc(|_ctx| async {
///     Err(TaskError::fail("temporary failure"))
/// });
///
/// let spec = TaskSpec::restartable("worker", task)
///     .with_timeout(Duration::from_secs(30))
///     .with_max_retries(NonZeroU32::new(5).unwrap());
/// ```
///
/// Here, `max_retries = 5` allows the first failed attempt and five retries in one failure streak.
/// A success resets the counter.
///
/// # See also
///
/// - [`Task`](crate::Task) defines the attempt contract.
/// - [`TaskFn`](crate::TaskFn) adapts an async closure.
#[derive(Clone)]
#[must_use]
pub struct TaskSpec {
    /// Immutable name used by registry identity and events.
    name: Arc<str>,
    /// Restart policy selected explicitly or inherited from [`TaskDefaults`].
    restart: TaskSetting<RestartPolicy>,
    /// Backoff policy selected explicitly or inherited from [`TaskDefaults`].
    backoff: TaskSetting<BackoffPolicy>,
    /// Per-attempt timeout; `Explicit(None)` disables an inherited timeout.
    timeout: TaskSetting<Option<Duration>>,
    /// Retry limit; `Explicit(None)` selects unlimited retries.
    max_retries: TaskSetting<Option<NonZeroU32>>,
    /// Task object reused for attempts in this registration.
    task: TaskRef,
}

/// Marks a task setting as explicit or inherited from [`TaskDefaults`].
///
/// For optional fields, `TaskSetting<Option<T>>` distinguishes inheritance from an explicit `None`:
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
    /// Uses the matching [`TaskDefaults`] field at registry admission.
    Inherit,
    /// Uses this value instead of the matching default.
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

/// Actor settings produced by registry-time default resolution.
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
            .field("name", &self.name)
            .field("restart", &self.restart)
            .field("backoff", &self.backoff)
            .field("timeout", &self.timeout)
            .field("task", &"<dyn Task>")
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
            .field("task", &"<dyn Task>")
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

impl TaskSpec {
    /// Creates a named spec that inherits every execution setting.
    ///
    /// Registry admission resolves restart, backoff, timeout, and retry limit from [`TaskDefaults`].
    /// A later `with_*` call makes that field explicit.
    pub fn from_defaults(name: impl Into<Arc<str>>, task: TaskRef) -> Self {
        Self {
            name: name.into(),
            restart: TaskSetting::Inherit,
            backoff: TaskSetting::Inherit,
            timeout: TaskSetting::Inherit,
            max_retries: TaskSetting::Inherit,
            task,
        }
    }

    /// Creates a named spec with explicit restart, backoff, and timeout settings.
    ///
    /// Use this constructor when these settings must not inherit supervisor defaults.
    /// The named constructors are shorter for common lifecycles.
    ///
    /// `timeout` accepts a [`Duration`] or `Option<Duration>`; `None` and zero disable the attempt timeout.
    /// The retry limit starts as explicitly unlimited.
    /// Use [`with_max_retries`](Self::with_max_retries) to set a limit.
    pub fn new(
        name: impl Into<Arc<str>>,
        task: TaskRef,
        restart: RestartPolicy,
        backoff: BackoffPolicy,
        timeout: impl Into<Option<Duration>>,
    ) -> Self {
        Self {
            name: name.into(),
            restart: TaskSetting::Explicit(restart),
            backoff: TaskSetting::Explicit(backoff),
            timeout: TaskSetting::Explicit(normalize_timeout(timeout.into())),
            max_retries: TaskSetting::Explicit(None),
            task,
        }
    }

    /// Creates a named task that never starts a second attempt.
    ///
    /// Restart is explicitly [`Never`](RestartPolicy::Never).
    /// Backoff, timeout, and retry limit remain inherited, although restart never permits a retry.
    pub fn once(name: impl Into<Arc<str>>, task: TaskRef) -> Self {
        Self {
            name: name.into(),
            restart: TaskSetting::Explicit(RestartPolicy::Never),
            backoff: TaskSetting::Inherit,
            timeout: TaskSetting::Inherit,
            max_retries: TaskSetting::Inherit,
            task,
        }
    }

    /// Creates a named task that may retry a retryable failure.
    ///
    /// Restart is explicitly [`OnFailure`](RestartPolicy::OnFailure).
    /// Success, fatal failure, and cancellation stop the actor.
    /// Backoff, timeout, and the retry limit are inherited.
    pub fn restartable(name: impl Into<Arc<str>>, task: TaskRef) -> Self {
        Self {
            name: name.into(),
            restart: TaskSetting::Explicit(RestartPolicy::OnFailure),
            backoff: TaskSetting::Inherit,
            timeout: TaskSetting::Inherit,
            max_retries: TaskSetting::Inherit,
            task,
        }
    }

    /// Creates a named task that may run again after success or retryable failure.
    ///
    /// After success, the actor waits at least `every` before the next attempt.
    /// Taskvisor also keeps successive attempt starts at least one millisecond apart, so a fast
    /// task configured below one millisecond can wait longer than `every`. A zero value removes
    /// the configured interval; the one-millisecond start-spacing guard still applies.
    ///
    /// Retryable failures use the backoff policy, not `every`. A retry limit can stop
    /// the task after repeated failures. Fatal failure and cancellation always stop it.
    ///
    /// The delay begins after an attempt completes. This is not a wall-clock schedule.
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use taskvisor::{TaskFn, TaskRef, TaskSpec};
    ///
    /// let tick: TaskRef = TaskFn::arc(|_ctx| async move {
    ///     println!("tick");
    ///     Ok(())
    /// });
    ///
    /// // Starts the next attempt 30 seconds after this successful attempt ends.
    /// let spec = TaskSpec::periodic("tick", tick, Duration::from_secs(30));
    /// ```
    #[doc(alias = "interval")]
    #[doc(alias = "fixed delay")]
    pub fn periodic(name: impl Into<Arc<str>>, task: TaskRef, every: Duration) -> Self {
        Self {
            name: name.into(),
            restart: TaskSetting::Explicit(RestartPolicy::Always {
                interval: Some(every).filter(|d| !d.is_zero()),
            }),
            backoff: TaskSetting::Inherit,
            timeout: TaskSetting::Inherit,
            max_retries: TaskSetting::Inherit,
            task,
        }
    }

    /// Returns the shared task object.
    #[must_use]
    pub fn task(&self) -> &TaskRef {
        &self.task
    }

    /// Returns the immutable registration name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Clones the backing name without allocating or copying the string.
    pub(crate) fn shared_name(&self) -> Arc<str> {
        Arc::clone(&self.name)
    }

    /// Returns the explicit restart policy, or `None` when inherited.
    #[must_use]
    pub fn restart_override(&self) -> Option<RestartPolicy> {
        self.restart.value()
    }

    /// Returns the explicit backoff policy, or `None` when inherited.
    #[must_use]
    pub fn backoff_override(&self) -> Option<BackoffPolicy> {
        self.backoff.value()
    }

    /// Returns the unresolved attempt-timeout setting.
    ///
    /// - [`TaskSetting::Inherit`] means inherit the default.
    /// - `TaskSetting::Explicit(None)` explicitly disables the timeout.
    /// - `TaskSetting::Explicit(Some(duration))` selects that timeout.
    #[must_use]
    pub fn timeout_override(&self) -> TaskSetting<Option<Duration>> {
        self.timeout
    }

    /// Returns the unresolved retry-limit setting.
    ///
    /// - [`TaskSetting::Inherit`] means inherit the default.
    /// - `TaskSetting::Explicit(None)` explicitly allows unlimited retries.
    /// - `TaskSetting::Explicit(Some(limit))` selects that retry limit.
    #[must_use]
    pub fn max_retries_override(&self) -> TaskSetting<Option<NonZeroU32>> {
        self.max_retries
    }

    /// Sets an explicit timeout for each attempt.
    ///
    /// A [`Duration`] enables the timeout. `None` or zero disables it, including a timeout set in [`TaskDefaults`].
    ///
    /// At the deadline, Taskvisor cancels the attempt context and drops the attempt future.
    /// This cannot interrupt synchronous code in a future poll or undo work already performed outside the future.
    /// Dropping a future is also synchronous; a blocking destructor can delay timeout completion.
    #[doc(alias = "watchdog")]
    #[doc(alias = "attempt deadline")]
    pub fn with_timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.timeout = TaskSetting::Explicit(normalize_timeout(timeout.into()));
        self
    }

    /// Sets an explicit delay policy for retryable failures.
    ///
    /// This delay is not used after success.
    /// See [`BackoffPolicy`] for the calculation and jitter order.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = TaskSetting::Explicit(backoff);
        self
    }

    /// Sets the explicit restart policy.
    ///
    /// The restart policy decides whether another attempt is eligible.
    /// The retry limit and backoff policy remain separate settings.
    pub fn with_restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = TaskSetting::Explicit(restart);
        self
    }

    /// Sets the maximum number of retries after the first failed attempt in one failure streak.
    ///
    /// Pass a [`NonZeroU32`] to set a limit.
    /// Pass `None` for unlimited retries, overriding any default limit.
    ///
    /// A success resets the count.
    #[doc(alias = "retry limit")]
    #[doc(alias = "retry budget")]
    pub fn with_max_retries(mut self, max_retries: impl Into<Option<NonZeroU32>>) -> Self {
        self.max_retries = TaskSetting::Explicit(max_retries.into());
        self
    }

    /// Sets an explicit retry limit from a raw integer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `max_retries` is zero.
    /// Use [`with_max_retries`](Self::with_max_retries) with `None` for no limit.
    pub fn try_with_max_retries(self, max_retries: u32) -> Result<Self, ConfigError> {
        let max_retries = NonZeroU32::new(max_retries).ok_or(ConfigError::Zero {
            field: "max_retries",
        })?;
        Ok(self.with_max_retries(max_retries))
    }

    /// Resolves inherited fields for registry admission.
    pub(crate) fn resolve(self, defaults: &TaskDefaults) -> ResolvedTaskSpec {
        let Self {
            name,
            restart,
            backoff,
            timeout,
            max_retries,
            task,
        } = self;
        drop(name);
        ResolvedTaskSpec {
            restart: restart.resolve(defaults.restart()),
            backoff: backoff.resolve(defaults.backoff()),
            timeout: timeout.resolve(defaults.timeout()),
            max_retries: max_retries.resolve(defaults.max_retries()),
            task,
        }
    }
}

impl ResolvedTaskSpec {
    /// Returns the task object for actor construction.
    pub(crate) fn task(&self) -> &TaskRef {
        &self.task
    }

    /// Returns the actor's restart policy.
    pub(crate) fn restart(&self) -> RestartPolicy {
        self.restart
    }

    /// Returns the actor's failure-backoff policy.
    pub(crate) fn backoff(&self) -> BackoffPolicy {
        self.backoff
    }

    /// Returns the actor's attempt timeout.
    pub(crate) fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Returns the actor's retry limit for one failure streak.
    pub(crate) fn max_retries(&self) -> Option<NonZeroU32> {
        self.max_retries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::{BoxTaskFuture, JitterPolicy, Task, TaskContext, TaskFn};

    fn task() -> TaskRef {
        TaskFn::arc(|_ctx: TaskContext| async { Ok(()) })
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
        let inherited = TaskSpec::from_defaults("inherited", task());
        assert!(inherited.restart_override().is_none());
        assert_inherits_non_restart_settings(&inherited);

        let once = TaskSpec::once("once", task());
        assert!(matches!(
            once.restart_override(),
            Some(RestartPolicy::Never)
        ));
        assert_inherits_non_restart_settings(&once);

        let restartable = TaskSpec::restartable("restartable", task());
        assert!(matches!(
            restartable.restart_override(),
            Some(RestartPolicy::OnFailure)
        ));
        assert_inherits_non_restart_settings(&restartable);

        let every = Duration::from_secs(30);
        let spec = TaskSpec::periodic("tick", task(), every);
        assert!(
            matches!(spec.restart_override(), Some(RestartPolicy::Always { interval: Some(d) }) if d == every),
            "periodic must set RestartPolicy::Always with the given interval, got {:?}",
            spec.restart_override()
        );
        assert_inherits_non_restart_settings(&spec);

        let immediate = TaskSpec::periodic("immediate", task(), Duration::ZERO);
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
        let spec = TaskSpec::new("explicit", task(), RestartPolicy::Never, backoff, timeout);

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
        let spec = TaskSpec::restartable("disabled", task())
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
        let task = task();
        let expected_task = Arc::clone(&task);
        let spec = TaskSpec::restartable("worker", task);

        let resolved = spec.resolve(&defaults);

        assert!(Arc::ptr_eq(resolved.task(), &expected_task));
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
            "explicit",
            task(),
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
            TaskSpec::once("zero-duration", task()).with_timeout(Duration::ZERO),
            None,
            "with_timeout(ZERO) must normalize to None",
        );
        assert_explicit_timeout(
            TaskSpec::once("zero-option", task()).with_timeout(Some(Duration::ZERO)),
            None,
            "with_timeout(Some(ZERO)) must normalize to None",
        );
        assert_explicit_timeout(
            TaskSpec::new(
                "z",
                task(),
                RestartPolicy::Never,
                BackoffPolicy::default(),
                Some(Duration::ZERO),
            ),
            None,
            "new(.., Some(ZERO)) must normalize to None",
        );

        let duration = Duration::from_secs(1);
        assert_explicit_timeout(
            TaskSpec::once("positive-duration", task()).with_timeout(duration),
            Some(duration),
            "a positive Duration must be preserved",
        );
        assert_explicit_timeout(
            TaskSpec::once("positive-option", task()).with_timeout(Some(duration)),
            Some(duration),
            "a positive Some(Duration) must be preserved",
        );
        assert_explicit_timeout(
            TaskSpec::once("none-inference", task()).with_timeout(None),
            None,
            "None must infer Option<Duration> and explicitly disable the timeout",
        );
    }

    #[test]
    fn raw_retry_limit_is_validated_like_task_defaults() {
        let spec = TaskSpec::once("limited", task())
            .try_with_max_retries(3)
            .expect("a positive retry limit must be accepted");
        assert!(matches!(
            spec.max_retries_override(),
            TaskSetting::Explicit(Some(limit)) if limit.get() == 3
        ));

        assert_eq!(
            TaskSpec::once("zero", task())
                .try_with_max_retries(0)
                .unwrap_err(),
            ConfigError::Zero {
                field: "max_retries"
            }
        );
    }

    #[test]
    fn shared_name_clones_the_same_arc_without_copying_the_string() {
        let name: Arc<str> = Arc::from("shared");
        let spec = TaskSpec::once(Arc::clone(&name), task());
        let shared = spec.shared_name();

        assert!(Arc::ptr_eq(&name, &shared));
        assert_eq!(spec.name(), "shared");
    }

    #[test]
    fn debug_uses_owned_name_without_spawning_the_task() {
        struct NoSpawn;

        impl Task for NoSpawn {
            fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
                unreachable!("formatting a spec must not spawn its task")
            }
        }

        let spec = TaskSpec::once("debug-name", Arc::new(NoSpawn));
        let rendered = format!("{spec:?}");
        assert!(rendered.contains("debug-name"));
        assert!(rendered.contains("<dyn Task>"));

        let resolved = spec.resolve(&TaskDefaults::default());
        let rendered = format!("{resolved:?}");
        assert!(rendered.contains("<dyn Task>"));
    }
}
