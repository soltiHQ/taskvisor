//! # Non-blocking event fan-out to multiple subscribers.
//!
//! Provides [`SubscriberSet`] — distributes events to multiple subscribers
//! concurrently without blocking the publisher.
//!
//! ## Architecture
//! ```text
//! emit(event)
//!     │
//!     ├──► [queue 1] ──► worker 1 ──► subscriber1.on_event()
//!     │    (bounded)         └──────► panic → SubscriberPanicked
//!     ├──► [queue 2] ──► worker 2 ──► subscriber2.on_event()
//!     │    (bounded)
//!     └──► [queue N] ──► worker N ──► subscriberN.on_event()
//!          (bounded)
//! ```
//!
//! ## Rules
//! - **No cross-subscriber ordering**: subscriber A may process event N while B processes N+5
//! - **Overflow**: event dropped for that subscriber only, `SubscriberOverflow` published
//! - **Non-blocking**: `emit()` returns immediately (uses `try_send`)
//! - **Isolation**: slow/panicking subscriber doesn't affect others
//! - **Per-subscriber FIFO**: each subscriber sees events in order
//!
//! ## Panic handling
//! Worker tasks use `catch_unwind` to isolate panics:
//! - Panic is caught and converted to `SubscriberPanicked` event
//! - Worker continues processing next event
//! - Other subscribers unaffected
//!
//! **Warning**: `AssertUnwindSafe` is used, which can leave shared state inconsistent
//! if subscriber uses `Arc<Mutex<T>>` and panics while holding the lock.
//!
//! ## Example
//! ```rust,ignore
//! use std::sync::Arc;
//! use taskvisor::Subscribe;
//! use async_trait::async_trait;
//!
//! struct Metrics;
//!
//! #[async_trait]
//! impl Subscribe for Metrics {
//!     async fn on_event(&self, _ev: &taskvisor::events::Event) {
//!         // Process event (won't block other subscribers)
//!     }
//!     fn name(&self) -> &'static str { "metrics" }
//! }
//!
//! struct Alerts;
//!
//! #[async_trait]
//! impl Subscribe for Alerts {
//!     async fn on_event(&self, _ev: &taskvisor::events::Event) {
//!         // Process event independently
//!     }
//!     fn name(&self) -> &'static str { "alerts" }
//! }
//!
//! // Usage in Supervisor:
//! let subscribers: Vec<Arc<dyn Subscribe>> = vec![
//!      Arc::new(Metrics),
//!      Arc::new(Alerts),
//! ];
//! let supervisor = Supervisor::new(config, subscribers);
//! ```

use std::sync::Arc;

use futures::FutureExt;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::events::{Bus, Event, EventKind};
use crate::subscribers::Subscribe;

/// Per-subscriber channel metadata.
struct SubscriberChannel {
    name: &'static str,
    sender: mpsc::Sender<Arc<Event>>,
}

/// Fan-out coordinator for multiple event subscribers.
///
/// Manages per-subscriber queues and worker tasks, providing:
/// - **Concurrent delivery**: events sent to all subscribers simultaneously
/// - **Isolation**: each subscriber has dedicated queue and worker
/// - **Panic safety**: panics caught and reported, don't crash runtime
/// - **Overflow handling**: dropped events reported via `SubscriberOverflow`
pub struct SubscriberSet {
    channels: Vec<SubscriberChannel>,
    workers: Vec<JoinHandle<()>>,
    bus: Bus,
}

impl SubscriberSet {
    /// Creates a new set and spawns one worker task per subscriber.
    ///
    /// ### Per-subscriber setup
    /// - Bounded mpsc queue (capacity from [`Subscribe::queue_capacity`])
    /// - Dedicated worker task (runs until queue closed)
    /// - Panic isolation via `catch_unwind`
    ///
    /// ### Notes
    /// - Workers start immediately and process events until shutdown
    /// - Minimum queue capacity is 1 (enforced)
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

    /// Emits an event to all subscribers (clones the event).
    ///
    /// - Clones event, wraps in `Arc`, calls [`emit_arc`](Self::emit_arc)
    /// - Returns immediately (non-blocking)
    ///
    /// ### Notes
    /// For hot paths, use [`emit_arc`](Self::emit_arc) to avoid cloning.
    pub fn emit(&self, event: &Event) {
        self.emit_arc(Arc::new(event.clone()));
    }

    /// Emits a pre-allocated `Arc<Event>` to all subscribers.
    ///
    /// - Uses `try_send` (non-blocking)
    /// - On queue full: drops event, publishes `SubscriberOverflow`
    /// - On queue closed: publishes `SubscriberOverflow` with reason "closed"
    ///
    /// ### Overflow prevention
    /// Prevents infinite loops: `SubscriberOverflow` events are not re-published if they themselves overflow.
    ///
    /// ### Rules
    /// Preferred over [`emit`](Self::emit) in hot paths (no clone).
    pub fn emit_arc(&self, event: Arc<Event>) {
        let is_overflow_evt = matches!(event.kind, EventKind::SubscriberOverflow);

        for channel in &self.channels {
            match channel.sender.try_send(Arc::clone(&event)) {
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

    /// Gracefully shuts down all subscriber workers.
    ///
    /// 1. Drops all channel senders (workers see channel closed)
    /// 2. Awaits all worker tasks to finish
    pub async fn shutdown(self) {
        drop(self.channels);

        for h in self.workers {
            let _ = h.await;
        }
    }
}
