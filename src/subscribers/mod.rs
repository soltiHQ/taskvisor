//! Subscriber infrastructure (fan-out & handlers).
//!
//! This module exposes the subscriber extension points and the fan-out
//! coordinator used by the runtime to deliver events to user-defined sinks.
//!
//! ## Overview
//! ```text
//! Bus (broadcast) ──► Supervisor::subscriber_listener
//!                         ├─► AliveTracker::update()
//!                         └─► SubscriberSet::emit{,_arc}()
//!                                   ├─► [queue for S1] ──► worker ──► S1.on_event()
//!                                   ├─► [queue for S2] ──► worker ──► S2.on_event()
//!                                   └─► ...
//! ```
//!
//! ## Contracts
//! - [`Subscribe`] is the trait you implement to consume events.
//! - [`SubscriberSet`] owns per-subscriber bounded queues and workers,
//!   isolates panics, and reports overflow via internal events.

mod subscriber;
mod subscriber_set;

#[cfg(feature = "logging")]
mod embedded;

pub use subscriber::Subscribe;
pub use subscriber_set::SubscriberSet;

#[cfg(feature = "logging")]
pub use embedded::LogWriter;