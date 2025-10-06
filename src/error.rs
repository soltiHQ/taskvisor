//! # Error types used by the taskvisor runtime and tasks.
//!
//! This module defines two main error enums:
//!
//! - [`RuntimeError`] — errors raised by the orchestration runtime itself.
//! - [`TaskError`] — errors raised by individual task executions.
//!
//! Both types provide helper methods (`as_label`, `as_message`) for logging/metrics
//! and additional utilities such as [`TaskError::is_retryable`].

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

    /// Control channel to the orchestrator is closed (supervisor shutting down).
    #[error("supervisor control channel closed")]
    ControlChannelClosed,
}

impl RuntimeError {
    /// Returns a short stable label (snake_case) for use in logs/metrics.
    pub fn as_label(&self) -> &'static str {
        match self {
            RuntimeError::GraceExceeded { .. } => "runtime_grace_exceeded",
            RuntimeError::TaskAlreadyExists { .. } => "runtime_task_already_exists",
            RuntimeError::TaskNotFound { .. } => "runtime_task_not_found",
            RuntimeError::ControlChannelClosed => "runtime_control_channel_closed",
        }
    }

    /// Returns a human-readable message with details about the error.
    pub fn as_message(&self) -> String {
        match self {
            RuntimeError::GraceExceeded { grace, stuck } => {
                format!("grace exceeded after {grace:?}; stuck tasks={stuck:?}")
            }
            RuntimeError::TaskAlreadyExists { name } => {
                format!("task '{name}' already exists in registry")
            }
            RuntimeError::TaskNotFound { name } => {
                format!("task '{name}' not found in registry")
            }
            RuntimeError::ControlChannelClosed => {
                "supervisor control channel closed (shutting down?)".to_string()
            }
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
    #[error("fatal error (no retry): {error}")]
    Fatal { error: String },

    /// Task execution failed but may succeed if retried.
    #[error("execution failed: {error}")]
    Fail { error: String },

    /// Task was cancelled due to shutdown or parent cancellation.
    ///
    /// Returned when task detects `CancellationToken::is_cancelled()` and exits gracefully.
    /// This is **not an error** in traditional sense, but signals intentional termination.
    #[error("context cancelled")]
    Canceled,
}

impl TaskError {
    /// Returns a short stable label (snake_case) for use in logs/metrics.
    pub fn as_label(&self) -> &'static str {
        match self {
            TaskError::Timeout { .. } => "task_timeout",
            TaskError::Fatal { .. } => "task_fatal",
            TaskError::Fail { .. } => "task_failed",
            TaskError::Canceled => "task_canceled",
        }
    }

    /// Returns a human-readable message with details about the error.
    pub fn as_message(&self) -> String {
        match self {
            TaskError::Timeout { timeout } => format!("timeout: {timeout:?}"),
            TaskError::Fatal { error } => format!("fatal: {error}"),
            TaskError::Fail { error } => format!("error: {error}"),
            TaskError::Canceled => "context cancelled".to_string(),
        }
    }

    /// Indicates whether the error type is safe to retry.
    pub fn is_retryable(&self) -> bool {
        matches!(self, TaskError::Fail { .. } | TaskError::Timeout { .. })
    }
}
