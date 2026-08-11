//! # Controller API errors

use thiserror::Error;

/// Errors returned by controller configuration and submission operations.
///
/// Match with a wildcard arm because this enum is non-exhaustive.
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
    /// Returned only by fail-fast `try_submit*` methods, including those on [`PreparedSubmission`](crate::PreparedSubmission).
    /// Use the corresponding async submit method to wait for command capacity.
    #[error("submission queue full")]
    Full,

    /// A bounded controller resource is exhausted.
    ///
    /// This is reported before the submission crosses the controller command boundary.
    #[error("controller resource limit reached: {resource} (limit: {limit})")]
    #[non_exhaustive]
    ResourceLimit {
        /// Stable resource name suitable for logs and metrics.
        resource: &'static str,
        /// Configured hard limit for the resource.
        limit: usize,
    },

    /// The controller command channel is closed.
    ///
    /// This usually means the controller loop has stopped or the supervisor is shutting down.
    #[error("controller channel closed")]
    Closed,

    /// Compatibility variant for the former fallible controller-start guard.
    ///
    /// Current controller startup is idempotent and does not produce this
    /// value. It remains public so existing callers that name, construct, or
    /// format the variant continue to compile.
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
            ControllerError::ResourceLimit { .. } => "controller_resource_limit",
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
}
