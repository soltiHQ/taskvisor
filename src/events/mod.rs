//! Exposes Taskvisor's best-effort lifecycle stream for observability.
//!
//! The registry, task actors, controller, and shutdown workflow publish ordinary
//! [`Event`] values to an internal bounded bus. The runtime relay forwards
//! retained events to the bounded queue of each [`Subscribe`](crate::Subscribe)
//! implementation. Internal subscriber diagnostics can start at the relay or a
//! subscriber lane and bypass the shared bus.
//!
//! ```text
//! runtime components
//!        │ Event
//!        ▼
//! bounded Bus
//!        │ retained events
//!        ▼
//! event relay ──► subscriber queues ──► Subscribe callbacks
//!
//! internal diagnostics ──► event relay or subscriber lane ──► callbacks
//! ```
//!
//! # Choosing the right result path
//!
//! | Need                                  | Use                                      |
//! |---------------------------------------|------------------------------------------|
//! | Logs, metrics, alerts, or diagnostics | [`Subscribe`](crate::Subscribe) events   |
//! | Final outcome for watched work        | [`TaskWaiter`](crate::TaskWaiter)        |
//! | Result of a management command        | The management method's returned result  |
//!
//! The stream is observational, not a reliable confirmation channel. Bus
//! overflow and subscriber queue pressure can drop events. Missing an event
//! does not mean the action did not happen, and runtime state never depends on
//! delivery.
//!
//! [`EventKind`] identifies what happened. [`Event`] carries its metadata.
//! [`TaskOutcomeKind`](crate::TaskOutcomeKind), [`BackoffSource`], and
//! [`RejectionKind`] provide typed details for outcomes, backoff, and rejection
//! events. Implement [`Subscribe`](crate::Subscribe) for custom handling, or use
//! the feature-gated `LogWriter` and `TracingBridge` subscribers for ready-made
//! output.
//!
//! Important stream rules:
//!
//! - [`AttemptFailed`](EventKind::AttemptFailed) and
//!   [`AttemptTimedOut`](EventKind::AttemptTimedOut) describe one attempt. A
//!   later attempt may still run.
//! - [`TaskFinished`](EventKind::TaskFinished) carries the final outcome class.
//!   Registry cleanup then attempts [`TaskRemoved`](EventKind::TaskRemoved).
//! - Cancellation between attempts can reach `TaskFinished(Canceled)` without
//!   an [`AttemptCanceled`](EventKind::AttemptCanceled) event.
//! - Rejected work never enters the registry. It has no `TaskFinished` or
//!   `TaskRemoved` event.
//! - [`Event::seq`] records process-local construction order. It is not a
//!   causal clock, and gaps are expected when events are dropped.
//! - [`Event::reason`] is diagnostic text. Use typed enums and their stable
//!   `as_label` methods for machine decisions and telemetry.

mod event;
pub use event::{BackoffSource, Event, EventKind, RejectionKind};

mod bus;
pub(crate) use bus::{Bus, BusMessage, BusReceiver, TryRecvError};
