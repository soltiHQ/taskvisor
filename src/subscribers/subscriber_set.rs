//! # Non-blocking event fan-out to multiple subscribers.
//!
//! [`SubscriberSet`] distributes events to multiple subscribers concurrently without blocking the publisher.
//!
//! ## Architecture
//!
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
//!
//! - **No cross-subscriber ordering**: subscriber A may process event N while B processes N+5
//! - **Overflow**: event is dropped for that subscriber only, `SubscriberOverflow` is published
//! - **Non-blocking**: `emit()` returns immediately (uses `try_send`)
//! - **Isolation**: a slow or panicking subscriber doesn't affect others
//! - **Per-subscriber FIFO**: each subscriber sees events in order
//!
//! ## Panic handling
//!
//! Worker tasks use `AssertUnwindSafe` + `catch_unwind` to isolate panics:
//! - Panic is caught and converted to `SubscriberPanicked` event
//! - `on_event` is called inside `catch_unwind`
//! - Worker continues processing next event
//! - Other subscribers are unaffected
//!
//! **Warning**: `AssertUnwindSafe` is used, which can leave shared state inconsistent
//! if a subscriber uses `Arc<Mutex<T>>` and panics while holding the lock.
//!
//! See [`Subscribe`] for the subscriber trait contract.

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
/// - **Concurrent delivery**: events are dispatched to all subscriber queues in a single pass
/// - **Panic safety**: panics are caught and reported, do not crash the runtime
/// - **Overflow handling**: dropped events are reported via `SubscriberOverflow`
/// - **Isolation**: each subscriber has a dedicated queue and worker
///
/// ## Shutdown
///
/// Call [`close`](Self::close) to gracefully drain all subscriber queues.
/// Workers finish processing remaining events before exiting.
///
/// ## Also
///
/// - See [`Subscribe`] for the subscriber trait contract.
/// - See [`Event`](crate::Event) for the event structure delivered to subscribers.
pub(crate) struct SubscriberSet {
    /// Per-subscriber senders.
    ///
    /// Wrapped in `Mutex` so [`close`](Self::close) can drop them from `&self` (through `Arc`).
    /// The lock is uncontended in the hot path - `emit_arc` is called from a single task (`subscriber_listener`).
    channels: std::sync::Mutex<Vec<SubscriberChannel>>,

    /// Worker join handles. Taken once during [`close`](Self::close).
    workers: std::sync::Mutex<Vec<JoinHandle<()>>>,

    bus: Bus,
}

impl SubscriberSet {
    /// Creates a new set and spawns one worker task per subscriber.
    ///
    /// ### Per-subscriber setup
    ///
    /// - Bounded `mpsc` queue (capacity from [`Subscribe::queue_capacity`], clamped to >= 1)
    /// - Panic isolation via `catch_unwind` around synchronous `on_event` call
    /// - Dedicated worker task (runs until the queue is closed)
    ///
    /// ### Notes
    ///
    /// - Workers start immediately and process events until shutdown
    #[must_use]
    pub(crate) fn new(subs: Vec<Arc<dyn Subscribe>>, bus: Bus) -> Self {
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
                    // If the panicking subscriber holds an Arc<Mutex<T>>, that mutex will be poisoned, which is the expected Rust behavior.
                    // The worker continues processing the next event after reporting the panic via SubscriberPanicked.
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

    /// Emits a pre-allocated `Arc<Event>` to all subscribers.
    /// - Uses `try_send` (non-blocking)
    /// - On queue full: drops the event, publishes `SubscriberOverflow`
    /// - On queue closed: publishes `SubscriberOverflow` with reason "closed"
    ///
    /// ### Overflow prevention
    ///
    /// Prevents infinite loops and event storms: if the **incoming** event is `SubscriberOverflow` **or** `SubscriberPanicked`,
    /// we do not publish further overflow diagnostics for it.
    pub(crate) fn emit_arc(&self, event: Arc<Event>) {
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
    /// - Drops all channel senders (workers observe channel closure and drain remaining events)
    /// - Awaits all worker tasks to finish processing
    ///
    /// Safe to call multiple times: subsequent calls are no-ops.
    pub(crate) async fn close(&self) {
        {
            let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
            channels.clear();
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventKind;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct CountingSub {
        count: Arc<AtomicU64>,
        capacity: usize,
    }

    impl CountingSub {
        fn new(capacity: usize) -> (Arc<AtomicU64>, Arc<Self>) {
            let count = Arc::new(AtomicU64::new(0));
            let sub = Arc::new(Self {
                count: Arc::clone(&count),
                capacity,
            });
            (count, sub)
        }
    }

    impl Subscribe for CountingSub {
        fn on_event(&self, _event: &Event) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        fn name(&self) -> &'static str {
            "counting"
        }
        fn queue_capacity(&self) -> usize {
            self.capacity
        }
    }

    /// Subscriber that panics on every event.
    struct PanicSub;

    impl Subscribe for PanicSub {
        fn on_event(&self, _event: &Event) {
            panic!("boom");
        }
        fn name(&self) -> &'static str {
            "panicking"
        }
        fn queue_capacity(&self) -> usize {
            16
        }
    }

    #[tokio::test]
    async fn overflow_publishes_subscriber_overflow() {
        let bus = Bus::new(64);
        let mut bus_rx = bus.subscribe();

        let (count, sub) = CountingSub::new(1);
        let set = SubscriberSet::new(vec![sub], bus.clone());

        let ev1 = Arc::new(Event::new(EventKind::TaskStarting).with_task("t"));
        let ev2 = Arc::new(Event::new(EventKind::TaskStarting).with_task("t"));
        let ev3 = Arc::new(Event::new(EventKind::TaskStarting).with_task("t"));

        set.emit_arc(ev1);
        set.emit_arc(ev2);
        set.emit_arc(ev3);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        set.close().await;

        let mut saw_overflow = false;
        while let Ok(ev) = bus_rx.try_recv() {
            if matches!(ev.kind, EventKind::SubscriberOverflow) {
                saw_overflow = true;
            }
        }

        assert!(
            saw_overflow,
            "expected SubscriberOverflow on bus (count={})",
            count.load(Ordering::Relaxed)
        );
    }

    #[tokio::test]
    async fn overflow_of_internal_event_does_not_publish_further_overflow() {
        let bus = Bus::new(64);
        let mut bus_rx = bus.subscribe();

        let (_count, sub) = CountingSub::new(1);
        let set = SubscriberSet::new(vec![sub], bus.clone());

        let internal_ev = Arc::new(Event::new(EventKind::SubscriberOverflow).with_task("test"));
        for _ in 0..5 {
            set.emit_arc(Arc::clone(&internal_ev));
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        set.close().await;

        while let Ok(ev) = bus_rx.try_recv() {
            assert!(
                !matches!(ev.kind, EventKind::SubscriberOverflow),
                "internal event overflow must not publish further SubscriberOverflow"
            );
        }
    }

    #[tokio::test]
    async fn panic_in_subscriber_publishes_subscriber_panicked_and_continues() {
        let bus = Bus::new(64);
        let mut bus_rx = bus.subscribe();
        let set = SubscriberSet::new(vec![Arc::new(PanicSub)], bus.clone());

        for _ in 0..3 {
            let ev = Arc::new(Event::new(EventKind::TaskStarting).with_task("t"));
            set.emit_arc(ev);
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        set.close().await;

        let mut panic_count = 0u64;
        while let Ok(ev) = bus_rx.try_recv() {
            if matches!(ev.kind, EventKind::SubscriberPanicked) {
                panic_count += 1;
            }
        }

        assert!(
            panic_count >= 3,
            "expected at least 3 SubscriberPanicked events, got {panic_count}"
        );
    }

    #[tokio::test]
    async fn close_drains_queued_events() {
        let bus = Bus::new(64);
        let (count, sub) = CountingSub::new(128);
        let set = SubscriberSet::new(vec![sub], bus);

        let n = 10u64;
        for _ in 0..n {
            let ev = Arc::new(Event::new(EventKind::TaskStopped).with_task("t"));
            set.emit_arc(ev);
        }
        set.close().await;

        assert_eq!(
            count.load(Ordering::Relaxed),
            n,
            "close() must drain all queued events"
        );
    }
}
