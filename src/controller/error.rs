//! Reports failures before a controller submission enters slot admission.

use std::time::Duration;

use thiserror::Error;

/// A failure before controller slot admission.
///
/// [`NotConfigured`](Self::NotConfigured) can come from preparation or submission.
/// Other variants describe submission command intake.
/// `Ok` from preparation creates a local prepared value.
/// `Ok` from a submission terminal confirms command intake.
/// Neither result confirms slot admission.
///
/// Use the error variants as follows:
///
/// - [`NotConfigured`](Self::NotConfigured) means the builder did not install a controller.
/// - [`Full`](Self::Full) is fail-fast command-queue backpressure; the async submit form waits for that capacity.
/// - [`ResourceLimit`](Self::ResourceLimit) and [`ThreadStartFailed`](Self::ThreadStartFailed) describe cleanup ownership that Taskvisor could not reserve before intake.
/// - [`OwnershipAdmissionTimeout`](Self::OwnershipAdmissionTimeout) means a caller-provided ownership-only deadline expired before intake.
/// - [`Closed`](Self::Closed) means controller shutdown has closed intake.
///
/// Match the enum with a wildcard arm because it is non-exhaustive.
/// Use `..` when matching a data-carrying variant.
#[non_exhaustive]
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerError {
    /// No controller was installed on this supervisor.
    ///
    /// Configure one with [`SupervisorBuilder::with_controller`](crate::SupervisorBuilder::with_controller) before building the supervisor.
    #[error("controller not configured")]
    NotConfigured,

    /// The ordered controller command queue has no capacity now.
    ///
    /// This variant is limited to fail-fast [`Submit::try_intake`](crate::Submit::try_intake).
    /// It describes command intake, not a full per-slot queue.
    /// `execute().await` waits for command capacity instead.
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

    /// The caller's bounded wait for cleanup ownership expired before command intake.
    ///
    /// No controller command was committed.
    /// The timeout covers only ownership admission.
    /// It does not bound a later wait for controller command capacity.
    #[error("timeout waiting for ownership admission after {timeout:?}")]
    #[non_exhaustive]
    OwnershipAdmissionTimeout {
        /// Maximum duration allowed for ownership admission.
        timeout: Duration,
    },

    /// Taskvisor could not start a cleanup worker required before intake.
    ///
    /// The fields preserve the worker identity and the available operating system error details.
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
}

impl ControllerError {
    /// Stable variant label for logs and metrics.
    ///
    /// The label is distinct from the human-readable [`Display`](std::fmt::Display) message.
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            ControllerError::ThreadStartFailed { .. } => "controller_thread_start_failed",
            ControllerError::ResourceLimit { .. } => "controller_resource_limit",
            ControllerError::OwnershipAdmissionTimeout { .. } => {
                "controller_ownership_admission_timeout"
            }
            ControllerError::NotConfigured => "controller_not_configured",
            ControllerError::Closed => "controller_closed",
            ControllerError::Full => "controller_full",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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
                ControllerError::OwnershipAdmissionTimeout {
                    timeout: Duration::from_millis(25),
                },
                "controller_ownership_admission_timeout",
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
    fn ownership_timeout_preserves_duration_and_display() {
        let error = ControllerError::OwnershipAdmissionTimeout {
            timeout: Duration::from_millis(25),
        };
        assert_eq!(
            error.to_string(),
            "timeout waiting for ownership admission after 25ms"
        );
        let ControllerError::OwnershipAdmissionTimeout { timeout, .. } = error else {
            unreachable!("the constructed variant is an ownership admission timeout")
        };
        assert_eq!(timeout, Duration::from_millis(25));
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
