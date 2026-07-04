//! Error types for runtime orchestration and task execution.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::identity::TaskId;

/// Boxed source error carried by [`TaskError::Fail`] and [`TaskError::Fatal`].
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Shared source error carried on the completion plane.
pub type SharedError = Arc<dyn std::error::Error + Send + Sync + 'static>;

/// Errors produced by the supervisor runtime.
///
/// These are orchestration errors: add, remove, shutdown, and run failures.
/// They are separate from [`TaskError`], which is returned by task attempts.
///
/// Match with a wildcard arm because this enum is non-exhaustive.
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
    /// Some tasks did not stop in time and had to be force-terminated.
    #[error("shutdown timeout {grace:?} exceeded; stuck: {stuck:?}; forcing termination")]
    GraceExceeded {
        /// Configured shutdown grace duration.
        grace: Duration,
        /// Task names that did not stop in time.
        stuck: Vec<Arc<str>>,
    },

    /// A task with the same name is already registered.
    #[error("task '{name}' already exists in registry")]
    TaskAlreadyExists {
        /// Duplicate task name.
        name: Arc<str>,
    },

    /// Timed out while waiting for removal confirmation.
    #[error("timeout waiting for task {id} removal after {timeout:?}")]
    TaskRemoveTimeout {
        /// Task id that did not report removal in time.
        id: TaskId,
        /// Wait duration before timing out.
        timeout: Duration,
    },

    /// Timed out while waiting for registration confirmation.
    #[error("timeout waiting for task '{name}' registration after {timeout:?}")]
    TaskAddTimeout {
        /// Task name that was not confirmed in time.
        name: Arc<str>,
        /// Wait duration before timing out.
        timeout: Duration,
    },

    /// OS signal listener setup failed.
    ///
    /// Signal-based shutdown is unavailable.
    /// The original [`std::io::Error`] is preserved as the source.
    #[error("failed to install shutdown signal handlers: {source}")]
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
            RuntimeError::TaskRemoveTimeout { .. } => "runtime_task_remove_timeout",
            RuntimeError::TaskAddTimeout { .. } => "runtime_task_add_timeout",
            RuntimeError::SignalSetupFailed { .. } => "runtime_signal_setup_failed",
            RuntimeError::ShuttingDown => "runtime_shutting_down",
            RuntimeError::AlreadyRunning => "runtime_already_running",
        }
    }
}

/// Errors returned by a task attempt.
///
/// A task returns `Result<(), TaskError>` from [`Task::spawn`](crate::Task::spawn).
///
/// Restart behavior:
/// - [`Canceled`](Self::Canceled) is cooperative shutdown and is not retried,
/// - [`Fatal`](Self::Fatal) is not retryable,
/// - [`Timeout`](Self::Timeout) is retryable,
/// - [`Fail`](Self::Fail) is retryable.
///
/// Match with a wildcard arm because this enum is non-exhaustive.
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
    Timeout {
        /// Timeout duration that was exceeded.
        timeout: Duration,
    },

    /// Permanent task failure.
    ///
    /// This stops the actor and is not retried.
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
    /// The actor may restart after this error, depending on restart policy.
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

    /// Attaches a process-style exit code to `Fail` or `Fatal`.
    ///
    /// No-op for `Timeout` and `Canceled`.
    #[must_use]
    pub fn with_exit_code(mut self, code: impl Into<Option<i32>>) -> Self {
        let code = code.into();
        if let TaskError::Fail { exit_code, .. } | TaskError::Fatal { exit_code, .. } = &mut self {
            *exit_code = code;
        }
        self
    }

    /// Attaches a source error to `Fail` or `Fatal`.
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

    /// Returns `true` for errors that may be retried by restart policy.
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

/// Umbrella error for mixed supervisor and controller call chains.
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
    fn exit_code_is_some_for_fail_with_code() {
        let e = TaskError::fail("x").with_exit_code(5);
        assert_eq!(e.exit_code(), Some(5));
        assert!(e.is_retryable());
        assert!(!e.is_fatal());
    }

    #[test]
    fn exit_code_is_some_for_fatal_with_code() {
        let e = TaskError::fatal("x").with_exit_code(137);
        assert_eq!(e.exit_code(), Some(137));
        assert!(!e.is_retryable());
        assert!(e.is_fatal());
    }

    #[test]
    fn exit_code_is_none_for_logical_fail() {
        let e = TaskError::fail("logical");
        assert_eq!(e.exit_code(), None);
    }

    #[test]
    fn exit_code_is_none_for_timeout_and_canceled() {
        assert_eq!(
            TaskError::Timeout {
                timeout: Duration::from_secs(1),
            }
            .exit_code(),
            None,
        );
        assert_eq!(TaskError::Canceled.exit_code(), None);
    }

    #[test]
    fn display_still_renders_reason_only() {
        let e = TaskError::fail("boom").with_exit_code(1);
        assert_eq!(e.to_string(), "execution failed: boom");
    }

    #[test]
    fn fail_constructor_is_retryable_and_sourceless() {
        let e = TaskError::fail("logical");
        assert_eq!(e.to_string(), "execution failed: logical");
        assert!(e.is_retryable());
        assert_eq!(e.exit_code(), None);
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
    fn with_exit_code_accepts_bare_and_option() {
        assert_eq!(TaskError::fail("x").with_exit_code(7).exit_code(), Some(7));

        let dynamic: Option<i32> = None;
        assert_eq!(
            TaskError::fail("y").with_exit_code(dynamic).exit_code(),
            None
        );
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
