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
//!
//! ## Panic Handling
//!
//! Worker threads run `on_event` directly inside `catch_unwind`.
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

use crate::events::{Bus, Event};
use crate::subscribers::Subscribe;

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
    finished: Arc<AtomicBool>,
    done: oneshot::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SubscriberWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
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
    subscriber: Arc<dyn Subscribe>,
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

    /// Creates a subscriber set with an explicit shared shutdown timeout.
    #[must_use]
    pub(crate) fn new_with_shutdown_timeout(
        subs: Vec<Arc<dyn Subscribe>>,
        bus: Bus,
        shutdown_timeout: Duration,
    ) -> Self {
        let definitions = subs
            .into_iter()
            .map(|subscriber| SubscriberDefinition {
                capacity: subscriber.queue_capacity().get(),
                name: Arc::from(subscriber.name()),
                subscriber,
            })
            .collect();

        Self {
            state: std::sync::Mutex::new(SubscriberState::Pending(definitions)),
            shutdown_timeout,
            bus,
        }
    }

    /// Starts one dedicated worker thread per subscriber.
    ///
    /// Safe to call more than once. Calls after startup or shutdown are no-ops.
    pub(crate) fn start(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let SubscriberState::Pending(definitions) = &mut *state else {
            return;
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
        let definitions = std::mem::take(definitions);
        let mut channels = Vec::with_capacity(definitions.len());
        let mut workers = Vec::with_capacity(definitions.len());

        for (index, definition) in definitions.into_iter().enumerate() {
            let SubscriberDefinition {
                name,
                capacity,
                subscriber,
            } = definition;
            let (sender, receiver) = mpsc::channel::<Arc<Event>>(capacity);
            let overflow = Arc::new(SubscriberOverflowState::default());
            let stop = Arc::new(AtomicBool::new(false));
            let finished = Arc::new(AtomicBool::new(false));
            let (done, done_rx) = oneshot::channel();
            let name_for_worker = Arc::clone(&name);
            let bus_for_worker = self.bus.clone();
            let overflow_for_worker = Arc::clone(&overflow);
            let stop_for_worker = Arc::clone(&stop);
            let finished_for_worker = Arc::clone(&finished);
            let runtime_for_worker = runtime.clone();
            let thread = std::thread::Builder::new()
                .name(format!("taskvisor-subscriber-{index}"))
                .spawn(move || {
                    let _runtime = runtime_for_worker.enter();
                    let _completion = WorkerCompletion {
                        finished: finished_for_worker,
                        done: Some(done),
                    };
                    run_subscriber_worker(
                        receiver,
                        subscriber,
                        name_for_worker,
                        bus_for_worker,
                        overflow_for_worker,
                        stop_for_worker,
                    );
                })
                .unwrap_or_else(|error| panic!("failed to start subscriber worker: {error}"));

            channels.push(SubscriberChannel {
                name,
                sender,
                overflow,
                closed_reported: AtomicBool::new(false),
            });
            workers.push(SubscriberWorker {
                stop,
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
                        self.bus.publish(Event::subscriber_overflow(
                            Arc::clone(&channel.name),
                            "closed",
                        ));
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
                SubscriberState::Pending(_) | SubscriberState::Closed => Vec::new(),
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

/// Runs one subscriber serially without creating a blocking-pool task per event.
fn run_subscriber_worker(
    mut receiver: mpsc::Receiver<Arc<Event>>,
    subscriber: Arc<dyn Subscribe>,
    name: Arc<str>,
    bus: Bus,
    overflow: Arc<SubscriberOverflowState>,
    stop: Arc<AtomicBool>,
) {
    while let Some(event) = receiver.blocking_recv() {
        if stop.load(Ordering::Acquire) {
            break;
        }
        invoke_subscriber(&subscriber, event.as_ref(), &name, &bus);
        if stop.load(Ordering::Acquire) {
            break;
        }
        if receiver.is_empty() {
            report_overflow(&subscriber, &name, &bus, &overflow);
        }
    }

    if !stop.load(Ordering::Acquire) {
        report_overflow(&subscriber, &name, &bus, &overflow);
    }
}

/// Delivers one coalesced overflow report directly to the affected subscriber.
fn report_overflow(
    subscriber: &Arc<dyn Subscribe>,
    name: &Arc<str>,
    bus: &Bus,
    overflow: &SubscriberOverflowState,
) {
    let dropped = overflow.take_dropped();
    if dropped == 0 {
        return;
    }
    let event = Event::subscriber_overflow(Arc::clone(name), "full").with_dropped(dropped);
    invoke_subscriber(subscriber, &event, name, bus);
}

/// Calls user subscriber code behind its panic boundary.
fn invoke_subscriber(subscriber: &Arc<dyn Subscribe>, event: &Event, name: &Arc<str>, bus: &Bus) {
    let is_internal_event = event.is_internal_diagnostic();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        subscriber.on_event(event);
    }));
    if let Err(panic_err) = result
        && !is_internal_event
    {
        bus.publish(Event::subscriber_panicked(
            Arc::clone(name),
            extract_panic_info(&panic_err),
        ));
    }
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
        let (sub, first_gate) = blocking_order_sub();
        let set = SubscriberSet::new_with_shutdown_timeout(
            vec![Arc::clone(&sub) as Arc<dyn Subscribe>],
            Bus::new(64),
            Duration::ZERO,
        );
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
        set.emit_arc(ev("second"));
        let first_entered = wait_for_gate(&first_gate, |state| state.entered).await;

        let close_result = tokio::time::timeout(Duration::from_secs(1), set.close()).await;
        let worker_was_still_running = !worker_finished.load(Ordering::Acquire);
        let first_was_still_running = !first_gate
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finished;
        let second_was_waiting = !sub.second_entered.load(Ordering::Acquire);

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
