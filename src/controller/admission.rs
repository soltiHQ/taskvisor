//! Per-slot admission policies.
//!
//! The controller admits work by **slot**, not by task name.
//! At most one task may occupy a slot at a time.
//!
//! Use [`ControllerSpec::with_slot`](crate::ControllerSpec::with_slot) when several differently named tasks should share one admission lane.
//! A slot is the submission key returned by [`ControllerSpec::slot_name`](crate::ControllerSpec::slot_name).
//! If no slot is set, it defaults to the task name.
//!
//! ## Busy Slots
//!
//! A slot is busy while it is:
//! - `Terminating`: the task is being removed, and the controller waits for `TaskRemoved`,
//! - `Admitting`: the task was sent to the runtime, but `TaskAdded` is not seen yet,
//! - `Running`: the task is registered and running.
//!
//! When a new submission targets a busy slot, [`AdmissionPolicy`] decides what happens.
//!
//! ## Policy Summary
//!
//! | Policy                                            | Busy slot behavior                                   | Common use                           |
//! |---------------------------------------------------|------------------------------------------------------|--------------------------------------|
//! | [`Queue`](AdmissionPolicy::Queue)                 | Wait behind older queued work                        | Job queue, ordered pipeline          |
//! | [`Replace`](AdmissionPolicy::Replace)             | Keep the latest next task, supersede older next task | Debounce, latest deploy              |
//! | [`DropIfRunning`](AdmissionPolicy::DropIfRunning) | Reject while busy                                    | Periodic checks, skip duplicate work |
//!
//! ## Rejection
//!
//! Rejected submissions emit `ControllerRejected`.
//! If submitted through `submit_and_watch`, the waiter resolves to [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected).

/// Policy for handling a submission when its target slot is busy.
///
/// If the slot is idle, all policies admit the submission immediately.
/// This enum is non-exhaustive. Match with a wildcard arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionPolicy {
    /// Reject the submission if the slot is busy.
    ///
    /// No queued work is added and the current slot owner is left untouched.
    /// Use this when duplicate or stale work should be skipped.
    /// Good fits:
    /// - periodic health checks,
    /// - cache refreshes,
    /// - debounce-style "do not overlap" jobs.
    DropIfRunning,

    /// Replace the next pending submission for this slot.
    ///
    /// If the slot is running, the controller asks the runtime to remove the current owner.
    /// The replacement starts only after `TaskRemoved` is observed.
    ///
    /// Repeated `Replace` submissions do not grow the queue.
    /// They supersede the queued head, so the latest replacement wins.
    /// Use this when newer work makes older work obsolete.
    /// Good fits:
    /// - latest deployment for an environment,
    /// - search/index rebuild for the newest revision,
    /// - "only the newest request matters" workflows.
    Replace,

    /// Queue the submission behind the current slot owner.
    ///
    /// Queued submissions run in FIFO order for this slot.
    /// The queue is limited by [`ControllerConfig::max_slot_queue`](crate::ControllerConfig::max_slot_queue).
    /// Use this when every submission should run and order matters.
    /// Good fits:
    /// - sequential job processing,
    /// - ordered pipelines,
    /// - per-customer or per-resource command lanes.
    Queue,
}
