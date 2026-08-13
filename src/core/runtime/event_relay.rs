//! Forwards best-effort runtime events from the bus into subscriber queues.
//!
//! Runtime components publish to [`Bus`](crate::events::Bus). When that bus is
//! enabled, lifecycle startup takes its single receiver and starts this relay,
//! which feeds [`SubscriberSet`] queues. Shutdown cancels the relay and later
//! joins it from the common cleanup tail.
//!
//! ```text
//! runtime components ──► Bus ──► event relay ──► subscriber queues
//! ```
//!
//! Delivery is intentionally lossy. Lag becomes a subscriber-overflow
//! diagnostic. Shutdown forwards only a bounded retained tail before it closes
//! publication. It then detaches and drops every remaining event outside the
//! event-ring lock.

use std::sync::Arc;

use super::SupervisorCore;
use crate::{
    events::{BusMessage, BusReceiver, Event, TryRecvError},
    subscribers::SubscriberSet,
};

/// Caps retained events forwarded after runtime cancellation.
///
/// Remaining retained events are detached and dropped instead of forwarded.
pub(super) const SHUTDOWN_RELAY_DRAIN_LIMIT: usize = 1024;

impl SupervisorCore {
    /// Forwards one bounded retained tail and closes the event receiver.
    ///
    /// Pending events beyond the limit are detached under the ring lock and
    /// dropped after the lock is released. One overflow diagnostic reports the
    /// combined lag and discarded count.
    pub(super) fn drain_pending(rx: &mut BusReceiver, set: &SubscriberSet) {
        let mut dropped = 0_u64;
        for _ in 0..rx.retained_capacity().min(SHUTDOWN_RELAY_DRAIN_LIMIT) {
            match rx.try_recv() {
                Ok(BusMessage::Event(event)) => set.emit_arc(Arc::new(event)),
                Ok(BusMessage::Lagged {
                    dropped: skipped,
                    event,
                }) => {
                    set.emit_arc(Arc::new(event));
                    dropped = dropped.saturating_add(skipped);
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        let (pending, skipped) = rx.close_and_take_pending();
        let discarded = u64::try_from(pending.len()).unwrap_or(u64::MAX);
        dropped = dropped.saturating_add(skipped).saturating_add(discarded);
        drop(pending);
        if dropped != 0 {
            set.emit_arc(Arc::new(
                Event::subscriber_overflow("subscriber_listener", format!("lagged({dropped})"))
                    .with_dropped(dropped),
            ));
        }
    }

    /// Takes the bus receiver and starts the relay task.
    ///
    /// After a lag gap, the retained event enters subscriber queues before its
    /// overflow diagnostic. Runtime cancellation runs the bounded tail drain.
    ///
    /// # Panics
    ///
    /// Panics outside an active Tokio runtime or after the bus receiver was taken.
    pub(super) fn subscriber_listener(&self) {
        let mut rx = self.bus.take_receiver();
        let set = Arc::clone(&self.subs);
        let rt = self.runtime_token.clone();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    _ = rt.cancelled() => {
                        Self::drain_pending(&mut rx, &set);
                        break;
                    }

                    msg = rx.recv() => match msg {
                        BusMessage::Event(ev) => set.emit_arc(Arc::new(ev)),
                        BusMessage::Lagged {
                            dropped: skipped,
                            event,
                        } => {
                            set.emit_arc(Arc::new(event));
                            let arc_e = Arc::new(
                                Event::subscriber_overflow(
                                    "subscriber_listener",
                                    format!("lagged({skipped})"),
                                )
                                .with_dropped(skipped),
                            );
                            set.emit_arc(arc_e);
                        }
                    }
                }
            }
        });

        *self.subscriber_handle.lock().unwrap() = Some(handle);
    }

    /// Joins the relay and reports a runtime event when the join fails.
    ///
    /// Returns whether the relay joined cleanly. A disabled or unstarted relay
    /// counts as clean.
    pub(super) async fn join_subscriber_listener(&self) -> bool {
        let handle = self
            .subscriber_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let Some(handle) = handle else {
            return true;
        };

        match handle.await {
            Ok(()) => true,
            Err(error) => {
                self.subs.emit_arc(Arc::new(Event::runtime_failure(
                    "subscriber_listener",
                    format!("listener join failed: {error}"),
                )));
                false
            }
        }
    }

    /// Forces the relay join-failure path in shutdown tests.
    #[cfg(test)]
    pub(super) fn abort_subscriber_listener_for_test(&self) {
        if let Some(handle) = self
            .subscriber_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            handle.abort();
        }
    }
}
