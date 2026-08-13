//! Reports failures before a controller submission enters slot admission.
//!
//! [`SupervisorHandle::prepare_submission`](crate::SupervisorHandle::prepare_submission)
//! checks that the supervisor has a controller before creating a local
//! [`PreparedSubmission`](crate::PreparedSubmission). Controller `submit*`
//! methods can return active [`ControllerError`] variants at command intake.
//!
//! Slot admission and runtime registration happen after command intake. Their
//! rejections are not `ControllerError` values. A watched submission reports
//! them as [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected) through its
//! [`TaskWaiter`](crate::TaskWaiter).

use thiserror::Error;

/// A failure before controller slot admission.
///
/// [`NotConfigured`](Self::NotConfigured) can come from preparation or
/// submission. The active variants below describe submission command intake;
/// [`AlreadyStarted`](Self::AlreadyStarted) remains only for compatibility.
/// `Ok` from preparation creates a local prepared value. `Ok` from a submit
/// method confirms command intake. Neither result confirms slot admission.
///
/// Use the error variants as follows:
///
/// - [`NotConfigured`](Self::NotConfigured) means the builder did not install a
///   controller.
/// - [`Full`](Self::Full) is fail-fast command-queue backpressure; the async
///   submit form waits for that capacity.
/// - [`ResourceLimit`](Self::ResourceLimit) and
///   [`ThreadStartFailed`](Self::ThreadStartFailed) describe cleanup ownership
///   that Taskvisor could not reserve before intake.
/// - [`Closed`](Self::Closed) means controller shutdown has closed intake.
///
/// Match the enum with a wildcard arm because it is non-exhaustive. Use `..`
/// when matching a data-carrying variant.
#[non_exhaustive]
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerError {
    /// No controller was installed on this supervisor.
    ///
    /// Configure one with
    /// [`SupervisorBuilder::with_controller`](crate::SupervisorBuilder::with_controller)
    /// before building the supervisor.
    #[error("controller not configured")]
    NotConfigured,

    /// The ordered controller command queue has no capacity now.
    ///
    /// Only fail-fast `try_submit*` methods return this variant. It describes
    /// command intake, not a full per-slot queue. The corresponding async method
    /// waits for command capacity.
    #[error("submission queue full")]
    Full,

    /// Taskvisor cannot reserve another bounded user-value cleanup lifetime.
    ///
    /// The fields identify the resource and its configured limit.
    #[error("controller resource limit reached: {resource} (limit: {limit})")]
    #[non_exhaustive]
    ResourceLimit {
        /// Stable resource name suitable for logs and metrics.
        resource: &'static str,
        /// Configured hard limit for the resource.
        limit: usize,
    },

    /// Taskvisor could not start a cleanup worker required before intake.
    ///
    /// The fields preserve the worker identity and the available operating
    /// system error details.
    #[error(
        "failed to start {component} worker {worker} during controller submission: {kind:?} (raw OS error: {raw_os_error:?})"
    )]
    #[non_exhaustive]
    ThreadStartFailed {
        /// Stable component name suitable for logs and metrics.
        component: &'static str,
        /// Zero-based worker index reported by the component.
        worker: usize,
        /// Portable I/O error category.
        kind: std::io::ErrorKind,
        /// Platform-specific OS error code, when one was available.
        raw_os_error: Option<i32>,
    },

    /// Controller intake closed before accepting the command.
    ///
    /// The controller loop is stopping or has stopped.
    #[error("controller channel closed")]
    Closed,

    /// A compatibility variant for the former fallible controller-start guard.
    ///
    /// Current controller startup does not return this variant. It remains
    /// available for source compatibility.
    #[error("controller already started")]
    AlreadyStarted,
}

impl ControllerError {
    /// Returns the stable variant label for logs and metrics.
    ///
    /// The label is distinct from the human-readable [`Display`](std::fmt::Display)
    /// message.
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            ControllerError::AlreadyStarted => "controller_already_started",
            ControllerError::NotConfigured => "controller_not_configured",
            ControllerError::Closed => "controller_closed",
            ControllerError::Full => "controller_full",
            ControllerError::ResourceLimit { .. } => "controller_resource_limit",
            ControllerError::ThreadStartFailed { .. } => "controller_thread_start_failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ControllerError;
    use crate::core::deferred_drop::OWNERSHIP_RESOURCE;

    #[test]
    fn public_error_labels_cover_every_public_variant() {
        for (error, expected) in [
            (ControllerError::NotConfigured, "controller_not_configured"),
            (ControllerError::Full, "controller_full"),
            (
                ControllerError::ResourceLimit {
                    resource: OWNERSHIP_RESOURCE,
                    limit: 1024,
                },
                "controller_resource_limit",
            ),
            (
                ControllerError::ThreadStartFailed {
                    component: "destructor_isolation",
                    worker: 2,
                    kind: std::io::ErrorKind::PermissionDenied,
                    raw_os_error: Some(13),
                },
                "controller_thread_start_failed",
            ),
            (ControllerError::Closed, "controller_closed"),
            (
                ControllerError::AlreadyStarted,
                "controller_already_started",
            ),
        ] {
            assert_eq!(error.as_label(), expected);
        }
    }

    #[test]
    fn resource_limit_preserves_resource_and_limit() {
        let error = ControllerError::ResourceLimit {
            resource: OWNERSHIP_RESOURCE,
            limit: 1024,
        };
        assert_eq!(
            error.to_string(),
            "controller resource limit reached: owned_user_lifetimes (limit: 1024)"
        );
        let ControllerError::ResourceLimit {
            resource, limit, ..
        } = error
        else {
            unreachable!("the constructed variant is a resource limit")
        };
        assert_eq!(resource, OWNERSHIP_RESOURCE);
        assert_eq!(limit, 1024);
    }

    #[test]
    fn thread_start_failure_preserves_copyable_io_details() {
        let error = ControllerError::ThreadStartFailed {
            component: "destructor_isolation",
            worker: 2,
            kind: std::io::ErrorKind::PermissionDenied,
            raw_os_error: Some(13),
        };
        assert_eq!(
            error.to_string(),
            "failed to start destructor_isolation worker 2 during controller submission: PermissionDenied (raw OS error: Some(13))"
        );
        let ControllerError::ThreadStartFailed {
            component,
            worker,
            kind,
            raw_os_error,
            ..
        } = error
        else {
            unreachable!("the constructed variant is a thread-start failure")
        };
        assert_eq!(component, "destructor_isolation");
        assert_eq!(worker, 2);
        assert_eq!(kind, std::io::ErrorKind::PermissionDenied);
        assert_eq!(raw_os_error, Some(13));
    }
}
