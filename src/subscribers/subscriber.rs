//! # Event subscriber trait.
//!
//! Provides [`Subscribe`] an extension point for plugging custom event handlers into the runtime.
//!
//! Each subscriber gets:
//! - **Dedicated worker task** (runs independently)
//! - **Per-subscriber bounded queue** (capacity via [`Subscribe::queue_capacity`])
//! - **Panic isolation** (panics are caught and reported as `EventKind::SubscriberPanicked`)
//!
//! ## Architecture
//! ```text
//! SubscriberSet ──► [bounded queue] ──► worker task ──► subscriber.on_event()
//!                                    └─► panic caught → EventKind::SubscriberPanicked
//! ```
//!
//! ## Rules
//! - A slow subscriber only affects its own queue.
//! - Queue overflow drops the event **for this subscriber only** and publishes
//!   `EventKind::SubscriberOverflow`; other subscribers are unaffected.
//! - Events are processed sequentially (FIFO) per subscriber.
//! - Subscribers do not block publishers or each other.
//!
//! ## Overflow behavior
//! 1) The new event is **dropped** for this subscriber only.
//! 2) The runtime publishes `EventKind::SubscriberOverflow`.
//! 3) Other subscribers are unaffected.
//!
//! ## Example
//! ```rust
//! # #[cfg(feature = "events")]
//! # {
//! use async_trait::async_trait;
//! // Import from the crate root: these are re-exported when the "events" feature is enabled.
//! use taskvisor::{Subscribe, Event, EventKind};
//!
//! struct Metrics;
//!
//! #[async_trait]
//! impl Subscribe for Metrics {
//!     async fn on_event(&self, ev: &Event) {
//!         if matches!(ev.kind, EventKind::TaskFailed) {
//!             // export a metric, etc.
//!         }
//!     }
//!
//!     fn name(&self) -> &'static str { "metrics" }      // prefer short, descriptive names
//!     fn queue_capacity(&self) -> usize { 2048 }        // larger buffer for metrics
//! }
//! # }
//! ```

use async_trait::async_trait;

use crate::events::Event;

/// Event subscriber for runtime observability.
///
/// Each subscriber runs in isolation:
/// - **Bounded queue** buffers events (capacity via [`Self::queue_capacity`]).
/// - **Dedicated worker task** processes events sequentially (FIFO).
/// - **Panic isolation**: panics are caught and published as `SubscriberPanicked`.
///
/// ### Implementation requirements
/// - Use async I/O; avoid blocking the executor.
/// - Handle errors internally; do not panic.
/// - Slow processing affects only this subscriber's queue.
#[async_trait]
pub trait Subscribe: Send + Sync + 'static {
    /// Processes a single event.
    ///
    /// Called from a dedicated worker task, not in the publisher context.
    /// Events are delivered in FIFO order per subscriber.
    ///
    /// Panics are caught; the runtime publishes `EventKind::SubscriberPanicked`.
    async fn on_event(&self, event: &Event);

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
