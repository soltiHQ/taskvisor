//! Explains which error type belongs to each Taskvisor boundary.
//!
//! | Boundary                                                              | Error type                            |
//! |-----------------------------------------------------------------------|---------------------------------------|
//! | Checked configuration constructors and setters                        | [`ConfigError`](crate::ConfigError)   |
//! | [`BackoffPolicy::new`](crate::BackoffPolicy::new)                     | [`BackoffError`](crate::BackoffError) |
//! | [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build) | [`BuildError`]                        |
//! | Runtime lifecycle, management, and outcome waiting                    | [`RuntimeError`]                      |
//! | Controller availability checks and submission command intake          | `ControllerError`                     |
//! | One task attempt                                                      | [`TaskError`]                         |
//! | Code that combines runtime and controller calls                       | [`enum@Error`]                        |
//!
//! ```text
//! task future ──► TaskError ──► actor policy
//!                                  ├── retry allowed ──► next attempt
//!                                  └── stop ───────────► cleanup and TaskOutcome
//! ```
//!
//! `ControllerError` is available with the `controller` feature.
//! Task code returns [`TaskError`].
//! Taskvisor APIs return the error for their boundary.
//! Applications may use [`enum@Error`] to combine runtime and controller calls.
//! Readable `Display` text is not a classification API.
//! Match variants or use `as_label` where available.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::identity::TaskId;

/// Owned source error attached to [`TaskError::Fail`] or [`TaskError::Fatal`].
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Shared source error carried by a cloneable final task outcome.
///
/// [`TaskOutcome`](crate::TaskOutcome) can clone this value even when the source type is not cloneable.
pub type SharedError = Arc<dyn std::error::Error + Send + Sync + 'static>;

/// Build-time failure before a [`Supervisor`](crate::Supervisor) is returned.
///
/// [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build) uses this type for invalid capacity.
/// It also covers insufficient subscriber ownership capacity and cleanup-worker startup failure.
///
/// This enum and its data-carrying variants are non-exhaustive.
/// Keep a fallback arm and use `..` when matching fields.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum BuildError {
    /// Required user-owned lifetimes exceed the supervisor's ownership budget.
    #[error("resource limit reached for {resource}: {limit}")]
    #[non_exhaustive]
    ResourceLimitReached {
        /// Stable resource name suitable for diagnostics.
        resource: &'static str,
        /// Reported supervisor-local limit.
        limit: usize,
    },
    /// A bounded capacity exceeds Taskvisor's structural async-capacity limit.
    #[error("{field} must not exceed {max}; got {value}")]
    #[non_exhaustive]
    CapacityTooLarge {
        /// Stable configuration field name.
        field: &'static str,
        /// Rejected value.
        value: usize,
        /// Largest value accepted by Taskvisor for this field.
        max: usize,
    },
    /// A required cleanup worker could not start during construction.
    ///
    /// Subscriber metadata callbacks have not run when this error is returned.
    #[error(
        "failed to start {component} worker {worker}: {kind:?} (raw OS error: {raw_os_error:?})"
    )]
    #[non_exhaustive]
    ThreadStartFailed {
        /// Stable build component name suitable for diagnostics.
        component: &'static str,
        /// Zero-based position in the worker startup batch.
        worker: usize,
        /// Portable I/O error category.
        kind: std::io::ErrorKind,
        /// Platform-specific OS error code, when one was available.
        raw_os_error: Option<i32>,
    },
}

impl BuildError {
    /// Stable category label for logs and metrics.
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            BuildError::ResourceLimitReached { .. } => "build_resource_limit_reached",
            BuildError::CapacityTooLarge { .. } => "build_capacity_too_large",
            BuildError::ThreadStartFailed { .. } => "build_thread_start_failed",
        }
    }
}

/// Failure of a supervisor lifecycle or management operation.
///
/// Runtime startup, static runs, dynamic management, outcome waiting, and shutdown use this type.
/// Task attempts use [`TaskError`] instead.
/// This enum and its data-carrying variants are non-exhaustive.
/// Keep a fallback arm and use `..` when matching fields.
///
/// # See also
///
/// - [`enum@Error`] combines runtime and feature-gated controller errors.
#[cfg_attr(
    feature = "controller",
    doc = "- [`ControllerError`](crate::ControllerError) covers controller availability and submission intake errors."
)]
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum RuntimeError {
    /// Runtime startup was requested without an active Tokio runtime.
    ///
    /// The supervisor remains stopped and startup may be retried from inside a Tokio runtime.
    #[error("runtime startup requires an active Tokio runtime")]
    TokioRuntimeUnavailable,

    /// A required Taskvisor worker thread could not start.
    ///
    /// The operation that needed the worker remains uncommitted.
    /// The source identifies the failed thread-creation or startup handshake.
    #[error("failed to start {component} thread: {source}")]
    #[non_exhaustive]
    ThreadStartFailed {
        /// Stable runtime component name suitable for diagnostics.
        component: &'static str,
        /// I/O error from thread creation or the transactional startup handshake.
        #[source]
        source: std::io::Error,
    },

    /// Task cleanup did not finish within the shared shutdown grace period.
    ///
    /// A listed name belongs to a force-aborted actor or to a removal owner still unfinished at the deadline.
    /// A force-aborted actor can stay physically active under Taskvisor ownership after this error is returned.
    #[error("shutdown timeout {grace:?} exceeded; logically force-aborted: {stuck:?}")]
    #[non_exhaustive]
    GraceExceeded {
        /// Configured shutdown grace duration.
        grace: Duration,
        /// Task names whose removal did not finish within the grace period.
        stuck: Vec<Arc<str>>,
    },

    /// A task name is already reserved or repeated in an atomic static batch.
    ///
    /// Registry membership and cleanup ownership of a physically active force-aborted actor can both reserve a name.
    #[error("task name '{name}' already exists")]
    #[non_exhaustive]
    TaskAlreadyExists {
        /// Conflicting task name.
        name: Arc<str>,
    },

    /// A configured runtime or user-lifetime budget was exhausted.
    #[error("resource limit reached for {resource}: {limit}")]
    #[non_exhaustive]
    ResourceLimitReached {
        /// Stable resource name used by diagnostics.
        resource: &'static str,
        /// Reported bound for the rejected resource.
        /// For `owned_user_lifetimes`, retired poisoned slots can reduce usable capacity below this value.
        limit: usize,
    },

    /// A fail-fast management call found its command queue full.
    ///
    /// The rejected request does not change task or controller-slot ownership.
    #[error("management command queue is full")]
    CommandQueueFull,

    /// The caller's bounded wait for cleanup ownership expired before command intake.
    ///
    /// No runtime or controller command was committed.
    /// The timeout covers only ownership admission.
    /// Later command-queue and registry waits remain unbounded by this setting.
    #[error("timeout waiting for ownership admission after {timeout:?}")]
    #[non_exhaustive]
    OwnershipAdmissionTimeout {
        /// Maximum duration allowed for ownership admission.
        timeout: Duration,
    },

    /// The caller's bounded wait for terminal registry cleanup expired.
    ///
    /// The stop request remains active.
    /// This error does not undo cancellation or change the supervisor's shutdown grace period.
    #[error("timeout waiting for task {id} termination after {timeout:?}")]
    #[non_exhaustive]
    TaskTerminationTimeout {
        /// Task whose terminal cleanup remained pending.
        id: TaskId,
        /// Wait duration before timing out.
        timeout: Duration,
    },

    /// A watched task or controller submission's direct outcome channel closed without a result.
    #[error("final outcome for task {id} is unavailable")]
    #[non_exhaustive]
    OutcomeUnavailable {
        /// Task identity whose outcome could not be delivered.
        id: TaskId,
    },

    /// Explicit operating-system signal setup failed.
    ///
    /// This can only come from [`Supervisor::run_with_os_signals`](crate::Supervisor::run_with_os_signals).
    /// Every caller joining that shared shutdown receives an equivalent source.
    #[error("failed to install shutdown signal handlers: {source}")]
    #[non_exhaustive]
    SignalSetupFailed {
        /// I/O error returned by signal registration.
        #[source]
        source: std::io::Error,
    },

    /// The runtime is shutting down and no longer accepts commands.
    #[error("supervisor is shutting down")]
    ShuttingDown,

    /// A static run tried to acquire an already owned or committed lifecycle.
    ///
    /// [`Supervisor::run`](crate::Supervisor::run),
    /// [`Supervisor::run_until`](crate::Supervisor::run_until), and
    /// [`Supervisor::run_with_os_signals`](crate::Supervisor::run_with_os_signals)
    /// share one single-shot lifecycle.
    #[error("supervisor run() was already started")]
    AlreadyRunning,
}

impl RuntimeError {
    /// Stable category label for logs and metrics.
    ///
    /// This label is not the same as `Display`.
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            RuntimeError::TokioRuntimeUnavailable => "runtime_tokio_runtime_unavailable",
            RuntimeError::ThreadStartFailed { .. } => "runtime_thread_start_failed",
            RuntimeError::GraceExceeded { .. } => "runtime_grace_exceeded",
            RuntimeError::TaskAlreadyExists { .. } => "runtime_task_already_exists",
            RuntimeError::ResourceLimitReached { .. } => "runtime_resource_limit_reached",
            RuntimeError::CommandQueueFull => "runtime_command_queue_full",
            RuntimeError::OwnershipAdmissionTimeout { .. } => "runtime_ownership_admission_timeout",
            RuntimeError::TaskTerminationTimeout { .. } => "runtime_task_termination_timeout",
            RuntimeError::OutcomeUnavailable { .. } => "runtime_outcome_unavailable",
            RuntimeError::SignalSetupFailed { .. } => "runtime_signal_setup_failed",
            RuntimeError::ShuttingDown => "runtime_shutting_down",
            RuntimeError::AlreadyRunning => "runtime_already_running",
        }
    }
}

/// Error returned by one [`Task`](crate::Task) attempt.
///
/// [`Fail`](Self::Fail) and [`Timeout`](Self::Timeout) are retry-eligible.
/// [`Fatal`](Self::Fatal) and [`Canceled`](Self::Canceled) stop the actor.
/// Restart policy and the retry limit still decide whether a retry-eligible error runs again.
/// This enum and its data-carrying variants are non-exhaustive.
/// Keep a fallback arm and use `..` when matching fields.
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum TaskError {
    /// A timeout was reported for this attempt.
    ///
    /// The runner creates this variant when its configured attempt deadline expires.
    /// Task code may also return it directly.
    /// It is retry-eligible.
    #[error("timed out after {timeout:?}")]
    #[non_exhaustive]
    Timeout {
        /// Duration reported for the timeout.
        timeout: Duration,
    },

    /// Permanent task failure that stops the actor.
    ///
    /// This category is never retried.
    #[error("fatal error (no retry): {reason}")]
    #[non_exhaustive]
    Fatal {
        /// Human-readable diagnostic reason.
        reason: String,
        /// Process-style exit code, when available.
        ///
        /// `None` means this was a logical error with no process exit code.
        exit_code: Option<i32>,
        /// Optional source preserved for error-chain inspection.
        #[source]
        source: Option<BoxError>,
    },

    /// Retry-eligible task failure.
    ///
    /// Restart policy and the retry limit still decide whether another attempt starts.
    #[error("execution failed: {reason}")]
    #[non_exhaustive]
    Fail {
        /// Human-readable diagnostic reason.
        reason: String,
        /// Process-style exit code, when available.
        ///
        /// `None` means this was a logical error with no process exit code.
        exit_code: Option<i32>,
        /// Optional source preserved for error-chain inspection.
        #[source]
        source: Option<BoxError>,
    },

    /// Cooperative cancellation.
    ///
    /// Return this after observing cancellation through [`TaskContext`](crate::TaskContext).
    #[error("context canceled")]
    Canceled,
}

impl TaskError {
    /// Retry-eligible failure without a source error.
    pub fn fail(reason: impl Into<String>) -> Self {
        TaskError::Fail {
            reason: reason.into(),
            exit_code: None,
            source: None,
        }
    }

    /// Permanent failure without a source error.
    pub fn fatal(reason: impl Into<String>) -> Self {
        TaskError::Fatal {
            reason: reason.into(),
            exit_code: None,
            source: None,
        }
    }

    /// Retry-eligible failure that preserves its source error.
    ///
    /// The display reason comes from `source.to_string()`.
    /// The original value remains available through [`std::error::Error::source`].
    pub fn fail_from<E>(source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        TaskError::Fail {
            reason: source.to_string(),
            exit_code: None,
            source: Some(Box::new(source)),
        }
    }

    /// Permanent failure that preserves its source error.
    ///
    /// Source preservation follows [`fail_from`](Self::fail_from).
    pub fn fatal_from<E>(source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        TaskError::Fatal {
            reason: source.to_string(),
            exit_code: None,
            source: Some(Box::new(source)),
        }
    }

    /// Retry-eligible timeout with the reported duration.
    #[must_use]
    pub const fn timeout(timeout: Duration) -> Self {
        TaskError::Timeout { timeout }
    }

    /// Process-style exit code for `Fail` or `Fatal`.
    ///
    /// Pass an integer to set it or `None` to clear it.
    /// Other variants are returned unchanged.
    #[must_use]
    pub fn with_exit_code(mut self, code: impl Into<Option<i32>>) -> Self {
        let code = code.into();
        if let TaskError::Fail { exit_code, .. } | TaskError::Fatal { exit_code, .. } = &mut self {
            *exit_code = code;
        }
        self
    }

    /// Source error for `Fail` or `Fatal`.
    ///
    /// No-op for `Timeout` and `Canceled`.
    #[must_use]
    pub fn with_source(mut self, source: impl Into<BoxError>) -> Self {
        if let TaskError::Fail { source: s, .. } | TaskError::Fatal { source: s, .. } = &mut self {
            *s = Some(source.into());
        }
        self
    }

    /// Owned source error, if one is present.
    #[must_use]
    pub fn into_source(self) -> Option<BoxError> {
        match self {
            TaskError::Fail { source, .. } | TaskError::Fatal { source, .. } => source,
            _ => None,
        }
    }

    /// Stable category label for logs and metrics.
    ///
    /// This label is not the same as `Display`.
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            TaskError::Timeout { .. } => "task_timeout",
            TaskError::Fatal { .. } => "task_fatal",
            TaskError::Fail { .. } => "task_failed",
            TaskError::Canceled => "task_canceled",
        }
    }

    /// Whether the error category is retry-eligible.
    ///
    /// The active restart policy and retry limit can still stop the task.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, TaskError::Timeout { .. } | TaskError::Fail { .. })
    }

    /// Whether the error is [`TaskError::Fatal`].
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        matches!(self, TaskError::Fatal { .. })
    }

    /// Process-style exit code attached to `Fail` or `Fatal`.
    #[must_use]
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            TaskError::Fatal { exit_code, .. } | TaskError::Fail { exit_code, .. } => *exit_code,
            TaskError::Timeout { .. } | TaskError::Canceled => None,
        }
    }
}

/// Error wrapper for code that combines runtime and controller operations.
///
/// [`RuntimeError`] and feature-gated `ControllerError` values convert into this type through `?`.
/// Match the variant to recover the original error.
///
/// ```rust
/// use taskvisor::{Error, RuntimeError};
///
/// fn stopped() -> Result<(), Error> {
///     Err(RuntimeError::ShuttingDown)?;
///     Ok(())
/// }
///
/// assert!(matches!(stopped(), Err(Error::Runtime(_))));
/// ```
///
/// Match with a wildcard arm because this enum is non-exhaustive.
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum Error {
    /// Error from runtime lifecycle, management, or outcome waiting.
    #[error(transparent)]
    Runtime(#[from] RuntimeError),

    /// Error from a controller availability check or submission command intake.
    ///
    /// Requires the `controller` feature.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    #[error(transparent)]
    Controller(#[from] crate::controller::ControllerError),
}

impl Error {
    /// Stable category label from the wrapped error.
    ///
    /// The wrapper does not introduce a second category.
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            Error::Runtime(e) => e.as_label(),
            #[cfg(feature = "controller")]
            Error::Controller(e) => e.as_label(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_resource_error_preserves_public_diagnostics() {
        let error = BuildError::ResourceLimitReached {
            resource: "owned_user_lifetimes",
            limit: 1024,
        };
        assert_eq!(error.as_label(), "build_resource_limit_reached");
        assert_eq!(
            error.to_string(),
            "resource limit reached for owned_user_lifetimes: 1024"
        );
    }

    #[test]
    fn build_capacity_error_preserves_public_diagnostics() {
        let error = BuildError::CapacityTooLarge {
            field: "subscriber_queue_capacity",
            value: 17,
            max: 16,
        };
        assert_eq!(error.as_label(), "build_capacity_too_large");
        assert_eq!(
            error.to_string(),
            "subscriber_queue_capacity must not exceed 16; got 17"
        );
    }

    #[test]
    fn build_thread_start_error_preserves_copyable_public_diagnostics() {
        let error = BuildError::ThreadStartFailed {
            component: "destructor_isolation",
            worker: 2,
            kind: std::io::ErrorKind::WouldBlock,
            raw_os_error: Some(11),
        };
        assert_eq!(error.as_label(), "build_thread_start_failed");
        assert_eq!(
            error.to_string(),
            "failed to start destructor_isolation worker 2: WouldBlock (raw OS error: Some(11))"
        );
        let copied = error;
        assert_eq!(copied, error);
    }

    #[test]
    fn runtime_error_labels_are_stable() {
        let id = TaskId::next();
        let cases = [
            (
                RuntimeError::TokioRuntimeUnavailable,
                "runtime_tokio_runtime_unavailable",
            ),
            (
                RuntimeError::ThreadStartFailed {
                    component: "subscriber_dispatch",
                    source: std::io::Error::other("worker unavailable"),
                },
                "runtime_thread_start_failed",
            ),
            (
                RuntimeError::GraceExceeded {
                    grace: Duration::from_secs(1),
                    stuck: vec![Arc::from("worker")],
                },
                "runtime_grace_exceeded",
            ),
            (
                RuntimeError::TaskAlreadyExists {
                    name: Arc::from("worker"),
                },
                "runtime_task_already_exists",
            ),
            (RuntimeError::CommandQueueFull, "runtime_command_queue_full"),
            (
                RuntimeError::OwnershipAdmissionTimeout {
                    timeout: Duration::from_millis(25),
                },
                "runtime_ownership_admission_timeout",
            ),
            (
                RuntimeError::TaskTerminationTimeout {
                    id,
                    timeout: Duration::from_secs(1),
                },
                "runtime_task_termination_timeout",
            ),
            (
                RuntimeError::OutcomeUnavailable { id },
                "runtime_outcome_unavailable",
            ),
            (
                RuntimeError::SignalSetupFailed {
                    source: std::io::Error::other("boom"),
                },
                "runtime_signal_setup_failed",
            ),
            (RuntimeError::ShuttingDown, "runtime_shutting_down"),
            (RuntimeError::AlreadyRunning, "runtime_already_running"),
        ];

        for (error, expected) in cases {
            assert_eq!(error.as_label(), expected, "{error:?}");
        }
    }

    #[test]
    fn runtime_error_displays_are_stable() {
        assert_eq!(
            RuntimeError::TokioRuntimeUnavailable.to_string(),
            "runtime startup requires an active Tokio runtime"
        );

        let startup = RuntimeError::ThreadStartFailed {
            component: "subscriber_dispatch",
            source: std::io::Error::new(std::io::ErrorKind::WouldBlock, "thread limit"),
        };
        assert_eq!(
            startup.to_string(),
            "failed to start subscriber_dispatch thread: thread limit"
        );
        assert_eq!(
            std::error::Error::source(&startup)
                .and_then(|source| source.downcast_ref::<std::io::Error>())
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::WouldBlock)
        );

        assert_eq!(
            RuntimeError::CommandQueueFull.to_string(),
            "management command queue is full"
        );
        let ownership = RuntimeError::OwnershipAdmissionTimeout {
            timeout: Duration::from_millis(25),
        };
        assert_eq!(
            ownership.to_string(),
            "timeout waiting for ownership admission after 25ms"
        );

        let id = TaskId::next();
        let error = RuntimeError::TaskTerminationTimeout {
            id,
            timeout: Duration::from_secs(1),
        };
        assert_eq!(
            error.to_string(),
            format!("timeout waiting for task {id} termination after 1s")
        );
    }

    #[test]
    fn timeout_constructor_is_const_and_preserves_payload_and_display() {
        const TIMEOUT: TaskError = TaskError::timeout(Duration::from_secs(1));

        assert!(matches!(
            &TIMEOUT,
            TaskError::Timeout { timeout, .. } if *timeout == Duration::from_secs(1)
        ));
        assert_eq!(TIMEOUT.to_string(), "timed out after 1s");
    }

    #[test]
    fn fail_constructor_preserves_reason_and_is_sourceless() {
        let e = TaskError::fail("logical");
        assert_eq!(e.to_string(), "execution failed: logical");
        assert!(std::error::Error::source(&e).is_none());
    }

    #[test]
    fn fail_from_preserves_source_chain_and_io_kind() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let e = TaskError::fail_from(io);

        assert!(e.to_string().contains("denied"));
        let src = std::error::Error::source(&e).expect("source must be present");
        let io_ref = src
            .downcast_ref::<std::io::Error>()
            .expect("source must downcast to the original io::Error");
        assert_eq!(io_ref.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn with_exit_code_and_with_source_builders_compose() {
        let io = std::io::Error::other("boom");
        let e = TaskError::fail("upload failed")
            .with_exit_code(13)
            .with_source(io);

        assert_eq!(e.exit_code(), Some(13));
        assert_eq!(e.to_string(), "execution failed: upload failed");
        assert!(std::error::Error::source(&e).is_some());
    }

    #[test]
    fn classification_and_exit_codes_cover_every_task_error_variant() {
        let dynamic: Option<i32> = None;
        let cases = [
            (
                "fail",
                TaskError::fail("x").with_exit_code(7),
                "task_failed",
                Some(7),
                true,
                false,
            ),
            (
                "fatal",
                TaskError::fatal("x").with_exit_code(137),
                "task_fatal",
                Some(137),
                false,
                true,
            ),
            (
                "optional exit code",
                TaskError::fail("y").with_exit_code(dynamic),
                "task_failed",
                None,
                true,
                false,
            ),
            (
                "timeout",
                TaskError::timeout(Duration::from_secs(1)),
                "task_timeout",
                None,
                true,
                false,
            ),
            (
                "canceled",
                TaskError::Canceled,
                "task_canceled",
                None,
                false,
                false,
            ),
        ];

        for (case, error, label, exit_code, retryable, fatal) in cases {
            assert_eq!(error.as_label(), label, "{case}");
            assert_eq!(error.exit_code(), exit_code, "{case}");
            assert_eq!(error.is_retryable(), retryable, "{case}");
            assert_eq!(error.is_fatal(), fatal, "{case}");
        }
    }

    #[test]
    fn fatal_from_is_fatal_and_carries_source() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let e = TaskError::fatal_from(io);

        assert!(e.is_fatal());
        assert!(!e.is_retryable());

        let src = std::error::Error::source(&e).expect("source present");
        assert_eq!(
            src.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn signal_setup_failed_exposes_io_source() {
        let io = std::io::Error::new(std::io::ErrorKind::AddrInUse, "in use");
        let e = RuntimeError::SignalSetupFailed { source: io };

        assert!(e.to_string().contains("in use"));

        let src = std::error::Error::source(&e).expect("source present");
        assert_eq!(
            src.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::AddrInUse
        );
    }
}
