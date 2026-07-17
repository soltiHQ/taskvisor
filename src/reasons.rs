//! Internal fragments used to compose readable diagnostics.
//!
//! These strings are deliberately not public API and carry no compatibility guarantee.
//! Runtime consumers must branch on typed fields such as `TaskOutcomeKind` and `RejectionKind`, never parse `reason`.

/// Registration/rejection reason: another registered task already uses this name.
pub(crate) const ALREADY_EXISTS: &str = "a registered task already uses this name";

/// Registration reason: another item caused an all-or-nothing static batch to be rejected.
pub(crate) const BATCH_REJECTED: &str = "another item rejected the all-or-nothing batch";

/// Rejection reason: a queued task was removed before it started.
#[cfg(feature = "controller")]
pub(crate) const REMOVED_FROM_QUEUE: &str = "removed from controller queue before start";

/// Rejection reason: a newer `Replace` task took this task's place.
#[cfg(feature = "controller")]
pub(crate) const SUPERSEDED_BY_REPLACE: &str = "superseded by a newer replacement";

/// Rejection reason: the controller is shutting down.
#[cfg(feature = "controller")]
pub(crate) const CONTROLLER_SHUTTING_DOWN: &str = "controller is shutting down";

/// Diagnostic text used when `DropIfRunning` rejects a busy slot.
#[cfg(feature = "controller")]
pub(crate) const DROP_IF_RUNNING: &str = "slot is busy; DropIfRunning rejected the submission";

/// Diagnostic text used when a controller slot queue is full.
#[cfg(feature = "controller")]
pub(crate) const QUEUE_FULL: &str = "slot queue is full";
