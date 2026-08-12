//! Lossy event-bus relay and subscriber listener lifecycle.

use std::sync::Arc;

use super::SupervisorCore;
use crate::{
    events::{BusMessage, BusReceiver, Event, TryRecvError},
    subscribers::SubscriberSet,
};

/// Maximum number of retained real events replayed during shutdown.
/// Remaining observability data stays lossy; lifecycle cleanup never scales
/// with an arbitrarily large configured ring.
pub(super) const SHUTDOWN_RELAY_DRAIN_LIMIT: usize = 1024;

impl SupervisorCore {
    /// Returns an authoritative sorted list of registered tasks inside an attempt.
    pub(crate) async fn snapshot(&self) -> Vec<Arc<str>> {
        self.registry.alive_snapshot().await
    }

    /// Returns whether the registered task is currently inside an attempt.
    pub(crate) async fn is_alive(&self, name: &str) -> bool {
        self.registry.is_alive(name).await
    }

    /// Drains retained events from a bus receiver.
    ///
    /// Used when the subscriber listener is shutting down.
    /// Retained events are delivered before one coalesced lag diagnostic.
    pub(super) fn drain_pending(rx: &mut BusReceiver, set: &SubscriberSet) {
        // Publishers may still be active while shutdown runs. Consuming at most
        // one retained ring's worth of real events keeps this phase bounded.
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
        // Close publication and detach the remainder in one ring-mutex
        // critical section. Publishers that passed the atomic fast path before
        // this point are rejected by the ring's closed-state recheck.
        let (pending, skipped) = rx.close_and_take_pending();
        let discarded = u64::try_from(pending.len()).unwrap_or(u64::MAX);
        dropped = dropped.saturating_add(skipped).saturating_add(discarded);
        // Event-owned values are destroyed after the ring mutex is released.
        drop(pending);
        if dropped != 0 {
            set.emit_arc(Arc::new(
                Event::subscriber_overflow("subscriber_listener", format!("lagged({dropped})"))
                    .with_dropped(dropped),
            ));
        }
    }

    /// Starts the subscriber listener task.
    ///
    /// The listener relays bus events to subscriber queues.
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
                            // Real retained events have priority over synthetic
                            // diagnostics in downstream bounded queues.
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

    /// Awaits the subscriber listener.
    ///
    /// Returns `false` when Tokio reports that the listener did not join cleanly.
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

    /// Aborts the subscriber listener so shutdown join-failure handling can be tested.
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
