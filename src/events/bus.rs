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
//! - **Non-blocking publish**: `publish()` never blocks, returns immediately
//! - **Bounded capacity**: ring buffer of fixed size (oldest events dropped on overflow)
//! - **Single ring buffer**: all publishers write to the same buffer
//! - **Lagged detection**: slow subscribers receive `RecvError::Lagged` on overflow
//! - **No persistence**: events are lost if no active subscribers exist
//!
//! ## Capacity behavior
//! When the channel reaches capacity and new events are published:
//! - Ring buffer fills up (capacity events)
//! - Oldest events are overwritten by new events
//! - Subscriber receives `RecvError::Lagged(n)` indicating n missed events

use super::event::Event;
use tokio::sync::broadcast;

/// Broadcast channel for runtime events.
///
/// Thin wrapper over [`tokio::sync::broadcast`] that provides `publish`/`subscribe` API.
/// Multiple publishers can publish concurrently; subscribers receive all events.
///
/// ### Rules
/// - **Non-blocking**: `publish()` returns immediately
/// - **Fire-and-forget**: no delivery guarantees
/// - **Cloneable**: cheap to clone (Arc-wrapped Sender)
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Event>,
}

impl Bus {
    /// Creates a new bus with the given channel capacity.
    ///
    /// ### Notes
    /// - Capacity is **shared** across all subscribers (not per-subscriber)
    /// - When full, oldest buffered events are dropped
    /// - Minimum capacity is 1 (enforced)
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publishes an event to all active subscribers.
    ///
    /// - Moves event into the channel (not cloned at publish time)
    /// - If no subscribers exist, event is dropped silently
    /// - Returns immediately (non-blocking)
    pub fn publish(&self, ev: Event) {
        let _ = self.tx.send(ev);
    }

    /// Creates a new subscriber that will receive all future events.
    ///
    /// - Each call creates an **independent** receiver
    /// - Receiver gets events published **after** subscription
    /// - Each subscriber receives a **clone** of every event (at recv() time)
    /// - Multiple subscribers can coexist
    ///
    /// ### Slow subscribers
    /// 1. Ring buffer fills to capacity
    /// 2. Oldest events are overwritten by new events
    /// 3. `recv()` returns `RecvError::Lagged(n)` where n = missed events
    /// 4. Next `recv()` gets the next available event
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}
