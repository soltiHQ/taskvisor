//! Connects best-effort runtime events to application observers.
//!
//! Register [`Subscribe`] implementations through
//! [`SupervisorBuilder::with_subscribers`](crate::SupervisorBuilder::with_subscribers).
//! Implement the trait for metrics, alerts, or application-specific output.
//! Enable the `logging` feature for `LogWriter` or the `tracing` feature for `TracingBridge`.
//! Every configured subscriber receives its own bounded, serial callback lane.
//!
//! ```text
//! ordinary runtime components
//!      │ Event
//!      ▼
//! bounded event bus
//!      ▼
//! runtime event relay
//!      ▼
//! SubscriberSet
//!      ├── queue A ──► serial lane A ──► subscriber A::on_event
//!      └── queue B ──► serial lane B ──► subscriber B::on_event
//!
//! internal diagnostics ──► event relay or subscriber lane ──► callbacks
//! ```
//!
//! Internal subscriber diagnostics can bypass the shared bus.
//! Subscriber delivery is for logs, metrics, alerts, and diagnostics.
//! It does not own registry state or watched task outcomes.
//! Publishing an ordinary event never calls subscriber code.
//! Events can be lost at the shared bus or at an individual subscriber queue.
//! Each subscriber queue has its own capacity and ordinary-event loss count.
//! A blocked callback on the shared worker can delay other shared lanes until their queues fill.
//!
//! Each lane preserves FIFO callback order.
//! Short callbacks use one fixed shared worker by default.
//! [`SubscriberExecution::Dedicated`] assigns one lane its own worker when blocking isolation is required.
//! `LogWriter` and the tracing bridges select dedicated execution because their output sinks run synchronously.
//! Shutdown gives all lanes one shared drain deadline.
//! With no configured subscribers, the event bus stays disabled and no event relay or callback worker starts.
//!
//! Shared ingress and subscriber queues have separate capacities.
//! Use [`SupervisorConfig::with_bus_capacity`](crate::SupervisorConfig::with_bus_capacity) for bursts before fan-out.
//! Override [`Subscribe::queue_capacity`] for bursts in one observer, and keep its callback short.
//! Larger queues absorb longer bursts but consume more memory.
//!
//! # Choosing an observer
//!
//! | Need                                  | Observer                         |
//! |---------------------------------------|----------------------------------|
//! | Quick human-readable console output   | `LogWriter` with `logging`       |
//! | Structured fields in `tracing`        | `TracingBridge` with `tracing`   |
//! | Metrics, alerts, or another transport | A custom [`Subscribe`] type      |
//!
//! Use [`TaskWaiter`](crate::TaskWaiter) instead when application logic needs a watched task's final result.
//! Subscriber delivery is intentionally lossy.

mod subscriber;
mod subscriber_set;

mod embedded;

pub use subscriber::{Subscribe, SubscriberExecution};
pub(crate) use subscriber_set::SubscriberSet;

#[cfg(feature = "logging")]
#[cfg_attr(docsrs, doc(cfg(feature = "logging")))]
pub use embedded::LogWriter;

#[cfg(feature = "tracing")]
#[cfg_attr(docsrs, doc(cfg(feature = "tracing")))]
pub use embedded::{TracingBridge, TracingBridgeWithReasons};
