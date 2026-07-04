//! # Subscriber.
//!
//! Subscribers observe runtime events without blocking the supervisor.
//!
//! [`Subscribe`] is the trait for custom event handlers.
//! Each subscriber gets its own bounded queue and one worker task.
//! A slow subscriber can overflow its own queue, but it does not block other subscribers or task execution.
//!
//! Delivery is best-effort: events may be dropped if the bus or a subscriber queue falls behind.
//! Drops and subscriber panics are reported as diagnostic events when possible.
//!
//! See [`Subscribe`] for the public contract.
//!
//! ## Flow
//!
//! ```text
//! Bus (broadcast) ──► internal subscriber listener
//!                         ├─► AliveTracker::update()
//!                         └─► SubscriberSet::emit_arc()
//!                                   ├─► [queue S1] ──► worker ──► S1.on_event()
//!                                   ├─► [queue S2] ──► worker ──► S2.on_event()
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
