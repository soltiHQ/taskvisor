//! # Stable `reason` strings.
//!
//! Some events and outcomes have a `reason` string.
//! This module lists the stable strings.
//!
//! ## Where each value appears
//!
//! | Constant                     | Used by                                                | Meaning                                |
//! |------------------------------|--------------------------------------------------------|----------------------------------------|
//! | [`POLICY_EXHAUSTED_SUCCESS`] | `ActorExhausted`                                       | Normal one-shot finish.                |
//! | [`TASK_RETURNED_CANCELED`]   | `ActorExhausted`                                       | The task stopped itself.               |
//! | [`MAX_RETRIES_EXCEEDED`]     | `ActorExhausted` (prefix)                              | Retry limit reached.                   |
//! | [`ALREADY_EXISTS`]           | `TaskAddFailed`, `TaskOutcome::Rejected`               | A task with this name already exists.  |
//! | [`BATCH_REJECTED`]           | `TaskAddFailed`                                        | Another item rejected the whole batch. |
//! | [`QUEUE_FULL`]               | `ControllerRejected`, `TaskOutcome::Rejected` (prefix) | The slot queue is full.                |
//! | [`REMOVED_FROM_QUEUE`]       | `ControllerRejected`, `TaskOutcome::Rejected`          | A queued task was removed.             |
//! | [`SUPERSEDED_BY_REPLACE`]    | `ControllerRejected`, `TaskOutcome::Rejected`          | A newer `Replace` task took its place. |
//! | [`CONTROLLER_SHUTTING_DOWN`] | `ControllerRejected`, `TaskOutcome::Rejected`          | The controller is shutting down.       |
//!
//! Controller rejection values are used only with the `controller` feature.
//! The constants themselves are always available.

/// `ActorExhausted` reason: the task finished successfully under a `Never`/`OnFailure` policy.
/// This is not an error.
pub const POLICY_EXHAUSTED_SUCCESS: &str = "policy_exhausted_success";

/// `ActorExhausted` reason:
/// the task body returned [`TaskError::Canceled`](crate::TaskError::Canceled), but the runtime did not cancel it.
/// This is not an error.
pub const TASK_RETURNED_CANCELED: &str = "task_returned_canceled";

/// `ActorExhausted` reason **prefix**: the task stopped after it used all retry attempts.
///
/// The full reason is `max_retries_exceeded(<attempt>/<limit>): <last error>`.
/// Match with `starts_with`, not equality.
pub const MAX_RETRIES_EXCEEDED: &str = "max_retries_exceeded";

/// Registration/rejection reason: another running task already uses this name.
pub const ALREADY_EXISTS: &str = "already_exists";

/// Registration reason: another item caused an atomic static batch to be rejected.
pub const BATCH_REJECTED: &str = "batch_rejected";

/// Rejection reason **prefix**: a controller slot queue is full.
///
/// The full reason is `queue_full: <depth>/<limit>`.
/// Match with `starts_with`, not equality.
pub const QUEUE_FULL: &str = "queue_full";

/// Rejection reason: a queued task was removed before it started.
pub const REMOVED_FROM_QUEUE: &str = "removed_from_queue";

/// Rejection reason: a newer `Replace` task took this task's place.
pub const SUPERSEDED_BY_REPLACE: &str = "superseded_by_replace";

/// Rejection reason: the controller is shutting down.
pub const CONTROLLER_SHUTTING_DOWN: &str = "controller_shutting_down";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_values_are_pinned() {
        assert_eq!(POLICY_EXHAUSTED_SUCCESS, "policy_exhausted_success");
        assert_eq!(TASK_RETURNED_CANCELED, "task_returned_canceled");
        assert_eq!(MAX_RETRIES_EXCEEDED, "max_retries_exceeded");
        assert_eq!(ALREADY_EXISTS, "already_exists");
        assert_eq!(BATCH_REJECTED, "batch_rejected");
        assert_eq!(QUEUE_FULL, "queue_full");
        assert_eq!(REMOVED_FROM_QUEUE, "removed_from_queue");
        assert_eq!(SUPERSEDED_BY_REPLACE, "superseded_by_replace");
        assert_eq!(CONTROLLER_SHUTTING_DOWN, "controller_shutting_down");
    }
}
