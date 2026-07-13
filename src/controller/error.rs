//! # Controller API errors
//!
//! Most of these errors mean that a request was not accepted by the controller
//! command path. [`ControllerError::AlreadyStarted`] is a controller lifecycle
//! guard. These errors are not task outcomes. A request accepted by the
//! controller can still be rejected later by a slot policy; use
//! `submit_and_watch` when you need that final result.

use thiserror::Error;

/// Error from controller setup or command admission.
///
/// Include a wildcard arm when matching because new errors may be added.
#[non_exhaustive]
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerError {
    /// The supervisor was built without a controller.
    ///
    /// Enable the `controller` feature and configure the supervisor with `with_controller(...)` before using controller submission methods.
    #[error("controller not configured")]
    NotConfigured,

    /// The ordered controller command queue is full.
    ///
    /// Returned only by `try_submit` and `try_submit_and_watch`. Use async
    /// `submit` or `submit_and_watch` to wait for command capacity.
    #[error("submission queue full")]
    Full,

    /// The controller command channel is closed.
    ///
    /// This usually means the controller loop has stopped or the supervisor is shutting down.
    #[error("controller channel closed")]
    Closed,

    /// The controller loop was started more than once.
    ///
    /// This is a lifecycle guard. Normal submission APIs do not return it.
    #[error("controller already started")]
    AlreadyStarted,
}

impl ControllerError {
    /// Returns a short stable label for logs and metrics.
    ///
    /// The label is not the same as `Display`.
    /// It is intended for machine-readable dimensions.
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            ControllerError::AlreadyStarted => "controller_already_started",
            ControllerError::NotConfigured => "controller_not_configured",
            ControllerError::Closed => "controller_closed",
            ControllerError::Full => "controller_full",
        }
    }
}
