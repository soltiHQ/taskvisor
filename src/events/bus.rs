//! # Internal event bus
//!
//! [`Bus`] is a small wrapper around [`tokio::sync::broadcast`].
//! Runtime components use it to publish [`Event`] values without waiting for consumers.
//!
//! ## Architecture
//!
//! ```text
//! Supervisor / registry / task runners
//!                  ▼
//!          bounded broadcast bus
//!                  ▼
//!          subscriber listener
//!                  ▼
//!        per-subscriber queues
//! ```
//!
//! Each `subscribe()` call creates an independent receiver position.
//! All receivers share one bounded ring that keeps the newest events.
//!
//! ## Delivery model
//!
//! - Publishing is non-blocking.
//! - Delivery is best-effort.
//! - A new receiver only sees events sent after it subscribes.
//! - If a receiver is too slow, it gets `RecvError::Lagged(n)` and skips old events.
//! - If there are no active receivers, published events are dropped.
//!
//! This bus is only for observability. It does not provide durable, at-least-once, or exactly-once delivery.

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
/// - **Best-effort**: no delivery or durability guarantees.
#[derive(Clone, Debug)]
pub(crate) struct Bus {
    tx: broadcast::Sender<Arc<Event>>,
}

/// Broadcast channel for runtime events.
///
/// This is an internal runtime primitive.
/// Public user code normally consumes events through [`Subscribe`](crate::Subscribe), not by subscribing to `Bus` directly.
///
/// The bus is cloneable and multi-producer.
/// Receivers are independent, but they share one bounded broadcast buffer.
impl Bus {
    /// Creates a new bus.
    ///
    /// `capacity` is the number of recent events kept in the shared broadcast ring.
    /// The value is clamped to at least `1`.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let (tx, _rx) = broadcast::channel::<Arc<Event>>(capacity);
        Self { tx }
    }

    /// Publishes an event to all active bus receivers.
    ///
    /// This never waits for receivers.
    /// If there are no receivers, or a receiver is too slow, the event may be dropped for that receiver.
    pub fn publish(&self, ev: Event) {
        let _ = self.tx.send(Arc::new(ev));
    }

    /// Creates an independent receiver.
    ///
    /// The receiver observes only events sent after this call.
    /// If it falls behind, `recv()` returns `RecvError::Lagged(n)` before continuing with retained events.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.tx.subscribe()
    }

    /// Returns the current receiver count for internal architecture tests.
    #[cfg(test)]
    pub(crate) fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventKind;
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};

    #[tokio::test]
    async fn capacity_zero_clamps_to_one() {
        let bus = Bus::new(0);
        let mut rx = bus.subscribe();
        bus.publish(Event::new(EventKind::ShutdownRequested));
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.kind, EventKind::ShutdownRequested);
    }

    #[tokio::test]
    async fn every_receiver_observes_each_event() {
        let bus = Bus::new(16);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();

        bus.publish(Event::new(EventKind::TaskStarting));

        assert_eq!(a.recv().await.unwrap().kind, EventKind::TaskStarting);
        assert_eq!(b.recv().await.unwrap().kind, EventKind::TaskStarting);
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_dropped() {
        let bus = Bus::new(16);
        bus.publish(Event::new(EventKind::TaskStarting));

        let mut rx = bus.subscribe();
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn slow_receiver_observes_lagged_then_resumes() {
        let bus = Bus::new(2);
        let mut rx = bus.subscribe();

        for _ in 0..4 {
            bus.publish(Event::new(EventKind::TaskStarting));
        }

        let err = rx
            .recv()
            .await
            .expect_err("lagged receiver must report the skip");
        assert!(
            matches!(err, RecvError::Lagged(_)),
            "expected Lagged, got {err:?}"
        );
        assert_eq!(rx.recv().await.unwrap().kind, EventKind::TaskStarting);
    }
}
