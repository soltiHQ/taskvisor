//! # Error types used by the taskvisor runtime and tasks.
//!
//! This module defines two main error enums:
//! - [`RuntimeError`] errors raised by the orchestration runtime itself.
//! - [`TaskError`] errors raised by individual task executions.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::identity::TaskId;

/// Boxed, type-erased source error carried by [`TaskError`] failure variants.
///
/// Failures cross task/thread boundaries and chain into `anyhow`/`eyre` via [`std::error::Error::source`].
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Shared, type-erased source error carried on the completion plane ([`TaskOutcome`](crate::TaskOutcome)).
pub type SharedError = Arc<dyn std::error::Error + Send + Sync + 'static>;

/// # Errors produced by the taskvisor runtime.
///
/// These represent failures in the orchestration system itself.
///
/// # Also
///
/// - [`Supervisor`](crate::Supervisor) - returns `RuntimeError` from [`run`](crate::Supervisor::run)
/// - [`SupervisorHandle`](crate::SupervisorHandle) - returns `RuntimeError` from management methods
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum RuntimeError {
    /// Shutdown grace period was exceeded; some tasks remained stuck and had to be force-terminated.
    #[error("shutdown timeout {grace:?} exceeded; stuck: {stuck:?}; forcing termination")]
    GraceExceeded {
        /// The configured grace duration.
        grace: Duration,
        /// List of task names that did not shut down in time.
        stuck: Vec<Arc<str>>,
    },
    /// Attempted to add a task with a name that already exists in the registry.
    #[error("task '{name}' already exists in registry")]
    TaskAlreadyExists {
        /// The duplicate task name.
        name: Arc<str>,
    },
    /// A cancelled task did not confirm termination within the wait window.
    #[error("timeout waiting for task {id} removal after {timeout:?}")]
    TaskRemoveTimeout {
        /// The runtime identity that did not stop in time.
        id: TaskId,
        /// How long we waited before giving up.
        timeout: Duration,
    },
    /// Timeout waiting for task registration confirmation.
    #[error("timeout waiting for task '{name}' registration after {timeout:?}")]
    TaskAddTimeout {
        /// The task name that was not registered in time.
        name: Arc<str>,
        /// How long we waited.
        timeout: Duration,
    },
    /// OS signal-listener registration failed; signal-based shutdown is unavailable.
    #[error("failed to install shutdown signal handlers: {source}")]
    SignalSetupFailed {
        /// The underlying I/O error from signal registration (preserves [`ErrorKind`](std::io::ErrorKind)).
        #[source]
        source: std::io::Error,
    },
    /// The supervisor runtime is shutting down; the command channel is closed.
    #[error("supervisor is shutting down")]
    ShuttingDown,
    /// [`run`](crate::Supervisor::run) was called more than once on the same supervisor; it is single-shot.
    #[error("supervisor run() was already started")]
    AlreadyRunning,
}

impl RuntimeError {
    /// Returns a short stable label (snake_case) for use in logs/metrics.
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

/// # Errors produced by task execution.
///
/// These represent failures of individual async tasks managed by the runtime.
/// Some errors are retryable (`Timeout`, `Fail`), others are considered fatal.
///
/// # Also
///
/// - [`Task`](crate::Task) - trait whose [`spawn`](crate::Task::spawn) returns `Result<(), TaskError>`
/// - [`RestartPolicy`](crate::RestartPolicy) - determines restart behavior based on error variant
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum TaskError {
    /// Task execution exceeded its timeout duration.
    #[error("timed out after {timeout:?}")]
    Timeout { timeout: Duration },

    /// Non-recoverable fatal error (should not be retried).
    #[error("fatal error (no retry): {reason}")]
    #[non_exhaustive]
    Fatal {
        /// Human-readable failure reason (rendered by `Display`).
        reason: String,
        /// Numeric exit code when the error came from a process-like runtime;
        /// `None` for logical errors with no process behind them.
        exit_code: Option<i32>,
        /// Underlying error, preserved for [`source`](std::error::Error::source) chains.
        #[source]
        source: Option<BoxError>,
    },

    /// Task execution failed but may succeed if retried.
    #[error("execution failed: {reason}")]
    #[non_exhaustive]
    Fail {
        /// Human-readable failure reason (rendered by `Display`).
        reason: String,
        /// Numeric exit code when the error came from a process-like runtime;
        /// `None` for logical errors with no process behind them.
        exit_code: Option<i32>,
        /// Underlying error, preserved for [`source`](std::error::Error::source) chains.
        #[source]
        source: Option<BoxError>,
    },

    /// Task was canceled due to shut down or parent cancellation.
    #[error("context canceled")]
    Canceled,
}

impl TaskError {
    /// A retryable failure with a human-readable reason and no underlying error.
    pub fn fail(reason: impl Into<String>) -> Self {
        TaskError::Fail {
            reason: reason.into(),
            exit_code: None,
            source: None,
        }
    }

    /// A permanent (non-retryable) failure with a human-readable reason and no underlying error.
    pub fn fatal(reason: impl Into<String>) -> Self {
        TaskError::Fatal {
            reason: reason.into(),
            exit_code: None,
            source: None,
        }
    }

    /// A retryable failure wrapping an underlying error.
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

    /// A permanent failure wrapping an underlying error (see [`fail_from`](Self::fail_from)).
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

    /// Builder: attach a process-style exit code to a [`Fail`](Self::Fail)/[`Fatal`](Self::Fatal) error.
    #[must_use]
    pub fn with_exit_code(mut self, code: impl Into<Option<i32>>) -> Self {
        let code = code.into();
        if let TaskError::Fail { exit_code, .. } | TaskError::Fatal { exit_code, .. } = &mut self {
            *exit_code = code;
        }
        self
    }

    /// Builder: attach an underlying source error to a [`Fail`](Self::Fail)/[`Fatal`](Self::Fatal) error.
    #[must_use]
    pub fn with_source(mut self, source: impl Into<BoxError>) -> Self {
        if let TaskError::Fail { source: s, .. } | TaskError::Fatal { source: s, .. } = &mut self {
            *s = Some(source.into());
        }
        self
    }

    /// Consumes the error and returns its underlying source, if any.
    #[must_use]
    pub fn into_source(self) -> Option<BoxError> {
        match self {
            TaskError::Fail { source, .. } | TaskError::Fatal { source, .. } => source,
            _ => None,
        }
    }

    /// Returns a short stable label.
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            TaskError::Timeout { .. } => "task_timeout",
            TaskError::Fatal { .. } => "task_fatal",
            TaskError::Fail { .. } => "task_failed",
            TaskError::Canceled => "task_canceled",
        }
    }

    /// Indicates whether the error type is safe to retry.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, TaskError::Timeout { .. } | TaskError::Fail { .. })
    }

    /// Indicates whether the error is fatal.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        matches!(self, TaskError::Fatal { .. })
    }

    /// Numeric exit code when the error originated from a process-like runtime.
    /// `None` for `Timeout`, `Canceled`, and for logical `Fail`/`Fatal` errors that have no process behind them.
    #[must_use]
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            TaskError::Fatal { exit_code, .. } | TaskError::Fail { exit_code, .. } => *exit_code,
            TaskError::Timeout { .. } | TaskError::Canceled => None,
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
        use std::time::Duration;
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
