//! # Event bus for broadcasting runtime events.
//!
//! [`Bus`] is a wrapper around [`tokio::sync::broadcast`] that allows task actors
//! and the supervisor to broadcast [`Event`]s to multiple subscribers simultaneously.
//!
//! ## Key characteristics:
//! - **Broadcast semantics**: all active subscribers receive a clone of each event
//! - **Non-persistent**: events are lost if there are no active subscribers
//! - **Bounded capacity**: old events are dropped when the channel is full
//! - **Multiple subscribers**: any number of receivers can subscribe independently
//!
//! ## Usage:
//! - [`Bus::publish`] broadcasts an event to all current subscribers (non-blocking)
//! - [`Bus::subscribe`] creates a new receiver that will receive all future events
//!
//! This is used internally by the [`Supervisor`](crate::core::Supervisor)
//! to deliver task lifecycle events to observers and subscribers like `AliveTracker`.

use super::event::Event;
use tokio::sync::broadcast;

/// Broadcast channel for runtime events.
///
/// Wrapper over [`tokio::sync::broadcast`] that provides `publish`/`subscribe` methods
/// for broadcasting [`Event`]s to multiple concurrent subscribers.
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Event>,
}

impl Bus {
    /// Creates a new bus with the given channel capacity.
    ///
    /// # Parameters
    /// - `capacity`: maximum number of events that can be buffered in the channel.
    ///   When capacity is exceeded, the oldest unsent events are dropped.
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publishes an event to all active subscribers.
    ///
    /// The event is cloned and sent to each subscriber independently.
    /// If there are no active subscribers, the event is dropped silently.
    /// This is intentional as the system can operate without observers.
    pub fn publish(&self, ev: Event) {
        let _ = self.tx.send(ev);
    }

    /// Creates a new subscriber that will receive all future events.
    ///
    /// Each call to `subscribe()` creates an independent receiver.
    /// Multiple subscribers can exist simultaneously, each receiving
    /// a clone of every published event.
    ///
    /// # Returns
    /// A new [`broadcast::Receiver<Event>`] that will receive all events
    /// published after this subscription is created.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}
