//! # Non-blocking event fan-out to multiple subscribers.
//!
//! Provides [`SubscriberSet`] distributes events to multiple subscribers
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
//! - **Overflow**: event is dropped for that subscriber only, `SubscriberOverflow` is published
//! - **Non-blocking**: `emit()` returns immediately (uses `try_send`)
//! - **Isolation**: a slow or panicking subscriber doesn't affect others
//! - **Per-subscriber FIFO**: each subscriber sees events in order
//!
//! ## Panic handling
//! Worker tasks use `AssertUnwindSafe` + `catch_unwind` to isolate panics:
//! - `on_event` is called inside `catch_unwind`
//! - Panic is caught and converted to `SubscriberPanicked` event
//! - Worker continues processing next event
//! - Other subscribers are unaffected
//!
//! **Warning**: `AssertUnwindSafe` is used, which can leave shared state inconsistent
//! if a subscriber uses `Arc<Mutex<T>>` and panics while holding the lock.
//!
//! ## Example
//! ```rust
//! use std::sync::Arc;
//! use taskvisor::{Subscribe, Event};
//!
//! struct Metrics;
//!
//! impl Subscribe for Metrics {
//!     fn on_event(&self, _ev: &Event) {
//!         // Process event (won't block other subscribers — each has its own worker)
//!     }
//!     fn name(&self) -> &'static str { "metrics" }
//! }
//!
//! struct Alerts;
//!
//! impl Subscribe for Alerts {
//!     fn on_event(&self, _ev: &Event) {
//!         // Process event independently
//!     }
//!     fn name(&self) -> &'static str { "alerts" }
//! }
//! // In Supervisor: let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(Metrics), Arc::new(Alerts)];
//! ```

use std::sync::Arc;

use tokio::{sync::mpsc, task::JoinHandle};

use crate::events::{Bus, Event};
use crate::subscribers::Subscribe;

/// Per-subscriber channel metadata.
struct SubscriberChannel {
    name: &'static str,
    sender: mpsc::Sender<Arc<Event>>,
}

/// Fan-out coordinator for multiple event subscribers.
///
/// Manages per-subscriber queues and worker tasks, providing:
/// - **Concurrent delivery**: events are sent to all subscribers simultaneously
/// - **Isolation**: each subscriber has a dedicated queue and worker
/// - **Panic safety**: panics are caught and reported, do not crash the runtime
/// - **Overflow handling**: dropped events are reported via `SubscriberOverflow`
///
/// ## Shutdown
/// Call [`close`](Self::close) to gracefully drain all subscriber queues.
/// Workers finish processing remaining events before exiting.
pub struct SubscriberSet {
    /// Per-subscriber senders. Wrapped in `Mutex` so [`close`](Self::close)
    /// can drop them from `&self` (through `Arc`). The lock is uncontended
    /// in the hot path — `emit_arc` is called from a single task
    /// (`subscriber_listener`).
    channels: std::sync::Mutex<Vec<SubscriberChannel>>,

    /// Worker join handles. Taken once during [`close`](Self::close).
    workers: std::sync::Mutex<Vec<JoinHandle<()>>>,

    bus: Bus,
}

impl SubscriberSet {
    /// Creates a new set and spawns one worker task per subscriber.
    ///
    /// ### Per-subscriber setup
    /// - Bounded `mpsc` queue (capacity from [`Subscribe::queue_capacity`], clamped to >= 1)
    /// - Dedicated worker task (runs until the queue is closed)
    /// - Panic isolation via `catch_unwind` around synchronous `on_event` call
    ///
    /// ### Notes
    /// - Workers start immediately and process events until shutdown
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
                    // SAFETY (unwind):
                    // Each subscriber runs in its own worker task with its own mpsc channel:
                    // a panic cannot corrupt another subscriber's state.
                    //
                    // `on_event` is synchronous, so a single `catch_unwind` call suffices.
                    //
                    // If the panicking subscriber holds an Arc<Mutex<T>>, that mutex
                    // will be poisoned, which is the expected Rust behavior.
                    // The worker continues processing the next event after reporting
                    // the panic via SubscriberPanicked.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        s.on_event(ev.as_ref());
                    }));

                    if let Err(panic_err) = result {
                        let info = extract_panic_info(&panic_err);
                        bus_for_worker.publish(Event::subscriber_panicked(s.name(), info));
                    }
                }
            });

            channels.push(SubscriberChannel { name, sender: tx });
            workers.push(handle);
        }

        Self {
            channels: std::sync::Mutex::new(channels),
            workers: std::sync::Mutex::new(workers),
            bus,
        }
    }

    /// Emits an event to all subscribers (clones the event).
    ///
    /// - Clones the event, wraps it in `Arc`, then calls [`emit_arc`](Self::emit_arc)
    /// - Returns immediately (non-blocking)
    ///
    /// ### Notes
    /// For hot paths, prefer [`emit_arc`](Self::emit_arc) to avoid cloning.
    pub fn emit(&self, event: &Event) {
        self.emit_arc(Arc::new(event.clone()));
    }

    /// Emits a pre-allocated `Arc<Event>` to all subscribers.
    ///
    /// - Uses `try_send` (non-blocking)
    /// - On queue full: drops the event, publishes `SubscriberOverflow`
    /// - On queue closed: publishes `SubscriberOverflow` with reason "closed"
    ///
    /// ### Overflow prevention
    /// Prevents infinite loops and event storms: if the **incoming** event is
    /// `SubscriberOverflow` **or** `SubscriberPanicked`, we do not publish further
    /// overflow diagnostics for it.
    pub fn emit_arc(&self, event: Arc<Event>) {
        let is_internal_event = event.is_internal_diagnostic();
        let channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());

        for channel in channels.iter() {
            match channel.sender.try_send(Arc::clone(&event)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if !is_internal_event {
                        self.bus
                            .publish(Event::subscriber_overflow(channel.name, "full"));
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    if !is_internal_event {
                        self.bus
                            .publish(Event::subscriber_overflow(channel.name, "closed"));
                    }
                }
            }
        }
    }

    /// Gracefully closes all subscriber channels and waits for workers to drain.
    ///
    /// - Drops all channel senders (workers observe channel closure and drain remaining events)
    /// - Awaits all worker tasks to finish processing
    ///
    /// Safe to call multiple times — subsequent calls are no-ops.
    /// Safe to call through `Arc<Self>` (unlike the previous `shutdown(self)` which
    /// required ownership and was unreachable through `Arc`).
    pub async fn close(&self) {
        // Drop senders — workers will see channel closed after draining.
        {
            let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
            channels.clear();
        }

        // Take workers and await them.
        let workers = {
            let mut w = self.workers.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *w)
        };

        for h in workers {
            let _ = h.await;
        }
    }
}

/// Extracts a human-readable message from a panic payload.
fn extract_panic_info(panic_err: &Box<dyn std::any::Any + Send>) -> String {
    let any = &**panic_err;
    if let Some(msg) = any.downcast_ref::<&'static str>() {
        (*msg).to_string()
    } else if let Some(msg) = any.downcast_ref::<String>() {
        msg.clone()
    } else {
        "unknown panic".to_string()
    }
}
