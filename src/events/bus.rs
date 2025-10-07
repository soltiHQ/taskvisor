//! # Event bus for broadcasting runtime events.
//!
//! [`Bus`] is a thin wrapper around [`tokio::sync::broadcast`] that provides
//! non-blocking event publishing from multiple sources (actors, runner, supervisor).
//!
//! ## Architecture
//! ```text
//! Publishers (many):                 Subscriber (one):
//!   Actor 1 ──┐
//!   Actor 2 ──┼──────► Bus ───────► subscriber_listener ────► SubscriberSet
//!   Actor N ──┤  (broadcast chan)     (in Supervisor)
//!   Runner  ──┘
//! ```
//!
//! taskvisor uses a single subscriber (`Supervisor::subscriber_listener`) that fans out events
//! to multiple user-defined subscribers via [`SubscriberSet`](crate::SubscriberSet).
//!
//! ## Rules
//! - **Non-blocking publish**: `publish()` never blocks; it calls `broadcast::Sender::send`.
//! - **Bounded capacity**: a single ring buffer stores recent events for all receivers.
//! - **Lag handling**: slow receivers get `RecvError::Lagged(n)` and skip `n` oldest items.
//! - **No persistence**: events are lost if there are no active subscribers at send time.
//!
//! ## Capacity behavior
//! When the channel reaches capacity and new events are sent:
//! - The ring buffer keeps only the most recent `capacity` events.
//! - Receivers that fell behind observe `RecvError::Lagged(n)` on the next `recv()`,
//!   indicating how many events were skipped.

use tokio::sync::broadcast;

use super::event::Event;

/// Broadcast channel for runtime events.
///
/// Thin wrapper over [`tokio::sync::broadcast`] that provides `publish`/`subscribe` API.
/// Multiple publishers can publish concurrently; subscribers receive clones of each event.
///
/// ### Properties
/// - **Non-blocking**: `publish()` returns immediately (send clones internally).
/// - **Fire-and-forget**: no delivery or durability guarantees.
/// - **Cloneable**: cheap to clone (internally holds an `Arc`-backed sender).
#[derive(Clone, Debug)]
pub struct Bus {
    tx: broadcast::Sender<Event>,
}

impl Bus {
    /// Creates a new bus with the given channel capacity.
    ///
    /// ### Notes
    /// - Capacity is **shared** across all receivers (not per-subscriber).
    /// - When receivers lag, they will observe `RecvError::Lagged`.
    /// - The minimum capacity is 1 (clamped).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let (tx, _rx) = broadcast::channel::<Event>(capacity);
        Self { tx }
    }

    /// Publishes an event to all active subscribers.
    ///
    /// - Takes ownership of the event; the broadcast channel clones it for each receiver.
    /// - If there are no receivers, the event is dropped (this function still returns immediately).
    pub fn publish(&self, ev: Event) {
        let _ = self.tx.send(ev);
    }

    /// Creates a new receiver that will observe subsequent events.
    ///
    /// - Each call creates an **independent** receiver.
    /// - A receiver only gets events **sent after** it subscribes.
    /// - Slow receivers get `RecvError::Lagged(n)` and skip over missed items.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Publishes a borrowed event by cloning it.
    ///
    /// Shorthand for `publish(ev.clone())`, useful when you already have a reference.
    pub fn publish_ref(&self, ev: &Event) {
        let _ = self.tx.send(ev.clone());
    }
}
