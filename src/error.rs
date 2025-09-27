//! Error types used by the taskvisor runtime and tasks.
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
/// These represent failures in the orchestration system itself,
/// such as a shutdown sequence exceeding its grace period.
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
}

impl RuntimeError {
    /// Returns a short stable label (snake_case) for use in logs/metrics.
    ///
    /// # Example
    /// ```
    /// use taskvisor::RuntimeError;
    /// use std::time::Duration;
    ///
    /// let err = RuntimeError::GraceExceeded { grace: Duration::from_secs(5), stuck: vec![] };
    /// assert_eq!(err.as_label(), "runtime_grace_exceeded");
    /// ```
    pub fn as_label(&self) -> &'static str {
        match self {
            RuntimeError::GraceExceeded { .. } => "runtime_grace_exceeded",
        }
    }

    /// Returns a human-readable message with details about the error.
    pub fn as_message(&self) -> String {
        match self {
            RuntimeError::GraceExceeded { grace, stuck } => {
                format!("grace exceeded after {grace:?}; stuck tasks={stuck:?}")
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
    Timeout {
        /// The timeout duration that was exceeded.
        timeout: Duration,
    },

    /// Non-recoverable fatal error (should not be retried).
    #[error("fatal error (no retry): {error}")]
    Fatal {
        /// The underlying error message.
        error: String,
    },

    /// Task execution failed but may succeed if retried.
    #[error("execution failed: {error}")]
    Fail {
        /// The underlying error message.
        error: String,
    },

    /// Task was cancelled due to parent context shutdown.
    #[error("context cancelled")]
    Canceled,
}

impl TaskError {
    /// Returns a short stable label (snake_case) for use in logs/metrics.
    ///
    /// # Example
    /// ```
    /// use taskvisor::TaskError;
    /// use std::time::Duration;
    ///
    /// let err = TaskError::Timeout { timeout: Duration::from_secs(1) };
    /// assert_eq!(err.as_label(), "task_timeout");
    /// ```
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
    ///
    /// Returns `true` for [`TaskError::Fail`] and [`TaskError::Timeout`],
    /// `false` otherwise.
    ///
    /// # Example
    /// ```
    /// use taskvisor::TaskError;
    ///
    /// let retryable = TaskError::Fail { error: "boom".into() };
    /// assert!(retryable.is_retryable()); // true
    ///
    /// let fatal = TaskError::Fatal { error: "nope".into() };
    /// assert!(!fatal.is_retryable()); // false
    /// ```
    pub fn is_retryable(&self) -> bool {
        matches!(self, TaskError::Fail { .. } | TaskError::Timeout { .. })
    }
}
