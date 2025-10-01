//! # Event subscribers for the taskvisor runtime (fire-and-forget).
//!
//! This module exposes the core extension point [`Subscribe`] and the composite
//! fan-out [`SubscriberSet`]. Incoming [`Event`](crate::events::Event)s are
//! **fanned out without awaiting** subscriber processing.
//!
//! ## Architecture (fan-out, per-subscriber queues)
//! ```text
//! Publisher ──► SubscriberSet::emit(&Event) ──(clone Arc<Event>)────┐
//!                                                                  │
//!                     ┌───────────────┬───────────────┬────────────┴─────┐
//!                     ▼               ▼               ▼                  ▼
//!                mpsc queue       mpsc queue      mpsc queue        mpsc queue
//!                 (cap S1)         (cap S2)        (cap S3)          (cap SN)
//!                     │               │               │                  │
//!             worker_task S1   worker_task S2  worker_task S3    worker_task SN
//!                     │               │               │                  │
//!             sub.on_event()   sub.on_event()  sub.on_event()    sub.on_event()
//! ```
//!
//! ### Delivery semantics
//! - `emit(&Event)` returns immediately (no barrier per event).
//! - Per-subscriber **FIFO** is preserved (queue order). No global ordering.
//! - If a queue is **full**, the event is **dropped** for that subscriber (warn).
//! - A panic inside a subscriber is **isolated**; it is logged and does not crash
//!   the runtime or other subscribers.
//!
//! ### Shutdown semantics
//! Call [`SubscriberSet::shutdown`] to close all queues and await worker tasks.
//! Emitting after shutdown is not supported.
//!
//! ## Minimal example
//! (Adapt the `Event` creation to your actual type.)
//! ```rust
//! // use taskvisor::subscribers::{Subscribe, SubscriberSet};
//! // use taskvisor::events::Event;
//! //
//! // struct Metrics;
//! // #[async_trait::async_trait]
//! // impl Subscribe for Metrics {
//! //     async fn on_event(&self, ev: &Event) {
//! //         // export metrics here...
//! //     }
//! //     fn name(&self) -> &'static str { "metrics" }
//! //     fn queue_capacity(&self) -> usize { 1024 }
//! // }
//! //
//! // let set = SubscriberSet::new(vec![std::sync::Arc::new(Metrics) as _]);
//! // // set.emit(&event);
//! ```

mod embedded;
mod set;
mod subscribe;

pub use set::SubscriberSet;
pub use subscribe::Subscribe;

pub use embedded::AliveTracker;
pub use embedded::LogWriter;
