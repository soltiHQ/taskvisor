//! # Event data model
//!
//! [`Event`] is a flat record.
//! [`EventKind`] tells you what happened, and the optional fields give details.
//! Delivery is best-effort; events are not durable storage and are not a reliable completion signal.
//!
//! | Type              | Role                                       |
//! |-------------------|--------------------------------------------|
//! | [`EventKind`]     | Event classification                       |
//! | [`Event`]         | Event payload and metadata                 |
//! | [`BackoffSource`] | Why a `BackoffScheduled` event was emitted |
//! | [`RejectionKind`] | Machine-readable submission rejection      |
//! | [`TaskOutcomeKind`]| Machine-readable final task outcome        |
//!
//! ## Sequence numbers
//!
//! [`Event::new`] gives each event a process-local increasing `seq`.
//! Use it to sort observed events and detect gaps.
//! It is not stored across process restarts.
//!
//! With concurrent publishers, it shows event construction order, not a guaranteed order of runtime effects or subscriber callbacks.
//!
//! ## Fields
//!
//! [`Event`] is a flat record with optional fields. Which fields are set depends on [`EventKind`].
//!
//! Always present:
//! - `seq`: process-local event sequence.
//! - `at`: wall-clock timestamp.
//! - `kind`: event type.
//!
//! Present when relevant:
//! - `id`: the stable [`TaskId`] for one submission and run.
//! - `attempt`: task attempt number, starting from 1.
//! - `task`: usually a task name. Subscriber diagnostics use it for the subscriber name, and controller events use it for the slot name.
//! - `outcome_kind`: machine-readable final outcome for `TaskFinished` and rejected work.
//! - `rejection_kind`: machine-readable category for a rejected add or controller submission.
//!
//! `timeout_ms`, `delay_ms`, and `duration_ms` use whole milliseconds.
//! Values above `u32::MAX` milliseconds are stored as `u32::MAX`.
//!
//! Treat `reason` as readable diagnostic text, not schema.
//! Use [`TaskOutcomeKind`] and [`RejectionKind`] for machine decisions.
//! > Use [`EventKind::as_label`] for a stable event label.
//!
//! ## Example
//!
//! ```rust
//! use std::time::Duration;
//! use taskvisor::{Event, EventKind};
//!
//! let ev = Event::new(EventKind::AttemptFailed)
//!     .with_task("demo-task")
//!     .with_reason("boom")
//!     .with_attempt(3)
//!     .with_duration(Duration::from_millis(42));
//!
//! assert_eq!(ev.kind, EventKind::AttemptFailed);
//! assert_eq!(ev.task.as_deref(), Some("demo-task"));
//! assert_eq!(ev.reason.as_deref(), Some("boom"));
//! assert_eq!(ev.duration_ms, Some(42));
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, SystemTime};

use crate::{TaskOutcomeKind, identity::TaskId};

/// Process-local counter for `seq` values.
///
/// It wraps after `2^64` allocations.
static EVENT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Describes what happened in the runtime.
///
/// Every event has `seq`, `at`, and `kind`.
/// Variant docs list only the additional fields normally set by the runtime.
/// Include a wildcard arm when matching because new event kinds may be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventKind {
    /// A subscriber panicked while processing an event.
    ///
    /// Sets:
    /// - `task`: subscriber name
    /// - `reason`: panic info/message
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    SubscriberPanicked,

    /// An internal runtime component failed.
    ///
    /// This includes a caught panic or a worker that did not join cleanly.
    ///
    /// Sets:
    /// - `task`: runtime component name
    /// - `reason`: diagnostic failure details
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    RuntimeFailure,

    /// An event was lost because a subscriber path fell behind or closed.
    ///
    /// Sets:
    /// - `task`: subscriber name (or the internal consumer that lagged)
    /// - `reason`: `"full"`, `"closed"`, or `"lagged(n)"`
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    SubscriberOverflow,

    /// Shutdown was requested.
    ///
    /// This can come from an OS signal or an explicit runtime shutdown request.
    ///
    /// Sets:
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    ShutdownRequested,

    /// All tasks stopped within configured grace period.
    ///
    /// Sets:
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    AllStoppedWithinGrace,

    /// Grace period exceeded; some tasks did not stop in time.
    ///
    /// Sets:
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    GraceExceeded,

    /// A task attempt is starting.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: attempt number (1-based for this task run)
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    AttemptStarting,

    /// A task attempt returned `Ok(())`.
    ///
    /// This is an attempt result, not always the final task result. Under
    /// [`RestartPolicy::Always`](crate::RestartPolicy::Always), another attempt follows.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `duration_ms`: attempt duration
    AttemptSucceeded,

    /// Task attempt returned [`TaskError::Canceled`](crate::TaskError::Canceled).
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `duration_ms`: attempt duration
    AttemptCanceled,

    /// A task attempt returned a failure.
    ///
    /// This includes retryable failures, fatal errors, task-returned timeouts,
    /// and panics caught while running user code. A configured per-attempt
    /// deadline instead emits [`AttemptTimedOut`](Self::AttemptTimedOut).
    /// A later event shows whether Taskvisor retries or reaches a terminal state.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `duration_ms`: attempt duration
    /// - `reason`: error message
    /// - `exit_code`: process-like exit code, when available
    AttemptFailed,

    /// Task exceeded its configured timeout for this attempt.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `timeout_ms`: configured timeout
    /// - `duration_ms`: elapsed attempt duration
    AttemptTimedOut,

    /// The next attempt was scheduled after success or failure.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: previous attempt number
    /// - `delay_ms`: delay before the next attempt (ms)
    /// - `backoff_source`: `Success` or `Failure`
    /// - `reason`: last failure message (only for failure-driven backoff)
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    BackoffScheduled,

    /// An add request was published before Taskvisor processed it.
    ///
    /// This does not confirm admission.
    /// For an all-or-nothing batch, Taskvisor publishes one request event per item before it sends the whole batch command.
    ///
    /// Sets:
    /// - `id`: task run identity (pre-allocated for this add request)
    /// - `task`: logical task name
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    TaskAddRequested,

    /// A task was registered and its managed runner was spawned.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    TaskAdded,

    /// A task was not added because its name conflicted or its all-or-nothing batch was rejected.
    ///
    /// No task runner is spawned for a rejected dynamic add.
    /// If an all-or-nothing batch is rejected, no task runner is spawned for any item.
    ///
    /// Sets:
    /// - `id`: task run identity of the rejected add request
    /// - `task`: task name
    /// - `outcome_kind`: [`TaskOutcomeKind::Rejected`]
    /// - `rejection_kind`: [`RejectionKind::AlreadyExists`] or [`RejectionKind::BatchRejected`]
    /// - `reason`: diagnostic rejection details
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    TaskAddFailed,

    /// A remove request was published before Taskvisor completed it.
    ///
    /// This is not proof of removal.
    /// Use the management method's result or a waiter when you need a reliable answer.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name, when known
    /// - `reason`: optional removal reason
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    TaskRemoveRequested,

    /// Task was removed from the supervisor after terminal cleanup.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    TaskRemoved,

    /// A registered task reached its final outcome and will not start another attempt.
    ///
    /// This is emitted once after the task runner is joined and before
    /// [`TaskRemoved`](Self::TaskRemoved). It also covers force-abort and an
    /// internal runner panic, even when no attempt-level terminal event exists.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `outcome_kind`: stable machine-readable final category
    /// - `reason`: optional diagnostic detail; never parse it as schema
    /// - `exit_code`: process-like exit code, when available
    /// - `at`: wall-clock timestamp
    /// - `seq`: process-local sequence
    TaskFinished,

    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    /// The controller rejected a submission.
    ///
    /// Sets:
    /// - `task`: slot name, when known
    /// - `id`: the rejected submission's [`TaskId`]
    /// - `outcome_kind`: [`TaskOutcomeKind::Rejected`]
    /// - `rejection_kind`: stable machine-readable rejection category
    /// - `reason`: readable rejection details
    ControllerRejected,

    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    /// The controller accepted a submission.
    ///
    /// The task may still be queued or waiting for runtime registration.
    /// This event does not mean that the task body has started.
    ///
    /// Sets:
    /// - `task`: slot name
    /// - `id`: the submission's [`TaskId`]
    /// - `reason`: a readable admission summary, e.g. `admission=Queue status=admitting` or
    ///   `started_from_queue depth=N` (exact text is diagnostic, not a stable contract)
    ControllerSubmitted,

    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    /// A controller slot changed state.
    ///
    /// Sets:
    /// - `task`: slot name
    /// - `reason`: readable transition text; it is not a stable machine contract
    ControllerSlotTransition,
}

impl EventKind {
    /// Returns a stable machine-readable label for logs and metrics.
    ///
    /// The label is the snake_case form of the variant name.
    /// Use it as an event name in tracing or as a metrics label value.
    ///
    /// ```text
    /// EventKind::AttemptStarting
    ///           │ as_label()
    ///           ▼
    ///     "attempt_starting"
    ///        ├── log field:    event="attempt_starting"
    ///        └── metric label: event="attempt_starting"
    /// ```
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

/// Reason for scheduling the next run/backoff.
///
/// A closed set (success vs failure); intentionally **not** `#[non_exhaustive]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackoffSource {
    /// Delay after a successful attempt under `RestartPolicy::Always`.
    Success,
    /// Delay after a retryable failure.
    Failure,
}

impl BackoffSource {
    /// Returns the stable machine-readable label used by logs and metrics.
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

/// Reason why a task or controller submission did not start.
///
/// [`Event::reason`] and the `reason` field on [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected) retain readable details.
/// Use this enum for branching, metrics, and state transitions.
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
    /// The registry already contains a task with the requested name.
    AlreadyExists,
    /// One conflicting item caused an all-or-nothing batch to reject this non-conflicting item.
    BatchRejected,
    /// `DropIfRunning` rejected a submission because the controller slot was busy.
    SlotBusy,
    /// The controller slot queue reached its configured capacity.
    QueueFull,
    /// A newer `Replace` submission displaced this queued submission.
    SupersededByReplace,
    /// An explicit remove or cancel operation removed the queued submission.
    RemovedFromQueue,
    /// Runtime shutdown rejected work that had not reached registry admission.
    ControllerShuttingDown,
    /// The controller could not commit the submission to the runtime registry.
    AdmissionFailed,
}

impl RejectionKind {
    /// Returns a stable machine-readable label for logs and metrics.
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
        }
    }
}

/// One runtime event with optional metadata.
///
/// - `at`: wall-clock timestamp (for logs)
/// - `seq`: process-local construction sequence; see the module-level limits
/// - other optional fields are set depending on the [`EventKind`]
///
/// Fields are public for reading. Create an event with [`Event::new`] and add optional values with the `with_*` builders.
/// > Use `..` when matching the struct because more fields may be added.
///
/// # Also
///
/// - [`EventKind`] - event classification
/// - [`Subscribe`](crate::Subscribe) - user-defined event handler trait
/// - `LogWriter` (feature = `logging`) - built-in readable event printer
#[derive(Clone)]
#[non_exhaustive]
pub struct Event {
    /// Process-local sequence number allocated when the event was created.
    ///
    /// It increases until the `u64` counter wraps and is not stored across process restarts.
    pub seq: u64,
    /// Wall-clock timestamp captured when the event was created.
    ///
    /// Wall clocks can move. Use `seq` for observed ordering, not `at`.
    pub at: SystemTime,

    /// Task timeout in milliseconds (compact).
    pub timeout_ms: Option<u32>,
    /// Backoff delay before next attempt in milliseconds (compact).
    pub delay_ms: Option<u32>,
    /// Elapsed duration of the attempt in milliseconds.
    pub duration_ms: Option<u32>,
    /// Human-readable diagnostic detail.
    ///
    /// This text is not schema and may change. Use typed fields such as
    /// [`outcome_kind`](Self::outcome_kind) and
    /// [`rejection_kind`](Self::rejection_kind) for machine decisions.
    pub reason: Option<Arc<str>>,
    /// Machine-readable final category for `TaskFinished` and rejected work.
    ///
    /// Use [`TaskOutcomeKind`] for branching and [`TaskOutcomeKind::as_label`]
    /// for telemetry labels.
    pub outcome_kind: Option<TaskOutcomeKind>,
    /// Machine-readable category for `TaskAddFailed` and `ControllerRejected`.
    ///
    /// Readable details remain available in [`reason`](Self::reason).
    pub rejection_kind: Option<RejectionKind>,
    /// Attempt count (starting from 1).
    pub attempt: Option<u32>,
    /// This is normally a task name. Subscriber diagnostics use it for a subscriber name, and controller events use it for a slot name.
    pub task: Option<Arc<str>>,
    /// Submission/run identity this event belongs to, if applicable.
    ///
    /// This is the canonical correlation key.
    /// Unlike [`task`](Self::task), it does not change during one submission.
    /// Controller events may carry it before runtime admission.
    ///
    /// See [`TaskId`] for process and counter limits.
    pub id: Option<TaskId>,
    /// Numeric exit code, from a process-like runtime.
    /// `None` for events that have no process behind them.
    pub exit_code: Option<i32>,
    /// Event classification.
    pub kind: EventKind,
    /// Source for backoff scheduling (success vs failure).
    pub backoff_source: Option<BackoffSource>,
}

impl Event {
    /// Creates an event with the current wall-clock time and the next sequence number.
    #[must_use]
    pub fn new(kind: EventKind) -> Self {
        Self {
            seq: EVENT_SEQ.fetch_add(1, AtomicOrdering::Relaxed),
            kind,
            at: SystemTime::now(),
            backoff_source: None,
            timeout_ms: None,
            delay_ms: None,
            duration_ms: None,
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
    /// This also sets [`outcome_kind`](Self::outcome_kind) to
    /// [`TaskOutcomeKind::Rejected`].
    #[inline]
    #[must_use]
    pub fn with_rejection_kind(mut self, kind: RejectionKind) -> Self {
        self.rejection_kind = Some(kind);
        self.outcome_kind = Some(TaskOutcomeKind::Rejected);
        self
    }

    /// Attaches a task name.
    #[inline]
    #[must_use]
    pub fn with_task(mut self, task: impl Into<Arc<str>>) -> Self {
        self.task = Some(task.into());
        self
    }

    /// Attaches the submission/run identity ([`TaskId`]).
    #[inline]
    #[must_use]
    pub fn with_id(mut self, id: TaskId) -> Self {
        self.id = Some(id);
        self
    }

    /// Attaches a timeout duration (stored as milliseconds).
    #[inline]
    #[must_use]
    pub fn with_timeout(mut self, d: Duration) -> Self {
        let ms = d.as_millis().min(u128::from(u32::MAX)) as u32;
        self.timeout_ms = Some(ms);
        self
    }

    /// Attaches a backoff delay (stored as milliseconds).
    #[inline]
    #[must_use]
    pub fn with_delay(mut self, d: Duration) -> Self {
        let ms = d.as_millis().min(u128::from(u32::MAX)) as u32;
        self.delay_ms = Some(ms);
        self
    }

    /// Attaches the attempt's elapsed duration (stored as milliseconds).
    #[inline]
    #[must_use]
    pub fn with_duration(mut self, d: Duration) -> Self {
        let ms = d.as_millis().min(u128::from(u32::MAX)) as u32;
        self.duration_ms = Some(ms);
        self
    }

    /// Attaches the 1-based attempt number.
    #[inline]
    #[must_use]
    pub fn with_attempt(mut self, n: u32) -> Self {
        self.attempt = Some(n);
        self
    }

    /// Attaches a numeric exit code (from a process-like runtime).
    #[inline]
    #[must_use]
    pub fn with_exit_code(mut self, code: i32) -> Self {
        self.exit_code = Some(code);
        self
    }

    /// Attaches the source that caused a backoff to be scheduled.
    #[inline]
    #[must_use]
    pub fn with_backoff_source(mut self, source: BackoffSource) -> Self {
        self.backoff_source = Some(source);
        self
    }

    /// Marks that this backoff comes from a successful attempt.
    #[inline]
    #[must_use]
    pub fn with_backoff_success(self) -> Self {
        self.with_backoff_source(BackoffSource::Success)
    }

    /// Marks that this backoff comes from a failed attempt.
    #[inline]
    #[must_use]
    pub fn with_backoff_failure(self) -> Self {
        self.with_backoff_source(BackoffSource::Failure)
    }

    /// Creates a subscriber overflow event.
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

    /// Creates a subscriber panic event.
    #[inline]
    #[must_use]
    pub fn subscriber_panicked(subscriber: impl Into<Arc<str>>, info: impl Into<Arc<str>>) -> Self {
        Event::new(EventKind::SubscriberPanicked)
            .with_task(subscriber)
            .with_reason(info)
    }

    /// Creates an internal runtime failure event.
    #[inline]
    #[must_use]
    pub fn runtime_failure(component: impl Into<Arc<str>>, reason: impl Into<Arc<str>>) -> Self {
        Event::new(EventKind::RuntimeFailure)
            .with_task(component)
            .with_reason(reason)
    }

    /// Returns `true` for internal diagnostic events.
    #[inline]
    #[must_use]
    pub fn is_internal_diagnostic(&self) -> bool {
        matches!(
            self.kind,
            EventKind::SubscriberOverflow
                | EventKind::SubscriberPanicked
                | EventKind::RuntimeFailure
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
    fn event_kind_labels_are_stable() {
        let cases = [
            (EventKind::SubscriberPanicked, "subscriber_panicked"),
            (EventKind::RuntimeFailure, "runtime_failure"),
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
        ] {
            assert!(Event::new(kind).is_internal_diagnostic(), "{kind:?}");
        }
        assert!(!Event::new(EventKind::AttemptStarting).is_internal_diagnostic());
    }

    #[test]
    fn diagnostic_factories_set_kind_task_and_reason() {
        let overflow = Event::subscriber_overflow("my-sub", "full");
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
