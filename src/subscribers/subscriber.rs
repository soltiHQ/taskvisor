//! # Event subscriber trait.
//!
//! Provides [`Subscribe`] — the extension point for plugging custom event handlers into the runtime.
//!
//! Each subscriber gets:
//! - **Dedicated worker task** (runs independently)
//! - **Bounded queue** (configurable capacity via [`Subscribe::queue_capacity`])
//! - **Panic isolation** (panics caught, reported as `SubscriberPanicked` event)
//!
//! ## Architecture
//! ```text
//! SubscriberSet ──► [queue] ──► worker task ──► subscriber.on_event()
//!                  (bounded)             └────► panic caught & isolated
//! ```
//!
//! ## Rules
//! - Slow subscribers only affect themselves (queue overflow → event drop)
//! - Panics are **isolated** (do not crash runtime or other subscribers)
//! - Subscribers **do not block** publishers or other subscribers
//! - Queue capacity is **per-subscriber** (not global)
//!
//! ## Overflow behavior
//! When subscriber's queue is full:
//! 1. Event is **dropped** for this subscriber only
//! 2. `SubscriberOverflow` event is published (for observability)
//! 3. Other subscribers are **unaffected**
//!
//! ## Example
//! ```rust,ignore
//! use taskvisor::Subscribe;
//! use async_trait::async_trait;
//!
//! struct Metrics {
//!     // metrics backend
//! }
//!
//! #[async_trait]
//! impl Subscribe for Metrics {
//!     async fn on_event(&self, ev: &taskvisor::events::Event) {
//!         // Export metrics (can be slow, won't block runtime)
//!         match ev.kind {
//!             taskvisor::events::EventKind::TaskFailed => {
//!                 // increment failure counter
//!             }
//!             _ => {}
//!         }
//!     }
//!
//!     fn name(&self) -> &'static str {
//!         "metrics"
//!     }
//!     fn queue_capacity(&self) -> usize {
//!         2048  // Higher capacity for metrics
//!     }
//! }
//! ```

use async_trait::async_trait;

use crate::events::Event;

/// Event subscriber for runtime observability.
///
/// Receives events from the runtime via a dedicated worker task with bounded queue.
///
/// Each subscriber runs in isolation:
/// - **Bounded queue** buffers events (capacity via [`queue_capacity`](Self::queue_capacity))
/// - **Panic handling** isolates failures (reported as `SubscriberPanicked`)
/// - **Dedicated worker task** processes events sequentially
///
/// ### Implementation requirements
/// - **Performance**: Slow processing only affects this subscriber's queue
/// - **Async-friendly**: Avoid blocking operations, use async I/O
/// - **Error handling**: Handle errors internally, do not panic
///
/// ### Rules
/// - `on_event()` runs in dedicated worker (not in publisher context)
/// - Queue overflow drops events for this subscriber only
/// - Panics are caught and isolated (runtime continues)
#[async_trait]
pub trait Subscribe: Send + Sync + 'static {
    /// Processes a single event.
    ///
    /// ### Context
    /// - Called from dedicated worker task (not publisher)
    /// - Events processed sequentially (FIFO order)
    /// - Panics are caught and reported as `SubscriberPanicked`
    ///
    /// ### Implementation notes
    /// - Use async I/O (tokio, request, etc.)
    /// - Handle errors internally (don't panic)
    /// - Slow processing only affects this subscriber's queue
    async fn on_event(&self, event: &Event);

    /// Returns subscriber name for logging and metrics.
    ///
    /// ### Notes
    /// - Used in `SubscriberOverflow` and `SubscriberPanicked` events
    /// - Keep short and descriptive (e.g., "metrics", "audit", "slack")
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Returns preferred queue capacity for this subscriber.
    ///
    /// ### Overflow behavior
    /// 1. New event is **dropped** (not queued)
    /// 2. `SubscriberOverflow` event published
    /// 3. Other subscribers unaffected
    ///
    /// ### Default
    /// Returns 1024 (reasonable for most use cases).
    fn queue_capacity(&self) -> usize {
        1024
    }
}
