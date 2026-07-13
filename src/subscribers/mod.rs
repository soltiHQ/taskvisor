//! # Event subscribers
//!
//! A subscriber receives best-effort runtime events for logs, metrics, alerts,
//! or other integrations. Implement [`Subscribe`] to add one.
//!
//! ## Threading and backpressure
//!
//! ```text
//! runtime publishers
//!       |
//!       v
//! bounded shared event bus          (may drop old events on lag)
//!       |
//!       v
//! internal listener
//!       |
//!       +--> bounded queue A ------> blocking pool --> A.on_event()
//!       |
//!       `--> bounded queue B ------> blocking pool --> B.on_event()
//! ```
//!
//! Publishing never waits for subscriber code. Each subscriber has its own
//! bounded queue, so one slow subscriber does not fill another subscriber's
//! queue. Its events can still be dropped. Callbacks run one at a time per
//! subscriber on Tokio's blocking pool, in queue order.
//!
//! There are two places where events can be lost:
//!
//! - the shared event bus, if its listener falls behind;
//! - one subscriber queue, if that subscriber falls behind.
//!
//! Taskvisor reports drops and callback panics as diagnostic events when it can.
//! Those diagnostic events are also best-effort. During shutdown, all
//! subscriber queues share the configured drain deadline.
//!
//! See [`Event`](crate::Event) for the data model and [`Subscribe`] for the full
//! callback contract.

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
