use std::time::Duration;

use thiserror::Error;

#[non_exhaustive]
#[derive(Error, Debug)]
pub enum RuntimeError {
    #[error("shutdown timeout {grace:?} exceeded; stuck: {stuck:?}; forcing termination")]
    GraceExceeded { grace: Duration, stuck: Vec<String> },
}

impl RuntimeError {
    pub fn as_label(&self) -> &'static str {
        match self {
            RuntimeError::GraceExceeded { .. } => "runtime_grace_exceeded",
        }
    }
    pub fn as_message(&self) -> String {
        match self {
            RuntimeError::GraceExceeded { grace, stuck } => {
                format!("grace exceeded after {grace:?}; stuck tasks={stuck:?}")
            }
        }
    }
}

#[non_exhaustive]
#[derive(Error, Debug)]
pub enum TaskError {
    #[error("timed out after {timeout:?}")]
    Timeout { timeout: Duration },
    #[error("fatal error (no retry): {error}")]
    Fatal { error: String },
    #[error("execution failed: {error}")]
    Fail { error: String },
    #[error("context cancelled")]
    Canceled,
}

impl TaskError {
    pub fn as_label(&self) -> &'static str {
        match self {
            TaskError::Timeout { .. } => "task_timeout",
            TaskError::Fatal { .. } => "task_fatal",
            TaskError::Fail { .. } => "task_failed",
            TaskError::Canceled => "task_canceled",
        }
    }

    pub fn as_message(&self) -> String {
        match self {
            TaskError::Timeout { timeout } => format!("timeout: {timeout:?}"),
            TaskError::Fatal { error } => format!("fatal: {error}"),
            TaskError::Fail { error } => format!("error: {error}"),
            TaskError::Canceled => "context cancelled".to_string(),
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, TaskError::Fail { .. } | TaskError::Timeout { .. })
    }
}
