//! # Event bus for broadcasting runtime events.
//!
//! [`Bus`] is a thin wrapper around [`tokio::sync::broadcast`] that provides non-blocking
//! event publishing from multiple sources (actors, runner, supervisor).
//!
//! ## Architecture
//!
//! ```text
//! Publishers (many):                 Subscriber (one):
//!   Actor 1 ──┐
//!   Actor 2 ──┼──────► Bus ───────► subscriber_listener ────► SubscriberSet
//!   Actor N ──┤  (broadcast chan)     (in Supervisor)
//!   Runner  ──┘
//! ```
//!
//! taskvisor uses a single internal listener that fans out events to multiple user-defined subscribers via `SubscriberSet`.
//!
//! ## Rules
//!
//! - **Non-blocking publish**: `publish()` never blocks; it calls `broadcast::Sender::send`.
//! - **Lag handling**: slow receivers get `RecvError::Lagged(n)` and skip `n` oldest items.
//! - **No persistence**: events are lost if there are no active subscribers at send time.
//! - **Bounded capacity**: a single ring buffer stores recent events for all receivers.
//!
//! ## Capacity behavior
//!
//! When the channel reaches capacity and new events are sent:
//! - Receivers that fell behind observe `RecvError::Lagged(n)` on the next `recv()`, indicating how many events were skipped.
//! - The ring buffer keeps only the most recent `capacity` events.

use std::sync::Arc;
use tokio::sync::broadcast;

use super::event::Event;

/// Broadcast channel for runtime events.
///
/// Thin wrapper over [`tokio::sync::broadcast`] that provides `publish`/`subscribe` API.
/// Multiple publishers can publish concurrently; subscribers receive clones of each event.
///
/// ### Properties
///
/// - **Non-blocking**: `publish()` returns immediately (send clones internally).
/// - **Cloneable**: cheap to clone (internally holds an `Arc`-backed sender).
/// - **Fire-and-forget**: no delivery or durability guarantees.
#[derive(Clone, Debug)]
pub(crate) struct Bus {
    tx: broadcast::Sender<Arc<Event>>,
}

impl Bus {
    /// Creates a new bus with the given channel capacity.
    ///
    /// ### Notes
    ///
    /// - Capacity is **shared** across all receivers (not per-subscriber).
    /// - When receivers lag, they will observe `RecvError::Lagged`.
    /// - The minimum capacity is 1 (clamped).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let (tx, _rx) = broadcast::channel::<Arc<Event>>(capacity);
        Self { tx }
    }

    /// Publishes an event to all active subscribers.
    pub fn publish(&self, ev: Event) {
        let _ = self.tx.send(Arc::new(ev));
    }

    /// Creates a new receiver that will observe subsequent events.
    /// - Slow receivers get `RecvError::Lagged(n)` and skip over missed items.
    /// - A receiver only gets events **sent after** it subscribes.
    /// - Each call creates an **independent** receiver.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventKind;

    #[tokio::test]
    async fn capacity_zero_clamps_to_one() {
        let bus = Bus::new(0);
        let mut rx = bus.subscribe();
        bus.publish(Event::new(EventKind::ShutdownRequested));
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.kind, EventKind::ShutdownRequested);
    }

    #[tokio::test]
    async fn subscriber_receives_event_after_subscribe() {
        let bus = Bus::new(16);
        let mut rx = bus.subscribe();
        bus.publish(Event::new(EventKind::TaskStarting));
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.kind, EventKind::TaskStarting);
    }
}
