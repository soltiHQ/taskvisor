//! # Errors produced by the controller (feature = `controller`).
//!
//! [`ControllerError`] covers submission-path failures: the controller being
//! absent, its queue being full, or its channel being closed.

use thiserror::Error;

/// Errors returned by [`SupervisorHandle::submit`](crate::SupervisorHandle::submit)
/// and [`try_submit`](crate::SupervisorHandle::try_submit).
#[non_exhaustive]
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerError {
    /// Controller is not configured (builder didn't call `with_controller`).
    #[error("controller not configured")]
    NotConfigured,

    /// Submission queue is full (try again later or use async `submit`).
    #[error("submission queue full")]
    Full,

    /// Controller channel is closed (controller task died).
    #[error("controller channel closed")]
    Closed,

    /// Controller is already running (double `run()` call).
    #[error("controller already running")]
    AlreadyRunning,
}
