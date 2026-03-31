//! # Event subscriber trait.
//!
//! [`Subscribe`] is the extension point for plugging custom event handlers into the runtime.
//!
//! Each subscriber gets:
//! - **Dedicated worker task** (runs independently)
//! - **Per-subscriber bounded queue** (capacity via [`Subscribe::queue_capacity`])
//! - **Panic isolation** (panics are caught and reported as `EventKind::SubscriberPanicked`)
//!
//! ## Architecture
//!
//! ```text
//! SubscriberSet ──► [bounded queue] ──► worker task  ──► subscriber.on_event()
//!                                   └─► panic caught ──► EventKind::SubscriberPanicked
//! ```
//!
//! ## Rules
//!
//! - Queue overflow drops the event **for this subscriber only** and publishes`EventKind::SubscriberOverflow`;
//!   other subscribers are unaffected.
//! - Events are processed sequentially (FIFO) per subscriber.
//! - Subscribers do not block publishers or each other.
//! - A slow subscriber only affects its own queue.
//!
//! ## Example
//!
//! ```rust
//! use taskvisor::{Subscribe, Event, EventKind};
//!
//! struct Metrics;
//!
//! impl Subscribe for Metrics {
//!     fn on_event(&self, ev: &Event) {
//!         if matches!(ev.kind, EventKind::TaskFailed) {
//!             // update counters, push to channel, etc.
//!         }
//!     }
//!
//!     fn name(&self) -> &'static str { "metrics" }   // prefer short, descriptive names
//!     fn queue_capacity(&self) -> usize { 2048 }     // larger buffer for metrics
//! }
//! ```

use crate::events::Event;

/// Event subscriber for runtime observability.
///
/// Each subscriber runs in isolation:
/// - **Panic isolation**: panics are caught and published as `SubscriberPanicked`.
/// - **Bounded queue** buffers events (capacity via [`Self::queue_capacity`]).
/// - **Dedicated worker task** processes events sequentially (FIFO).
///
/// ### Implementation requirements
///
/// - Keep `on_event` fast: it runs on a dedicated worker task but blocks that worker's event loop.
///   For async I/O, send to a channel and process elsewhere.
/// - Slow processing affects only this subscriber's queue.
/// - Handle errors internally; do not panic.
///
/// ### Synchronous design
///
/// `on_event` is intentionally synchronous:
/// - The `SubscriberSet` infrastructure already provides async fan-out via per-subscriber `mpsc` channels and dedicated worker tasks.
/// - If a subscriber needs async I/O, send events to a channel inside `on_event` and process them in a separate task.
/// - Adding async to `on_event` would force a `Box::pin` allocation per event per subscriber with no benefit;
///   All real subscribers are synchronous.
///
/// ## Also
///
/// - See [`Event`](crate::Event) and [`EventKind`](crate::EventKind) for the event structure.
/// - For a built-in reference implementation see [`LogWriter`](crate::LogWriter) *(feature = `logging`)*.
pub trait Subscribe: Send + Sync + 'static {
    /// Processes a single event synchronously.
    ///
    /// Called from a dedicated worker task, not in the publisher context.
    /// Events are delivered in FIFO order per subscriber.
    ///
    /// Panics are caught; the runtime publishes `EventKind::SubscriberPanicked`.
    fn on_event(&self, event: &Event);

    /// Returns the subscriber name used in logs/metrics and overflow/panic events.
    ///
    /// Prefer short, descriptive names (e.g., "metrics", "audit", "slack").
    /// The default uses `type_name::<Self>()`, which can be verbose - override it when possible.
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Returns the preferred queue capacity for this subscriber.
    ///
    /// Overflow behavior:
    /// 1) The new event is dropped for this subscriber only,
    /// 2) an `EventKind::SubscriberOverflow` is published,
    /// 3) other subscribers are unaffected.
    ///
    /// The runtime clamps capacity to a minimum of 1.
    ///
    /// Default: 1024.
    fn queue_capacity(&self) -> usize {
        1024
    }
}
