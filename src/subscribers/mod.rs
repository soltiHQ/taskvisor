//! # Subscriber.
//!
//! Subscribers observe runtime events without running callbacks on Tokio async workers.
//!
//! [`Subscribe`] is the trait for custom event handlers.
//! Each subscriber gets its own bounded queue and one queue worker task.
//! Event publishers enqueue without waiting, and the queue worker runs callbacks on Tokio's blocking pool.
//! A slow subscriber can overflow its own queue without occupying Tokio async worker threads or blocking event publishers.
//!
//! Delivery is best-effort: events may be dropped if the bus or a subscriber queue falls behind.
//! Drops and subscriber panics are reported as diagnostic events when possible.
//! During shutdown, all subscriber queues share one configurable drain timeout.
//!
//! See [`Subscribe`] for the public contract.
//!
//! ## Flow
//!
//! ```text
//! Bus (broadcast) ──► internal subscriber listener
//!                         ├─► AliveTracker::update()
//!                         └─► SubscriberSet::emit_arc()
//!                                   ├─► [queue S1] ──► worker ──► blocking pool ──► S1.on_event()
//!                                   ├─► [queue S2] ──► worker ──► blocking pool ──► S2.on_event()
//!                                   └─► ...
//! ```
//!
//! See [`Event`](crate::Event) and [`EventKind`](crate::EventKind) for event data.

mod subscriber;
mod subscriber_set;

#[cfg(any(feature = "logging", feature = "tracing"))]
mod embedded;

pub use subscriber::Subscribe;
pub(crate) use subscriber_set::SubscriberSet;

#[cfg(feature = "logging")]
pub use embedded::LogWriter;

#[cfg(feature = "tracing")]
pub use embedded::TracingBridge;
