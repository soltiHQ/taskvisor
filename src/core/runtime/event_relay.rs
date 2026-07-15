//! Lossy event-bus relay, alive tracking, and subscriber listener lifecycle.

use std::sync::Arc;

use tokio::sync::broadcast;

use super::SupervisorCore;
use crate::{
    core::alive::AliveTracker, events::Event, identity::TaskId, subscribers::SubscriberSet,
};

impl SupervisorCore {
    /// Returns a best-effort sorted list of task names currently marked alive.
    ///
    /// This is an event-derived activity view, not registry membership.
    /// A registered actor can be marked not alive while it waits to retry.
    pub(crate) async fn snapshot(&self) -> Vec<Arc<str>> {
        self.alive.snapshot().await
    }

    /// Returns true if any task with this name is currently marked alive.
    ///
    /// This is a best-effort activity query from the alive tracker, not a registry membership check.
    pub(crate) async fn is_alive(&self, name: &str) -> bool {
        self.alive.is_alive(name).await
    }

    /// Applies one event to alive tracking and subscriber fan-out.
    async fn distribute(alive: &AliveTracker, set: &SubscriberSet, ev: Arc<Event>) {
        alive.update(&ev).await;
        set.emit_arc(ev);
    }

    /// Drains retained events from a bus receiver.
    ///
    /// Used when the subscriber listener is shutting down.
    /// Broadcast lag gaps are skipped so the retained tail can still be delivered to alive tracking and subscribers.
    pub(super) async fn drain_pending(
        rx: &mut broadcast::Receiver<Arc<Event>>,
        alive: &AliveTracker,
        set: &SubscriberSet,
    ) {
        loop {
            match rx.try_recv() {
                Ok(ev) => Self::distribute(alive, set, ev).await,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    }

    /// Starts the subscriber listener task.
    ///
    /// The listener relays bus events to the alive tracker and subscriber queues.
    pub(super) fn subscriber_listener(&self) {
        let mut rx = self.bus.subscribe();
        let set = Arc::clone(&self.subs);
        let alive = Arc::clone(&self.alive);
        let registry = Arc::clone(&self.registry);
        let rt = self.runtime_token.clone();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    msg = rx.recv() => match msg {
                        Ok(arc_ev) => {
                            if let Err(panic) = crate::core::panic_guard::guarded(
                                Self::distribute(&alive, &set, arc_ev),
                            )
                            .await
                            {
                                set.emit_arc(Arc::new(Event::runtime_failure(
                                    "subscriber_listener",
                                    format!("listener panic: {panic}"),
                                )));
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            let arc_e = Arc::new(Event::subscriber_overflow(
                                "subscriber_listener",
                                format!("lagged({skipped})"),
                            ));
                            alive.update(&arc_e).await;
                            set.emit_arc(arc_e);

                            let live: std::collections::HashSet<TaskId> = registry
                                .list()
                                .await
                                .into_iter()
                                .map(|(id, _)| id)
                                .collect();
                            alive.reconcile(&live).await;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },

                    _ = rt.cancelled() => {
                        Self::drain_pending(&mut rx, &alive, &set).await;
                        break;
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
