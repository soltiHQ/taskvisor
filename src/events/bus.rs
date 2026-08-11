//! # Internal event ingress
//!
//! Runtime components publish [`Event`] values into one bounded, non-blocking
//! newest-retaining ring. The single event relay owns the consumer and fans
//! events out to subscriber queues. Events are observability only: overflow is
//! coalesced and never used for lifecycle correctness.

use std::{
    collections::VecDeque,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::Notify;

#[cfg(test)]
use tokio::sync::broadcast;

use super::event::Event;

struct RingState {
    events: VecDeque<Event>,
    dropped: u64,
    closed: bool,
}

struct Shared {
    capacity: usize,
    state: Mutex<RingState>,
    available: Notify,
    enabled: AtomicBool,
    receiver_taken: AtomicBool,
    /// Unit tests observe internal events without changing the production
    /// single-consumer architecture.
    #[cfg(test)]
    observers: broadcast::Sender<Arc<Event>>,
}

/// Cloneable publisher for the bounded event ring.
#[derive(Clone)]
pub(crate) struct Bus {
    shared: Arc<Shared>,
}

impl fmt::Debug for Bus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let queued = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .events
            .len();
        formatter
            .debug_struct("Bus")
            .field("capacity", &self.shared.capacity)
            .field("queued", &queued)
            .finish_non_exhaustive()
    }
}

/// Exclusive consumer owned by the runtime event relay.
pub(crate) struct BusReceiver {
    shared: Arc<Shared>,
}

#[derive(Debug)]
pub(crate) enum BusMessage {
    Event(Event),
    /// One retained event plus the number of older events displaced before it.
    ///
    /// Carrying both values atomically prevents a continuous publisher from
    /// making every receiver turn report lag without ever advancing the ring.
    Lagged {
        dropped: u64,
        event: Event,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum TryRecvError {
    Empty,
}

impl BusReceiver {
    pub(crate) fn retained_capacity(&self) -> usize {
        self.shared.capacity
    }

    fn try_message(&self) -> Result<BusMessage, TryRecvError> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let event = state.events.pop_front().ok_or(TryRecvError::Empty)?;
        let dropped = std::mem::take(&mut state.dropped);
        if dropped == 0 {
            Ok(BusMessage::Event(event))
        } else {
            Ok(BusMessage::Lagged { dropped, event })
        }
    }

    pub(crate) async fn recv(&mut self) -> BusMessage {
        loop {
            let notified = self.shared.available.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.try_message() {
                Ok(message) => return message,
                Err(TryRecvError::Empty) => notified.await,
            }
        }
    }

    pub(crate) fn try_recv(&mut self) -> Result<BusMessage, TryRecvError> {
        self.try_message()
    }

    /// Atomically closes publication and extracts all retained ingress state.
    ///
    /// The returned events are owned by the caller so their destructors run
    /// after the ring mutex has been released.
    pub(crate) fn close_and_take_pending(&mut self) -> (VecDeque<Event>, u64) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        self.shared.enabled.store(false, Ordering::Release);
        let events = std::mem::take(&mut state.events);
        let dropped = std::mem::take(&mut state.dropped);
        drop(state);
        (events, dropped)
    }
}

impl Bus {
    /// Creates one bounded event ingress. Zero is normalized to one.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        #[cfg(test)]
        let (observers, _unused) = broadcast::channel(capacity);
        Self {
            shared: Arc::new(Shared {
                capacity,
                state: Mutex::new(RingState {
                    // Capacity is a logical retention limit. Growing on demand
                    // avoids an eager allocation proportional to a configured
                    // maximum before the first event exists.
                    events: VecDeque::new(),
                    dropped: 0,
                    closed: false,
                }),
                available: Notify::new(),
                enabled: AtomicBool::new(false),
                receiver_taken: AtomicBool::new(false),
                #[cfg(test)]
                observers,
            }),
        }
    }

    /// Publishes without waiting, retaining the newest `capacity` events.
    #[cfg(test)]
    pub fn publish(&self, event: Event) {
        if !self.shared.enabled.load(Ordering::Acquire) {
            return;
        }
        self.publish_enabled(event);
    }

    /// Constructs and publishes an event only when delivery is enabled.
    pub(crate) fn publish_lazy(&self, make_event: impl FnOnce() -> Event) {
        if !self.shared.enabled.load(Ordering::Acquire) {
            return;
        }
        self.publish_enabled(make_event());
    }

    fn publish_enabled(&self, event: Event) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // `enabled` is only a fast-path hint. A publisher may have observed it
        // before shutdown closed the ring, so admission is decided again under
        // the same mutex that linearizes close-and-take.
        if state.closed {
            drop(state);
            return;
        }
        #[cfg(test)]
        let observed = Arc::new(event.clone());
        if state.events.len() == self.shared.capacity {
            state.events.pop_front();
            state.dropped = state.dropped.saturating_add(1);
        }
        state.events.push_back(event);
        drop(state);
        #[cfg(test)]
        let _ = self.shared.observers.send(observed);
        self.shared.available.notify_one();
    }

    /// Enables event retention when at least one downstream consumer exists.
    pub(crate) fn enable(&self) {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !state.closed {
            self.shared.enabled.store(true, Ordering::Release);
        }
    }

    /// Returns whether runtime event delivery has a downstream consumer.
    pub(crate) fn is_enabled(&self) -> bool {
        self.shared.enabled.load(Ordering::Acquire)
    }

    /// Transfers the single production consumer to the event relay.
    pub(crate) fn take_receiver(&self) -> BusReceiver {
        self.enable();
        assert!(
            !self.shared.receiver_taken.swap(true, Ordering::AcqRel),
            "the event relay receiver is taken exactly once"
        );
        BusReceiver {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Adds a test-only observer without changing production fan-out.
    #[cfg(test)]
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.enable();
        self.shared.observers.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn receiver_count(&self) -> usize {
        self.shared.observers.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventKind;
    use tokio::sync::broadcast::error::{
        RecvError as ObserverRecvError, TryRecvError as ObserverTryRecvError,
    };

    #[tokio::test]
    async fn capacity_zero_clamps_to_one() {
        let bus = Bus::new(0);
        let mut rx = bus.take_receiver();
        bus.publish(Event::new(EventKind::ShutdownRequested));
        assert!(matches!(
            rx.recv().await,
            BusMessage::Event(event) if event.kind == EventKind::ShutdownRequested
        ));
    }

    #[tokio::test]
    async fn runtime_receiver_reports_coalesced_overflow_and_retains_newest() {
        let bus = Bus::new(2);
        let mut rx = bus.take_receiver();
        for attempt in 1..=5 {
            bus.publish(Event::new(EventKind::AttemptStarting).with_attempt(attempt));
        }

        assert!(matches!(
            rx.recv().await,
            BusMessage::Lagged { dropped: 3, event } if event.attempt == Some(4)
        ));
        assert!(matches!(
            rx.recv().await,
            BusMessage::Event(event) if event.attempt == Some(5)
        ));
    }

    #[test]
    fn continuous_overflow_cannot_starve_retained_events() {
        let bus = Bus::new(1);
        let mut rx = bus.take_receiver();

        for turn in 1..=128_u32 {
            let displaced = turn.saturating_mul(2).saturating_sub(1);
            let retained = turn.saturating_mul(2);
            bus.publish(Event::new(EventKind::AttemptStarting).with_attempt(displaced));
            bus.publish(Event::new(EventKind::AttemptStarting).with_attempt(retained));

            assert!(matches!(
                rx.try_recv(),
                Ok(BusMessage::Lagged { dropped: 1, event })
                    if event.attempt == Some(retained)
            ));
        }
    }

    #[tokio::test]
    async fn test_observers_each_receive_events() {
        let bus = Bus::new(16);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.publish(Event::new(EventKind::AttemptStarting));
        assert_eq!(a.recv().await.unwrap().kind, EventKind::AttemptStarting);
        assert_eq!(b.recv().await.unwrap().kind, EventKind::AttemptStarting);
    }

    #[tokio::test]
    async fn publishing_before_test_observer_is_not_replayed() {
        let bus = Bus::new(16);
        bus.publish(Event::new(EventKind::AttemptStarting));
        let mut rx = bus.subscribe();
        assert!(matches!(rx.try_recv(), Err(ObserverTryRecvError::Empty)));
    }

    #[tokio::test]
    async fn slow_test_observer_reports_lag_and_resumes() {
        let bus = Bus::new(2);
        let mut rx = bus.subscribe();
        for _ in 0..4 {
            bus.publish(Event::new(EventKind::AttemptStarting));
        }
        assert!(matches!(rx.recv().await, Err(ObserverRecvError::Lagged(_))));
        assert_eq!(rx.recv().await.unwrap().kind, EventKind::AttemptStarting);
    }

    #[test]
    fn close_rejects_a_publisher_that_already_passed_the_fast_path() {
        let bus = Bus::new(2);
        let mut rx = bus.take_receiver();
        bus.publish(Event::new(EventKind::AttemptStarting).with_attempt(1));

        let (pending, dropped) = rx.close_and_take_pending();
        // Calling the inner path models a publisher that observed `enabled`
        // before close and reached the mutex afterwards.
        bus.publish_enabled(Event::new(EventKind::AttemptStarting).with_attempt(2));

        assert_eq!(pending.len(), 1);
        assert_eq!(pending.front().and_then(|event| event.attempt), Some(1));
        assert_eq!(dropped, 0);
        assert!(!bus.is_enabled());
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn close_returns_pinned_events_for_destruction_outside_the_ring_lock() {
        let bus = Bus::new(1);
        let mut rx = bus.take_receiver();
        let reason: Arc<str> = Arc::from("pinned-event");
        let reason_probe = Arc::downgrade(&reason);
        bus.publish(Event::new(EventKind::RuntimeFailure).with_reason(Arc::clone(&reason)));
        drop(reason);

        let (pending, dropped) = rx.close_and_take_pending();
        assert_eq!(dropped, 0);
        assert!(reason_probe.upgrade().is_some());
        let ring = bus
            .shared
            .state
            .try_lock()
            .expect("close-and-take must release the ring mutex before returning");
        drop(ring);

        // The mutex was released by `close_and_take_pending`; dropping the
        // extracted ring cannot run an event destructor while holding it.
        drop(pending);
        assert!(reason_probe.upgrade().is_none());
    }
}
