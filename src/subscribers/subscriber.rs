//! # Event subscriber trait.
//!
//! [`Subscribe`] is the extension point for observing runtime events.
//!
//! Each subscriber gets:
//! - a dedicated queue worker task,
//! - a bounded queue,
//! - panic isolation.
//!
//! Delivery is best-effort.
//! If a subscriber falls behind, new events may be dropped for that subscriber only.
//! Callbacks run on Tokio's blocking pool instead of its async workers.
//!
//! ## Flow
//!
//! ```text
//! SubscriberSet ──► [bounded queue] ──► worker ──► blocking pool ──► subscriber.on_event()
//!                                   └─► overflow/panic diagnostics
//! ```
//!
//! ## Rules
//!
//! - Ordinary overflows are reported as [`EventKind::SubscriberOverflow`](crate::EventKind::SubscriberOverflow).
//! - Ordinary panics are reported as [`EventKind::SubscriberPanicked`](crate::EventKind::SubscriberPanicked).
//! - Diagnostic events are not re-reported if they overflow or panic, to avoid feedback loops.
//! - Events are processed sequentially (FIFO) per subscriber.
//! - Queue overflow drops the event for this subscriber only.
//! - A slow subscriber can fill only its own queue.
//!
//! ## Example
//!
//! ```rust
//! use taskvisor::{Event, EventKind, Subscribe};
//!
//! struct Metrics;
//!
//! impl Subscribe for Metrics {
//!     fn on_event(&self, ev: &Event) {
//!         if matches!(ev.kind, EventKind::TaskFailed) {
//!             // update counters, push to a channel, etc.
//!         }
//!     }
//!
//!     fn name(&self) -> &str { "metrics" }
//!     fn queue_capacity(&self) -> usize { 2048 }
//! }
//! ```

use crate::events::Event;

/// Event subscriber for runtime observability.
///
/// `Subscribe` is synchronous by design.
/// When Taskvisor delivers events, a dedicated queue worker schedules one callback at a time on Tokio's blocking pool.
///
/// Keep [`on_event`](Self::on_event) fast; for async I/O, send data to a channel and process it elsewhere.
/// During shutdown, Taskvisor gives all subscriber queues one shared drain timeout.
/// At the deadline, queued events are dropped; an already-running callback may continue after Taskvisor returns.
/// Tokio runtime shutdown may still wait for that running blocking callback.
///
/// Panics are caught and isolated.
/// A panic while handling an ordinary event is reported as `SubscriberPanicked`.
/// A panic while handling an internal diagnostic event is not reported again, to avoid a feedback loop.
pub trait Subscribe: Send + Sync + 'static {
    /// Processes one event.
    ///
    /// When Taskvisor delivers an event, it calls this method on Tokio's blocking pool, not from the publisher or an async worker.
    /// Events are delivered in FIFO order per subscriber.
    fn on_event(&self, event: &Event);

    /// Returns the subscriber name used in logs and diagnostic events.
    /// The runtime snapshots it once at registration.
    ///
    /// The default returns the fully-qualified type path via [`type_name`](std::any::type_name).
    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }

    /// Returns this subscriber's preferred queue capacity.
    ///
    /// The runtime clamps this value to at least `1`.
    /// If the queue is full, ordinary events are dropped for this subscriber and reported as `SubscriberOverflow`.
    ///
    /// Default: `1024`.
    fn queue_capacity(&self) -> usize {
        1024
    }
}
