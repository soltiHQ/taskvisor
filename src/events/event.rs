//! # Runtime events emitted by the supervisor and task actors.
//!
//! The [`EventKind`] enum classifies event types across three categories:
//! - **Lifecycle events**: task execution flow (starting, stopped, failed, timeout)
//! - **Management events**: runtime task control (add, remove requests and confirmations)
//! - **Terminal events**: actor final states (exhausted policy, dead)
//!
//! The [`Event`] struct carries additional metadata such as timestamps, task name,
//! error messages, and backoff delays.
//!
//! ## Ordering guarantees
//! Each event has a globally unique sequence number (`seq`) that increases monotonically.
//! This guarantees that events can be ordered correctly even when delivered out-of-order
//! through async channels.
//!
//! ## Event flow examples
//!
//! ### Task addition flow
//! ```text
//! Supervisor::add_task()
//!   → TaskAddRequested
//!   → [spawn actor]
//!   → TaskAdded
//!   → TaskStarting
//! ```
//!
//! ### Task removal flow
//! ```text
//! Supervisor::remove_task()
//!   → TaskRemoveRequested
//!   → [cancel actor token]
//!   → TaskStopped (with Canceled error)
//!   → ActorExhausted
//!   → TaskRemoved
//! ```
//!
//! ### Actor exhaustion (RestartPolicy::Never)
//! ```text
//! TaskStarting
//!   → TaskStopped
//!   → ActorExhausted
//!   → [auto-cleanup from registry]
//! ```
//!
//! ## Example
//! ```rust
//! # #[cfg(feature = "events")]
//! # {
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
//! # }
//! ```

use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant, SystemTime};

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
    /// - `error`: panic info/message
    /// - `monotonic`: creation timestamp
    SubscriberPanicked,
    /// Subscriber dropped an event (queue full or worker closed).
    ///
    /// Sets:
    /// - `task`: subscriber name
    /// - `error`: reason string (e.g., "full", "closed")
    /// - `monotonic`: creation timestamp
    SubscriberOverflow,

    // === Shutdown events ===
    /// Shutdown requested (OS signal received).
    /// Shutdown requested (OS signal observed).
    ///
    /// Sets:
    /// - `monotonic`: creation timestamp
    ShutdownRequested,
    /// All tasks stopped within configured grace period.
    ///
    /// Sets:
    /// - `monotonic`: creation timestamp
    AllStoppedWithin,
    /// Grace period exceeded; some tasks did not stop in time.
    ///
    /// Sets:
    /// - `monotonic`: creation timestamp
    GraceExceeded,

    // === Task lifecycle events ===
    /// Task is starting an attempt.
    ///
    /// Sets:
    /// - `task`: task name
    /// - `attempt`: attempt number (1-based, monotonic per actor)
    /// - `monotonic`: creation timestamp
    TaskStarting,
    /// Task has stopped (finished successfully **or** was cancelled gracefully).
    ///
    /// Sets:
    /// - `task`: task name
    /// - `monotonic`: creation timestamp
    TaskStopped,
    /// Task failed with a (non-fatal) error for this attempt.
    ///
    /// Sets:
    /// - `task`: task name
    /// - `attempt`: attempt number
    /// - `error`: error message (from `TaskError`)
    /// - `monotonic`: creation timestamp
    TaskFailed,
    /// Task exceeded its configured timeout for this attempt.
    ///
    /// Sets:
    /// - `task`: task name
    /// - `timeout`: configured attempt timeout
    /// - `monotonic`: creation timestamp
    TimeoutHit,
    /// Retry scheduled after a failed attempt.
    ///
    /// Sets:
    /// - `task`: task name
    /// - `attempt`: attempt number (of the failed attempt)
    /// - `delay`: backoff delay before next attempt
    /// - `error`: last failure’s message (cause of the retry)
    /// - `monotonic`: creation timestamp
    BackoffScheduled,

    // === Runtime task management events ===
    /// Request to add a new task to the supervisor.
    ///
    /// Sets:
    /// - `task`: logical task name
    /// - `spec` (private, internal): `TaskSpec` to spawn
    /// - `monotonic`: creation timestamp
    TaskAddRequested,
    /// Task was successfully added (actor spawned and registered).
    ///
    /// Sets:
    /// - `task`: task name
    /// - `monotonic`: creation timestamp
    TaskAdded,
    /// Request to remove a task from the supervisor.
    ///
    /// Sets:
    /// - `task`: task name
    /// - `monotonic`: creation timestamp
    TaskRemoveRequested,
    /// Task was removed from the supervisor (after join/cleanup).
    ///
    /// Sets:
    /// - `task`: task name
    /// - `monotonic`: creation timestamp
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
    /// - `error`: optional error message
    /// - `monotonic`: creation timestamp
    ActorExhausted,

    // Actor terminated permanently due to a fatal error.
    ///
    /// Emitted when:
    /// - Task returned `TaskError::Fatal`
    /// - (Future) max retries exceeded
    ///
    /// Sets:
    /// - `task`: task name
    /// - `attempt`: last attempt number
    /// - `error`: fatal error message
    /// - `monotonic`: creation timestamp
    ActorDead,
}

/// Runtime event with optional metadata.
///
/// Carries information about task lifecycle, retries, errors, backoff delays, and timing.
///
/// ## Fields
///
/// - `seq`: Unique sequence number for ordering (monotonically increasing)
/// - `at`: Wall-clock timestamp (may go backwards due to NTP; use for logging only)
/// - `monotonic`: Monotonic timestamp (never goes backwards; use for interval measurements)
/// - `kind`: Event classification
/// - `task`, `error`, `attempt`, `timeout`, `delay`: Optional metadata
#[derive(Clone)]
pub struct Event {
    /// Globally unique, monotonically increasing sequence number.
    /// Used to determine event ordering across async boundaries.
    pub seq: u64,
    /// Wall-clock timestamp (may go backwards, use for logging only).
    pub at: SystemTime,
    /// Monotonic timestamp (never goes backwards, use for interval measurements).
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

    /// Task specification (private, used internally for TaskAddRequested).
    ///
    /// Only populated for `TaskAddRequested` events.
    /// The registry extracts this to spawn the actor.
    pub(crate) spec: Option<crate::tasks::TaskSpec>,
}

impl Event {
    /// Creates a new event of the given kind with current timestamps and next sequence number.
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
            spec: None,
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

    /// Creates a subscriber overflow event.
    ///
    /// Emitted when a subscriber's queue is full and an event is dropped.
    pub fn subscriber_overflow(subscriber: &'static str, reason: &'static str) -> Self {
        Event::now(EventKind::SubscriberOverflow)
            .with_task(subscriber)
            .with_error(format!("subscriber={subscriber} reason={reason}"))
    }

    /// Creates a subscriber panic event.
    ///
    /// Emitted when a subscriber panics during event processing.
    pub fn subscriber_panicked(subscriber: &'static str, info: String) -> Self {
        Event::now(EventKind::SubscriberPanicked)
            .with_task(subscriber)
            .with_error(info)
    }

    /// True if this is a `SubscriberOverflow` event.
    pub fn is_subscriber_overflow(&self) -> bool {
        matches!(self.kind, EventKind::SubscriberOverflow)
    }

    /// True if this is a `SubscriberPanicked` event.
    pub fn is_subscriber_panic(&self) -> bool {
        matches!(self.kind, EventKind::SubscriberPanicked)
    }

    pub(crate) fn with_spec(mut self, spec: crate::tasks::TaskSpec) -> Self {
        self.spec = Some(spec);
        self
    }
}
