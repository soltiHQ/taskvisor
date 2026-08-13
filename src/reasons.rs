//! Supplies shared readable text for admission diagnostics.
//!
//! ```text
//! registry or controller decision
//!      ├── category ──► RejectionKind
//!      └── detail ────► reason text built from these fragments
//! ```
//!
//! Registry and controller paths reuse these strings when they build event and
//! watched-outcome payloads. The text is diagnostic only and has no stability
//! guarantee. Consumers must branch on typed categories instead of parsing it.

/// Name conflict detected during registry admission.
pub(crate) const ALREADY_EXISTS: &str = "a registered task already uses this name";

/// Static batch item rejected because another item failed admission.
pub(crate) const BATCH_REJECTED: &str = "another item rejected the all-or-nothing batch";

/// Queued controller submission removed before it started.
#[cfg(feature = "controller")]
pub(crate) const REMOVED_FROM_QUEUE: &str = "removed from controller queue before start";

/// Queued submission displaced by a newer replacement.
#[cfg(feature = "controller")]
pub(crate) const SUPERSEDED_BY_REPLACE: &str = "superseded by a newer replacement";

/// Submission rejected during controller shutdown.
#[cfg(feature = "controller")]
pub(crate) const CONTROLLER_SHUTTING_DOWN: &str = "controller is shutting down";

/// Controller admission ended before ownership transfer committed.
#[cfg(feature = "controller")]
pub(crate) const CONTROLLER_ADMISSION_INTERRUPTED: &str =
    "controller admission was interrupted before ownership transfer";

/// Busy-slot rejection under `DropIfRunning`.
#[cfg(feature = "controller")]
pub(crate) const DROP_IF_RUNNING: &str = "slot is busy; DropIfRunning rejected the submission";

/// Controller slot queue capacity rejection.
#[cfg(feature = "controller")]
pub(crate) const QUEUE_FULL: &str = "slot queue is full";

/// Registry membership limit rejection.
pub(crate) const REGISTERED_TASK_LIMIT: &str = "registered task limit reached";

/// Controller slot-count limit rejection.
#[cfg(feature = "controller")]
pub(crate) const CONTROLLER_SLOT_LIMIT: &str = "controller slot limit reached";

/// Aggregate controller pending-work limit rejection.
#[cfg(feature = "controller")]
pub(crate) const CONTROLLER_PENDING_LIMIT: &str = "controller pending limit reached";
