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
//! A slot is busy while it is admitting, running, or terminating.
//! Here, `Running` means that registration was confirmed and the controller has not yet applied terminal cleanup.
//! It does not prove that the task body is executing now.
//! The task may be waiting for a concurrency permit, sleeping between attempts, or finishing cleanup.
//!
//! When a new submission targets a busy slot, [`AdmissionPolicy`] decides what happens.
//!
//! ## Policy Summary
//!
//! | Policy                                            | Busy slot behavior                    | Common use                           |
//! |---------------------------------------------------|---------------------------------------|--------------------------------------|
//! | [`Queue`](AdmissionPolicy::Queue)                 | Queue if capacity allows; else reject | Job queue, ordered pipeline          |
//! | [`Replace`](AdmissionPolicy::Replace)             | Create/replace head; retire if needed | Debounce, latest deploy              |
//! | [`DropIfRunning`](AdmissionPolicy::DropIfRunning) | Reject while busy                     | Periodic checks, skip duplicate work |
//!
//! ## Rejection
//!
//! Controller-side rejections emit a best-effort [`ControllerRejected`](crate::EventKind::ControllerRejected) event.
//! A duplicate task name rejected by the registry emits [`TaskAddFailed`](crate::EventKind::TaskAddFailed) instead.
//! When either layer rejects a watched submission, its waiter resolves to [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected).

/// Policy for handling a submission when its target slot is busy.
///
/// If the slot is idle, every policy tries to start registry admission.
/// The slot becomes `Running` only after the registry accepts the task.
/// This enum is non-exhaustive. Match with a wildcard arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionPolicy {
    /// Rejects the new submission if the slot is busy.
    ///
    /// No queued work is added and the current slot owner is left untouched.
    /// Use this when duplicate or stale work should be skipped.
    ///
    /// Good fits:
    /// - periodic health checks,
    /// - cache refreshes,
    /// - debounce-style "do not overlap" jobs.
    DropIfRunning,

    /// Requests replacement of the current owner and places this submission next.
    ///
    /// For a registered owner, the controller asks the runtime to remove it.
    /// The replacement starts only after terminal registry cleanup is confirmed.
    /// If the owner's registration is still pending, the controller waits for the direct registry decision.
    /// A rejection needs no removal.
    /// After an acceptance, the owner is removed before the replacement starts.
    ///
    /// A `Replace` submission creates or replaces the queue head.
    /// Repeated replacements keep the latest one next.
    /// Other FIFO items already behind the head remain queued.
    /// Use this when newer `Replace` work makes the current next item obsolete.
    ///
    /// Good fits:
    /// - latest deployment for an environment,
    /// - search/index rebuild for the newest revision,
    /// - "only the newest request matters" workflows that use `Replace` consistently.
    Replace,

    /// Queues the submission behind older work when pending capacity allows.
    ///
    /// Queued submissions are considered for admission in FIFO order.
    /// The queue is limited by [`ControllerConfig::max_slot_queue`](crate::ControllerConfig::max_slot_queue).
    /// A full pending queue rejects the new submission. Use this when pending submissions should keep their order.
    ///
    /// Good fits:
    /// - sequential job processing,
    /// - ordered pipelines,
    /// - per-customer or per-resource command lanes.
    Queue,
}
