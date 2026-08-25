//! Defines the event values delivered to subscribers.
//!
//! Runtime components create an [`Event`] to describe a lifecycle action. Ordinary events enter the bounded bus and may reach
//! subscriber callbacks. Internal overflow and relay-failure diagnostics can enter subscriber lanes directly.
//! No event feeds back into task management or registry cleanup.
//! Applications normally read events through [`Subscribe`](crate::Subscribe), not construct them.
//!
//! ```text
//! runtime action
//!      │ Event::new + metadata builders
//!      ▼
//! Event
//!      │ best-effort delivery
//!      ▼
//!      ├── ordinary event ──► event bus ──► subscribers
//!      └── internal diagnostic ──► subscriber lanes
//! ```
//!
//! [`EventKind`] defines the event meaning. [`BackoffSource`], [`RejectionKind`], and [`TaskOutcomeKind`]
//! provide typed categories where free-form text would be unsafe for machine decisions.
//!
//! Every event contains `kind`, `at`, and `seq`. Other fields depend on the event kind.
//! Read the variant documentation before using an optional field.
//! Duration builders store whole milliseconds and clamp values above `u32::MAX` milliseconds.
//!
//! `seq` is an increasing process-local construction sequence.
//! Concurrent effects and callbacks may occur in another order.
//! The sequence is not persisted and panics on exhaustion instead of wrapping.
//!
//! # Interpreting an event
//!
//! ```rust
//! use taskvisor::{Event, EventKind};
//!
//! fn observe(event: &Event) {
//!     match event.kind {
//!         EventKind::TaskFinished => {
//!             let outcome = event.outcome_kind
//!                 .map(|kind| kind.as_label())
//!                 .unwrap_or("unknown");
//!             println!(
//!                 "id={:?} task={:?} outcome={outcome}",
//!                 event.id,
//!                 event.task.as_deref(),
//!             );
//!         }
//!         EventKind::SubscriberOverflow => {
//!             eprintln!("lost {} events", event.dropped.unwrap_or(0));
//!         }
//!         _ => {}
//!     }
//! }
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, SystemTime};

use crate::{TaskOutcomeKind, identity::TaskId};

/// Process-local counter for `seq` values.
///
/// Zero is the exhausted sentinel and is never returned.
static EVENT_SEQ: AtomicU64 = AtomicU64::new(1);

#[inline]
fn advance_event_seq(current: u64) -> Option<u64> {
    match current {
        0 => None,
        u64::MAX => Some(0),
        value => Some(value + 1),
    }
}

#[inline]
fn next_event_seq() -> u64 {
    EVENT_SEQ
        .fetch_update(
            AtomicOrdering::Relaxed,
            AtomicOrdering::Relaxed,
            advance_event_seq,
        )
        .unwrap_or_else(|_| panic!("event sequence exhausted; ordering cannot wrap safely"))
}

/// Classifies one best-effort runtime event.
///
/// Every [`Event`] has `seq`, `at`, and `kind`. Variant documentation lists the additional metadata
/// set by Taskvisor. Match on this value before reading optional metadata. This enum is non-exhaustive;
/// include a wildcard arm when matching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventKind {
    /// A subscriber callback panicked.
    ///
    /// Carries the subscriber name in `task` and panic details in `reason`.
    SubscriberPanicked,

    /// An internal runtime component failed or did not join cleanly.
    ///
    /// Carries the component name in `task` and diagnostic details in `reason`.
    RuntimeFailure,

    /// Cleanup retirement permanently reduced this supervisor's ownership capacity.
    ///
    /// Carries the original finite limit in `configured_capacity`, the remaining usable limit in `effective_capacity`,
    /// and the units retired by this transition in `retired_units`. `task` may identify the runtime component
    /// that observed the retirement.
    OwnershipCapacityRetired,

    /// An event path fell behind or closed before delivery.
    ///
    /// `task` identifies the subscriber or internal relay.
    /// `dropped` carries a loss count when known. `reason` contains diagnostic details.
    SubscriberOverflow,

    /// Explicit shutdown entered the shared shutdown workflow.
    ///
    /// This includes a handle request, an application shutdown future, or a configured
    /// operating-system signal. Natural shutdown does not emit it.
    ShutdownRequested,

    /// Registry task cleanup finished within the shared grace window.
    ///
    /// A task force-aborted before this shutdown may still be physically active.
    AllStoppedWithinGrace,

    /// Registry task cleanup did not finish within the shared grace window.
    GraceExceeded,

    /// A registered task is about to run one attempt.
    ///
    /// Carries `id`, `task`, and the one-based `attempt` number.
    AttemptStarting,

    /// One task attempt returned `Ok(())`.
    ///
    /// This is not always the final task result. Carries `id`, `task`, `attempt`, and `duration_ms`.
    AttemptSucceeded,

    /// One task attempt returned [`TaskError::Canceled`](crate::TaskError::Canceled).
    ///
    /// Carries `id`, `task`, `attempt`, and `duration_ms`.
    AttemptCanceled,

    /// One task attempt returned or produced a failure.
    ///
    /// This includes retryable and fatal errors, task-returned timeouts, and caught task panics.
    /// A configured deadline normally emits [`AttemptTimedOut`](Self::AttemptTimedOut).
    /// Cleanup failure at that deadline emits this variant instead.
    /// Carries `id`, `task`, `attempt`, `duration_ms`, `reason`, and an optional `exit_code`.
    AttemptFailed,

    /// One task attempt exceeded its configured deadline.
    ///
    /// Carries `id`, `task`, `attempt`, `timeout_ms`, and `duration_ms`.
    AttemptTimedOut,

    /// The task actor scheduled a delay before another attempt.
    ///
    /// Carries `id`, `task`, the previous `attempt`, `delay_ms`, and `backoff_source`.
    /// Failure backoff also carries the last error in `reason`.
    BackoffScheduled,

    /// A task add request was published before registry processing.
    ///
    /// This does not confirm admission. An all-or-nothing batch publishes one event per item before
    /// sending its single registry command. Carries the reserved `id` and requested `task` name.
    TaskAddRequested,

    /// Registry admission accepted the task.
    ///
    /// The task body may not have started. Carries `id` and `task`.
    TaskAdded,

    /// Registry admission rejected a task add.
    ///
    /// No rejected task body starts. Batch rejection starts no item in the batch. Carries `id`, `task`,
    /// diagnostic `reason`, [`TaskOutcomeKind::Rejected`], and a registry [`RejectionKind`].
    TaskAddFailed,

    /// A remove or cancel request entered runtime or controller processing.
    ///
    /// This is not proof that a target existed or reached terminal cleanup.
    /// Carries `id`, an optional `reason`, and a task name when available.
    TaskRemoveRequested,

    /// Registry cleanup attempted the closing event for a removed task.
    ///
    /// Registry membership is already absent. Final watched-outcome delivery, when requested,
    /// was attempted first. A force-aborted task may still be physically active. Carries `id` and `task`.
    TaskRemoved,

    /// Registry cleanup classified the final outcome of a registered task.
    ///
    /// Membership is already absent. This event is attempted before watched outcome delivery and
    /// [`TaskRemoved`](Self::TaskRemoved). Except for force-abort, task execution is physically
    /// joined first. Carries `id`, `task`, `outcome_kind`, and optional diagnostic `reason`
    /// and `exit_code`. Use [`TaskWaiter`](crate::TaskWaiter) when the final
    /// outcome must not rely on best-effort event delivery.
    TaskFinished,

    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    /// The controller rejected a submission before registry admission.
    ///
    /// Carries `id`, [`TaskOutcomeKind::Rejected`], `rejection_kind`, diagnostic `reason`, and the slot name in `task` when known.
    ControllerRejected,

    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    /// The controller accepted a submission into slot ownership or its queue.
    ///
    /// Runtime registry admission and task execution may not have started.
    /// Carries `id`, the slot name in `task`, and a diagnostic summary in `reason`.
    ControllerSubmitted,

    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    /// A controller slot changed admission state.
    ///
    /// Carries the slot name in `task` and diagnostic transition text in `reason`.
    ControllerSlotTransition,
}

impl EventKind {
    /// Returns the stable snake-case label for logs and metrics.
    ///
    /// ```rust
    /// use taskvisor::EventKind;
    ///
    /// assert_eq!(EventKind::AttemptStarting.as_label(), "attempt_starting");
    /// assert_eq!(EventKind::BackoffScheduled.as_label(), "backoff_scheduled");
    /// ```
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            EventKind::SubscriberPanicked => "subscriber_panicked",
            EventKind::RuntimeFailure => "runtime_failure",
            EventKind::OwnershipCapacityRetired => "ownership_capacity_retired",
            EventKind::SubscriberOverflow => "subscriber_overflow",
            EventKind::ShutdownRequested => "shutdown_requested",
            EventKind::AllStoppedWithinGrace => "all_stopped_within_grace",
            EventKind::GraceExceeded => "grace_exceeded",
            EventKind::AttemptStarting => "attempt_starting",
            EventKind::AttemptSucceeded => "attempt_succeeded",
            EventKind::AttemptCanceled => "attempt_canceled",
            EventKind::AttemptFailed => "attempt_failed",
            EventKind::AttemptTimedOut => "attempt_timed_out",
            EventKind::BackoffScheduled => "backoff_scheduled",
            EventKind::TaskAddRequested => "task_add_requested",
            EventKind::TaskAdded => "task_added",
            EventKind::TaskAddFailed => "task_add_failed",
            EventKind::TaskRemoveRequested => "task_remove_requested",
            EventKind::TaskRemoved => "task_removed",
            EventKind::TaskFinished => "task_finished",
            #[cfg(feature = "controller")]
            EventKind::ControllerRejected => "controller_rejected",
            #[cfg(feature = "controller")]
            EventKind::ControllerSubmitted => "controller_submitted",
            #[cfg(feature = "controller")]
            EventKind::ControllerSlotTransition => "controller_slot_transition",
        }
    }
}

/// Identifies why a [`BackoffScheduled`](EventKind::BackoffScheduled) delay exists.
///
/// This enum is exhaustive: a delay follows either success or failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackoffSource {
    /// A successful `RestartPolicy::Always` attempt scheduled its interval.
    Success,
    /// A retryable failure scheduled its backoff policy.
    Failure,
}

impl BackoffSource {
    /// Returns the stable label used by logs and metrics.
    ///
    /// ```rust
    /// use taskvisor::BackoffSource;
    ///
    /// assert_eq!(BackoffSource::Success.as_label(), "success");
    /// assert_eq!(BackoffSource::Failure.as_label(), "failure");
    /// ```
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            BackoffSource::Success => "success",
            BackoffSource::Failure => "failure",
        }
    }
}

/// Classifies why submitted work was rejected before its task body ran.
///
/// Use this enum for branching and telemetry. [`Event::reason`] and [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected)
/// keep readable details. This enum is non-exhaustive; include a wildcard arm when matching it.
///
/// ```rust
/// use taskvisor::RejectionKind;
///
/// assert_eq!(RejectionKind::QueueFull.as_label(), "queue_full");
/// assert_eq!(RejectionKind::RemovedFromQueue.as_label(), "removed_from_queue");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RejectionKind {
    /// The name is reserved or repeated in the same all-or-nothing batch.
    ///
    /// An active or removing task reserves its name. A force-aborted task keeps the name reserved until
    /// Taskvisor has observed the actor's physical exit and collected its terminal state.
    AlreadyExists,
    /// A conflict elsewhere caused this item in an all-or-nothing batch to fail.
    BatchRejected,
    /// `DropIfRunning` rejected a submission because the controller slot was busy.
    SlotBusy,
    /// The controller slot queue reached its configured capacity.
    QueueFull,
    /// A newer `Replace` submission displaced this queued submission.
    SupersededByReplace,
    /// An explicit remove or cancel operation rejected work before registry commit.
    RemovedFromQueue,
    /// Controller shutdown rejected work that had not reached registry admission.
    ControllerShuttingDown,
    /// The controller could not commit the submission to the runtime registry.
    AdmissionFailed,
    /// A configured registry or controller resource budget rejected admission.
    ResourceLimit,
}

impl RejectionKind {
    /// Returns the stable label used by logs and metrics.
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            Self::AlreadyExists => "already_exists",
            Self::BatchRejected => "batch_rejected",
            Self::SlotBusy => "slot_busy",
            Self::QueueFull => "queue_full",
            Self::SupersededByReplace => "superseded_by_replace",
            Self::RemovedFromQueue => "removed_from_queue",
            Self::ControllerShuttingDown => "controller_shutting_down",
            Self::AdmissionFailed => "admission_failed",
            Self::ResourceLimit => "resource_limit",
        }
    }
}

/// One best-effort runtime event with optional typed metadata.
///
/// Subscriber callbacks receive this value by reference. Match [`kind`](Self::kind) first, then read the
/// fields documented for that variant. Do not parse [`reason`](Self::reason) for program logic;
/// use typed category fields and their stable labels.
///
/// [`Event::new`] sets `kind`, `at`, and `seq`. The `with_*` builders attach metadata but do not
/// validate that a field belongs to the selected kind.
///
/// This struct is non-exhaustive; use `..` when matching it.
#[derive(Clone)]
#[non_exhaustive]
pub struct Event {
    /// Process-local construction sequence.
    ///
    /// It increases without wrapping and is not persisted across restarts.
    /// Gaps can appear because event delivery is best-effort.
    /// Concurrent effects may occur in a different order.
    pub seq: u64,
    /// Wall-clock timestamp captured when the event was created.
    ///
    /// Wall clocks can move. This value is not a monotonic ordering source.
    pub at: SystemTime,

    /// Configured task deadline in whole milliseconds.
    pub timeout_ms: Option<u32>,
    /// Delay before the next attempt in whole milliseconds.
    pub delay_ms: Option<u32>,
    /// Attempt duration in whole milliseconds.
    pub duration_ms: Option<u32>,
    /// Events represented by one coalesced overflow report.
    pub dropped: Option<u64>,
    /// Original finite ownership capacity for an `OwnershipCapacityRetired` event.
    pub configured_capacity: Option<usize>,
    /// Remaining usable ownership capacity after an `OwnershipCapacityRetired` event.
    pub effective_capacity: Option<usize>,
    /// Ownership units permanently removed by one `OwnershipCapacityRetired` event.
    pub retired_units: Option<usize>,
    /// Human-readable diagnostic detail.
    ///
    /// This text is not schema and may change. Use typed fields such as [`outcome_kind`](Self::outcome_kind)
    /// and [`rejection_kind`](Self::rejection_kind) for machine decisions.
    pub reason: Option<Arc<str>>,
    /// Final category for `TaskFinished` and rejected work.
    ///
    /// Use [`TaskOutcomeKind`] for branching and [`TaskOutcomeKind::as_label`] for telemetry labels.
    pub outcome_kind: Option<TaskOutcomeKind>,
    /// Rejection category for `TaskAddFailed` and `ControllerRejected`.
    ///
    /// Readable details remain available in [`reason`](Self::reason).
    pub rejection_kind: Option<RejectionKind>,
    /// One-based attempt number for attempt and backoff events.
    ///
    /// This is not the total number of attempts and is not set on `TaskFinished`.
    pub attempt: Option<u32>,
    /// Name associated with the event.
    ///
    /// Usually a task name. Diagnostics may store a subscriber, relay, or runtime component name.
    /// Controller events store a slot name.
    pub task: Option<Arc<str>>,
    /// Submission and run identity associated with the event.
    ///
    /// This is the canonical correlation key.
    /// Unlike [`task`](Self::task), it does not change during one submission.
    /// Controller events may carry it before runtime admission.
    ///
    /// See [`TaskId`] for process and counter limits.
    pub id: Option<TaskId>,
    /// Numeric exit code from a process-like task, when available.
    pub exit_code: Option<i32>,
    /// Event classification.
    pub kind: EventKind,
    /// Cause of a scheduled delay.
    pub backoff_source: Option<BackoffSource>,
}

impl Event {
    /// Creates an event with the current wall-clock time and next sequence value.
    ///
    /// # Panics
    ///
    /// Panics when the process-local event sequence is exhausted.
    #[must_use]
    pub fn new(kind: EventKind) -> Self {
        Self {
            seq: next_event_seq(),
            kind,
            at: SystemTime::now(),
            backoff_source: None,
            timeout_ms: None,
            delay_ms: None,
            duration_ms: None,
            dropped: None,
            configured_capacity: None,
            effective_capacity: None,
            retired_units: None,
            attempt: None,
            reason: None,
            outcome_kind: None,
            rejection_kind: None,
            task: None,
            id: None,
            exit_code: None,
        }
    }

    /// Attaches a readable reason.
    #[inline]
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<Arc<str>>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Attaches a machine-readable final outcome category.
    #[inline]
    #[must_use]
    pub fn with_outcome_kind(mut self, kind: TaskOutcomeKind) -> Self {
        self.outcome_kind = Some(kind);
        self
    }

    /// Attaches a machine-readable submission rejection category.
    ///
    /// This also sets [`outcome_kind`](Self::outcome_kind) to [`TaskOutcomeKind::Rejected`].
    #[inline]
    #[must_use]
    pub fn with_rejection_kind(mut self, kind: RejectionKind) -> Self {
        self.rejection_kind = Some(kind);
        self.outcome_kind = Some(TaskOutcomeKind::Rejected);
        self
    }

    /// Attaches the task, diagnostic component, or controller slot name.
    #[inline]
    #[must_use]
    pub fn with_task(mut self, task: impl Into<Arc<str>>) -> Self {
        self.task = Some(task.into());
        self
    }

    /// Attaches the submission and run identity.
    #[inline]
    #[must_use]
    pub fn with_id(mut self, id: TaskId) -> Self {
        self.id = Some(id);
        self
    }

    /// Attaches a deadline, stored as whole milliseconds.
    #[inline]
    #[must_use]
    pub fn with_timeout(mut self, d: Duration) -> Self {
        let ms = d.as_millis().min(u128::from(u32::MAX)) as u32;
        self.timeout_ms = Some(ms);
        self
    }

    /// Attaches a retry or restart delay, stored as whole milliseconds.
    #[inline]
    #[must_use]
    pub fn with_delay(mut self, d: Duration) -> Self {
        let ms = d.as_millis().min(u128::from(u32::MAX)) as u32;
        self.delay_ms = Some(ms);
        self
    }

    /// Attaches an attempt duration, stored as whole milliseconds.
    #[inline]
    #[must_use]
    pub fn with_duration(mut self, d: Duration) -> Self {
        let ms = d.as_millis().min(u128::from(u32::MAX)) as u32;
        self.duration_ms = Some(ms);
        self
    }

    /// Attaches the number of events represented by an overflow report.
    #[inline]
    #[must_use]
    pub fn with_dropped(mut self, dropped: u64) -> Self {
        self.dropped = Some(dropped);
        self
    }

    /// Attaches the 1-based attempt number.
    #[inline]
    #[must_use]
    pub fn with_attempt(mut self, n: u32) -> Self {
        self.attempt = Some(n);
        self
    }

    /// Attaches a numeric exit code from a process-like task.
    #[inline]
    #[must_use]
    pub fn with_exit_code(mut self, code: i32) -> Self {
        self.exit_code = Some(code);
        self
    }

    /// Attaches the cause of a scheduled delay.
    #[inline]
    #[must_use]
    pub fn with_backoff_source(mut self, source: BackoffSource) -> Self {
        self.backoff_source = Some(source);
        self
    }

    /// Marks a delay scheduled after success.
    #[inline]
    #[must_use]
    pub fn with_backoff_success(self) -> Self {
        self.with_backoff_source(BackoffSource::Success)
    }

    /// Marks a delay scheduled after failure.
    #[inline]
    #[must_use]
    pub fn with_backoff_failure(self) -> Self {
        self.with_backoff_source(BackoffSource::Failure)
    }

    /// Creates an overflow event for a subscriber or the internal relay.
    #[inline]
    #[must_use]
    pub fn subscriber_overflow(
        subscriber: impl Into<Arc<str>>,
        reason: impl Into<Arc<str>>,
    ) -> Self {
        Event::new(EventKind::SubscriberOverflow)
            .with_task(subscriber)
            .with_reason(reason)
    }

    /// Creates an event for a panicked subscriber callback.
    #[inline]
    #[must_use]
    pub fn subscriber_panicked(subscriber: impl Into<Arc<str>>, info: impl Into<Arc<str>>) -> Self {
        Event::new(EventKind::SubscriberPanicked)
            .with_task(subscriber)
            .with_reason(info)
    }

    /// Creates an event for an internal runtime failure.
    #[inline]
    #[must_use]
    pub fn runtime_failure(component: impl Into<Arc<str>>, reason: impl Into<Arc<str>>) -> Self {
        Event::new(EventKind::RuntimeFailure)
            .with_task(component)
            .with_reason(reason)
    }

    /// Creates an event for one permanent reduction of a finite ownership capacity.
    #[inline]
    #[must_use]
    pub fn ownership_capacity_retired(
        configured_capacity: usize,
        effective_capacity: usize,
        retired_units: usize,
    ) -> Self {
        let mut event = Event::new(EventKind::OwnershipCapacityRetired);
        event.configured_capacity = Some(configured_capacity);
        event.effective_capacity = Some(effective_capacity);
        event.retired_units = Some(retired_units);
        event
    }

    /// Returns whether this is an internal diagnostic event.
    #[inline]
    #[must_use]
    pub fn is_internal_diagnostic(&self) -> bool {
        matches!(
            self.kind,
            EventKind::SubscriberOverflow
                | EventKind::SubscriberPanicked
                | EventKind::RuntimeFailure
                | EventKind::OwnershipCapacityRetired
        )
    }
}

impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Event");
        d.field("seq", &self.seq);
        d.field("kind", &self.kind);
        if let Some(id) = self.id {
            d.field("id", &id);
        }
        if let Some(ref task) = self.task {
            d.field("task", task);
        }
        if let Some(attempt) = self.attempt {
            d.field("attempt", &attempt);
        }
        if let Some(ref reason) = self.reason {
            d.field("reason", reason);
        }
        if let Some(outcome_kind) = self.outcome_kind {
            d.field("outcome_kind", &outcome_kind);
        }
        if let Some(rejection_kind) = self.rejection_kind {
            d.field("rejection_kind", &rejection_kind);
        }
        if let Some(timeout_ms) = self.timeout_ms {
            d.field("timeout_ms", &timeout_ms);
        }
        if let Some(delay_ms) = self.delay_ms {
            d.field("delay_ms", &delay_ms);
        }
        if let Some(duration_ms) = self.duration_ms {
            d.field("duration_ms", &duration_ms);
        }
        if let Some(dropped) = self.dropped {
            d.field("dropped", &dropped);
        }
        if let Some(configured_capacity) = self.configured_capacity {
            d.field("configured_capacity", &configured_capacity);
        }
        if let Some(effective_capacity) = self.effective_capacity {
            d.field("effective_capacity", &effective_capacity);
        }
        if let Some(retired_units) = self.retired_units {
            d.field("retired_units", &retired_units);
        }
        if let Some(exit_code) = self.exit_code {
            d.field("exit_code", &exit_code);
        }
        if let Some(ref src) = self.backoff_source {
            d.field("backoff_source", src);
        }
        d.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_increases_monotonically() {
        let a = Event::new(EventKind::AttemptStarting);
        let b = Event::new(EventKind::AttemptSucceeded);
        assert!(b.seq > a.seq, "seq must grow: {} vs {}", a.seq, b.seq);
    }

    #[test]
    fn sequence_uses_zero_as_an_exhausted_sentinel() {
        assert_eq!(advance_event_seq(1), Some(2));
        assert_eq!(advance_event_seq(u64::MAX - 1), Some(u64::MAX));
        assert_eq!(advance_event_seq(u64::MAX), Some(0));
        assert_eq!(advance_event_seq(0), None);
    }

    #[test]
    fn event_kind_labels_are_stable() {
        let cases = [
            (EventKind::SubscriberPanicked, "subscriber_panicked"),
            (EventKind::RuntimeFailure, "runtime_failure"),
            (
                EventKind::OwnershipCapacityRetired,
                "ownership_capacity_retired",
            ),
            (EventKind::SubscriberOverflow, "subscriber_overflow"),
            (EventKind::ShutdownRequested, "shutdown_requested"),
            (EventKind::AllStoppedWithinGrace, "all_stopped_within_grace"),
            (EventKind::GraceExceeded, "grace_exceeded"),
            (EventKind::AttemptStarting, "attempt_starting"),
            (EventKind::AttemptSucceeded, "attempt_succeeded"),
            (EventKind::AttemptCanceled, "attempt_canceled"),
            (EventKind::AttemptFailed, "attempt_failed"),
            (EventKind::AttemptTimedOut, "attempt_timed_out"),
            (EventKind::BackoffScheduled, "backoff_scheduled"),
            (EventKind::TaskAddRequested, "task_add_requested"),
            (EventKind::TaskAdded, "task_added"),
            (EventKind::TaskAddFailed, "task_add_failed"),
            (EventKind::TaskRemoveRequested, "task_remove_requested"),
            (EventKind::TaskRemoved, "task_removed"),
            (EventKind::TaskFinished, "task_finished"),
            #[cfg(feature = "controller")]
            (EventKind::ControllerRejected, "controller_rejected"),
            #[cfg(feature = "controller")]
            (EventKind::ControllerSubmitted, "controller_submitted"),
            #[cfg(feature = "controller")]
            (
                EventKind::ControllerSlotTransition,
                "controller_slot_transition",
            ),
        ];

        for (kind, expected) in cases {
            assert_eq!(kind.as_label(), expected, "{kind:?}");
        }
    }

    #[test]
    fn new_event_leaves_all_optionals_empty() {
        let ev = Event::new(EventKind::AttemptStarting);
        assert_eq!(ev.timeout_ms, None);
        assert_eq!(ev.delay_ms, None);
        assert_eq!(ev.duration_ms, None);
        assert_eq!(ev.dropped, None);
        assert_eq!(ev.configured_capacity, None);
        assert_eq!(ev.effective_capacity, None);
        assert_eq!(ev.retired_units, None);
        assert_eq!(ev.attempt, None);
        assert_eq!(ev.exit_code, None);
        assert_eq!(ev.reason, None);
        assert_eq!(ev.outcome_kind, None);
        assert_eq!(ev.rejection_kind, None);
        assert_eq!(ev.task, None);
        assert_eq!(ev.id, None);
        assert_eq!(ev.backoff_source, None);
    }

    #[test]
    fn rejection_kind_labels_are_stable() {
        let cases = [
            (RejectionKind::AlreadyExists, "already_exists"),
            (RejectionKind::BatchRejected, "batch_rejected"),
            (RejectionKind::SlotBusy, "slot_busy"),
            (RejectionKind::QueueFull, "queue_full"),
            (RejectionKind::SupersededByReplace, "superseded_by_replace"),
            (RejectionKind::RemovedFromQueue, "removed_from_queue"),
            (
                RejectionKind::ControllerShuttingDown,
                "controller_shutting_down",
            ),
            (RejectionKind::AdmissionFailed, "admission_failed"),
            (RejectionKind::ResourceLimit, "resource_limit"),
        ];

        for (kind, expected) in cases {
            assert_eq!(kind.as_label(), expected, "{kind:?}");
        }

        let rejected =
            Event::new(EventKind::TaskAddFailed).with_rejection_kind(RejectionKind::AlreadyExists);
        assert_eq!(rejected.rejection_kind, Some(RejectionKind::AlreadyExists));
        assert_eq!(rejected.outcome_kind, Some(TaskOutcomeKind::Rejected));
    }

    #[test]
    fn ms_builders_set_then_clamp_to_u32_max() {
        let normal = Duration::from_millis(42);
        let huge = Duration::from_millis(u64::from(u32::MAX) + 1000);
        type Builder = fn(Event, Duration) -> Event;
        type ReadMs = fn(&Event) -> Option<u32>;

        let cases: [(&str, EventKind, Builder, ReadMs); 3] = [
            (
                "timeout",
                EventKind::AttemptTimedOut,
                Event::with_timeout,
                |e| e.timeout_ms,
            ),
            (
                "delay",
                EventKind::BackoffScheduled,
                Event::with_delay,
                |e| e.delay_ms,
            ),
            (
                "duration",
                EventKind::AttemptSucceeded,
                Event::with_duration,
                |e| e.duration_ms,
            ),
        ];

        for (label, kind, build, read) in cases {
            assert_eq!(read(&build(Event::new(kind), normal)), Some(42), "{label}");
            assert_eq!(
                read(&build(Event::new(kind), huge)),
                Some(u32::MAX),
                "{label} must saturate"
            );
        }
    }

    #[test]
    fn is_internal_diagnostic_covers_all_variants() {
        for kind in [
            EventKind::SubscriberOverflow,
            EventKind::SubscriberPanicked,
            EventKind::RuntimeFailure,
            EventKind::OwnershipCapacityRetired,
        ] {
            assert!(Event::new(kind).is_internal_diagnostic(), "{kind:?}");
        }
        assert!(!Event::new(EventKind::AttemptStarting).is_internal_diagnostic());
    }

    #[test]
    fn diagnostic_factories_set_kind_task_and_reason() {
        let overflow = Event::subscriber_overflow("my-sub", "full").with_dropped(7);
        assert_eq!(overflow.kind, EventKind::SubscriberOverflow);
        assert_eq!(
            overflow.task.as_deref(),
            Some("my-sub"),
            "subscriber name lives in `task`"
        );
        assert_eq!(
            overflow.reason.as_deref(),
            Some("full"),
            "`reason` is the bare cause, not a re-encoding of the subscriber name"
        );
        assert_eq!(overflow.dropped, Some(7));

        let panicked = Event::subscriber_panicked("my-sub", "boom");
        assert_eq!(panicked.kind, EventKind::SubscriberPanicked);
        assert_eq!(panicked.task.as_deref(), Some("my-sub"));
        assert_eq!(panicked.reason.as_deref(), Some("boom"));

        let runtime_failure = Event::runtime_failure("registry", "listener join failed");
        assert_eq!(runtime_failure.kind, EventKind::RuntimeFailure);
        assert_eq!(runtime_failure.task.as_deref(), Some("registry"));
        assert_eq!(
            runtime_failure.reason.as_deref(),
            Some("listener join failed")
        );
    }

    #[test]
    fn ownership_retirement_factory_sets_typed_capacity_values() {
        let event = Event::ownership_capacity_retired(16, 14, 2);

        assert_eq!(event.kind, EventKind::OwnershipCapacityRetired);
        assert_eq!(event.configured_capacity, Some(16));
        assert_eq!(event.effective_capacity, Some(14));
        assert_eq!(event.retired_units, Some(2));
        assert!(event.is_internal_diagnostic());
    }

    #[test]
    fn with_exit_code_keeps_sign() {
        for (kind, code) in [
            (EventKind::AttemptFailed, 42),
            (EventKind::TaskFinished, -1),
        ] {
            assert_eq!(Event::new(kind).with_exit_code(code).exit_code, Some(code));
        }
    }

    #[test]
    fn backoff_source_labels_and_builders_are_stable() {
        for (source, label) in [
            (BackoffSource::Success, "success"),
            (BackoffSource::Failure, "failure"),
        ] {
            assert_eq!(source.as_label(), label);
        }

        let generic =
            Event::new(EventKind::BackoffScheduled).with_backoff_source(BackoffSource::Failure);
        assert_eq!(generic.backoff_source, Some(BackoffSource::Failure));

        assert_eq!(
            Event::new(EventKind::BackoffScheduled)
                .with_backoff_success()
                .backoff_source,
            Some(BackoffSource::Success)
        );
        assert_eq!(
            Event::new(EventKind::BackoffScheduled)
                .with_backoff_failure()
                .backoff_source,
            Some(BackoffSource::Failure)
        );
    }

    #[test]
    fn debug_renders_exit_code_only_when_set() {
        let ev = Event::new(EventKind::TaskFinished)
            .with_outcome_kind(TaskOutcomeKind::ForceAborted)
            .with_exit_code(137);
        assert!(
            format!("{ev:?}").contains("exit_code: 137"),
            "Debug must surface exit_code when present"
        );
        assert!(
            format!("{ev:?}").contains("outcome_kind: ForceAborted"),
            "Debug must surface outcome_kind when present"
        );

        let none = Event::new(EventKind::AttemptSucceeded);
        assert!(
            !format!("{none:?}").contains("exit_code"),
            "Debug must omit exit_code when absent"
        );
    }
}
