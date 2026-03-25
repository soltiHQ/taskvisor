//! # Non-blocking event fan-out to multiple subscribers.
//!
//! Provides [`SubscriberSet`] distributes events to multiple subscribers
//! concurrently without blocking the publisher.
//!
//! ## Architecture
//! ```text
//! emit(event)
//!     │
//!     ├──► [queue 1] ──► worker 1 ──► subscriber1.on_event().await
//!     │    (bounded)         └──────► panic → SubscriberPanicked
//!     ├──► [queue 2] ──► worker 2 ──► subscriber2.on_event().await
//!     │    (bounded)
//!     └──► [queue N] ──► worker N ──► subscriberN.on_event().await
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
//! - The future returned by `on_event` is polled inside `catch_unwind` on each poll
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
//! use taskvisor::{Subscribe, BoxSubscriberFuture, Event};
//!
//! struct Metrics;
//!
//! impl Subscribe for Metrics {
//!     fn on_event<'a>(&'a self, _ev: &'a Event) -> BoxSubscriberFuture<'a> {
//!         Box::pin(async {
//!             // Process event asynchronously (won't block other subscribers)
//!         })
//!     }
//!     fn name(&self) -> &'static str { "metrics" }
//! }
//!
//! struct Alerts;
//!
//! impl Subscribe for Alerts {
//!     fn on_event<'a>(&'a self, _ev: &'a Event) -> BoxSubscriberFuture<'a> {
//!         Box::pin(async {
//!             // Process event independently
//!         })
//!     }
//!     fn name(&self) -> &'static str { "alerts" }
//! }
//! // In Supervisor: let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(Metrics), Arc::new(Alerts)];
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::{sync::mpsc, task::JoinHandle};

use crate::events::{Bus, Event};
use crate::subscribers::Subscribe;

/// Per-subscriber channel metadata.
struct SubscriberChannel {
    name: &'static str,
    sender: mpsc::Sender<Arc<Event>>,
}

/// A future wrapper that catches panics during polling.
///
/// Wraps an inner future with `AssertUnwindSafe` and catches panics on each `poll()`.
/// If the inner future panics, this future resolves to `Err(panic_payload)`.
struct CatchUnwindFuture<F> {
    inner: F,
}

impl<F: Future<Output = ()> + Unpin> Future for CatchUnwindFuture<F> {
    type Output = Result<(), Box<dyn std::any::Any + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let inner = &mut this.inner;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Pin::new(inner).poll(cx))) {
            Ok(Poll::Ready(())) => Poll::Ready(Ok(())),
            Ok(Poll::Pending) => Poll::Pending,
            Err(panic_err) => Poll::Ready(Err(panic_err)),
        }
    }
}

/// Fan-out coordinator for multiple event subscribers.
///
/// Manages per-subscriber queues and worker tasks, providing:
/// - **Concurrent delivery**: events are sent to all subscribers simultaneously
/// - **Isolation**: each subscriber has a dedicated queue and worker
/// - **Panic safety**: panics are caught and reported, do not crash the runtime
/// - **Overflow handling**: dropped events are reported via `SubscriberOverflow`
pub struct SubscriberSet {
    channels: Vec<SubscriberChannel>,
    workers: Vec<JoinHandle<()>>,
    bus: Bus,
}

impl SubscriberSet {
    /// Creates a new set and spawns one worker task per subscriber.
    ///
    /// ### Per-subscriber setup
    /// - Bounded `mpsc` queue (capacity from [`Subscribe::queue_capacity`], clamped to >= 1)
    /// - Dedicated worker task (runs until the queue is closed)
    /// - Panic isolation via `CatchUnwindFuture` (polls inner future inside `catch_unwind`)
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
                    // `CatchUnwindFuture` wraps each poll in `catch_unwind(AssertUnwindSafe(...))`
                    // so panics during any `.await` point inside `on_event` are caught.
                    //
                    // If the panicking subscriber holds an Arc<Mutex<T>>, that mutex
                    // will be poisoned, which is the expected Rust behavior.
                    // The worker continues processing the next event after reporting
                    // the panic via SubscriberPanicked.
                    let fut = s.on_event(ev.as_ref());
                    let wrapped = CatchUnwindFuture { inner: fut };

                    if let Err(panic_err) = wrapped.await {
                        let info = extract_panic_info(&panic_err);
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

        for channel in &self.channels {
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

    /// Gracefully shuts down all subscriber workers.
    ///
    /// - Drops all channel senders (workers observe channel closure),
    /// - Awaits all worker tasks to finish.
    pub async fn shutdown(self) {
        drop(self.channels);

        for h in self.workers {
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
