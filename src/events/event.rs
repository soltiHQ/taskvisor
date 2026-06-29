//! # Runtime events emitted by the supervisor and task actors.
//!
//! The [`EventKind`] enum classifies event types across three categories:
//! - **Management events**: runtime task control (add/remove requests and confirmations)
//! - **Lifecycle events**: task execution flow (starting, stopped, failed, timeout)
//! - **Terminal events**: actor final states (exhausted policy, dead)
//!
//! The [`Event`] struct carries additional metadata such as timestamps, task name, reasons, and backoff delays.
//!
//! ## Ordering guarantees
//!
//! Each event has a globally unique sequence number (`seq`) that increases monotonically.
//! Use `seq` to restore the exact order when events are delivered out of order.
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
//!     .with_timeout(Duration::from_secs(5));
//!
//! assert_eq!(ev.kind, EventKind::TaskFailed);
//! assert_eq!(ev.task.as_deref(), Some("demo-task"));
//! assert_eq!(ev.reason.as_deref(), Some("boom"));
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, SystemTime};

use crate::identity::TaskId;

/// Global sequence counter for event ordering.
static EVENT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Classification of runtime events.
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
    /// - `task`: task name
    /// - `attempt`: attempt number (1-based, per actor)
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskStarting,

    /// Task attempt finished successfully.
    ///
    /// Sets:
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskStopped,

    /// Task attempt exited via graceful cancellation (`TaskError::Canceled` returned while its token was cancelled).
    ///
    /// Sets:
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskCanceled,

    /// Task failed with a (non-fatal) error for this attempt.
    ///
    /// Sets:
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `reason`: failure message
    /// - `exit_code`: numeric exit code when the error came from a
    ///   process-like runtime; `None` for logical errors
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskFailed,

    /// Task exceeded its configured timeout for this attempt.
    ///
    /// Sets:
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `timeout_ms`: configured attempt timeout (ms)
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TimeoutHit,

    /// Next attempt scheduled (after success or failure).
    ///
    /// Sets:
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
    /// Published by Supervisor on the bus for observability before sending
    /// the `Add` command to Registry via mpsc.
    ///
    /// Sets:
    /// - `task`: logical task name
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskAddRequested,

    /// Task was successfully added (actor spawned and registered).
    ///
    /// Sets:
    /// - `task`: task name
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskAdded,

    /// Task could not be added: a task with the same name is already registered.
    ///
    /// Published by Registry instead of `TaskAdded` when an `Add` command targets a name that already exists;
    /// no new actor is spawned.
    ///
    /// Sets:
    /// - `task`: task name
    /// - `reason`: e.g. "already_exists"
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskAddFailed,

    /// Request to remove a task from the supervisor.
    ///
    /// Sets:
    /// - `task`: task name
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskRemoveRequested,

    /// Task was removed from the supervisor (after join/cleanup).
    ///
    /// Sets:
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
    /// - `id`: the rejected submission's [`TaskId`]
    /// - `reason`: rejection reason ("queue_full", "add_failed: ...", "superseded_by_replace", "controller_shutting_down", etc)
    ControllerRejected,

    #[cfg(feature = "controller")]
    /// Task submitted successfully to controller slot.
    ///
    /// Sets:
    /// - `task`: slot name
    /// - `id`: the submission's [`TaskId`]
    /// - `reason`: "admission={admission} status={status} depth={N}"
    ControllerSubmitted,

    #[cfg(feature = "controller")]
    /// Slot transitioned state (Running → Terminating, etc).
    ///
    /// Sets:
    /// - `task`: slot name
    /// - `reason`: "running→terminating" (Replace), "terminating→idle", etc
    ControllerSlotTransition,
}

/// Reason for scheduling the next run/backoff.
///
/// A closed set (success vs failure); intentionally **not** `#[non_exhaustive]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackoffSource {
    Success,
    Failure,
}

/// Runtime event with optional metadata.
///
/// - `seq`: monotonic global sequence for ordering
/// - `at`: wall-clock timestamp (for logs)
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
        // Name lives in `task`; `reason` is the bare cause ("full"/"closed"/"lagged(n)").
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
    fn with_timeout_clamps_large_duration() {
        let huge = Duration::from_millis(u64::from(u32::MAX) + 1000);
        let ev = Event::new(EventKind::TimeoutHit).with_timeout(huge);
        assert_eq!(ev.timeout_ms, Some(u32::MAX));
    }

    #[test]
    fn with_delay_clamps_large_duration() {
        let huge = Duration::from_millis(u64::from(u32::MAX) + 1000);
        let ev = Event::new(EventKind::BackoffScheduled).with_delay(huge);
        assert_eq!(ev.delay_ms, Some(u32::MAX));
    }

    #[test]
    fn with_duration_sets_and_clamps() {
        let ev = Event::new(EventKind::TaskStopped).with_duration(Duration::from_millis(42));
        assert_eq!(ev.duration_ms, Some(42));

        let huge = Duration::from_millis(u64::from(u32::MAX) + 1000);
        let clamped = Event::new(EventKind::TaskStopped).with_duration(huge);
        assert_eq!(clamped.duration_ms, Some(u32::MAX));
    }

    #[test]
    fn new_event_has_no_duration() {
        assert_eq!(Event::new(EventKind::TaskStopped).duration_ms, None);
    }

    #[test]
    fn is_internal_diagnostic_covers_both_variants() {
        let overflow = Event::new(EventKind::SubscriberOverflow);
        let panic = Event::new(EventKind::SubscriberPanicked);
        let normal = Event::new(EventKind::TaskStarting);

        assert!(overflow.is_internal_diagnostic());
        assert!(panic.is_internal_diagnostic());
        assert!(!normal.is_internal_diagnostic());
    }

    #[test]
    fn subscriber_overflow_factory_sets_fields() {
        let ev = Event::subscriber_overflow("my-sub", "full");
        assert_eq!(ev.kind, EventKind::SubscriberOverflow);
        assert_eq!(
            ev.task.as_deref(),
            Some("my-sub"),
            "subscriber name lives in `task`"
        );
        assert_eq!(
            ev.reason.as_deref(),
            Some("full"),
            "`reason` is the bare cause, not a re-encoding of the subscriber name"
        );
    }

    #[test]
    fn new_event_has_no_exit_code() {
        let ev = Event::new(EventKind::TaskFailed);
        assert_eq!(ev.exit_code, None);
    }

    #[test]
    fn with_exit_code_populates_field() {
        let ev = Event::new(EventKind::TaskFailed).with_exit_code(42);
        assert_eq!(ev.exit_code, Some(42));

        let neg = Event::new(EventKind::ActorDead).with_exit_code(-1);
        assert_eq!(neg.exit_code, Some(-1));
    }
}
