//! # Runtime events
//!
//! Events describe what Taskvisor is doing. Use them for logs, metrics, dashboards, alerts, and tests.
//!
//! | Type              | Role                                       |
//! |-------------------|--------------------------------------------|
//! | [`EventKind`]     | Event classification                       |
//! | [`Event`]         | Event payload and metadata                 |
//! | [`BackoffSource`] | Why a `BackoffScheduled` event was emitted |
//!
//! ## Events and final outcomes are different
//!
//! Event delivery is best-effort.
//! There are two bounded steps: the shared event bus and each subscriber's own queue.
//! A slow consumer may miss events at either step.
//!
//! ```text
//! runtime action
//!      ├── best-effort ──► event bus ──► subscriber queue ──► Subscribe
//!      └── reliable terminal result ────────────────────────► TaskWaiter
//! ```
//!
//! Use events for observation.
//! Do not use them as the only proof that work finished.
//! If you need one final result, use an `*_and_watch` method and await the returned [`TaskWaiter`](crate::TaskWaiter).
//!
//! ## Common task flow
//!
//! ```text
//! Add:
//!   TaskAddRequested ──► TaskAdded ──► TaskStarting
//!                   └──► TaskAddFailed
//!
//! Attempt:
//!   TaskStarting ──► TaskStopped
//!              ├──► TaskCanceled
//!              ├──► TaskFailed
//!              └──► TimeoutHit ──► TaskFailed
//!
//! Successful attempt:
//!   TaskStopped ──► ActorExhausted
//!              ├──► BackoffScheduled(Success) ──► TaskStarting
//!              └──► TaskStarting (Always without an interval)
//!
//! Retryable failure:
//!   TaskFailed ──► BackoffScheduled(Failure) ──► TaskStarting
//!             └──► ActorExhausted
//!
//! Fatal failure:
//!   TaskFailed ──► ActorDead
//!
//! Registry cleanup:
//!   [optional TaskRemoveRequested] ──► TaskRemoved
//!
//! Queued controller removal (feature `controller`):
//!   TaskRemoveRequested ──► ControllerRejected(removed_from_queue)
//!
//! Shutdown:
//!   ShutdownRequested ──► AllStoppedWithinGrace | GraceExceeded
//! ```
//!
//! [`TaskRemoved`](EventKind::TaskRemoved) confirms registry cleanup.
//! It is sent after the managed task runner has been joined or cleaned up.
//!
//! ## Read the Stream Safely
//!
//! - [`TaskFailed`](EventKind::TaskFailed) describes one attempt.
//!   The task may retry. [`ActorExhausted`](EventKind::ActorExhausted) and [`ActorDead`](EventKind::ActorDead) mean that no more attempts will start.
//! - [`Event::seq`] is process-local construction order.
//!   It can help sort events and detect gaps, but it is not a causal clock.
//! - Use [`EventKind::as_label`] for a stable telemetry label.
//!   Treat free-form [`Event::reason`] text as diagnostic unless it is documented in [`reasons`](crate::reasons).
//!
//! ## Subscribers
//!
//! Implement [`Subscribe`](crate::Subscribe) to consume events.
//! With the `tracing` feature, `TracingBridge` sends them to `tracing`.
//! With the `logging` feature, `LogWriter` prints simple development logs.

mod event;
pub use event::{BackoffSource, Event, EventKind};

mod bus;
pub(crate) use bus::Bus;
