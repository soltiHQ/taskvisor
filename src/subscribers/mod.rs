//! # Event subscribers
//!
//! A subscriber receives best-effort runtime events for logs, metrics, alerts, or other integrations. Implement [`Subscribe`] to add one.
//!
//! ## Threading and backpressure
//!
//! ```text
//! runtime publishers
//!       ▼
//! bounded shared event bus          (may drop old events on lag)
//!       ▼
//! internal listener
//!       ├──► bounded queue A ──────► dedicated thread A ──► A.on_event()
//!       └──► bounded queue B ──────► dedicated thread B ──► B.on_event()
//! ```
//!
//! Publishing never waits for subscriber code.
//! Each subscriber has its own bounded queue; one slow subscriber does not fill another subscriber's queue.
//!
//! Its events can still be dropped.
//! Callbacks run one at a time per subscriber on its dedicated thread, in queue order.
//!
//! There are two places where events can be lost:
//!
//! - the shared event bus, if its listener falls behind;
//! - one subscriber queue, if that subscriber falls behind.
//!
//! Queue drops are counted per subscriber and coalesced into one direct overflow summary after that queue catches up.
//! The summary does not re-enter the shared event bus.
//! Callback panic diagnostics remain best-effort events.
//! During shutdown, all subscriber queues share the configured drain deadline.
//!
//! See [`Event`](crate::Event) for the data model and [`Subscribe`] for the full callback contract.

mod subscriber;
mod subscriber_set;

mod embedded;

pub use subscriber::Subscribe;
pub(crate) use subscriber_set::SubscriberSet;

#[cfg(feature = "logging")]
#[cfg_attr(docsrs, doc(cfg(feature = "logging")))]
pub use embedded::LogWriter;

#[cfg(feature = "tracing")]
#[cfg_attr(docsrs, doc(cfg(feature = "tracing")))]
pub use embedded::{TracingBridge, TracingBridgeWithReasons};
