//! Defines the application callback boundary for runtime events.
//!
//! [`Subscribe`] implementations enter a supervisor through
//! [`SupervisorBuilder::with_subscribers`](crate::SupervisorBuilder::with_subscribers).
//! Each implementation gets its own bounded queue and serial callback lane
//! after runtime startup.
//!
//! Ordinary runtime events pass through the shared bus and the subscriber
//! queue. After a full lane catches up, Taskvisor delivers its coalesced
//! overflow summary directly when the lane remains active. Subscribers are for
//! observation, not runtime state or reliable task results.

use std::num::NonZeroUsize;

use crate::events::Event;

const DEFAULT_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

/// Synchronous observer for best-effort [`Event`] values.
///
/// Each subscriber has one serial lane on a supervisor-local callback
/// executor. Events delivered to [`on_event`](Self::on_event) keep FIFO order
/// for that subscriber. Shutdown or a failed callback lane can still discard
/// queued events. Different subscribers may run concurrently. These callbacks
/// do not use Tokio async workers or its blocking pool.
///
/// Keep callbacks short. Copy the needed fields into an application-owned
/// channel when handling requires async I/O, blocking I/O, or a long wait. The
/// borrowed event is valid only for the callback.
///
/// Queue overflow affects only that subscriber. Taskvisor counts dropped
/// ordinary events and delivers one direct
/// [`SubscriberOverflow`](crate::EventKind::SubscriberOverflow) summary after
/// the lane catches up. Dropping an internal diagnostic, or panicking while
/// handling one, does not generate another diagnostic.
///
/// Taskvisor catches an unwinding panic from an ordinary event callback and
/// tries to publish a [`SubscriberPanicked`](crate::EventKind::SubscriberPanicked)
/// event. A `panic = "abort"` build exits instead.
///
/// During shutdown, all subscriber lanes share one drain timeout. Queued events
/// are dropped at the deadline. A callback already running cannot be aborted
/// and may continue on its executor thread after shutdown returns.
///
/// # Examples
///
/// ```rust,no_run
/// use std::num::NonZeroUsize;
/// use std::sync::Arc;
/// use taskvisor::{Event, EventKind, Subscribe, Supervisor, SupervisorConfig};
///
/// struct Metrics;
///
/// impl Subscribe for Metrics {
///     fn on_event(&self, event: &Event) {
///         if event.kind == EventKind::AttemptFailed {
///             // Update an in-memory counter.
///         }
///     }
///
///     fn name(&self) -> &str {
///         "metrics"
///     }
///
///     fn queue_capacity(&self) -> NonZeroUsize {
///         NonZeroUsize::new(2048).unwrap()
///     }
/// }
///
/// let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(Metrics)];
/// let supervisor = Supervisor::builder(SupervisorConfig::default())
///     .with_subscribers(subscribers)
///     .build();
/// // Start it with `Supervisor::run`, `run_until`, or `serve`.
/// ```
pub trait Subscribe: Send + Sync + 'static {
    /// Processes one event delivered by this subscriber's serial lane.
    ///
    /// Taskvisor calls this method on a callback-executor thread. Calls never
    /// run in the event publisher or on a Tokio worker.
    fn on_event(&self, event: &Event);

    /// Returns the name used by subscriber diagnostics.
    ///
    /// Supervisor construction reads and stores this value once. Choose a
    /// stable, recognizable name for logs and alerts. The default is the fully
    /// qualified [`type_name`](std::any::type_name).
    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }

    /// Returns the maximum number of queued events for this subscriber.
    ///
    /// Supervisor construction reads this value once. Values above Tokio's
    /// structural bounded-channel maximum make
    /// [`SupervisorBuilder::try_build`](crate::SupervisorBuilder::try_build)
    /// return [`BuildError::CapacityTooLarge`](crate::BuildError::CapacityTooLarge).
    /// A full queue drops the new event for this subscriber. Increase capacity
    /// for short bursts. A larger queue does not make a slow callback faster.
    ///
    /// The default capacity is `1024`.
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
