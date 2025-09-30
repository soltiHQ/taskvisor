//! Event bus for broadcasting runtime events.
//!
//! [`Bus`] is a thin wrapper around [`tokio::sync::broadcast`] that allows task actors and the supervisor to exchange [`Event`]s.
//!
//! - [`Bus::publish`] sends an event to all subscribers (non-blocking).
//! - [`Bus::subscribe`] creates a new receiver for consuming events.
//!
//! This is used internally by the [`Supervisor`](crate::core::supervisor::Supervisor)
//! to deliver task lifecycle events to the [`Observer`](crate::observer::Observer)
//! and to the [`AliveTracker`](crate::alive::AliveTracker).

use tokio::sync::broadcast;

use crate::event::Event;

/// Broadcast channel for runtime events.
///
/// Wrapper over [`tokio::sync::broadcast`] that provides `publish`/`subscribe` methods for working with [`Event`]s.
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Event>,
}

impl Bus {
    /// Creates a new bus with the given channel capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publishes an event to all subscribers.
    ///
    /// Errors are ignored if there are no active subscribers.
    pub fn publish(&self, ev: Event) {
        let _ = self.tx.send(ev);
    }

    /// Subscribes to the bus and returns a new receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}
