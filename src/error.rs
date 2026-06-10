//! # Error types used by the taskvisor runtime and tasks.
//!
//! This module defines two main error enums:
//! - [`RuntimeError`] errors raised by the orchestration runtime itself.
//! - [`TaskError`] errors raised by individual task executions.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::identity::TaskId;

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
    /// The supervisor runtime is shutting down; the command channel is closed.
    #[error("supervisor is shutting down")]
    ShuttingDown,
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
            RuntimeError::ShuttingDown => "runtime_shutting_down",
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
    Fatal {
        /// Human-readable failure reason (rendered by `Display`).
        reason: String,
        /// Numeric exit code when the error came from a process-like runtime;
        /// `None` for logical errors with no process behind them.
        exit_code: Option<i32>,
    },

    /// Task execution failed but may succeed if retried.
    #[error("execution failed: {reason}")]
    Fail {
        /// Human-readable failure reason (rendered by `Display`).
        reason: String,
        /// Numeric exit code when the error came from a process-like runtime;
        /// `None` for logical errors with no process behind them.
        exit_code: Option<i32>,
    },

    /// Task was canceled due to shut down or parent cancellation.
    #[error("context canceled")]
    Canceled,
}

impl TaskError {
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
        let e = TaskError::Fail {
            reason: "x".into(),
            exit_code: Some(5),
        };
        assert_eq!(e.exit_code(), Some(5));
        assert!(e.is_retryable());
        assert!(!e.is_fatal());
    }

    #[test]
    fn exit_code_is_some_for_fatal_with_code() {
        let e = TaskError::Fatal {
            reason: "x".into(),
            exit_code: Some(137),
        };
        assert_eq!(e.exit_code(), Some(137));
        assert!(!e.is_retryable());
        assert!(e.is_fatal());
    }

    #[test]
    fn exit_code_is_none_for_logical_fail() {
        let e = TaskError::Fail {
            reason: "logical".into(),
            exit_code: None,
        };
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
        let e = TaskError::Fail {
            reason: "boom".into(),
            exit_code: Some(1),
        };
        assert_eq!(e.to_string(), "execution failed: boom");
    }
}
