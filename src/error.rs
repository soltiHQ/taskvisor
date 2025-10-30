//! # Error types used by the taskvisor runtime and tasks.
//!
//! This module defines two main error enums:
//!
//! - [`RuntimeError`] errors raised by the orchestration runtime itself.
//! - [`TaskError`] errors raised by individual task executions.
//!
//! Both types provide helper methods `as_label` for metrics.
//! [`TaskError`] has additional methods: `is_retryable()` and `is_fatal()`

use std::time::Duration;

use thiserror::Error;

/// # Errors produced by the taskvisor runtime.
///
/// These represent failures in the orchestration system itself.
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum RuntimeError {
    /// Shutdown grace period was exceeded; some tasks remained stuck and had to be force-terminated.
    #[error("shutdown timeout {grace:?} exceeded; stuck: {stuck:?}; forcing termination")]
    GraceExceeded {
        /// The configured grace duration.
        grace: Duration,
        /// List of task names that did not shut down in time.
        stuck: Vec<String>,
    },
    /// Attempted to add a task with a name that already exists in the registry.
    #[error("task '{name}' already exists in registry")]
    TaskAlreadyExists {
        /// The duplicate task name.
        name: String,
    },
    /// Attempted to remove a task that doesn't exist in the registry.
    #[error("task '{name}' not found in registry")]
    TaskNotFound {
        /// The missing task name.
        name: String,
    },
    /// Timeout waiting for task removal confirmation.
    #[error("timeout waiting for task '{name}' removal after {timeout:?}")]
    TaskRemoveTimeout {
        /// Task which timeout on cancel.
        name: String,
        // Task timeout duration.
        timeout: Duration,
    },
}

impl RuntimeError {
    /// Returns a short stable label (snake_case) for use in logs/metrics.
    pub fn as_label(&self) -> &'static str {
        match self {
            RuntimeError::GraceExceeded { .. } => "runtime_grace_exceeded",
            RuntimeError::TaskAlreadyExists { .. } => "runtime_task_already_exists",
            RuntimeError::TaskNotFound { .. } => "runtime_task_not_found",
            RuntimeError::TaskRemoveTimeout { .. } => "runtime_task_remove_timeout",
        }
    }
}

/// # Errors produced by task execution.
///
/// These represent failures of individual async tasks managed by the runtime.
/// Some errors are retryable (`Timeout`, `Fail`), others are considered fatal.
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum TaskError {
    /// Task execution exceeded its timeout duration.
    #[error("timed out after {timeout:?}")]
    Timeout { timeout: Duration },

    /// Non-recoverable fatal error (should not be retried).
    #[error("fatal error (no retry): {reason}")]
    Fatal { reason: String },

    /// Task execution failed but may succeed if retried.
    #[error("execution failed: {reason}")]
    Fail { reason: String },

    /// Task was canceled due to shut down or parent cancellation.
    ///
    /// This is **not an error** in traditional sense, but signals intentional termination.
    #[error("context canceled")]
    Canceled,
}

impl TaskError {
    /// Returns a short stable label.
    pub fn as_label(&self) -> &'static str {
        match self {
            TaskError::Timeout { .. } => "task_timeout",
            TaskError::Fatal { .. } => "task_fatal",
            TaskError::Fail { .. } => "task_failed",
            TaskError::Canceled => "task_canceled",
        }
    }

    /// Indicates whether the error type is safe to retry.
    pub fn is_retryable(&self) -> bool {
        matches!(self, TaskError::Timeout { .. } | TaskError::Fail { .. })
    }

    /// Indicates whether the error is fatal.
    pub fn is_fatal(&self) -> bool {
        matches!(self, TaskError::Fatal { .. })
    }
}

impl From<tokio::time::error::Elapsed> for TaskError {
    fn from(e: tokio::time::error::Elapsed) -> Self {
        TaskError::Fail {
            reason: e.to_string(),
        }
    }
}
