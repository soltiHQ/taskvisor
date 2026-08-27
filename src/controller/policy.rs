//! Selects what happens when new work targets an occupied slot.
//!
//! A [`ControllerSpec`](crate::ControllerSpec) carries one [`AdmissionPolicy`] into the controller.
//! After preflight, every policy takes the same idle-slot path and attempts registry admission.
//! The policy matters only while the slot has an owner:
//!
//! ```text
//! ControllerSpec
//!      │ slot + policy
//!      ▼
//! controller slot
//!      ├── idle ──► start runtime registry admission
//!      └── busy ──► apply AdmissionPolicy
//! ```
//!
//! A slot is busy during registry admission, task lifetime, and physical release.
//! This is an ownership state. The task body need not be polling at that moment.
//!
//! The policy belongs to the incoming submission, not to the slot.
//! Submissions with different policies may target the same slot.

/// The conflict policy for one controller submission.
///
/// The policy does not change task execution settings in [`TaskSpec`](crate::TaskSpec).
/// It only controls admission to the slot.
///
/// Choose [`Queue`](Self::Queue) when every item should be considered in FIFO order.
/// Choose [`Replace`](Self::Replace) when the next item should contain the newest value.
/// Choose [`DropIfRunning`](Self::DropIfRunning) when duplicate work may be skipped.
///
/// Match with a wildcard arm because this enum is non-exhaustive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionPolicy {
    /// Rejection of incoming work while the slot has an owner.
    ///
    /// The task body does not start.
    /// The current owner and existing queue stay unchanged.
    /// A watched submission resolves to [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected)
    /// with [`RejectionKind::SlotBusy`](crate::RejectionKind::SlotBusy).
    DropIfRunning,

    /// Newest-head replacement with retirement of the current owner.
    ///
    /// On a busy slot, this submission creates or replaces the queue head.
    /// Repeated replacements retain only the newest head.
    /// FIFO items behind the head keep their order.
    /// `Replace` does not clear the entire queue.
    ///
    /// A registered owner receives a removal request.
    /// A pending registry admission must finish before removal can be ordered.
    /// The replacement starts after registry cleanup and physical release of the owner.
    ///
    /// Replacement has no separate wait timeout.
    /// While physical release is pending, snapshots report the slot as [`SlotStatusKind::Terminating`](crate::SlotStatusKind::Terminating).
    ///
    /// This policy does not use [`ControllerConfig::max_slot_queue`](crate::ControllerConfig::max_slot_queue).
    /// Creating a new queue head can still hit the aggregate pending limit.
    /// A displaced watched head resolves with [`RejectionKind::SupersededByReplace`](crate::RejectionKind::SupersededByReplace).
    Replace,

    /// FIFO admission behind the current owner and older pending work.
    ///
    /// The current owner leaves the slot first.
    /// Pending submissions are then considered from the front of the queue.
    /// A later `Replace` submission can still displace that head.
    /// The per-slot queue limit can reject with [`RejectionKind::QueueFull`](crate::RejectionKind::QueueFull).
    /// The aggregate pending limit can reject with [`RejectionKind::ResourceLimit`](crate::RejectionKind::ResourceLimit).
    ///
    /// Waiting for controller admission has no built-in deadline and is outside the task's per-attempt timeout.
    /// Keep the returned [`TaskId`](crate::TaskId) when the application may need to remove or cancel queued work.
    ///
    /// See [`ControllerConfig`](crate::ControllerConfig) for both limits.
    Queue,
}
