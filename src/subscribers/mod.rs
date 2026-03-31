//! # Subscriber infrastructure.
//!
//! [`Subscribe`] is the trait you implement to consume runtime events.
//! The supervisor delivers events to each subscriber via a dedicated bounded queue and worker task.
//!
//! See [`Subscribe`] for the full contract.
//!
//! ## Overview
//!
//! ```text
//! Bus (broadcast) ──► Supervisor::subscriber_listener
//!                         ├─► AliveTracker::update()
//!                         └─► SubscriberSet::emit_arc()
//!                                   ├─► [queue for S1] ──► worker ──► S1.on_event()
//!                                   ├─► [queue for S2] ──► worker ──► S2.on_event()
//!                                   └─► ...
//! ```
//!
//! See [`Event`](crate::Event) and [`EventKind`](crate::EventKind) for the event structure.

mod subscriber;
mod subscriber_set;

#[cfg(feature = "logging")]
mod embedded;

pub use subscriber::Subscribe;
pub(crate) use subscriber_set::SubscriberSet;

#[cfg(feature = "logging")]
pub use embedded::LogWriter;
