//! # Non-blocking event fan-out
//!
//! [`SubscriberSet`] sends each event to every registered subscriber.
//!
//! Each subscriber has its own bounded queue and one dedicated worker thread.
//!
//! ## Flow
//!
//! ```text
//! emit(event)
//!     ├──► [queue 1] ──► thread 1 ──► subscriber1.on_event()
//!     ├──► [queue 2] ──► thread 2 ──► subscriber2.on_event()
//!     └──► [queue N] ──► thread N ──► subscriberN.on_event()
//! ```
//!
//! ## Rules
//!
//! - No cross-subscriber ordering: subscribers may process different events at the same time.
//! - Diagnostic events are not re-reported on overflow or panic, to avoid feedback loops.
//! - Per-subscriber FIFO: successfully queued events are processed in queue order.
//! - Queue overflow is counted per subscriber and reported once after that queue catches up.
//! - Taskvisor tries to report an ordinary panic as `SubscriberPanicked`.
//! - `emit_arc` is non-blocking and uses `try_send`.
//! - Each configured subscriber holds one slot in the shared process-wide
//!   library-owned user-lifetime budget through physical worker completion.
//!
//! ## Panic Handling
//!
//! Worker threads run `on_event` directly inside `catch_unwind`. Final
//! library-owned subscriber destruction runs on the bounded destructor
//! executor after the worker physically exits.
//!
//! This protects the runtime and other subscribers from a panicking subscriber.
//! It does not protect the subscriber's own shared state.
//! For example, a panic while holding a `Mutex` may poison that mutex.
//!
//! See [`Subscribe`] for the subscriber trait contract.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

use crate::{
    BuildError,
    core::{
        MAX_ASYNC_CAPACITY,
        deferred_drop::{DropBundle, DropReservation},
    },
    events::{Bus, Event},
    subscribers::Subscribe,
};

/// Default time allowed for subscriber queues to drain during shutdown.
#[cfg(test)]
pub(crate) const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-subscriber channel metadata.
struct SubscriberChannel {
    name: Arc<str>,
    sender: mpsc::Sender<Arc<Event>>,
    overflow: Arc<SubscriberOverflowState>,
    closed_reported: AtomicBool,
}

/// Coalesces queue drops until the subscriber worker catches up.
#[derive(Default)]
struct SubscriberOverflowState {
    dropped: AtomicU64,
}

type SharedSubscriberReceiver = Arc<std::sync::Mutex<Option<mpsc::Receiver<Arc<Event>>>>>;

impl SubscriberOverflowState {
    fn record_drop(&self) {
        let _ = self
            .dropped
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_add(1))
            });
    }

    fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::AcqRel)
    }
}

/// One dedicated synchronous subscriber worker.
struct SubscriberWorker {
    stop: Arc<AtomicBool>,
    receiver: SharedSubscriberReceiver,
    finished: Arc<AtomicBool>,
    done: oneshot::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SubscriberWorker {
    fn take_receiver(&self) -> Option<mpsc::Receiver<Arc<Event>>> {
        self.receiver
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

impl Drop for SubscriberWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let receiver = self.take_receiver();
        // Queued events may own externally visible values. Destroy them after
        // releasing the receiver mutex.
        drop(receiver);
    }
}

/// Resolves worker completion even if an unexpected panic escapes its loop.
struct WorkerCompletion {
    finished: Arc<AtomicBool>,
    done: Option<oneshot::Sender<()>>,
}

impl Drop for WorkerCompletion {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
    }
}

/// Subscriber metadata retained until runtime startup.
struct SubscriberDefinition {
    name: Arc<str>,
    capacity: usize,
    ownership: OwnedSubscriber,
}

/// Subscriber ownership installed before any synchronous metadata callback.
///
/// Field order is deliberate: unwinding drops the callback reference while the
/// cleanup bundle still retains the final globally charged reference.
struct OwnedSubscriber {
    subscriber: Arc<dyn Subscribe>,
    cleanup: DropBundle,
}

/// Complete ownership transferred to one OS worker in a single closure capture.
///
/// Keeping `ownership` intact is required for `std::thread::Builder::spawn`
/// failure: dropping the rejected closure first drops the callback reference,
/// then submits the retained final reference through its charged bundle.
struct SubscriberThread {
    ownership: OwnedSubscriber,
    receiver: SharedSubscriberReceiver,
    name: Arc<str>,
    bus: Bus,
    overflow: Arc<SubscriberOverflowState>,
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    runtime: tokio::runtime::Handle,
    done: oneshot::Sender<()>,
}

impl SubscriberThread {
    fn run(self) {
        let Self {
            ownership,
            receiver,
            name,
            bus,
            overflow,
            stop,
            finished,
            runtime,
            done,
        } = self;
        let _completion = WorkerCompletion {
            finished,
            done: Some(done),
        };
        let OwnedSubscriber {
            subscriber,
            cleanup,
        } = ownership;
        let mut cleanup = cleanup;
        // Every user-bearing worker value stays inside this panic boundary.
        // The charged cleanup bundle remains outside so it can contain an
        // escaped payload and submit the final subscriber reference after
        // physical worker exit.
        let worker = || {
            let _runtime = runtime.enter();
            run_subscriber_worker(
                receiver,
                subscriber,
                name,
                bus,
                overflow,
                stop,
                &mut cleanup,
            );
        };
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(worker)) {
            let _ = destroy_worker_panic_payload(payload, &mut cleanup);
        }
        cleanup.submit();
    }
}

/// Lifecycle state shared by startup, delivery, and shutdown.
enum SubscriberState {
    /// Subscriber metadata is ready, but no Tokio workers exist yet.
    Pending(Vec<SubscriberDefinition>),
    /// Per-subscriber queues and workers are active.
    Started {
        channels: Vec<SubscriberChannel>,
        workers: Vec<SubscriberWorker>,
    },
    /// Worker startup unwound after runtime validation.
    ///
    /// Definitions and any partially started workers clean themselves up while
    /// unwinding. Retrying must not silently publish an empty Started state.
    Failed,
    /// Queues are closed and startup is permanently disabled.
    Closed,
}

/// Distributes best-effort events to subscribers.
///
/// `SubscriberSet` owns:
/// - one bounded queue per subscriber,
/// - one dedicated worker thread per subscriber,
/// - snapshotted subscriber names for diagnostics.
///
/// Delivery is best-effort.
/// Slow subscribers may lose events from their own queues.
/// Their callbacks run on dedicated threads and never occupy Tokio's async or blocking pools.
///
/// ## Shutdown
///
/// [`close`](Self::close) drops all senders and gives every worker one shared timeout to drain queued events.
/// At the deadline, unfinished workers are told to stop after their current callback and queued events are dropped.
/// A callback already running may continue on its dedicated thread after `close` returns.
///
/// ## Also
///
/// - See [`Subscribe`] for the subscriber trait contract.
/// - See [`Event`] for the event structure delivered to subscribers.
pub(crate) struct SubscriberSet {
    /// One synchronized lifecycle prevents `start` and `close` from crossing.
    ///
    /// The lock is uncontended in the hot path - `emit_arc` is called from a single task (`subscriber_listener`).
    state: std::sync::Mutex<SubscriberState>,

    /// One shared deadline for draining all subscriber workers.
    shutdown_timeout: Duration,

    bus: Bus,

    /// Persistent ownership reservations held by this subscriber set.
    ownership_slots: usize,
}

impl SubscriberSet {
    /// Creates a new inactive set.
    ///
    /// The subscriber name is read once and stored as `Arc<str>`.
    /// This supports dynamic names while keeping diagnostic events stable for the lifetime of the subscriber worker.
    /// Queue workers are created later by [`start`](Self::start).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new(subs: Vec<Arc<dyn Subscribe>>, bus: Bus) -> Self {
        Self::new_with_shutdown_timeout(subs, bus, DEFAULT_SHUTDOWN_TIMEOUT)
    }

    /// Creates an isolated test subscriber set with an explicit shared shutdown timeout.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new_with_shutdown_timeout(
        subs: Vec<Arc<dyn Subscribe>>,
        bus: Bus,
        shutdown_timeout: Duration,
    ) -> Self {
        let source = crate::core::deferred_drop::TestReservationSource::new(subs.len().max(1));
        Self::from_test_source(subs, bus, shutdown_timeout, &source)
            .expect("the isolated subscriber test budget fits every definition")
    }

    #[cfg(test)]
    fn from_test_source(
        subs: Vec<Arc<dyn Subscribe>>,
        bus: Bus,
        shutdown_timeout: Duration,
        source: &crate::core::deferred_drop::TestReservationSource,
    ) -> Result<Self, crate::core::deferred_drop::DropCapacityError> {
        let reservations = source.try_reserve_many(subs.len())?;
        Ok(
            Self::from_reserved(subs, reservations, bus, shutdown_timeout)
                .expect("subscriber test capacities fit the async structural limit"),
        )
    }

    /// Creates a subscriber set from an atomically reserved ownership batch.
    ///
    /// Reservations must be acquired before this function snapshots any
    /// user-provided subscriber metadata.
    pub(crate) fn from_reserved(
        subs: Vec<Arc<dyn Subscribe>>,
        reservations: Vec<DropReservation>,
        bus: Bus,
        shutdown_timeout: Duration,
    ) -> Result<Self, BuildError> {
        assert_eq!(
            subs.len(),
            reservations.len(),
            "every subscriber must have one ownership reservation"
        );
        // Complete ownership transfer for the whole atomic batch before the
        // first user metadata callback. If one callback unwinds, current and
        // not-yet-visited subscribers all retain charged final references.
        let owned: Vec<OwnedSubscriber> = subs
            .into_iter()
            .zip(reservations)
            .map(|(subscriber, reservation)| {
                let cleanup = reservation.bundle(Arc::clone(&subscriber));
                OwnedSubscriber {
                    subscriber,
                    cleanup,
                }
            })
            .collect();
        let ownership_slots = owned.len();
        let definitions: Vec<SubscriberDefinition> = owned
            .into_iter()
            .map(|mut owned| {
                let metadata = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let capacity = owned.subscriber.queue_capacity().get();
                    let name = Arc::from(owned.subscriber.name());
                    (name, capacity)
                }));
                match metadata {
                    Ok((name, capacity)) if capacity <= MAX_ASYNC_CAPACITY => {
                        Ok(SubscriberDefinition {
                            name,
                            capacity,
                            ownership: owned,
                        })
                    }
                    Ok((_name, capacity)) => Err(BuildError::CapacityTooLarge {
                        field: "subscriber_queue_capacity",
                        value: capacity,
                        max: MAX_ASYNC_CAPACITY,
                    }),
                    Err(payload) => {
                        let message = extract_panic_info(&payload);
                        owned.cleanup.attach_panic_payload(payload);
                        drop(owned);
                        panic!("subscriber metadata panicked: {message}")
                    }
                }
            })
            .collect::<Result<_, _>>()?;

        if !definitions.is_empty() {
            bus.enable();
        }

        Ok(Self {
            state: std::sync::Mutex::new(SubscriberState::Pending(definitions)),
            shutdown_timeout,
            bus,
            ownership_slots,
        })
    }

    /// Returns the process-wide ownership slots permanently held by this set.
    pub(crate) fn ownership_slots(&self) -> usize {
        self.ownership_slots
    }

    /// Starts one dedicated worker thread per subscriber.
    ///
    /// Safe to call more than once. Calls after startup or shutdown are no-ops.
    pub(crate) fn start(&self) {
        self.start_with(|index, worker| {
            std::thread::Builder::new()
                .name(format!("taskvisor-subscriber-{index}"))
                .spawn(move || worker.run())
        });
    }

    fn start_with(
        &self,
        mut spawn_worker: impl FnMut(
            usize,
            SubscriberThread,
        ) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let definitions = match &mut *state {
            SubscriberState::Pending(definitions) => definitions,
            SubscriberState::Started { .. } | SubscriberState::Closed => return,
            SubscriberState::Failed => {
                panic!("subscriber worker startup previously failed")
            }
        };
        if definitions.is_empty() {
            *state = SubscriberState::Started {
                channels: Vec::new(),
                workers: Vec::new(),
            };
            return;
        }

        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => runtime,
            Err(error) => {
                drop(state);
                panic!("SubscriberSet::start requires an active Tokio runtime: {error}");
            }
        };
        let definitions = match std::mem::replace(&mut *state, SubscriberState::Failed) {
            SubscriberState::Pending(definitions) => definitions,
            _ => unreachable!("startup owns the validated pending subscriber state"),
        };
        // Declaration order is deliberate. On partial startup unwind,
        // channels must drop before worker controls so a worker blocked in
        // `blocking_recv` wakes before `SubscriberWorker::drop` takes its
        // receiver.
        let mut workers = Vec::with_capacity(definitions.len());
        let mut channels = Vec::with_capacity(definitions.len());

        for (index, definition) in definitions.into_iter().enumerate() {
            let SubscriberDefinition {
                name,
                capacity,
                ownership,
            } = definition;
            let (sender, receiver) = mpsc::channel::<Arc<Event>>(capacity);
            let receiver = Arc::new(std::sync::Mutex::new(Some(receiver)));
            let overflow = Arc::new(SubscriberOverflowState::default());
            let stop = Arc::new(AtomicBool::new(false));
            let finished = Arc::new(AtomicBool::new(false));
            let (done, done_rx) = oneshot::channel();
            let worker = SubscriberThread {
                ownership,
                receiver: Arc::clone(&receiver),
                name: Arc::clone(&name),
                bus: self.bus.clone(),
                overflow: Arc::clone(&overflow),
                stop: Arc::clone(&stop),
                finished: Arc::clone(&finished),
                runtime: runtime.clone(),
                done,
            };
            let thread = spawn_worker(index, worker)
                .unwrap_or_else(|error| panic!("failed to start subscriber worker: {error}"));

            channels.push(SubscriberChannel {
                name,
                sender,
                overflow,
                closed_reported: AtomicBool::new(false),
            });
            workers.push(SubscriberWorker {
                stop,
                receiver,
                finished,
                done: done_rx,
                thread: Some(thread),
            });
        }

        *state = SubscriberState::Started { channels, workers };
    }

    /// Sends an event to all subscriber queues.
    ///
    /// This method does not wait for subscribers.
    /// It tries once to enqueue the event for each subscriber, then returns.
    pub(crate) fn emit_arc(&self, event: Arc<Event>) {
        let is_internal_event = event.is_internal_diagnostic();
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let SubscriberState::Started { channels, .. } = &*state else {
            return;
        };

        for channel in channels {
            match channel.sender.try_send(Arc::clone(&event)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if !is_internal_event {
                        channel.overflow.record_drop();
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    if !is_internal_event && !channel.closed_reported.swap(true, Ordering::AcqRel) {
                        self.bus.publish_lazy(|| {
                            Event::subscriber_overflow(Arc::clone(&channel.name), "closed")
                        });
                    }
                }
            }
        }
    }

    /// Closes subscriber queues and waits for workers until the shared shutdown deadline.
    ///
    /// Safe to call more than once.
    /// Later calls are no-ops.
    pub(crate) async fn close(&self) {
        let mut workers = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match std::mem::replace(&mut *state, SubscriberState::Closed) {
                SubscriberState::Pending(_) | SubscriberState::Failed | SubscriberState::Closed => {
                    Vec::new()
                }
                SubscriberState::Started { channels, workers } => {
                    drop(channels);
                    workers
                }
            }
        };

        if workers.is_empty() {
            return;
        }

        let drained = if self.shutdown_timeout.is_zero() {
            false
        } else {
            tokio::time::timeout(self.shutdown_timeout, async {
                for worker in &mut workers {
                    let _ = (&mut worker.done).await;
                }
            })
            .await
            .is_ok()
        };

        if !drained {
            for worker in &workers {
                worker.stop.store(true, Ordering::Release);
            }
            let mut abandoned_receivers = Vec::with_capacity(workers.len());
            for worker in &workers {
                if let Some(receiver) = worker.take_receiver() {
                    abandoned_receivers.push(receiver);
                }
            }
            // Drop every queued event after all receiver mutexes have been
            // released. A currently running user callback remains detached on
            // its dedicated worker.
            drop(abandoned_receivers);
        }

        for mut worker in workers {
            if worker.finished.load(Ordering::Acquire)
                && let Some(thread) = worker.thread.take()
            {
                let _ = thread.join();
            }
        }
    }
}

/// Destroys a caught panic payload only on that subscriber's dedicated thread.
///
/// A blocking payload destructor keeps the worker and its process-wide
/// ownership reservation alive, so it cannot extend the public shutdown
/// deadline or become uncharged. If destruction panics again, the nested
/// payload is retained under that reservation and the shared ownership budget
/// fails closed when the worker submits its poisoned cleanup bundle.
fn destroy_worker_panic_payload(
    payload: Box<dyn std::any::Any + Send>,
    cleanup: &mut DropBundle,
) -> bool {
    if let Err(nested) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(nested);
        cleanup.poison();
        return false;
    }
    true
}

/// Runs one subscriber serially without creating a blocking-pool task per event.
fn run_subscriber_worker(
    receiver: SharedSubscriberReceiver,
    subscriber: Arc<dyn Subscribe>,
    name: Arc<str>,
    bus: Bus,
    overflow: Arc<SubscriberOverflowState>,
    stop: Arc<AtomicBool>,
    cleanup: &mut DropBundle,
) {
    while let Some(event) = blocking_recv(&receiver) {
        if stop.load(Ordering::Acquire) {
            break;
        }
        if !invoke_subscriber(&subscriber, event.as_ref(), &name, &bus, cleanup) {
            return;
        }
        if stop.load(Ordering::Acquire) {
            break;
        }
        let Some(receiver_is_empty) = receiver_is_empty(&receiver) else {
            break;
        };
        if receiver_is_empty && !report_overflow(&subscriber, &name, &bus, &overflow, cleanup) {
            return;
        }
    }

    if !stop.load(Ordering::Acquire) {
        let _ = report_overflow(&subscriber, &name, &bus, &overflow, cleanup);
    }
}

/// Receives one event while holding exclusive access to the externally
/// detachable receiver. The mutex is released before any subscriber callback.
fn blocking_recv(receiver: &SharedSubscriberReceiver) -> Option<Arc<Event>> {
    receiver
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_mut()?
        .blocking_recv()
}

fn receiver_is_empty(receiver: &SharedSubscriberReceiver) -> Option<bool> {
    receiver
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(mpsc::Receiver::is_empty)
}

/// Delivers one coalesced overflow report directly to the affected subscriber.
fn report_overflow(
    subscriber: &Arc<dyn Subscribe>,
    name: &Arc<str>,
    bus: &Bus,
    overflow: &SubscriberOverflowState,
    cleanup: &mut DropBundle,
) -> bool {
    let dropped = overflow.take_dropped();
    if dropped == 0 {
        return true;
    }
    let event = Event::subscriber_overflow(Arc::clone(name), "full").with_dropped(dropped);
    invoke_subscriber(subscriber, &event, name, bus, cleanup)
}

/// Calls user subscriber code behind its panic boundary.
///
/// Returns `false` when destroying a caught panic payload itself panics. That
/// worker then stops permanently, preventing one hostile subscriber from
/// accumulating a new retained nested panic payload for every later event.
fn invoke_subscriber(
    subscriber: &Arc<dyn Subscribe>,
    event: &Event,
    name: &Arc<str>,
    bus: &Bus,
    cleanup: &mut DropBundle,
) -> bool {
    let is_internal_event = event.is_internal_diagnostic();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        subscriber.on_event(event);
    }));
    if let Err(panic_err) = result {
        let message = extract_panic_info(&panic_err);
        // This callback already runs on its own subscriber OS thread. Keep a
        // blocking destructor there instead of consuming global destructor
        // isolation capacity; shutdown detaches a worker at its configured
        // subscriber deadline. Contain a destructor panic separately because
        // its panic payload may itself have a hostile destructor.
        let cleanup_succeeded = destroy_worker_panic_payload(panic_err, cleanup);
        if !is_internal_event {
            bus.publish_lazy(|| Event::subscriber_panicked(Arc::clone(name), message));
        }
        return cleanup_succeeded;
    }
    true
}

/// Extracts a readable message from a panic payload.
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
    use std::num::NonZeroUsize;
    use std::sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc as std_mpsc,
    };
    use std::time::Duration;
    use tokio::sync::broadcast;

    fn ev(task: &str) -> Arc<Event> {
        Arc::new(Event::new(EventKind::AttemptStarting).with_task(task))
    }

    fn kind_ev(kind: EventKind) -> Arc<Event> {
        Arc::new(Event::new(kind).with_task("t"))
    }

    fn count(rx: &mut broadcast::Receiver<Arc<Event>>, kind: EventKind) -> usize {
        let mut n = 0;
        while let Ok(e) = rx.try_recv() {
            if e.kind == kind {
                n += 1;
            }
        }
        n
    }

    fn first(rx: &mut broadcast::Receiver<Arc<Event>>, kind: EventKind) -> Option<Arc<Event>> {
        while let Ok(e) = rx.try_recv() {
            if e.kind == kind {
                return Some(e);
            }
        }
        None
    }

    fn wait_for_test_reservations(
        source: &crate::core::deferred_drop::TestReservationSource,
        count: usize,
    ) -> Vec<crate::core::deferred_drop::DropReservation> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(reservations) = source.try_reserve_many(count) {
                return reservations;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "clean subscriber ownership was not released"
            );
            std::thread::yield_now();
        }
    }

    struct CountingSub {
        count: Arc<AtomicU64>,
        capacity: NonZeroUsize,
    }

    impl CountingSub {
        fn new(capacity: usize) -> (Arc<AtomicU64>, Arc<Self>) {
            let count = Arc::new(AtomicU64::new(0));
            let sub = Arc::new(Self {
                count: Arc::clone(&count),
                capacity: NonZeroUsize::new(capacity)
                    .expect("test subscriber capacity must be non-zero"),
            });
            (count, sub)
        }
    }

    impl Subscribe for CountingSub {
        fn on_event(&self, _event: &Event) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        fn name(&self) -> &str {
            "counting"
        }
        fn queue_capacity(&self) -> NonZeroUsize {
            self.capacity
        }
    }

    #[test]
    fn construction_with_a_subscriber_does_not_require_tokio() {
        let (count, subscriber) = CountingSub::new(8);
        let set = SubscriberSet::new_with_shutdown_timeout(
            vec![subscriber],
            Bus::new(8),
            Duration::from_secs(1),
        );

        let state = set.state.lock().unwrap_or_else(|e| e.into_inner());
        let SubscriberState::Pending(definitions) = &*state else {
            panic!("subscriber set must remain pending until start")
        };
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].capacity, 8);
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn metadata_panic_keeps_current_and_unvisited_subscribers_isolated() {
        for panic_behavior in [MetadataBehavior::PanicCapacity, MetadataBehavior::PanicName] {
            let source = crate::core::deferred_drop::TestReservationSource::new(2);
            let caller = std::thread::current().id();
            let (dropped_on, drops) = std_mpsc::channel();
            let subscribers: Vec<Arc<dyn Subscribe>> = vec![
                Arc::new(MetadataOwnershipProbe {
                    behavior: panic_behavior,
                    dropped_on: Some(dropped_on.clone()),
                }),
                Arc::new(MetadataOwnershipProbe {
                    behavior: MetadataBehavior::Ready,
                    dropped_on: Some(dropped_on),
                }),
            ];

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = SubscriberSet::from_test_source(
                    subscribers,
                    Bus::new(8),
                    Duration::from_secs(1),
                    &source,
                );
            }));

            assert!(result.is_err(), "subscriber metadata panic must propagate");
            for _ in 0..2 {
                let destructor_thread = drops
                    .recv_timeout(Duration::from_secs(2))
                    .expect("every reserved subscriber must reach destructor isolation");
                assert_ne!(
                    destructor_thread, caller,
                    "neither the current nor an unvisited subscriber may Drop on the metadata caller"
                );
            }
            drop(wait_for_test_reservations(&source, 2));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn metadata_panic_payload_stays_under_its_subscriber_reservation() {
        let source = crate::core::deferred_drop::TestReservationSource::new(1);
        let gate = Arc::new((Mutex::new(BlockingGateState::default()), Condvar::new()));
        let watchdog = spawn_gate_watchdog(Arc::clone(&gate));
        let subscriber: Arc<dyn Subscribe> = Arc::new(MetadataOwnershipProbe {
            behavior: MetadataBehavior::PanicCapacityPayload(Arc::clone(&gate)),
            dropped_on: None,
        });

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = SubscriberSet::from_test_source(
                vec![subscriber],
                Bus::new(8),
                Duration::from_secs(1),
                &source,
            );
        }));
        let payload_isolated = wait_for_gate(&gate, |state| state.entered).await;
        let ownership_still_charged = source.try_reserve().is_err();

        release_gate(&gate);
        let payload_finished = wait_for_gate(&gate, |state| state.finished).await;
        let released = tokio::time::timeout(Duration::from_secs(1), source.reserve())
            .await
            .expect("released metadata panic payload must return ownership")
            .expect("clean panic-payload destruction keeps admission open");
        drop(released);
        watchdog.join().expect("metadata watchdog must not panic");

        assert!(
            result.is_err(),
            "metadata panic must propagate to its caller"
        );
        assert!(
            payload_isolated,
            "the original panic payload must Drop in isolation"
        );
        assert!(
            ownership_still_charged,
            "the isolated payload must retain its slot"
        );
        assert!(payload_finished, "released payload destructor must finish");
    }

    #[test]
    fn blocking_metadata_holds_the_complete_atomic_batch() {
        let source = crate::core::deferred_drop::TestReservationSource::new(2);
        let gate = Arc::new((Mutex::new(BlockingGateState::default()), Condvar::new()));
        let subscribers: Vec<Arc<dyn Subscribe>> = vec![
            Arc::new(MetadataOwnershipProbe {
                behavior: MetadataBehavior::BlockCapacity(Arc::clone(&gate)),
                dropped_on: None,
            }),
            Arc::new(MetadataOwnershipProbe {
                behavior: MetadataBehavior::Ready,
                dropped_on: None,
            }),
        ];
        let source_for_builder = source.clone();
        let build_thread = std::thread::spawn(move || {
            SubscriberSet::from_test_source(
                subscribers,
                Bus::new(8),
                Duration::from_secs(1),
                &source_for_builder,
            )
            .expect("the isolated budget fits the complete batch")
        });

        let (state, ready) = &*gate;
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        while !state.entered {
            state = ready.wait(state).unwrap_or_else(|error| error.into_inner());
        }
        drop(state);
        assert!(
            source.try_reserve().is_err(),
            "the second subscriber slot must already be charged while first metadata blocks"
        );

        release_gate(&gate);
        let set = build_thread
            .join()
            .expect("metadata builder must not panic");
        drop(set);
        drop(wait_for_test_reservations(&source, 2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn partial_thread_spawn_failure_preserves_every_subscriber_reservation() {
        let source = crate::core::deferred_drop::TestReservationSource::new(3);
        let caller = std::thread::current().id();
        let (dropped_on, drops) = std_mpsc::channel();
        let subscribers: Vec<Arc<dyn Subscribe>> = (0..3)
            .map(|_| {
                Arc::new(MetadataOwnershipProbe {
                    behavior: MetadataBehavior::Ready,
                    dropped_on: Some(dropped_on.clone()),
                }) as Arc<dyn Subscribe>
            })
            .collect();
        drop(dropped_on);
        let set = SubscriberSet::from_test_source(
            subscribers,
            Bus::new(8),
            Duration::from_secs(1),
            &source,
        )
        .expect("the isolated budget fits every subscriber");

        let start_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            set.start_with(|index, worker| {
                if index == 1 {
                    return Err(std::io::Error::other("injected subscriber spawn failure"));
                }
                std::thread::Builder::new().spawn(move || worker.run())
            });
        }));
        assert!(
            start_result.is_err(),
            "the injected spawn failure must propagate"
        );
        let retry = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| set.start()));
        assert!(
            retry.is_err(),
            "a failed partial startup cannot retry as a silent empty subscriber set"
        );
        drop(set);

        for _ in 0..3 {
            let destructor_thread = drops
                .recv_timeout(Duration::from_secs(2))
                .expect("started, rejected, and unvisited subscribers must all clean up");
            assert_ne!(
                destructor_thread, caller,
                "spawn failure cannot destroy a final subscriber on its caller"
            );
        }
        drop(wait_for_test_reservations(&source, 3));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_is_idempotent_and_close_drains_delivery() {
        let (count, subscriber) = CountingSub::new(8);
        let set = SubscriberSet::new(vec![subscriber], Bus::new(8));

        set.start();
        set.start();
        for _ in 0..3 {
            set.emit_arc(ev("started"));
        }
        tokio::time::timeout(Duration::from_secs(1), set.close())
            .await
            .expect("close must drain started subscriber workers");

        assert_eq!(count.load(Ordering::Relaxed), 3);
        set.start();
        set.close().await;
        assert!(matches!(
            *set.state.lock().unwrap_or_else(|e| e.into_inner()),
            SubscriberState::Closed
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn normal_worker_exit_releases_subscriber_ownership() {
        let source = crate::core::deferred_drop::TestReservationSource::new(1);
        let (_count, subscriber) = CountingSub::new(8);
        let set = SubscriberSet::from_test_source(
            vec![subscriber],
            Bus::new(8),
            Duration::from_secs(1),
            &source,
        )
        .expect("the isolated budget has one subscriber slot");

        set.start();
        set.close().await;

        let released = tokio::time::timeout(Duration::from_secs(1), source.reserve())
            .await
            .expect("clean physical worker exit must release ownership")
            .expect("clean subscriber destruction keeps admission open");
        drop(released);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_never_started_set_isolates_final_subscriber_destructor() {
        let source = crate::core::deferred_drop::TestReservationSource::new(1);
        let gate = Arc::new((Mutex::new(BlockingGateState::default()), Condvar::new()));
        let subscriber: Arc<dyn Subscribe> = Arc::new(BlockingFinalSubscriberDrop {
            gate: Arc::clone(&gate),
        });
        let set = SubscriberSet::from_test_source(
            vec![subscriber],
            Bus::new(8),
            Duration::from_secs(1),
            &source,
        )
        .expect("the isolated budget has one subscriber slot");
        let (drop_done, dropped) = oneshot::channel();
        let drop_thread = std::thread::spawn(move || {
            drop(set);
            let _ = drop_done.send(());
        });

        let caller_returned = tokio::time::timeout(Duration::from_millis(500), dropped)
            .await
            .is_ok();
        let destructor_isolated = wait_for_gate(&gate, |state| state.entered).await;
        let ownership_still_charged = source.try_reserve().is_err();

        release_gate(&gate);
        let destructor_finished = wait_for_gate(&gate, |state| state.finished).await;
        let released = tokio::time::timeout(Duration::from_secs(1), source.reserve())
            .await
            .expect("released destructor must return subscriber ownership")
            .expect("clean subscriber destruction keeps admission open");
        drop(released);
        drop_thread.join().expect("drop caller must not panic");

        assert!(
            caller_returned,
            "pending-set Drop cannot run user Drop inline"
        );
        assert!(
            destructor_isolated,
            "the final Drop must reach its isolated worker"
        );
        assert!(
            ownership_still_charged,
            "blocking Drop must retain its slot"
        );
        assert!(destructor_finished, "released final Drop must finish");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_before_start_prevents_late_start_and_delivery() {
        let (count, subscriber) = CountingSub::new(8);
        let set = SubscriberSet::new(vec![subscriber], Bus::new(8));

        set.close().await;
        set.start();
        set.emit_arc(ev("after-close"));
        set.close().await;

        assert_eq!(count.load(Ordering::Relaxed), 0);
        assert!(matches!(
            *set.state.lock().unwrap_or_else(|e| e.into_inner()),
            SubscriberState::Closed
        ));
    }

    struct PanicSub {
        name: String,
    }

    impl PanicSub {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                name: "panicking".to_string(),
            })
        }
        fn named(name: &str) -> Arc<Self> {
            Arc::new(Self {
                name: name.to_string(),
            })
        }
    }

    impl Subscribe for PanicSub {
        fn on_event(&self, _event: &Event) {
            panic!("boom");
        }
        fn name(&self) -> &str {
            &self.name
        }
        fn queue_capacity(&self) -> NonZeroUsize {
            NonZeroUsize::new(16).unwrap()
        }
    }

    struct PanicPayloadWithPanickingDrop;

    impl Drop for PanicPayloadWithPanickingDrop {
        fn drop(&mut self) {
            panic!("nested panic while destroying subscriber panic payload");
        }
    }

    struct NestedDropPanicSub {
        calls: Arc<AtomicUsize>,
    }

    impl Subscribe for NestedDropPanicSub {
        fn on_event(&self, _event: &Event) {
            self.calls.fetch_add(1, Ordering::AcqRel);
            std::panic::panic_any(PanicPayloadWithPanickingDrop);
        }

        fn name(&self) -> &str {
            "nested-drop-panic"
        }

        fn queue_capacity(&self) -> NonZeroUsize {
            NonZeroUsize::new(16).expect("test capacity is non-zero")
        }
    }

    struct RecordingSub {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Subscribe for RecordingSub {
        fn on_event(&self, e: &Event) {
            if let Some(t) = e.task.as_deref() {
                self.seen.lock().unwrap().push(t.to_string());
            }
        }
        fn name(&self) -> &str {
            "recorder"
        }
        fn queue_capacity(&self) -> NonZeroUsize {
            NonZeroUsize::new(64).unwrap()
        }
    }

    #[derive(Default)]
    struct BlockingGateState {
        entered: bool,
        released: bool,
        finished: bool,
        watchdog_fired: bool,
    }

    type BlockingGate = Arc<(Mutex<BlockingGateState>, Condvar)>;

    struct BlockingOrderSub {
        first_gate: BlockingGate,
        second_entered: AtomicBool,
        active: AtomicUsize,
        max_active: AtomicUsize,
        seen: Mutex<Vec<String>>,
        overflow_reports: Mutex<Vec<(String, u64)>>,
    }

    impl Subscribe for BlockingOrderSub {
        fn on_event(&self, event: &Event) {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);

            if event.kind == EventKind::SubscriberOverflow {
                if let Some(reason) = event.reason.as_deref() {
                    self.overflow_reports
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push((reason.to_owned(), event.dropped.unwrap_or(0)));
                }
                self.active.fetch_sub(1, Ordering::SeqCst);
                return;
            }

            let task = event.task.as_deref().unwrap_or_default();
            self.seen.lock().unwrap().push(task.to_owned());
            match task {
                "first" => {
                    let (state, ready) = &*self.first_gate;
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    state.entered = true;
                    ready.notify_all();
                    while !state.released {
                        state = ready.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                    state.finished = true;
                    ready.notify_all();
                }
                "second" => self.second_entered.store(true, Ordering::Release),
                _ => {}
            }

            self.active.fetch_sub(1, Ordering::SeqCst);
        }

        fn name(&self) -> &str {
            "blocking-order"
        }

        fn queue_capacity(&self) -> NonZeroUsize {
            NonZeroUsize::new(4).unwrap()
        }
    }

    fn blocking_order_sub() -> (Arc<BlockingOrderSub>, BlockingGate) {
        let first_gate = Arc::new((Mutex::new(BlockingGateState::default()), Condvar::new()));
        let sub = Arc::new(BlockingOrderSub {
            first_gate: Arc::clone(&first_gate),
            second_entered: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            overflow_reports: Mutex::new(Vec::new()),
        });
        (sub, first_gate)
    }

    fn spawn_gate_watchdog(gate: BlockingGate) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let (state, ready) = &*gate;
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            while !state.entered && !state.released {
                state = ready.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            if state.released {
                return;
            }

            let (mut state, _) = ready
                .wait_timeout_while(state, Duration::from_secs(2), |state| !state.released)
                .unwrap_or_else(|e| e.into_inner());
            if !state.released {
                state.watchdog_fired = true;
                state.released = true;
                ready.notify_all();
            }
        })
    }

    fn release_gate(gate: &BlockingGate) {
        let (state, ready) = &**gate;
        state.lock().unwrap_or_else(|e| e.into_inner()).released = true;
        ready.notify_all();
    }

    struct BlockingPanicPayload(BlockingGate);

    impl Drop for BlockingPanicPayload {
        fn drop(&mut self) {
            let (state, ready) = &*self.0;
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            state.entered = true;
            ready.notify_all();
            while !state.released {
                state = ready.wait(state).unwrap_or_else(|error| error.into_inner());
            }
            state.finished = true;
            ready.notify_all();
        }
    }

    struct PanickingFinalSubscriberDrop {
        gate: BlockingGate,
    }

    impl Subscribe for PanickingFinalSubscriberDrop {
        fn on_event(&self, _event: &Event) {}

        fn name(&self) -> &str {
            "panicking-final-drop"
        }
    }

    impl Drop for PanickingFinalSubscriberDrop {
        fn drop(&mut self) {
            std::panic::panic_any(BlockingPanicPayload(Arc::clone(&self.gate)));
        }
    }

    struct BlockingFinalSubscriberDrop {
        gate: BlockingGate,
    }

    impl Subscribe for BlockingFinalSubscriberDrop {
        fn on_event(&self, _event: &Event) {}

        fn name(&self) -> &str {
            "blocking-final-drop"
        }
    }

    impl Drop for BlockingFinalSubscriberDrop {
        fn drop(&mut self) {
            let (state, ready) = &*self.gate;
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            state.entered = true;
            ready.notify_all();
            while !state.released {
                state = ready.wait(state).unwrap_or_else(|error| error.into_inner());
            }
            state.finished = true;
            ready.notify_all();
        }
    }

    enum MetadataBehavior {
        Ready,
        PanicName,
        PanicCapacity,
        PanicCapacityPayload(BlockingGate),
        BlockCapacity(BlockingGate),
    }

    struct MetadataOwnershipProbe {
        behavior: MetadataBehavior,
        dropped_on: Option<std_mpsc::Sender<std::thread::ThreadId>>,
    }

    impl Subscribe for MetadataOwnershipProbe {
        fn on_event(&self, _event: &Event) {}

        fn name(&self) -> &str {
            if matches!(&self.behavior, MetadataBehavior::PanicName) {
                panic!("subscriber name panic")
            }
            "metadata-ownership-probe"
        }

        fn queue_capacity(&self) -> NonZeroUsize {
            match &self.behavior {
                MetadataBehavior::PanicCapacity => panic!("subscriber capacity panic"),
                MetadataBehavior::PanicCapacityPayload(gate) => {
                    std::panic::panic_any(BlockingPanicPayload(Arc::clone(gate)))
                }
                MetadataBehavior::BlockCapacity(gate) => {
                    let (state, ready) = &**gate;
                    let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                    state.entered = true;
                    ready.notify_all();
                    while !state.released {
                        state = ready.wait(state).unwrap_or_else(|error| error.into_inner());
                    }
                }
                MetadataBehavior::Ready | MetadataBehavior::PanicName => {}
            }
            NonZeroUsize::new(8).expect("test capacity is non-zero")
        }
    }

    impl Drop for MetadataOwnershipProbe {
        fn drop(&mut self) {
            if let Some(dropped_on) = &self.dropped_on {
                let _ = dropped_on.send(std::thread::current().id());
            }
        }
    }

    async fn wait_for_gate(
        gate: &BlockingGate,
        predicate: impl Fn(&BlockingGateState) -> bool,
    ) -> bool {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let matches = {
                    let state = gate.0.lock().unwrap_or_else(|e| e.into_inner());
                    predicate(&state)
                };
                if matches {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_callback_keeps_runtime_responsive_and_close_joins_it() {
        let (sub, first_gate) = blocking_order_sub();
        let set = Arc::new(SubscriberSet::new(
            vec![Arc::clone(&sub) as Arc<dyn Subscribe>],
            Bus::new(64),
        ));
        set.start();

        let watchdog = spawn_gate_watchdog(Arc::clone(&first_gate));

        set.emit_arc(ev("first"));
        set.emit_arc(ev("second"));

        let close_set = Arc::clone(&set);
        let close_started = Arc::new(AtomicBool::new(false));
        let close_started_for_task = Arc::clone(&close_started);
        let close_task = tokio::spawn(async move {
            close_started_for_task.store(true, Ordering::Release);
            close_set.close().await;
        });

        let responsive = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let entered = first_gate
                    .0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entered;
                if entered && close_started.load(Ordering::Acquire) {
                    break;
                }
                tokio::task::yield_now().await;
            }

            tokio::time::sleep(Duration::from_millis(20)).await;
            let state = first_gate.0.lock().unwrap_or_else(|e| e.into_inner());
            !state.released
                && !state.finished
                && !state.watchdog_fired
                && !sub.second_entered.load(Ordering::Acquire)
                && !close_task.is_finished()
        })
        .await;

        release_gate(&first_gate);

        tokio::time::timeout(Duration::from_secs(5), close_task)
            .await
            .expect("close must finish after the callback is released")
            .expect("close task must not panic");
        watchdog.join().expect("watchdog thread must not panic");

        assert!(
            matches!(responsive, Ok(true)),
            "Tokio timers must run while a subscriber callback blocks; callbacks must stay serial and close must still wait"
        );
        assert_eq!(sub.max_active.load(Ordering::SeqCst), 1);
        assert_eq!(
            *sub.seen.lock().unwrap_or_else(|e| e.into_inner()),
            ["first", "second"]
        );
        let state = first_gate.0.lock().unwrap_or_else(|e| e.into_inner());
        assert!(state.finished);
        assert!(!state.watchdog_fired);
        assert!(sub.second_entered.load(Ordering::Acquire));
    }

    #[test]
    fn callbacks_do_not_use_tokio_blocking_pool() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("the test runtime must build");

        runtime.block_on(async {
            let pool_gate = Arc::new((Mutex::new(BlockingGateState::default()), Condvar::new()));
            let pool_gate_for_blocker = Arc::clone(&pool_gate);
            let blocker = tokio::task::spawn_blocking(move || {
                let (state, ready) = &*pool_gate_for_blocker;
                let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                state.entered = true;
                ready.notify_all();
                while !state.released {
                    state = ready.wait(state).unwrap_or_else(|error| error.into_inner());
                }
                state.finished = true;
                ready.notify_all();
            });
            let blocking_pool_is_occupied = wait_for_gate(&pool_gate, |state| state.entered).await;

            let (count, subscriber) = CountingSub::new(8);
            let set = SubscriberSet::new(vec![subscriber], Bus::new(8));
            set.start();
            set.emit_arc(ev("dedicated-thread"));
            let callback_ran_while_pool_was_occupied =
                tokio::time::timeout(Duration::from_secs(1), async {
                    while count.load(Ordering::Acquire) == 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .is_ok();

            release_gate(&pool_gate);
            let _ = blocker.await;
            set.close().await;

            assert!(blocking_pool_is_occupied);
            assert!(
                callback_ran_while_pool_was_occupied,
                "subscriber callbacks must use their dedicated worker, not Tokio's blocking pool"
            );
        });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn zero_shutdown_timeout_detaches_worker_and_drops_queued_events() {
        let source = crate::core::deferred_drop::TestReservationSource::new(1);
        let (sub, first_gate) = blocking_order_sub();
        let set = SubscriberSet::from_test_source(
            vec![Arc::clone(&sub) as Arc<dyn Subscribe>],
            Bus::new(64),
            Duration::ZERO,
            &source,
        )
        .expect("the isolated budget has one subscriber slot");
        set.start();
        let worker_finished = {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { workers, .. } = &*state else {
                panic!("subscriber worker must be started")
            };
            Arc::clone(
                &workers
                    .first()
                    .expect("the test configures one subscriber worker")
                    .finished,
            )
        };
        let watchdog = spawn_gate_watchdog(Arc::clone(&first_gate));

        set.emit_arc(ev("first"));
        let queued_event = ev("second");
        let queued_event_drop_probe = Arc::downgrade(&queued_event);
        set.emit_arc(queued_event);
        let first_entered = wait_for_gate(&first_gate, |state| state.entered).await;

        let close_result = tokio::time::timeout(Duration::from_secs(1), set.close()).await;
        let worker_was_still_running = !worker_finished.load(Ordering::Acquire);
        let first_was_still_running = !first_gate
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finished;
        let second_was_waiting = !sub.second_entered.load(Ordering::Acquire);
        let queued_event_was_dropped = queued_event_drop_probe.upgrade().is_none();
        let ownership_stayed_charged = source.try_reserve().is_err();

        release_gate(&first_gate);
        let first_finished = wait_for_gate(&first_gate, |state| state.finished).await;
        let worker_stopped_after_release = tokio::time::timeout(Duration::from_secs(1), async {
            while !worker_finished.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        watchdog.join().expect("watchdog thread must not panic");
        let seen = sub
            .seen
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let repeated_close = tokio::time::timeout(Duration::from_secs(1), set.close()).await;
        let released = tokio::time::timeout(Duration::from_secs(1), source.reserve())
            .await
            .expect("physical worker exit must release subscriber ownership")
            .expect("clean subscriber destruction keeps admission open");
        drop(released);

        assert!(first_entered, "the first callback must start before close");
        assert!(
            close_result.is_ok(),
            "zero subscriber shutdown timeout must return immediately"
        );
        assert!(
            worker_was_still_running,
            "close must detach a callback that outlives the zero deadline"
        );
        assert!(
            first_was_still_running,
            "close cannot stop an already-running blocking callback"
        );
        assert!(
            second_was_waiting,
            "the second callback must still be queued"
        );
        assert!(
            queued_event_was_dropped,
            "the detached receiver must release queued events before close returns"
        );
        assert!(
            ownership_stayed_charged,
            "detached callback ownership must remain charged until physical return"
        );
        assert!(first_finished, "cleanup must release the running callback");
        assert!(
            worker_stopped_after_release,
            "the detached worker must stop after its running callback returns"
        );
        assert_eq!(
            seen,
            ["first"],
            "releasing the detached worker cannot revive queued callbacks"
        );
        assert!(!sub.second_entered.load(Ordering::Acquire));
        assert!(repeated_close.is_ok(), "repeated close must remain a no-op");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn final_subscriber_drop_panic_payload_cannot_extend_close_deadline() {
        let gate = Arc::new((Mutex::new(BlockingGateState::default()), Condvar::new()));
        let source = crate::core::deferred_drop::TestReservationSource::new(1);
        let subscriber: Arc<dyn Subscribe> = Arc::new(PanickingFinalSubscriberDrop {
            gate: Arc::clone(&gate),
        });
        let set = SubscriberSet::from_test_source(
            vec![subscriber],
            Bus::new(8),
            Duration::from_millis(25),
            &source,
        )
        .expect("the isolated budget has one subscriber slot");
        set.start();

        let close_result = tokio::time::timeout(Duration::from_millis(500), set.close()).await;
        let payload_destructor_ran = gate
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entered;
        let later_reservation = tokio::time::timeout(Duration::from_secs(1), source.reserve())
            .await
            .expect("poisoned cleanup must resolve ownership admission");

        assert!(
            close_result.is_ok(),
            "a subscriber destructor panic payload cannot extend close"
        );
        assert!(
            !payload_destructor_ran,
            "the globally charged executor must retain, not destroy, a hostile destructor panic payload"
        );
        assert!(
            later_reservation.is_err(),
            "a subscriber destructor panic must fail-close its isolated ownership budget"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subscriber_workers_share_one_shutdown_deadline() {
        let mut subscribers = Vec::<Arc<dyn Subscribe>>::new();
        let mut gates = Vec::new();
        let mut watchdogs = Vec::new();
        for _ in 0..3 {
            let (sub, gate) = blocking_order_sub();
            subscribers.push(sub);
            watchdogs.push(spawn_gate_watchdog(Arc::clone(&gate)));
            gates.push(gate);
        }

        let set = SubscriberSet::new_with_shutdown_timeout(
            subscribers,
            Bus::new(64),
            Duration::from_millis(200),
        );
        set.start();
        set.emit_arc(ev("first"));
        let all_entered = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ready = gates
                    .iter()
                    .all(|gate| gate.0.lock().unwrap_or_else(|e| e.into_inner()).entered);
                if ready {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();

        let close_result = tokio::time::timeout(Duration::from_millis(450), set.close()).await;
        let callbacks_were_still_running = gates
            .iter()
            .all(|gate| !gate.0.lock().unwrap_or_else(|e| e.into_inner()).finished);

        for gate in &gates {
            release_gate(gate);
        }
        let mut all_finished = true;
        for gate in &gates {
            all_finished &= wait_for_gate(gate, |state| state.finished).await;
        }
        for watchdog in watchdogs {
            watchdog.join().expect("watchdog thread must not panic");
        }

        assert!(all_entered, "all callbacks must start before close");
        assert!(
            close_result.is_ok(),
            "all subscriber workers must share one 200 ms deadline"
        );
        assert!(
            callbacks_were_still_running,
            "the deadline must stop waiting, not stop blocking callbacks"
        );
        assert!(all_finished, "cleanup must release every callback");
    }

    #[tokio::test]
    async fn overflow_is_coalesced_and_does_not_reenter_the_shared_bus() {
        let bus = Bus::new(256);
        let mut bus_events = bus.subscribe();
        let (sub, gate) = blocking_order_sub();
        let set = SubscriberSet::new(vec![Arc::clone(&sub) as Arc<dyn Subscribe>], bus.clone());
        set.start();
        set.emit_arc(ev("first"));
        assert!(wait_for_gate(&gate, |state| state.entered).await);

        for _ in 0..4 {
            set.emit_arc(ev("queued"));
        }
        for _ in 0..100 {
            set.emit_arc(ev("dropped"));
        }

        release_gate(&gate);
        set.close().await;

        assert_eq!(
            *sub.overflow_reports
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            [("full".to_owned(), 100)],
            "one recovery report must summarize the complete overflow burst"
        );
        assert_eq!(
            count(&mut bus_events, EventKind::SubscriberOverflow),
            0,
            "queue overflow diagnostics must not reenter the shared event bus"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_internal_diagnostics_do_not_create_an_overflow_report() {
        let (sub, gate) = blocking_order_sub();
        let set = SubscriberSet::new(vec![Arc::clone(&sub) as Arc<dyn Subscribe>], Bus::new(64));
        set.start();
        set.emit_arc(ev("first"));
        assert!(wait_for_gate(&gate, |state| state.entered).await);

        for _ in 0..4 {
            set.emit_arc(kind_ev(EventKind::RuntimeFailure));
        }
        for _ in 0..100 {
            set.emit_arc(kind_ev(EventKind::RuntimeFailure));
        }

        release_gate(&gate);
        set.close().await;
        assert!(
            sub.overflow_reports
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty(),
            "dropping internal diagnostics must remain silent"
        );
    }

    #[tokio::test]
    async fn panic_in_subscriber_publishes_subscriber_panicked_and_continues() {
        let bus = Bus::new(64);
        let mut rx = bus.subscribe();
        let set = SubscriberSet::new(vec![PanicSub::new()], bus.clone());
        set.start();

        for _ in 0..3 {
            set.emit_arc(ev("t"));
        }
        tokio::time::timeout(Duration::from_secs(5), set.close())
            .await
            .expect("subscriber worker must continue after panics and close cleanly");

        assert_eq!(
            count(&mut rx, EventKind::SubscriberPanicked),
            3,
            "each ordinary-event panic must be reported, and the worker must continue"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn panic_payload_destructor_panic_stops_that_subscriber_worker() {
        let source = crate::core::deferred_drop::TestReservationSource::new(1);
        let bus = Bus::new(64);
        let mut events = bus.subscribe();
        let calls = Arc::new(AtomicUsize::new(0));
        let subscriber: Arc<dyn Subscribe> = Arc::new(NestedDropPanicSub {
            calls: Arc::clone(&calls),
        });
        let set =
            SubscriberSet::from_test_source(vec![subscriber], bus, Duration::from_secs(1), &source)
                .expect("the isolated budget has one subscriber slot");
        set.start();

        for _ in 0..3 {
            set.emit_arc(ev("nested-drop"));
        }
        tokio::time::timeout(Duration::from_secs(1), set.close())
            .await
            .expect("the poisoned subscriber worker must terminate");
        let later_reservation = tokio::time::timeout(Duration::from_secs(1), source.reserve())
            .await
            .expect("poisoned cleanup must resolve ownership admission");

        assert_eq!(
            calls.load(Ordering::Acquire),
            1,
            "a nested panic-payload destructor failure must permanently stop that worker"
        );
        assert_eq!(count(&mut events, EventKind::SubscriberPanicked), 1);
        assert!(
            later_reservation.is_err(),
            "a nested panic-payload destructor must fail-close its isolated budget"
        );
    }

    #[tokio::test]
    async fn panic_on_internal_diagnostic_does_not_republish() {
        for diagnostic in [
            EventKind::SubscriberPanicked,
            EventKind::SubscriberOverflow,
            EventKind::RuntimeFailure,
        ] {
            let bus = Bus::new(64);
            let mut rx = bus.subscribe();
            let set = SubscriberSet::new(vec![PanicSub::new()], bus.clone());
            set.start();

            set.emit_arc(kind_ev(diagnostic));
            set.close().await;

            assert_eq!(
                count(&mut rx, EventKind::SubscriberPanicked),
                0,
                "panicking on a {diagnostic:?} event must not republish — that is the feedback loop"
            );
        }
    }

    #[tokio::test]
    async fn dynamic_subscriber_name_surfaces_in_diagnostics() {
        let bus = Bus::new(64);
        let mut rx = bus.subscribe();
        let set = SubscriberSet::new(vec![PanicSub::named("slack-#alerts")], bus.clone());
        set.start();

        set.emit_arc(ev("t"));
        set.close().await;

        let panicked =
            first(&mut rx, EventKind::SubscriberPanicked).expect("the panic must be reported");
        assert_eq!(
            panicked.task.as_deref(),
            Some("slack-#alerts"),
            "a subscriber's dynamic name must surface in the diagnostic event's `task`"
        );
    }

    #[tokio::test]
    async fn panicking_subscriber_does_not_affect_others_and_order_is_fifo() {
        let bus = Bus::new(64);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorder = Arc::new(RecordingSub {
            seen: Arc::clone(&seen),
        });
        let set = SubscriberSet::new(vec![PanicSub::new(), recorder], bus);
        set.start();

        for i in 0..5 {
            set.emit_arc(ev(&format!("e{i}")));
        }
        set.close().await;

        assert_eq!(
            *seen.lock().unwrap(),
            vec!["e0", "e1", "e2", "e3", "e4"],
            "the healthy subscriber must see every event in FIFO order, unaffected by the panicking one"
        );
    }
}
