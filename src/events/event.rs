//! # Runtime events emitted by the supervisor and task actors.
//!
//! The [`EventKind`] enum classifies event types (shutdown, failures, retries, etc.).
//! The [`Event`] struct carries additional metadata such as timestamps, task name, error messages, and backoff delays.
//!
//! # Example
//! ```
//! use std::time::Duration;
//! use taskvisor::{Event, EventKind};
//!
//! let ev = Event::now(EventKind::TaskFailed)
//!     .with_task("demo-task")
//!     .with_error("boom")
//!     .with_attempt(3)
//!     .with_timeout(Duration::from_secs(5));
//!
//! assert_eq!(ev.kind, EventKind::TaskFailed);
//! assert_eq!(ev.task.as_deref(), Some("demo-task"));
//! assert_eq!(ev.error.as_deref(), Some("boom"));
//! ```

use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant, SystemTime};

/// Global sequence counter for event ordering
static EVENT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Classification of runtime events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// Subscriber event on panic,
    SubscriberPanicked,
    /// A subscriber dropped an event (queue full or worker closed).
    SubscriberOverflow,
    /// Shutdown requested (OS signal received).
    ShutdownRequested,
    /// A task is scheduled to back off before retrying.
    BackoffScheduled,
    /// All tasks stopped within the configured grace period.
    AllStoppedWithin,
    /// Grace period exceeded; some tasks did not stop in time.
    GraceExceeded,
    /// A task is starting execution.
    TaskStarting,
    /// A task has stopped (finished or cancelled).
    TaskStopped,
    /// A task failed with an error.
    TaskFailed,
    /// A task hit its configured timeout.
    TimeoutHit,
}

/// Runtime event with optional metadata.
///
/// Carries information about task lifecycle, retries, errors, backoff delays, and timing.
#[derive(Debug, Clone)]
pub struct Event {
    /// Globally unique, monotonically increasing sequence number.
    /// Used to determine event ordering across async boundaries.
    pub seq: u64,

    /// Wall-clock timestamp (may go backwards, use for logging only).
    pub at: SystemTime,

    /// Monotonic timestamp (guaranteed to never go backwards).
    /// Use this for interval measurements and ordering validation.
    pub monotonic: Instant,

    /// Task timeout (if relevant).
    pub timeout: Option<Duration>,
    /// Backoff delay before retry (if relevant).
    pub delay: Option<Duration>,
    /// Error message, if the event represents a failure.
    pub error: Option<String>,
    /// Attempt count (starting from 1).
    pub attempt: Option<u64>,
    /// Name of the task, if applicable.
    pub task: Option<String>,
    /// The kind of event.
    pub kind: EventKind,
}

impl Event {
    /// Creates a new event of the given kind with current timestamp.
    pub fn now(kind: EventKind) -> Self {
        Self {
            seq: EVENT_SEQ.fetch_add(1, AtomicOrdering::Relaxed),
            kind,
            at: SystemTime::now(),
            monotonic: Instant::now(),
            attempt: None,
            timeout: None,
            error: None,
            delay: None,
            task: None,
        }
    }

    /// Attaches an error message.
    pub fn with_error(mut self, msg: impl Into<String>) -> Self {
        self.error = Some(msg.into());
        self
    }

    /// Attaches a task name.
    pub fn with_task(mut self, name: impl Into<String>) -> Self {
        self.task = Some(name.into());
        self
    }

    /// Attaches a timeout duration.
    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    /// Attaches a backoff delay.
    pub fn with_delay(mut self, d: Duration) -> Self {
        self.delay = Some(d);
        self
    }

    /// Attaches an attempt count.
    pub fn with_attempt(mut self, n: u64) -> Self {
        self.attempt = Some(n);
        self
    }

    /// Build an overflow event emitted by the runtime when a subscriber drops an event.
    pub fn subscriber_overflow(subscriber: &'static str, reason: &'static str) -> Self {
        let mut ev = Event::now(EventKind::SubscriberOverflow);

        ev.error = Some(format!("subscriber={subscriber} reason={reason}"));
        ev.attempt = None;
        ev.timeout = None;
        ev.delay = None;
        ev.task = None;
        ev
    }

    pub fn subscriber_panicked(subscriber: &'static str, info: String) -> Self {
        Event::now(EventKind::SubscriberPanicked)
            .with_task(subscriber)
            .with_error(info)
    }
}
