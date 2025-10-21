use thiserror::Error;

/// Error returned by [`Supervisor::submit`](crate::Supervisor::submit).
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    /// Controller is not configured (builder didn't call `with_controller`).
    #[error("controller not configured")]
    NotConfigured,

    /// Submission queue is full (try again later or use async `submit`).
    #[error("submission queue full")]
    Full,

    /// Controller channel is closed (controller task died).
    #[error("controller channel closed")]
    Closed,
}
