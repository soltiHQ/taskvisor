//! # Per-slot admission policies
//!
//! The controller admits work by **slot**, not by task name.
//! At most one registered task may own a slot at a time.
//!
//! Use [`ControllerSpec::with_slot`](crate::ControllerSpec::with_slot) when several differently named tasks should share one admission lane.
//! A slot is the submission key returned by [`ControllerSpec::slot_name`](crate::ControllerSpec::slot_name).
//! If no slot is set, it defaults to the task name.
//!
//! ## Busy Slots
//!
//! A slot is busy while it is admitting, running, or terminating. Here,
//! `Running` means "registered and owns the slot". The task body may still be
//! waiting for the supervisor's global concurrency permit.
//!
//! When a new submission targets a busy slot, [`AdmissionPolicy`] decides what happens.
//!
//! ## Policy Summary
//!
//! | Policy                                            | Busy slot behavior                                   | Common use                           |
//! |---------------------------------------------------|------------------------------------------------------|--------------------------------------|
//! | [`Queue`](AdmissionPolicy::Queue)                 | Wait behind older queued work                        | Job queue, ordered pipeline          |
//! | [`Replace`](AdmissionPolicy::Replace)             | Retire owner; keep latest next task                  | Debounce, latest deploy              |
//! | [`DropIfRunning`](AdmissionPolicy::DropIfRunning) | Reject while busy                                    | Periodic checks, skip duplicate work |
//!
//! ## Rejection
//!
//! Rejected submissions emit a best-effort `ControllerRejected` event. A
//! `submit_and_watch` waiter uses a separate terminal channel and normally
//! resolves to [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected).

/// Policy for handling a submission when its target slot is busy.
///
/// If the slot is idle, all policies admit the submission immediately.
/// This enum is non-exhaustive. Match with a wildcard arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionPolicy {
    /// Rejects the new submission if the slot is busy.
    ///
    /// No queued work is added and the current slot owner is left untouched.
    /// Use this when duplicate or stale work should be skipped.
    /// Good fits:
    /// - periodic health checks,
    /// - cache refreshes,
    /// - debounce-style "do not overlap" jobs.
    DropIfRunning,

    /// Retires the current owner and places this submission next.
    ///
    /// If the slot is running, the controller asks the runtime to remove the current owner.
    /// The replacement starts only after terminal registry cleanup is confirmed.
    ///
    /// Repeated `Replace` submissions replace the queue head, so the latest one
    /// is next. Other FIFO items already behind the head remain queued.
    /// Use this when newer work makes older work obsolete.
    /// Good fits:
    /// - latest deployment for an environment,
    /// - search/index rebuild for the newest revision,
    /// - "only the newest request matters" workflows.
    Replace,

    /// Queues the submission behind older work for this slot.
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
