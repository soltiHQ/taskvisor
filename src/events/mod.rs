//! # Runtime events
//!
//! Events describe what Taskvisor is doing.
//! Use them for logs, metrics, dashboards, alerts, and tests.
//!
//! | Type                                        | Role                                       |
//! |---------------------------------------------|--------------------------------------------|
//! | [`EventKind`]                               | Event classification                       |
//! | [`Event`]                                   | Event payload and metadata                 |
//! | [`BackoffSource`]                           | Why a `BackoffScheduled` event was emitted |
//! | [`RejectionKind`]                           | Machine-readable submission rejection      |
//! | [`TaskOutcomeKind`](crate::TaskOutcomeKind) | Machine-readable final outcome             |
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
//!   TaskAddRequested ──► TaskAdded ──► AttemptStarting
//!                   └──► TaskAddFailed
//!
//! Attempt:
//!   AttemptStarting ──► AttemptSucceeded
//!              ├──► AttemptCanceled ──► TaskFinished(Canceled)
//!              ├──► AttemptFailed
//!              └──► AttemptTimedOut
//!
//! Successful attempt:
//!   AttemptSucceeded ──► TaskFinished(Completed)
//!              ├──► BackoffScheduled(Success) ──► AttemptStarting
//!              └──► AttemptStarting (Always without an interval)
//!
//! Retryable failure or configured timeout:
//!   AttemptFailed | AttemptTimedOut ──► BackoffScheduled(Failure) ──► AttemptStarting
//!                                  └──► TaskFinished(Failed)
//!
//! Fatal failure:
//!   AttemptFailed ──► TaskFinished(Fatal)
//!
//! Registry cleanup:
//!   [optional TaskRemoveRequested] ──► TaskFinished ──► TaskRemoved
//!
//! Queued controller removal (feature `controller`):
//!   TaskRemoveRequested ──► ControllerRejected(RemovedFromQueue)
//!
//! Shutdown:
//!   ShutdownRequested ──► AllStoppedWithinGrace | GraceExceeded
//! ```
//!
//! [`TaskRemoved`](EventKind::TaskRemoved) confirms registry-membership removal and terminal reporting; it can race with the internal logical-completion latch.
//! Except for [`TaskOutcomeKind::ForceAborted`](crate::TaskOutcomeKind::ForceAborted), the managed runner has been physically joined first.
//! A force-aborted synchronous poll can remain active under reaper ownership after this event.
//!
//! ## Read the Stream Safely
//!
//! - [`AttemptFailed`](EventKind::AttemptFailed) describes one attempt.
//!   The task may retry. [`TaskFinished`](EventKind::TaskFinished) means that a registered task has reached its final [`TaskOutcomeKind`](crate::TaskOutcomeKind).
//!   Cancellation while waiting for a permit or backoff can produce `TaskFinished(Canceled)` without an `AttemptCanceled` event.
//! - [`Event::seq`] is process-local construction order.
//!   It can help sort events and detect gaps, but it is not a causal clock.
//! - Use [`EventKind::as_label`] for a stable telemetry label.
//!   Treat free-form [`Event::reason`] text as diagnostic, never as schema.
//!   Use [`TaskOutcomeKind`](crate::TaskOutcomeKind) and [`RejectionKind`] for machine decisions.
//! - Use [`RejectionKind`] for machine-readable handling of `TaskAddFailed` and `ControllerRejected`.
//!   Rejected work never enters the registry, so it has no `TaskFinished` or `TaskRemoved` event.
//!
//! ## Subscribers
//!
//! Implement [`Subscribe`](crate::Subscribe) to consume events.
//! With the `tracing` feature, `TracingBridge` sends them to `tracing`.
//! With the `logging` feature, `LogWriter` prints simple development logs.

mod event;
pub use event::{BackoffSource, Event, EventKind, RejectionKind};

mod bus;
pub(crate) use bus::{Bus, BusMessage, BusReceiver, TryRecvError};
