//! # Stable `reason` values
//!
//! Some events and outcomes have a `reason` string.
//! Code can check the stable values and prefixes in this module.
//! Other reason strings are not stable and may change between releases.
//!
//! Exact value:
//!
//! ```rust
//! use taskvisor::reasons::ALREADY_EXISTS;
//! assert_eq!("already_exists", ALREADY_EXISTS);
//! ```
//!
//! Prefix with details:
//!
//! ```rust
//! use taskvisor::reasons::MAX_RETRIES_EXCEEDED;
//! let reason = "max_retries_exceeded(3/3): connection lost";
//! assert!(reason.starts_with(MAX_RETRIES_EXCEEDED));
//! ```
//!
//! ## Where each value appears
//!
//! | Constant                             | Used by                                                | Meaning                                |
//! |--------------------------------------|--------------------------------------------------------|----------------------------------------|
//! | [`POLICY_EXHAUSTED_SUCCESS`]         | `ActorExhausted`                                       | Normal one-shot finish.                |
//! | [`TASK_RETURNED_CANCELED`]           | `ActorExhausted`                                       | The task stopped itself.               |
//! | [`MAX_RETRIES_EXCEEDED`]             | `ActorExhausted` (prefix)                              | Retry limit reached.                   |
//! | [`ALREADY_EXISTS`]                   | `TaskAddFailed`, `TaskOutcome::Rejected`               | A task with this name already exists.  |
//! | [`BATCH_REJECTED`]                   | `TaskAddFailed`                                        | Another item rejected the whole batch. |
//! | [`QUEUE_FULL`]                       | `ControllerRejected`, `TaskOutcome::Rejected` (prefix) | The slot queue is full.                |
//! | [`DROP_IF_RUNNING`]                  | `ControllerRejected`, `TaskOutcome::Rejected` (prefix) | A busy slot rejected new work.         |
//! | [`REMOVED_FROM_QUEUE`]               | `ControllerRejected`, `TaskOutcome::Rejected`          | A queued task was removed.             |
//! | [`SUPERSEDED_BY_REPLACE`]            | `ControllerRejected`, `TaskOutcome::Rejected`          | A newer `Replace` task took its place. |
//! | [`CONTROLLER_SHUTTING_DOWN`]         | `ControllerRejected`, `TaskOutcome::Rejected`          | The controller is shutting down.       |
//! | [`FORCE_TERMINATED_AFTER_GRACE`]     | `TaskRemoved`                                          | The actor was force-aborted.           |
//!

/// `ActorExhausted` reason: the task finished successfully under a `Never`/`OnFailure` policy.
/// This is not an error.
pub const POLICY_EXHAUSTED_SUCCESS: &str = "policy_exhausted_success";

/// `ActorExhausted` reason: the task body returned [`TaskError::Canceled`](crate::TaskError::Canceled), but the runtime did not cancel it.
/// This is not an error.
pub const TASK_RETURNED_CANCELED: &str = "task_returned_canceled";

/// Registration/rejection reason: another registered task already uses this name.
pub const ALREADY_EXISTS: &str = "already_exists";

/// Registration reason: another item caused an all-or-nothing static batch to be rejected.
pub const BATCH_REJECTED: &str = "batch_rejected";

/// Rejection reason: a queued task was removed before it started.
pub const REMOVED_FROM_QUEUE: &str = "removed_from_queue";

/// Rejection reason: a newer `Replace` task took this task's place.
pub const SUPERSEDED_BY_REPLACE: &str = "superseded_by_replace";

/// Rejection reason: the controller is shutting down.
pub const CONTROLLER_SHUTTING_DOWN: &str = "controller_shutting_down";

/// `ControllerRejected` and `TaskOutcome::Rejected` reason **prefix**: `DropIfRunning` rejected a submission because its slot was busy.
///
/// The full reason is `dropped: slot busy (<status>)`.
/// Match with `starts_with`, not equality.
pub const DROP_IF_RUNNING: &str = "dropped: slot busy";

/// `TaskRemoved` reason: the actor did not stop within its grace window and was force-aborted.
pub const FORCE_TERMINATED_AFTER_GRACE: &str = "force_terminated_after_grace";

/// `ActorExhausted` reason **prefix**: the task stopped after it used all retry attempts.
///
/// The full reason is `max_retries_exceeded(<retries_used>/<retry_limit>): <last error>`.
/// Match with `starts_with`, not equality.
pub const MAX_RETRIES_EXCEEDED: &str = "max_retries_exceeded";

/// Rejection reason **prefix**: a controller slot queue is full.
///
/// The full reason is `queue_full: <depth>/<limit>`.
/// Match with `starts_with`, not equality.
pub const QUEUE_FULL: &str = "queue_full";
