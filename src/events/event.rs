//! # Runtime events emitted by the supervisor and task actors.
//!
//! The [`EventKind`] enum classifies event types across three categories:
//! - **Lifecycle events**: task execution flow (starting, stopped, failed, timeout)
//! - **Management events**: runtime task control (add/remove requests and confirmations)
//! - **Terminal events**: actor final states (exhausted policy, dead)
//!
//! The [`Event`] struct carries additional metadata such as timestamps, task name,
//! reasons, and backoff delays.
//!
//! ## Ordering guarantees
//! Each event has a globally unique sequence number (`seq`) that increases monotonically.
//! Use `seq` to restore the exact order when events are delivered out of order.
//!
//! ## Example
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

/// Global sequence counter for event ordering.
static EVENT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Classification of runtime events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    // === Subscriber events ===
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
    /// - `task`: subscriber name
    /// - `reason`: reason string (e.g., "full", "closed")
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    SubscriberOverflow,

    // === Shutdown events ===
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
    AllStoppedWithin,

    /// Grace period exceeded; some tasks did not stop in time.
    ///
    /// Sets:
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    GraceExceeded,

    // === Task lifecycle events ===
    /// Task is starting an attempt.
    ///
    /// Sets:
    /// - `task`: task name
    /// - `attempt`: attempt number (1-based, per actor)
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskStarting,

    /// Task has stopped (finished successfully **or** was cancelled gracefully).
    ///
    /// Sets:
    /// - `task`: task name
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    TaskStopped,

    /// Task failed with a (non-fatal) error for this attempt.
    ///
    /// Sets:
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `reason`: failure message
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

    // === Runtime task management events ===
    /// Request to add a new task to the supervisor.
    ///
    /// Sets:
    /// - `task`: logical task name
    /// - `spec` (private): task spec to spawn
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

    // === Actor terminal states ===
    /// Actor exhausted its restart policy and will not restart.
    ///
    /// Emitted when:
    /// - `RestartPolicy::Never` → task completed (success or handled case)
    /// - `RestartPolicy::OnFailure` → task completed successfully
    ///
    /// Sets:
    /// - `task`: task name
    /// - `attempt`: last attempt number
    /// - `reason`: optional message
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    ActorExhausted,

    /// Actor terminated permanently due to a fatal error.
    ///
    /// Emitted when:
    /// - Task returned `TaskError::Fatal`
    /// - (Future) max retries exceeded
    ///
    /// Sets:
    /// - `task`: task name
    /// - `attempt`: last attempt number
    /// - `reason`: fatal error message
    /// - `at`: wall-clock timestamp
    /// - `seq`: global sequence
    ActorDead,
}

/// Reason for schedule the next run/backoff.
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
#[derive(Clone)]
pub struct Event {
    /// Globally unique, monotonically increasing sequence number.
    pub seq: u64,
    /// Wall-clock timestamp.
    pub at: SystemTime,

    /// Task timeout in milliseconds (compact).
    pub timeout_ms: Option<u32>,
    /// Backoff delay before next attempt in milliseconds (compact).
    pub delay_ms: Option<u32>,
    /// Human-readable reason (errors, overflow details, etc.).
    pub reason: Option<Arc<str>>,
    /// Attempt count (starting from 1).
    pub attempt: Option<u32>,
    /// Name of the task, if applicable.
    pub task: Option<Arc<str>>,
    /// Event classification.
    pub kind: EventKind,
    /// Source for backoff scheduling (success vs failure).
    pub backoff_source: Option<BackoffSource>,

    /// Internal task specification (used only for TaskAddRequested).
    pub(crate) spec: Option<crate::tasks::TaskSpec>,
}

impl Event {
    /// Creates a new event of the given kind with current timestamp and next sequence number.
    pub fn new(kind: EventKind) -> Self {
        Self {
            seq: EVENT_SEQ.fetch_add(1, AtomicOrdering::Relaxed),
            kind,
            at: SystemTime::now(),
            attempt: None,
            timeout_ms: None,
            reason: None,
            delay_ms: None,
            backoff_source: None,
            task: None,
            spec: None,
        }
    }

    /// Attaches a human-readable reason.
    #[inline]
    pub fn with_reason(mut self, reason: impl Into<Arc<str>>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Attaches a task name.
    #[inline]
    pub fn with_task(mut self, task: impl Into<Arc<str>>) -> Self {
        self.task = Some(task.into());
        self
    }

    /// Attaches a timeout duration (stored as milliseconds).
    #[inline]
    pub fn with_timeout(mut self, d: Duration) -> Self {
        let ms = d.as_millis().min(u128::from(u32::MAX)) as u32;
        self.timeout_ms = Some(ms);
        self
    }

    /// Attaches a backoff delay (stored as milliseconds).
    #[inline]
    pub fn with_delay(mut self, d: Duration) -> Self {
        let ms = d.as_millis().min(u128::from(u32::MAX)) as u32;
        self.delay_ms = Some(ms);
        self
    }

    /// Attaches an attempt count.
    #[inline]
    pub fn with_attempt(mut self, n: u32) -> Self {
        self.attempt = Some(n);
        self
    }

    /// Marks that this backoff comes from a successful attempt.
    #[inline]
    pub fn with_backoff_success(mut self) -> Self {
        self.backoff_source = Some(BackoffSource::Success);
        self
    }

    /// Marks that this backoff comes from a failed attempt.
    #[inline]
    pub fn with_backoff_failure(mut self) -> Self {
        self.backoff_source = Some(BackoffSource::Failure);
        self
    }

    /// Creates a subscriber overflow event.
    #[inline]
    pub fn subscriber_overflow(subscriber: &'static str, reason: &'static str) -> Self {
        Event::new(EventKind::SubscriberOverflow)
            .with_task(subscriber)
            .with_reason(format!("subscriber={subscriber} reason={reason}"))
    }

    /// Creates a subscriber panic event.
    #[inline]
    pub fn subscriber_panicked(subscriber: &'static str, info: String) -> Self {
        Event::new(EventKind::SubscriberPanicked)
            .with_task(subscriber)
            .with_reason(info)
    }

    #[inline]
    pub fn is_subscriber_overflow(&self) -> bool {
        matches!(self.kind, EventKind::SubscriberOverflow)
    }

    #[inline]
    pub fn is_subscriber_panic(&self) -> bool {
        matches!(self.kind, EventKind::SubscriberPanicked)
    }

    #[inline]
    pub(crate) fn with_spec(mut self, spec: crate::tasks::TaskSpec) -> Self {
        self.spec = Some(spec);
        self
    }
}
