//! # Error model
//!
//! Taskvisor separates errors by where they happen:
//!
//! ```text
//! task attempt ----------------------> TaskError
//! add / cancel / shutdown / run ----> RuntimeError
//! controller command path ----------> ControllerError (feature `controller`)
//! mixed runtime + controller API ---> Error
//! ```
//!
//! [`TaskError`] is part of task behavior. It tells the supervisor whether a
//! failed attempt may be retried. [`RuntimeError`] and controller errors report
//! that a management operation could not complete.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::identity::TaskId;

/// Owned source error stored in [`TaskError::Fail`] or [`TaskError::Fatal`].
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Shared source error stored in final task outcomes.
///
/// It uses [`Arc`] so a [`TaskOutcome`](crate::TaskOutcome) can be cloned without
/// requiring the original source error to implement `Clone`.
pub type SharedError = Arc<dyn std::error::Error + Send + Sync + 'static>;

/// Errors from supervisor lifecycle and management operations.
///
/// These errors come from add, remove, cancel, shutdown, and run operations.
/// They are separate from [`TaskError`], which comes from a task attempt.
///
/// Match with a wildcard arm because this enum is non-exhaustive. Data-carrying
/// variants are also non-exhaustive, so include `..` when matching their fields.
///
/// # Also
///
/// - [`Supervisor`](crate::Supervisor) - returns `RuntimeError` from [`run`](crate::Supervisor::run)
/// - [`SupervisorHandle`](crate::SupervisorHandle) - returns `RuntimeError` from management methods
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum RuntimeError {
    /// Shutdown grace period was exceeded.
    ///
    /// Some managed task runners did not stop in time and were force-terminated.
    /// Work detached by user code is outside this guarantee.
    #[error("shutdown timeout {grace:?} exceeded; stuck: {stuck:?}; forcing termination")]
    #[non_exhaustive]
    GraceExceeded {
        /// Configured shutdown grace duration.
        grace: Duration,
        /// Task names that did not stop in time.
        stuck: Vec<Arc<str>>,
    },

    /// A task name is already registered or repeated in an all-or-nothing batch.
    #[error("task name '{name}' already exists")]
    #[non_exhaustive]
    TaskAlreadyExists {
        /// Duplicate task name.
        name: Arc<str>,
    },

    /// A bounded management command queue has no free capacity.
    ///
    /// A fail-fast management call could not enqueue its requested state change.
    /// That request does not change task or slot ownership.
    #[error("management command queue is full")]
    CommandQueueFull,

    /// Timed out while waiting for terminal registry cleanup.
    ///
    /// The stop request remains active. This error only ends the caller's wait;
    /// it does not undo cancellation or change the supervisor grace period.
    #[error("timeout waiting for task {id} termination after {timeout:?}")]
    #[non_exhaustive]
    TaskTerminationTimeout {
        /// Task id whose terminal cleanup did not finish in time.
        id: TaskId,
        /// Wait duration before timing out.
        timeout: Duration,
    },

    /// Compatibility spelling for [`TaskTerminationTimeout`](Self::TaskTerminationTimeout).
    ///
    /// The runtime no longer returns this variant. Match
    /// [`TaskTerminationTimeout`](Self::TaskTerminationTimeout) for cancellation timeouts.
    #[deprecated(
        note = "renamed to TaskTerminationTimeout; this compatibility variant is never returned"
    )]
    #[error("timeout waiting for task {id} removal after {timeout:?}")]
    #[non_exhaustive]
    TaskRemoveTimeout {
        /// Task id whose terminal cleanup did not finish in time.
        id: TaskId,
        /// Wait duration before timing out.
        timeout: Duration,
    },

    /// OS signal listener setup failed.
    ///
    /// Signal-based shutdown is unavailable.
    /// The I/O error kind, message, and source chain are preserved for callers
    /// that join the shared shutdown operation.
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

    /// [`Supervisor::run`](crate::Supervisor::run) was called more than once.
    ///
    /// A supervisor run is single-shot.
    #[error("supervisor run() was already started")]
    AlreadyRunning,
}

impl RuntimeError {
    /// Returns a stable machine-readable label for logs and metrics.
    ///
    /// This label is not the same as `Display`.
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            RuntimeError::GraceExceeded { .. } => "runtime_grace_exceeded",
            RuntimeError::TaskAlreadyExists { .. } => "runtime_task_already_exists",
            RuntimeError::CommandQueueFull => "runtime_command_queue_full",
            RuntimeError::TaskTerminationTimeout { .. } => "runtime_task_termination_timeout",
            #[allow(deprecated)]
            RuntimeError::TaskRemoveTimeout { .. } => "runtime_task_remove_timeout",
            RuntimeError::SignalSetupFailed { .. } => "runtime_signal_setup_failed",
            RuntimeError::ShuttingDown => "runtime_shutting_down",
            RuntimeError::AlreadyRunning => "runtime_already_running",
        }
    }
}

/// Result categories that a task attempt can return.
///
/// A task returns `Result<(), TaskError>` from [`Task::spawn`](crate::Task::spawn).
///
/// | Variant                    | Retry category | Meaning                              |
/// |----------------------------|----------------|--------------------------------------|
/// | [`Canceled`](Self::Canceled) | never          | cooperative stop                     |
/// | [`Fatal`](Self::Fatal)       | never          | permanent failure                    |
/// | [`Timeout`](Self::Timeout)   | eligible       | per-attempt time limit was exceeded  |
/// | [`Fail`](Self::Fail)         | eligible       | temporary or unknown failure         |
///
/// "Eligible" does not guarantee a retry. The restart policy and retry limit
/// must also allow it. In builds that unwind panics, a panic in the task body is
/// caught and converted to [`Fail`](Self::Fail). A build with `panic = "abort"`
/// exits the process instead.
///
/// Match with a wildcard arm because this enum is non-exhaustive. Data-carrying
/// variants are also non-exhaustive, so include `..` when matching their fields.
///
/// # Also
///
/// - [`Task`](crate::Task) - task trait
/// - [`RestartPolicy`](crate::RestartPolicy) - decides whether retryable errors restart
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum TaskError {
    /// The attempt exceeded its configured timeout.
    #[error("timed out after {timeout:?}")]
    #[non_exhaustive]
    Timeout {
        /// Timeout duration that was exceeded.
        timeout: Duration,
    },

    /// Permanent task failure.
    ///
    /// This stops the managed task and is not retried.
    #[error("fatal error (no retry): {reason}")]
    #[non_exhaustive]
    Fatal {
        /// Human-readable failure reason.
        reason: String,
        /// Process-style exit code, when available.
        ///
        /// `None` means this was a logical error with no process exit code.
        exit_code: Option<i32>,
        /// Underlying error preserved for source chains.
        #[source]
        source: Option<BoxError>,
    },

    /// Retryable task failure.
    ///
    /// Taskvisor may restart after this error if the restart policy and retry
    /// limit allow it.
    #[error("execution failed: {reason}")]
    #[non_exhaustive]
    Fail {
        /// Human-readable failure reason.
        reason: String,
        /// Process-style exit code, when available.
        ///
        /// `None` means this was a logical error with no process exit code.
        exit_code: Option<i32>,
        /// Underlying error preserved for source chains.
        #[source]
        source: Option<BoxError>,
    },

    /// Cooperative cancellation.
    ///
    /// Tasks should return this when they stop because [`TaskContext`](crate::TaskContext)
    /// was cancelled.
    #[error("context canceled")]
    Canceled,
}

impl TaskError {
    /// Creates a retry-eligible timeout error.
    #[must_use]
    pub const fn timeout(timeout: Duration) -> Self {
        TaskError::Timeout { timeout }
    }

    /// Creates a retryable failure with no source error.
    pub fn fail(reason: impl Into<String>) -> Self {
        TaskError::Fail {
            reason: reason.into(),
            exit_code: None,
            source: None,
        }
    }

    /// Creates a permanent failure with no source error.
    pub fn fatal(reason: impl Into<String>) -> Self {
        TaskError::Fatal {
            reason: reason.into(),
            exit_code: None,
            source: None,
        }
    }

    /// Creates a retryable failure from a source error.
    ///
    /// The display reason is copied from `source.to_string()`.
    /// The original error remains available through [`std::error::Error::source`].
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

    /// Creates a permanent failure from a source error.
    ///
    /// The display reason is copied from `source.to_string()`.
    /// The original error remains available through [`std::error::Error::source`].
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

    /// Sets or clears the process-style exit code on `Fail` or `Fatal`.
    ///
    /// Pass an integer to set it or `None` to clear it. This has no effect on
    /// `Timeout` or `Canceled`.
    #[must_use]
    pub fn with_exit_code(mut self, code: impl Into<Option<i32>>) -> Self {
        let code = code.into();
        if let TaskError::Fail { exit_code, .. } | TaskError::Fatal { exit_code, .. } = &mut self {
            *exit_code = code;
        }
        self
    }

    /// Sets a source error on `Fail` or `Fatal`.
    ///
    /// No-op for `Timeout` and `Canceled`.
    #[must_use]
    pub fn with_source(mut self, source: impl Into<BoxError>) -> Self {
        if let TaskError::Fail { source: s, .. } | TaskError::Fatal { source: s, .. } = &mut self {
            *s = Some(source.into());
        }
        self
    }

    /// Consumes the error and returns its boxed source, if any.
    #[must_use]
    pub fn into_source(self) -> Option<BoxError> {
        match self {
            TaskError::Fail { source, .. } | TaskError::Fatal { source, .. } => source,
            _ => None,
        }
    }

    /// Returns a stable machine-readable label for logs and metrics.
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

    /// Returns `true` for a retry-eligible error category.
    ///
    /// The active restart policy and retry limit can still stop the task.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, TaskError::Timeout { .. } | TaskError::Fail { .. })
    }

    /// Returns `true` for [`TaskError::Fatal`].
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        matches!(self, TaskError::Fatal { .. })
    }

    /// Returns the process-style exit code, if one was attached.
    #[must_use]
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            TaskError::Fatal { exit_code, .. } | TaskError::Fail { exit_code, .. } => *exit_code,
            TaskError::Timeout { .. } | TaskError::Canceled => None,
        }
    }
}

/// Umbrella error for code that mixes runtime and controller operations.
///
/// [`RuntimeError`] and `ControllerError` (feature `controller`) convert into this type with `?`.
/// Match on the variant to get the original error back.
///
/// Use it when one function mixes `add*` and `submit*` calls:
///
/// ```rust
/// use taskvisor::{Error, RuntimeError};
///
/// fn stop() -> Result<(), Error> {
///     Err(RuntimeError::ShuttingDown)? // `?` converts automatically
/// }
///
/// assert!(matches!(stop(), Err(Error::Runtime(_))));
/// ```
///
/// Match with a wildcard arm because this enum is non-exhaustive.
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum Error {
    /// Core runtime error from `run`, `add*`, waiters, and management methods.
    #[error(transparent)]
    Runtime(#[from] RuntimeError),

    /// Controller submission error from `submit*` methods.
    ///
    /// Requires the `controller` feature.
    #[cfg(feature = "controller")]
    #[error(transparent)]
    Controller(#[from] crate::controller::ControllerError),
}

impl Error {
    /// Returns a stable machine-readable label for logs and metrics.
    ///
    /// The label comes from the wrapped error.
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
    #[allow(deprecated)]
    fn runtime_error_labels_are_stable() {
        let id = TaskId::next();
        let cases = [
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
                RuntimeError::TaskTerminationTimeout {
                    id,
                    timeout: Duration::from_secs(1),
                },
                "runtime_task_termination_timeout",
            ),
            (
                RuntimeError::TaskRemoveTimeout {
                    id,
                    timeout: Duration::from_secs(1),
                },
                "runtime_task_remove_timeout",
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
            RuntimeError::CommandQueueFull.to_string(),
            "management command queue is full"
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
