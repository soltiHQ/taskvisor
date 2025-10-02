//! # SubscriberSet: non-blocking fan-out over multiple subscribers
//!
//! [`SubscriberSet`] distributes each [`Event`](crate::events::Event) to multiple
//! subscribers **without awaiting** their processing.
//!
//! ## What it guarantees
//! - `emit(&Event)` returns immediately.
//! - Per-subscriber FIFO (queue order).
//! - Panics inside subscribers are caught and logged (isolation).
//!
//! ## What it does **not** guarantee
//! - No global ordering across different subscribers.
//! - No retries on per-subscriber queue overflow (events are dropped for that
//!   subscriber).
//!
//! ## Diagram
//! ```text
//!    emit(&Event)
//!        │                        (Arc-clone per subscriber)
//!        ├────────────────► [queue S1] ─► worker S1 ─► on_event()
//!        ├────────────────► [queue S2] ─► worker S2 ─► on_event()
//!        └────────────────► [queue SN] ─► worker SN ─► on_event()
//! ```

use std::sync::Arc;

use futures::FutureExt;
use tokio::{sync::mpsc, task::JoinHandle};

use super::Subscribe;
use crate::events::{Bus, Event, EventKind};

/// Per-subscriber channel with metadata
struct SubscriberChannel {
    name: &'static str,
    sender: mpsc::Sender<Arc<Event>>,
}

/// Composite fan-out with per-subscriber bounded queues and worker tasks.
pub struct SubscriberSet {
    channels: Vec<SubscriberChannel>,
    workers: Vec<JoinHandle<()>>,
    bus: Bus,
}

impl SubscriberSet {
    /// Creates a new set and spawns one worker per subscriber.
    ///
    /// Each subscriber gets a bounded MPSC queue of size `max(queue_capacity, 1)`.
    /// Worker isolation: panics are caught and reported as `SubscriberPanicked`.
    #[must_use]
    pub fn new(subs: Vec<Arc<dyn Subscribe>>, bus: Bus) -> Self {
        let mut channels = Vec::with_capacity(subs.len());
        let mut workers = Vec::with_capacity(subs.len());

        for sub in subs {
            let cap = sub.queue_capacity().max(1);
            let name = sub.name();
            let (tx, mut rx) = mpsc::channel::<Arc<Event>>(cap);
            let s = Arc::clone(&sub);
            let bus_for_worker = bus.clone();

            let handle = tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    let fut = s.on_event(ev.as_ref());
                    if let Err(panic_err) = std::panic::AssertUnwindSafe(fut).catch_unwind().await {
                        let info = {
                            let any = &*panic_err;
                            if let Some(msg) = any.downcast_ref::<&'static str>() {
                                (*msg).to_string()
                            } else if let Some(msg) = any.downcast_ref::<String>() {
                                msg.clone()
                            } else {
                                "unknown panic".to_string()
                            }
                        };
                        bus_for_worker.publish(Event::subscriber_panicked(s.name(), info));
                    }
                }
            });

            channels.push(SubscriberChannel { name, sender: tx });
            workers.push(handle);
        }

        Self {
            channels,
            workers,
            bus,
        }
    }

    /// Fan-out one event to all subscribers (non-blocking).
    ///
    /// If a subscriber's queue is **full** or **closed**, the event is dropped for it
    /// and a `SubscriberOverflow` system event is published.
    pub fn emit(&self, event: &Event) {
        // Prevent infinite loops: do not generate overflow-on-overflow events.
        let is_overflow_evt = matches!(event.kind, EventKind::SubscriberOverflow);

        let ev = Arc::new(event.clone());
        for channel in &self.channels {
            match channel.sender.try_send(Arc::clone(&ev)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if !is_overflow_evt {
                        self.bus
                            .publish(Event::subscriber_overflow(channel.name, "full"));
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    if !is_overflow_evt {
                        self.bus
                            .publish(Event::subscriber_overflow(channel.name, "closed"));
                    }
                }
            }
        }
    }

    /// Graceful shutdown: close all queues and await worker completion.
    pub async fn shutdown(self) {
        drop(self.channels);
        for h in self.workers {
            let _ = h.await;
        }
    }

    /// True if there are no subscribers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    /// Number of subscribers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.channels.len()
    }
}
