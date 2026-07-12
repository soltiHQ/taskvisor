//! # Runtime event model.
//!
//! This module defines the event records emitted by taskvisor.
//!
//! Events are used for observability: logs, metrics, dashboards, tests, and subscriber integrations.
//! Delivery is best-effort; consumers can lag and miss events. Do not use the event bus as durable storage.
//!
//! | Type              | Role                                       |
//! |-------------------|--------------------------------------------|
//! | [`EventKind`]     | Event classification                       |
//! | [`Event`]         | Event payload and metadata                 |
//! | [`BackoffSource`] | Why a `BackoffScheduled` event was emitted |
//!
//! ## Sequence Numbers
//!
//! Each event has a unique, increasing `seq` assigned when the event is created.
//! `seq` is useful for sorting and de-duplication after a lag gap. `seq` is not a causal clock.
//! With concurrent publishers, it reflects event construction order, not a guaranteed runtime order.
//!
//! ## Field Model
//!
//! [`Event`] is a flat record with optional fields. Which fields are set depends on [`EventKind`].
//!
//! Common fields:
//! - `seq`: unique event sequence.
//! - `at`: wall-clock timestamp.
//! - `kind`: event type.
//!
//! Correlation fields:
//! - `id`: runtime task identity, when the event belongs to a task run.
//! - `attempt`: task attempt number, starting from 1.
//! - `task`: a task name or subscriber name.
//!
//! Timing fields are stored in milliseconds and saturate at `u32::MAX`.
//!
//! `reason` is a diagnostic text unless a variant documents a small stable set of values.
//!
//! ## Example
//!
//! ```rust
//! use std::time::Duration;
//! use taskvisor::{Event, EventKind};
//!
//! let ev = Event::new(EventKind::TaskFailed)
//!     .with_task("demo-task")
//!     .with_reason("boom")
//!     .with_attempt(3)
//!     .with_duration(Duration::from_millis(42));
//!
//! assert_eq!(ev.kind, EventKind::TaskFailed);
//! assert_eq!(ev.task.as_deref(), Some("demo-task"));
//! assert_eq!(ev.reason.as_deref(), Some("boom"));
//! assert_eq!(ev.duration_ms, Some(42));
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, SystemTime};

use crate::identity::TaskId;

/// Global counter minting unique, monotonic `seq` values at event construction.
static EVENT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Classification of runtime events.
///
/// Every event has `seq`, `at`, and `kind`.
/// Variant docs list only the additional fields normally set by the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventKind {
    /// Subscriber panicked during event processing.
    ///
    /// Sets:
    /// - `task`: subscriber name
    /// - `reason`: panic info/message
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    SubscriberPanicked,

    /// Subscriber dropped an event (queue full or worker closed).
    ///
    /// Sets:
    /// - `task`: subscriber name (or the internal consumer that lagged)
    /// - `reason`: bare cause — `"full"`, `"closed"`, or `"lagged(n)"`
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    SubscriberOverflow,

    /// Shutdown requested (OS signal observed).
    ///
    /// Sets:
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    ShutdownRequested,

    /// All tasks stopped within configured grace period.
    ///
    /// Sets:
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    AllStoppedWithinGrace,

    /// Grace period exceeded; some tasks did not stop in time.
    ///
    /// Sets:
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    GraceExceeded,

    /// Task is starting an attempt.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: attempt number (1-based, per actor)
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskStarting,

    /// Task attempt finished successfully.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `duration_ms`: attempt duration
    TaskStopped,

    /// Task attempt returned [`TaskError::Canceled`](crate::TaskError::Canceled).
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `duration_ms`: attempt duration
    TaskCanceled,

    /// Task attempt returned an error.
    ///
    /// This includes retryable failures, timeouts, and fatal errors.
    /// The actor later decides whether to retry, exhaust, or die.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `duration_ms`: attempt duration
    /// - `reason`: error message
    /// - `exit_code`: process-like exit code, when available
    TaskFailed,

    /// Task exceeded its configured timeout for this attempt.
    ///
    /// A timeout is followed by a `TaskFailed` event carrying `TaskError::Timeout`.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `timeout_ms`: configured timeout
    /// - `duration_ms`: elapsed attempt duration
    TimeoutHit,

    /// Next attempt scheduled (after success or failure).
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: previous attempt number
    /// - `delay_ms`: delay before the next attempt (ms)
    /// - `backoff_source`: `Success` or `Failure`
    /// - `reason`: last failure message (only for failure-driven backoff)
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    BackoffScheduled,

    /// Request to add a new task to the supervisor.
    ///
    /// Published before an `Add` command is sent. For an atomic `AddBatch`, one
    /// request event is published per item before the whole command is sent.
    ///
    /// Sets:
    /// - `id`: task run identity (pre-allocated for this add request)
    /// - `task`: logical task name
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskAddRequested,

    /// Task was successfully added (actor spawned and registered).
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskAdded,

    /// Task could not be added because its name conflicts or its atomic static
    /// batch was rejected.
    ///
    /// Published by Registry instead of `TaskAdded`. No actor is spawned for a
    /// rejected dynamic add or for any item in a rejected static batch.
    ///
    /// Sets:
    /// - `id`: task run identity of the rejected add request
    /// - `task`: task name
    /// - `reason`: e.g. "already_exists" or "batch_rejected"
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskAddFailed,

    /// Request to remove a task from the supervisor.
    ///
    /// This is a best-effort observability event, not a removal command or acknowledgement.
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name, when known
    /// - `reason`: optional removal reason
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskRemoveRequested,

    /// Task was removed from the supervisor (after join/cleanup).
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskRemoved,

    /// Actor exhausted its restart policy and will not restart.
    ///
    /// Emitted when:
    /// - `RestartPolicy::Never` → task completed (success or handled case)
    /// - `RestartPolicy::OnFailure` → task completed successfully
    /// - retry budget exceeded on a retryable failure
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: last attempt number
    /// - `reason`: optional message
    /// - `exit_code`: numeric exit code (process-like runtimes); `None` otherwise
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    ActorExhausted,

    /// Actor terminated permanently due to a fatal error.
    ///
    /// Emitted when:
    /// - Task returned `TaskError::Fatal`
    ///
    /// Sets:
    /// - `id`: task run identity
    /// - `task`: task name
    /// - `attempt`: last attempt number
    /// - `reason`: fatal error message
    /// - `exit_code`: numeric exit code when the fatal error; `None` for logical errors
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    ActorDead,

    #[cfg(feature = "controller")]
    /// Controller submission rejected (queue full, add failed, superseded, etc).
    ///
    /// Sets:
    /// - `task`: slot name
    /// - `id`: the rejected submission's [`TaskId`], when the rejection concerns a specific submission.
    ///   Absent for slot- or loop-level diagnostics that have no submission behind
    ///   them (e.g. a failed deferred removal or the controller loop exiting).
    /// - `reason`: rejection reason ("queue_full", "add_failed: ...", "superseded_by_replace", "controller_shutting_down", etc)
    ControllerRejected,

    #[cfg(feature = "controller")]
    /// Task submitted successfully to controller slot.
    ///
    /// Sets:
    /// - `task`: slot name
    /// - `id`: the submission's [`TaskId`]
    /// - `reason`: a human-readable admission summary, e.g. `admission=Queue status=admitting` or
    ///   `started_from_queue depth=N` (exact text is diagnostic, not a stable contract)
    ControllerSubmitted,

    #[cfg(feature = "controller")]
    /// Slot transitioned state (e.g. Admitting → Running, Running → Terminating).
    ///
    /// Sets:
    /// - `task`: slot name
    /// - `reason`: the transition, e.g. `admitting→running` or `running→terminating (replace)`
    ControllerSlotTransition,
}

impl EventKind {
    /// Returns a stable machine-readable label for logs and metrics.
    ///
    /// The label is the snake_case form of the variant name.
    /// Use it as an event name in tracing or as a metrics label value.
    ///
    /// ```rust
    /// use taskvisor::EventKind;
    ///
    /// assert_eq!(EventKind::TaskStarting.as_label(), "task_starting");
    /// assert_eq!(EventKind::BackoffScheduled.as_label(), "backoff_scheduled");
    /// ```
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            EventKind::SubscriberPanicked => "subscriber_panicked",
            EventKind::SubscriberOverflow => "subscriber_overflow",
            EventKind::ShutdownRequested => "shutdown_requested",
            EventKind::AllStoppedWithinGrace => "all_stopped_within_grace",
            EventKind::GraceExceeded => "grace_exceeded",
            EventKind::TaskStarting => "task_starting",
            EventKind::TaskStopped => "task_stopped",
            EventKind::TaskCanceled => "task_canceled",
            EventKind::TaskFailed => "task_failed",
            EventKind::TimeoutHit => "timeout_hit",
            EventKind::BackoffScheduled => "backoff_scheduled",
            EventKind::TaskAddRequested => "task_add_requested",
            EventKind::TaskAdded => "task_added",
            EventKind::TaskAddFailed => "task_add_failed",
            EventKind::TaskRemoveRequested => "task_remove_requested",
            EventKind::TaskRemoved => "task_removed",
            EventKind::ActorExhausted => "actor_exhausted",
            EventKind::ActorDead => "actor_dead",
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

/// Runtime event with optional metadata.
///
/// - `at`: wall-clock timestamp (for logs)
/// - `seq`: globally unique, monotonic sequence (construction order; see the module-level "Sequence numbers" note)
/// - other optional fields are set depending on the [`EventKind`]
///
/// Fields are public for reading;
/// Construct via [`Event::new`] and the `with_*` builders.
///
/// # Also
///
/// - [`EventKind`] - event classification
/// - [`Subscribe`](crate::Subscribe) - user-defined event handler trait
/// - `LogWriter` (feature = `logging`) - built-in human-readable event printer
#[derive(Clone)]
#[non_exhaustive]
pub struct Event {
    /// Globally unique, monotonically increasing sequence number.
    pub seq: u64,
    /// Wall-clock timestamp.
    pub at: SystemTime,

    /// Task timeout in milliseconds (compact).
    pub timeout_ms: Option<u32>,
    /// Backoff delay before next attempt in milliseconds (compact).
    pub delay_ms: Option<u32>,
    /// Wall-clock duration of the attempt in milliseconds (compact).
    pub duration_ms: Option<u32>,
    /// Human-readable reason (errors, overflow details, etc.).
    pub reason: Option<Arc<str>>,
    /// Attempt count (starting from 1).
    pub attempt: Option<u32>,
    /// Name of the task, if applicable. A free-form human **label** (not an identity).
    pub task: Option<Arc<str>>,
    /// Runtime identity of the task run instance this event belongs to, if applicable.
    ///
    /// This is the canonical correlation key: unlike [`task`](Self::task) (a human label
    /// that may repeat), a [`TaskId`] is unique per run instance and never reused.
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
    /// Creates a new event of the given kind with current timestamp and next sequence number.
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
            task: None,
            id: None,
            exit_code: None,
        }
    }

    /// Attaches a human-readable reason.
    #[inline]
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<Arc<str>>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Attaches a task name.
    #[inline]
    #[must_use]
    pub fn with_task(mut self, task: impl Into<Arc<str>>) -> Self {
        self.task = Some(task.into());
        self
    }

    /// Attaches the runtime task identity ([`TaskId`]).
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

    /// Attaches the attempt's wall-clock duration (stored as milliseconds).
    #[inline]
    #[must_use]
    pub fn with_duration(mut self, d: Duration) -> Self {
        let ms = d.as_millis().min(u128::from(u32::MAX)) as u32;
        self.duration_ms = Some(ms);
        self
    }

    /// Attaches an attempt count.
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

    /// Marks that this backoff comes from a successful attempt.
    #[inline]
    #[must_use]
    pub fn with_backoff_success(mut self) -> Self {
        self.backoff_source = Some(BackoffSource::Success);
        self
    }

    /// Marks that this backoff comes from a failed attempt.
    #[inline]
    #[must_use]
    pub fn with_backoff_failure(mut self) -> Self {
        self.backoff_source = Some(BackoffSource::Failure);
        self
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

    /// Returns `true` for internal diagnostic events.
    #[inline]
    #[must_use]
    pub fn is_internal_diagnostic(&self) -> bool {
        matches!(
            self.kind,
            EventKind::SubscriberOverflow | EventKind::SubscriberPanicked
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
        let a = Event::new(EventKind::TaskStarting);
        let b = Event::new(EventKind::TaskStopped);
        assert!(b.seq > a.seq, "seq must grow: {} vs {}", a.seq, b.seq);
    }

    #[test]
    fn new_event_leaves_all_optionals_empty() {
        let ev = Event::new(EventKind::TaskStarting);
        assert_eq!(ev.timeout_ms, None);
        assert_eq!(ev.delay_ms, None);
        assert_eq!(ev.duration_ms, None);
        assert_eq!(ev.attempt, None);
        assert_eq!(ev.exit_code, None);
        assert_eq!(ev.reason, None);
        assert_eq!(ev.task, None);
        assert_eq!(ev.id, None);
        assert_eq!(ev.backoff_source, None);
    }

    #[test]
    fn ms_builders_set_then_clamp_to_u32_max() {
        let normal = Duration::from_millis(42);
        let huge = Duration::from_millis(u64::from(u32::MAX) + 1000);

        assert_eq!(
            Event::new(EventKind::TimeoutHit)
                .with_timeout(normal)
                .timeout_ms,
            Some(42)
        );
        assert_eq!(
            Event::new(EventKind::TimeoutHit)
                .with_timeout(huge)
                .timeout_ms,
            Some(u32::MAX)
        );

        assert_eq!(
            Event::new(EventKind::BackoffScheduled)
                .with_delay(normal)
                .delay_ms,
            Some(42)
        );
        assert_eq!(
            Event::new(EventKind::BackoffScheduled)
                .with_delay(huge)
                .delay_ms,
            Some(u32::MAX)
        );

        assert_eq!(
            Event::new(EventKind::TaskStopped)
                .with_duration(normal)
                .duration_ms,
            Some(42)
        );
        assert_eq!(
            Event::new(EventKind::TaskStopped)
                .with_duration(huge)
                .duration_ms,
            Some(u32::MAX)
        );
    }

    #[test]
    fn is_internal_diagnostic_covers_both_variants() {
        assert!(Event::new(EventKind::SubscriberOverflow).is_internal_diagnostic());
        assert!(Event::new(EventKind::SubscriberPanicked).is_internal_diagnostic());
        assert!(!Event::new(EventKind::TaskStarting).is_internal_diagnostic());
    }

    #[test]
    fn subscriber_factories_set_kind_task_and_reason() {
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
    }

    #[test]
    fn with_exit_code_keeps_sign() {
        assert_eq!(
            Event::new(EventKind::TaskFailed)
                .with_exit_code(42)
                .exit_code,
            Some(42)
        );
        assert_eq!(
            Event::new(EventKind::ActorDead)
                .with_exit_code(-1)
                .exit_code,
            Some(-1)
        );
    }

    #[test]
    fn debug_renders_exit_code_only_when_set() {
        let ev = Event::new(EventKind::ActorExhausted).with_exit_code(137);
        assert!(
            format!("{ev:?}").contains("exit_code: 137"),
            "Debug must surface exit_code when present"
        );

        let none = Event::new(EventKind::TaskStopped);
        assert!(
            !format!("{none:?}").contains("exit_code"),
            "Debug must omit exit_code when absent"
        );
    }
}
