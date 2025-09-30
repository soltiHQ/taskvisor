//! # Core subscriber trait for handling runtime events.
//!
//! The [`Subscriber`] trait is the main extension point for users to plug in
//! custom event handling logic into the taskvisor runtime.
//!
//! Subscribers receive every [`Event`] published to the bus and can:
//! - Export metrics (Prometheus, OpenTelemetry, StatsD)
//! - Send alerts and notifications
//! - Log events in structured format
//! - Maintain custom state or statistics
//! - Trigger side effects based on task lifecycle
//!
//! ## Example: Custom metrics subscriber
//! ```no_run
//! use taskvisor::{Subscriber, Event, EventKind};
//! use async_trait::async_trait;
//! use std::sync::atomic::{AtomicU64, Ordering};
//! use std::sync::Arc;
//!
//! struct MetricsSubscriber {
//!     failures: Arc<AtomicU64>,
//!     timeouts: Arc<AtomicU64>,
//! }
//!
//! #[async_trait]
//! impl Subscriber for MetricsSubscriber {
//!     async fn handle(&self, event: &Event) {
//!         match event.kind {
//!             EventKind::TaskFailed => {
//!                 self.failures.fetch_add(1, Ordering::Relaxed);
//!                 println!("[metrics] task failed: {:?}", event.task);
//!             }
//!             EventKind::TimeoutHit => {
//!                 self.timeouts.fetch_add(1, Ordering::Relaxed);
//!                 println!("[metrics] timeout hit: {:?}", event.task);
//!             }
//!             _ => {}
//!         }
//!     }
//! }
//! ```

use crate::events::Event;
use async_trait::async_trait;

/// # Trait for subscribing to runtime events from the bus.
///
/// Subscribers are called asynchronously by the supervisor whenever a new [`Event`]
/// is broadcast. Each subscriber receives a reference to the event and can process it
/// independently of other subscribers.
///
/// ## Implementation notes
/// - The `handle` method should be non-blocking and fast
/// - Heavy processing should be offloaded to background tasks
/// - Subscribers run concurrently - use synchronization if sharing state
#[async_trait]
pub trait Subscriber {
    /// Called for every event broadcast through the bus.
    ///
    /// # Parameters
    /// - `event`: Reference to the broadcast event containing kind, metadata, and timestamp
    async fn handle(&self, event: &Event);
}
