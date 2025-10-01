//! # Core subscriber trait
//!
//! `Subscribe` is the extension point for plugging custom event handlers into the
//! runtime. Each subscriber is driven by a dedicated worker loop fed by a bounded
//! queue that is owned by the [`SubscriberSet`](crate::subscribers::SubscriberSet).
//!
//! ## Contract
//! - Implementations may be slow (I/O, batching, retries) – they do **not** block
//!   the publisher nor other subscribers.
//! - Each subscriber **declares** its preferred queue capacity via
//!   [`Subscribe::queue_capacity`]. If a queue overflows, events for that
//!   subscriber are **dropped** (warn).
//!
//! ## Example (skeleton)
//! ```rust
//! // use taskvisor::subscribers::Subscribe;
//! // use taskvisor::events::Event;
//! //
//! // struct Audit;
//! // #[async_trait::async_trait]
//! // impl Subscribe for Audit {
//! //     async fn on_event(&self, ev: &Event) {
//! //         // write audit record...
//! //     }
//! //     fn name(&self) -> &'static str { "audit" }
//! //     fn queue_capacity(&self) -> usize { 512 }
//! // }
//! ```

use crate::events::Event;
use async_trait::async_trait;

/// Contract for event subscribers.
///
/// Called from a subscriber-dedicated worker task. Implementations should avoid
/// blocking the async runtime (prefer async I/O and cooperative waits).
#[async_trait]
pub trait Subscribe: Send + Sync + 'static {
    /// Handle a single event for this subscriber.
    ///
    /// # Parameters
    /// - `event`: Reference to the event (does not transfer ownership)
    async fn on_event(&self, event: &Event);

    /// Human-readable name (for logs/metrics).
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Preferred capacity of this subscriber's queue.
    ///
    /// On overflow, events for this subscriber are **dropped** (warn).
    fn queue_capacity(&self) -> usize {
        1024
    }
}
