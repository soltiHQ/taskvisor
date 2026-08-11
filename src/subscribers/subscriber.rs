//! # Event subscriber trait
//!
//! [`Subscribe`] is the extension point for observing runtime events.
//!
//! Each registered subscriber gets:
//! - a bounded queue,
//! - a dedicated worker thread,
//! - panic isolation from the runtime and other subscribers.
//!
//! Delivery is best-effort.
//! If a subscriber queue is full, new events may be dropped for that subscriber.
//! Events may also be lost earlier if the shared event bus lags.
//!
//! ## Flow
//!
//! ```text
//! event ──► [bounded queue] ──► dedicated thread ──► on_event
//!                └── full: count drop; report once after the queue catches up
//! ```
//!
//! ## Rules
//!
//! - Ordinary queue drops are coalesced into one [`EventKind::SubscriberOverflow`](crate::EventKind::SubscriberOverflow) delivered directly to the affected subscriber after its queue catches up.
//! - Taskvisor tries to report ordinary panics as [`EventKind::SubscriberPanicked`](crate::EventKind::SubscriberPanicked).
//! - Successfully queued events are processed one at a time and in FIFO order for each subscriber.
//! - Diagnostic events are not re-reported if they overflow or panic, to avoid feedback loops.
//! - There is no processing order guarantee between different subscribers.
//! - Queue overflow drops the event for this subscriber only.
//! - A slow subscriber can fill only its own queue.
//!
//! ## Example
//!
//! ```rust
//! use std::num::NonZeroUsize;
//! use taskvisor::{Event, EventKind, Subscribe};
//!
//! struct Metrics;
//!
//! impl Subscribe for Metrics {
//!     fn on_event(&self, ev: &Event) {
//!         if matches!(ev.kind, EventKind::AttemptFailed) {
//!             // update counters, push to a channel, etc.
//!         }
//!     }
//!
//!     fn name(&self) -> &str { "metrics" }
//!     fn queue_capacity(&self) -> NonZeroUsize {
//!         NonZeroUsize::new(2048).unwrap()
//!     }
//! }
//! ```

use std::num::NonZeroUsize;

use crate::events::Event;

const DEFAULT_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

/// Synchronous handler for best-effort runtime events.
///
/// `Subscribe` is synchronous by design.
/// Each subscriber owns one dedicated worker thread that calls it serially.
/// Subscriber callbacks do not consume Tokio async workers or Tokio blocking-pool capacity.
///
/// Keep [`on_event`](Self::on_event) fast.
/// For async I/O or work that may wait a long time, send the event data to your own channel and process it elsewhere.
///
/// During shutdown, Taskvisor gives all subscriber queues one shared drain timeout.
/// At the deadline, queued events are dropped.
/// A callback that is already running cannot be aborted and may continue on its dedicated thread after Taskvisor returns.
///
/// Unwinding panics are caught and isolated.
/// A build with `panic = "abort"` cannot isolate panics because the process exits immediately.
/// Taskvisor tries to report a panic on an ordinary event as `SubscriberPanicked`.
/// A panic while handling an internal diagnostic event is not reported again, to avoid a feedback loop.
pub trait Subscribe: Send + Sync + 'static {
    /// Processes one successfully queued event.
    ///
    /// Taskvisor calls this method on the subscriber's dedicated thread, never from the event publisher or a Tokio worker.
    /// Calls are sequential and follow queue order for this subscriber.
    fn on_event(&self, event: &Event);

    /// Returns the name used in logs and diagnostic events.
    ///
    /// Taskvisor reads and stores the name once during registration.
    ///
    /// The default returns the fully-qualified type path via [`type_name`](std::any::type_name).
    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }

    /// Returns this subscriber's queue capacity.
    ///
    /// The return type guarantees that the queue can hold at least one event.
    /// Values above Tokio's structural bounded-channel maximum make
    /// [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build)
    /// return [`BuildError::CapacityTooLarge`](crate::BuildError::CapacityTooLarge).
    /// If the queue is full, Taskvisor drops the new ordinary event and increments this subscriber's overflow counter.
    /// After the queue catches up, the same worker delivers one direct `SubscriberOverflow` summary with the number of dropped events.
    ///
    /// > Default: `1024`.
    fn queue_capacity(&self) -> NonZeroUsize {
        DEFAULT_QUEUE_CAPACITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DefaultCapacity;

    impl Subscribe for DefaultCapacity {
        fn on_event(&self, _event: &Event) {}
    }

    #[test]
    fn subscriber_defaults_use_type_name_and_capacity_1024() {
        assert_eq!(
            DefaultCapacity.name(),
            std::any::type_name::<DefaultCapacity>()
        );
        assert_eq!(DefaultCapacity.queue_capacity().get(), 1024);
    }
}
