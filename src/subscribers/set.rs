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
//!
//! ## Example
//! ```rust
//! // use std::sync::Arc;
//! // use taskvisor::subscribers::{Subscribe, SubscriberSet};
//! // use taskvisor::events::Event;
//! //
//! // struct Printer;
//! // #[async_trait::async_trait]
//! // impl Subscribe for Printer {
//! //     async fn on_event(&self, ev: &Event) { let _ = ev; /* println! ... */ }
//! //     fn name(&self) -> &'static str { "printer" }
//! // }
//! //
//! // let set = SubscriberSet::new(vec![Arc::new(Printer) as _]);
//! // // set.emit(&event);
//! ```

use std::sync::Arc;

use futures::FutureExt;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::events::Event;

use super::Subscribe;

/// Per-subscriber channel with metadata
struct SubscriberChannel {
    name: &'static str,
    sender: mpsc::Sender<Arc<Event>>,
}

/// Composite fan-out with per-subscriber bounded queues and worker tasks.
pub struct SubscriberSet {
    channels: Vec<SubscriberChannel>,
    workers: Vec<JoinHandle<()>>,
}

impl SubscriberSet {
    /// Creates a new set and spawns one worker per subscriber.
    #[must_use]
    pub fn new(subs: Vec<Arc<dyn Subscribe>>) -> Self {
        let mut channels = Vec::with_capacity(subs.len());
        let mut workers = Vec::with_capacity(subs.len());

        for sub in subs {
            let cap = sub.queue_capacity().max(1);
            let name = sub.name();
            let (tx, mut rx) = mpsc::channel::<Arc<Event>>(cap);
            let s = Arc::clone(&sub);

            let handle = tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    let fut = s.on_event(ev.as_ref());
                    if let Err(panic_err) = std::panic::AssertUnwindSafe(fut).catch_unwind().await {
                        eprintln!(
                            "[taskvisor] subscriber '{}' panicked: {:?}",
                            s.name(),
                            panic_err
                        );
                    }
                }
            });

            channels.push(SubscriberChannel { name, sender: tx });
            workers.push(handle);
        }

        Self { channels, workers }
    }

    /// Fan-out one event to all subscribers (non-blocking).
    ///
    /// If a subscriber's queue is **full** or **closed**, the event is dropped for it
    /// and a warning is logged with the subscriber's name.
    pub fn emit(&self, event: &Event) {
        let ev = Arc::new(event.clone());
        for channel in &self.channels {
            match channel.sender.try_send(Arc::clone(&ev)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    eprintln!(
                        "[taskvisor] subscriber '{}' dropped event: queue full",
                        channel.name
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    eprintln!(
                        "[taskvisor] subscriber '{}' dropped event: worker closed",
                        channel.name
                    );
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
