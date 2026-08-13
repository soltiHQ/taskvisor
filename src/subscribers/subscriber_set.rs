//! Implements the internal fan-out engine behind [`Subscribe`].
//!
//! [`SupervisorBuilder`](crate::SupervisorBuilder) creates a [`SubscriberSet`]
//! from the configured subscribers. Construction reserves ownership for the
//! complete set before it reads subscriber names or queue capacities. Runtime
//! startup then creates one bounded lane per subscriber and starts the
//! supervisor-local callback executor.
//!
//! ```text
//! SupervisorBuilder ──► SubscriberSet::from_reserved ──► pending definitions
//! runtime start ──► SubscriberSet::start ──► lanes + callback executor
//!
//! runtime event relay
//!      │ Arc<Event>
//!      ▼
//! SubscriberSet::emit_arc
//!      ├── full lane for ordinary event ──► count one lane drop
//!      ├── full lane for internal diagnostic ──► discard silently
//!      └── lane with room ──► callback executor ──► Subscribe::on_event
//! ```
//!
//! Fan-out performs one bounded enqueue attempt per subscriber and never waits
//! for callbacks. Each lane preserves FIFO order. Separate lanes may run at the
//! same time. For a non-empty set, the executor starts with one worker. It may
//! add workers up to the subscriber count when lanes contend, and it retires
//! idle extra workers.
//! Ordinary drops in one full lane are counted and coalesced into a direct
//! overflow callback after that lane catches up.
//!
//! Callback unwinding is isolated with `catch_unwind`. Taskvisor transfers its
//! retained subscriber `Arc` to the supervisor's deferred-drop domain. Shutdown
//! closes all lanes and gives them one shared drain deadline. A callback already
//! running at that deadline may continue on a detached worker.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{
    BuildError, RuntimeError,
    core::{
        MAX_ASYNC_CAPACITY,
        deferred_drop::{DropBundle, DropReservation},
    },
    events::{Bus, Event},
    subscribers::Subscribe,
};

/// Test default for the shared subscriber drain deadline.
#[cfg(test)]
pub(crate) const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

const CALLBACK_QUANTUM: usize = 64;
const EXTRA_WORKER_IDLE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LanePhase {
    Idle,
    Scheduled,
    Running,
    Finished,
}

struct SubscriberLaneState {
    queue: VecDeque<Arc<Event>>,
    dropped: u64,
    phase: LanePhase,
    closing: bool,
    abort: bool,
    closed_reported: bool,
    ownership: Option<OwnedSubscriber>,
    done: Option<oneshot::Sender<()>>,
}

struct SubscriberLane {
    name: Arc<str>,
    capacity: usize,
    bus: Bus,
    finished: Arc<AtomicBool>,
    state: std::sync::Mutex<SubscriberLaneState>,
}

type SubscriberJob = Arc<SubscriberLane>;

struct ExecutorState {
    ready: VecDeque<SubscriberJob>,
    closed: bool,
}

struct SubscriberExecutor {
    shared: Arc<ExecutorShared>,
    coordinator: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct ExecutorShared {
    state: std::sync::Mutex<ExecutorState>,
    ready: std::sync::Condvar,
    max_workers: usize,
    worker_count: AtomicUsize,
    idle_count: AtomicUsize,
    starting_count: AtomicUsize,
    runtime: tokio::runtime::Handle,
    bus: Bus,
    spawn_failed: AtomicBool,
    control: Notify,
    coordinator_stop: CancellationToken,
    spawn_gate: std::sync::Mutex<()>,
    workers: std::sync::Mutex<Vec<SubscriberWorkerHandle>>,
    #[cfg(test)]
    injected_spawn_failures: AtomicUsize,
}

struct WorkerCountGuard {
    shared: Arc<ExecutorShared>,
    finished: Arc<AtomicBool>,
}

impl Drop for WorkerCountGuard {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
        self.shared.worker_count.fetch_sub(1, Ordering::AcqRel);
        self.shared.control.notify_one();
    }
}

struct SubscriberWorkerHandle {
    finished: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

/// Snapshotted metadata and charged ownership retained until startup.
struct SubscriberDefinition {
    name: Arc<str>,
    capacity: usize,
    ownership: OwnedSubscriber,
}

/// Callback ownership installed before any subscriber metadata is read.
///
/// Field order keeps the final charged reference in the cleanup bundle while
/// unwinding drops the callback reference.
struct OwnedSubscriber {
    subscriber: Arc<dyn Subscribe>,
    cleanup: DropBundle,
}

/// Mutually exclusive subscriber startup, delivery, and shutdown states.
enum SubscriberState {
    /// Metadata is ready, but the callback executor has not started.
    Pending(Vec<SubscriberDefinition>),
    /// Per-subscriber lanes and the callback executor are active.
    Started {
        lanes: Vec<SubscriberJob>,
        completions: Vec<oneshot::Receiver<()>>,
        executor: Arc<SubscriberExecutor>,
    },
    /// Delivery is closed and later startup is disabled.
    Closed,
}

enum EnqueueResult {
    Queued,
    Schedule,
    Closed(Arc<str>),
}

impl SubscriberLane {
    fn enqueue(&self, event: &Arc<Event>) -> EnqueueResult {
        let is_internal = event.is_internal_diagnostic();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.phase == LanePhase::Finished {
            if !is_internal && !state.closed_reported {
                state.closed_reported = true;
                return EnqueueResult::Closed(Arc::clone(&self.name));
            }
            return EnqueueResult::Queued;
        }
        if state.queue.len() == self.capacity {
            if !is_internal {
                state.dropped = state.dropped.saturating_add(1);
            }
            return EnqueueResult::Queued;
        }
        state.queue.push_back(Arc::clone(event));
        if state.phase == LanePhase::Idle {
            state.phase = LanePhase::Scheduled;
            EnqueueResult::Schedule
        } else {
            EnqueueResult::Queued
        }
    }

    fn begin_close(&self) -> Option<OwnedSubscriber> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closing = true;
        if state.phase == LanePhase::Idle {
            state.phase = LanePhase::Finished;
            self.finished.store(true, Ordering::Release);
            let done = state.done.take();
            let ownership = state.ownership.take();
            drop(state);
            if let Some(done) = done {
                let _ = done.send(());
            }
            ownership
        } else {
            None
        }
    }

    fn abort(&self) -> (Option<OwnedSubscriber>, VecDeque<Arc<Event>>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closing = true;
        state.abort = true;
        let queued = std::mem::take(&mut state.queue);
        state.dropped = 0;
        if matches!(state.phase, LanePhase::Idle | LanePhase::Scheduled) {
            state.phase = LanePhase::Finished;
            self.finished.store(true, Ordering::Release);
            let done = state.done.take();
            let ownership = state.ownership.take();
            drop(state);
            if let Some(done) = done {
                let _ = done.send(());
            }
            (ownership, queued)
        } else {
            (None, queued)
        }
    }

    fn finish_running(&self, owned: OwnedSubscriber) {
        let (done, queued) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            debug_assert_eq!(state.phase, LanePhase::Running);
            state.phase = LanePhase::Finished;
            let queued = std::mem::take(&mut state.queue);
            state.dropped = 0;
            self.finished.store(true, Ordering::Release);
            (state.done.take(), queued)
        };
        drop(queued);
        submit_owned_subscriber(owned);
        if let Some(done) = done {
            let _ = done.send(());
        }
    }

    fn fail_running_after_unwind(&self) {
        let (done, queued, ownership) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.phase == LanePhase::Finished {
                return;
            }
            state.phase = LanePhase::Finished;
            let queued = std::mem::take(&mut state.queue);
            state.dropped = 0;
            self.finished.store(true, Ordering::Release);
            (state.done.take(), queued, state.ownership.take())
        };
        drop(queued);
        if let Some(ownership) = ownership {
            submit_owned_subscriber(ownership);
        }
        if let Some(done) = done {
            let _ = done.send(());
        }
    }
}

impl SubscriberExecutor {
    fn new(max_workers: usize, runtime: tokio::runtime::Handle, bus: Bus) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(ExecutorShared {
                state: std::sync::Mutex::new(ExecutorState {
                    ready: VecDeque::new(),
                    closed: false,
                }),
                ready: std::sync::Condvar::new(),
                max_workers,
                worker_count: AtomicUsize::new(usize::from(max_workers != 0)),
                idle_count: AtomicUsize::new(0),
                starting_count: AtomicUsize::new(usize::from(max_workers != 0)),
                runtime,
                bus,
                spawn_failed: AtomicBool::new(false),
                control: Notify::new(),
                coordinator_stop: CancellationToken::new(),
                spawn_gate: std::sync::Mutex::new(()),
                workers: std::sync::Mutex::new(Vec::with_capacity(max_workers)),
                #[cfg(test)]
                injected_spawn_failures: AtomicUsize::new(0),
            }),
            coordinator: std::sync::Mutex::new(None),
        })
    }

    fn install_seed(&self, seed: std::thread::JoinHandle<()>, finished: Arc<AtomicBool>) {
        self.shared
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(SubscriberWorkerHandle {
                finished,
                thread: seed,
            });
    }

    fn start_coordinator(&self) {
        let shared = Arc::clone(&self.shared);
        let coordinator = tokio::spawn(async move {
            shared.coordinator_loop().await;
        });
        *self
            .coordinator
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(coordinator);
    }

    fn schedule(&self, lane: SubscriberJob) {
        self.shared.enqueue(lane);
    }

    async fn shutdown(&self, join_all: bool) {
        self.shared.close();
        let coordinator = self
            .coordinator
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(coordinator) = coordinator {
            let _ = coordinator.await;
        }
        let workers = {
            let _spawn = self
                .shared
                .spawn_gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            std::mem::take(
                &mut *self
                    .shared
                    .workers
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            )
        };
        let joinable: Vec<_> = workers
            .into_iter()
            .filter(|worker| join_all || worker.finished.load(Ordering::Acquire))
            .map(|worker| worker.thread)
            .collect();
        for worker in joinable {
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

impl Drop for SubscriberExecutor {
    fn drop(&mut self) {
        self.shared.close();
    }
}

impl ExecutorShared {
    fn enqueue(&self, lane: SubscriberJob) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return;
        }
        state.ready.push_back(lane);
        self.ready.notify_one();
        drop(state);
        self.control.notify_one();
    }

    fn close(&self) {
        let abandoned = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.closed = true;
            self.ready.notify_all();
            self.coordinator_stop.cancel();
            std::mem::take(&mut state.ready)
        };
        drop(abandoned);
    }

    async fn coordinator_loop(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.coordinator_stop.cancelled() => return,
                _ = self.control.notified() => {}
            }
            self.reap_finished().await;
            let reserve = {
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                if state.closed {
                    return;
                }
                let idle = self.idle_count.load(Ordering::Acquire);
                !state.ready.is_empty()
                    && state.ready.len() > idle
                    && self.starting_count.load(Ordering::Acquire) == 0
                    && self
                        .worker_count
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |workers| {
                            (workers < self.max_workers).then_some(workers + 1)
                        })
                        .is_ok()
            };
            if !reserve {
                continue;
            }
            self.starting_count.fetch_add(1, Ordering::AcqRel);
            self.spawn_reserved();
            let should_retry = {
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                !state.closed && !state.ready.is_empty()
            };
            if should_retry && self.spawn_failed.load(Ordering::Acquire) {
                tokio::select! {
                    _ = self.coordinator_stop.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(25)) => {}
                }
                self.control.notify_one();
            }
        }
    }

    async fn reap_finished(&self) {
        let finished = {
            let mut workers = self
                .workers
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut live = Vec::with_capacity(workers.len());
            let mut finished = Vec::new();
            for worker in std::mem::take(&mut *workers) {
                if worker.finished.load(Ordering::Acquire) {
                    finished.push(worker);
                } else {
                    live.push(worker);
                }
            }
            *workers = live;
            finished
        };
        for worker in finished {
            let _ = worker.thread.join();
        }
    }

    fn spawn_reserved(self: &Arc<Self>) {
        let _spawn = self
            .spawn_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let closed = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closed;
        if closed {
            self.worker_count.fetch_sub(1, Ordering::AcqRel);
            self.starting_count.fetch_sub(1, Ordering::AcqRel);
            return;
        }
        #[cfg(test)]
        if self
            .injected_spawn_failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            self.handle_spawn_failure(std::io::Error::other(
                "injected subscriber callback worker spawn failure",
            ));
            return;
        }
        let shared = Arc::clone(self);
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let index = self.worker_count.load(Ordering::Acquire).saturating_sub(1);
        match std::thread::Builder::new()
            .name(format!("taskvisor-subscriber-{index}"))
            .spawn(move || shared.worker_loop(false, worker_finished))
        {
            Ok(thread) => {
                self.spawn_failed.store(false, Ordering::Release);
                self.workers
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(SubscriberWorkerHandle { finished, thread });
            }
            Err(error) => self.handle_spawn_failure(error),
        }
    }

    fn handle_spawn_failure(&self, error: std::io::Error) {
        self.worker_count.fetch_sub(1, Ordering::AcqRel);
        self.starting_count.fetch_sub(1, Ordering::AcqRel);
        if !self.spawn_failed.swap(true, Ordering::AcqRel) {
            self.bus.publish_lazy(|| {
                Event::runtime_failure(
                    "subscriber_dispatch",
                    format!("failed to expand subscriber callback workers: {error}"),
                )
            });
        }
    }

    fn worker_loop(self: Arc<Self>, persistent: bool, finished: Arc<AtomicBool>) {
        let _runtime = self.runtime.enter();
        self.starting_count.fetch_sub(1, Ordering::AcqRel);
        self.control.notify_one();
        let _count = WorkerCountGuard {
            shared: Arc::clone(&self),
            finished,
        };
        loop {
            let lane = {
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                self.idle_count.fetch_add(1, Ordering::AcqRel);
                while state.ready.is_empty() && !state.closed {
                    if persistent {
                        state = self
                            .ready
                            .wait(state)
                            .unwrap_or_else(|error| error.into_inner());
                    } else {
                        let (next, timeout) = self
                            .ready
                            .wait_timeout(state, EXTRA_WORKER_IDLE_TIMEOUT)
                            .unwrap_or_else(|error| error.into_inner());
                        state = next;
                        if timeout.timed_out() && state.ready.is_empty() && !state.closed {
                            break;
                        }
                    }
                }
                self.idle_count.fetch_sub(1, Ordering::AcqRel);
                match state.ready.pop_front() {
                    Some(lane) => lane,
                    None => break,
                }
            };
            self.control.notify_one();

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_subscriber_quantum(&lane, &self);
            }));
            if let Err(payload) = result {
                let message = extract_panic_info(&payload);
                if let Err(nested) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(payload);
                })) {
                    std::mem::forget(nested);
                }
                lane.fail_running_after_unwind();
                self.bus.publish_lazy(|| {
                    Event::runtime_failure(
                        "subscriber_dispatch",
                        format!("subscriber callback worker panicked: {message}"),
                    )
                });
            }
        }
    }
}

fn submit_owned_subscriber(owned: OwnedSubscriber) {
    let OwnedSubscriber {
        subscriber,
        cleanup,
    } = owned;
    drop(subscriber);
    cleanup.submit();
}

/// Owns subscriber definitions, active lanes, and callback shutdown.
///
/// The runtime event relay calls [`emit_arc`](Self::emit_arc). Callback workers
/// consume each lane in FIFO order on library-owned OS threads. This keeps user
/// callbacks outside Tokio's async and blocking pools.
pub(crate) struct SubscriberSet {
    /// Serializes startup, fan-out, and shutdown state changes.
    ///
    /// The runtime has one event relay, which is the normal `emit_arc` caller.
    state: std::sync::Mutex<SubscriberState>,

    /// Shared deadline for draining every lane.
    shutdown_timeout: Duration,

    /// Event bus used for callback and lane diagnostics.
    bus: Bus,

    /// Number of charged subscriber ownership slots.
    ownership_slots: usize,
}

impl SubscriberSet {
    /// Creates a test set with snapshotted metadata and no active executor.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new(subs: Vec<Arc<dyn Subscribe>>, bus: Bus) -> Self {
        Self::new_with_shutdown_timeout(subs, bus, DEFAULT_SHUTDOWN_TIMEOUT)
    }

    /// Creates an isolated test set with an explicit drain timeout.
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

    /// Creates an inactive set from a complete ownership reservation batch.
    ///
    /// The caller acquires one reservation per subscriber before this method
    /// reads [`Subscribe::name`] or [`Subscribe::queue_capacity`]. Both values
    /// are stored for the lifetime of the lane.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::CapacityTooLarge`] when a subscriber queue exceeds
    /// Tokio's structural bounded-channel limit.
    ///
    /// # Panics
    ///
    /// Panics when the reservation count differs from the subscriber count.
    /// A panic from either metadata method reaches the caller with ownership
    /// already transferred to deferred-drop isolation.
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

    /// Returns the number of ownership slots charged to these subscribers.
    pub(crate) fn ownership_slots(&self) -> usize {
        self.ownership_slots
    }

    /// Creates subscriber lanes and starts the callback executor.
    ///
    /// This operation is idempotent. A closed set cannot be started again.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::TokioRuntimeUnavailable`] outside a Tokio runtime.
    /// Returns [`RuntimeError::ThreadStartFailed`] if the seed callback worker
    /// cannot start.
    pub(crate) fn start(&self) -> Result<(), RuntimeError> {
        self.start_with(|shared, finished| {
            std::thread::Builder::new()
                .name("taskvisor-subscriber-0".to_owned())
                .spawn(move || shared.worker_loop(true, finished))
        })
    }

    fn start_with(
        &self,
        spawn_seed: impl FnOnce(
            Arc<ExecutorShared>,
            Arc<AtomicBool>,
        ) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let definitions = match &mut *state {
            SubscriberState::Pending(definitions) => definitions,
            SubscriberState::Started { .. } | SubscriberState::Closed => return Ok(()),
        };
        if definitions.is_empty() {
            *state = SubscriberState::Started {
                lanes: Vec::new(),
                completions: Vec::new(),
                executor: SubscriberExecutor::new(
                    0,
                    tokio::runtime::Handle::try_current()
                        .map_err(|_| RuntimeError::TokioRuntimeUnavailable)?,
                    self.bus.clone(),
                ),
            };
            return Ok(());
        }

        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| RuntimeError::TokioRuntimeUnavailable)?;
        let executor = SubscriberExecutor::new(definitions.len(), runtime, self.bus.clone());
        let seed_finished = Arc::new(AtomicBool::new(false));
        let seed = spawn_seed(Arc::clone(&executor.shared), Arc::clone(&seed_finished)).map_err(
            |source| {
                executor.shared.worker_count.store(0, Ordering::Release);
                executor.shared.starting_count.store(0, Ordering::Release);
                RuntimeError::ThreadStartFailed {
                    component: "subscriber_dispatch",
                    source,
                }
            },
        )?;
        executor.install_seed(seed, seed_finished);
        executor.start_coordinator();
        let definitions = std::mem::take(definitions);
        let mut lanes = Vec::with_capacity(definitions.len());
        let mut completions = Vec::with_capacity(definitions.len());
        for definition in definitions {
            let SubscriberDefinition {
                name,
                capacity,
                ownership,
            } = definition;
            let finished = Arc::new(AtomicBool::new(false));
            let (done, done_rx) = oneshot::channel();
            lanes.push(Arc::new(SubscriberLane {
                name,
                capacity,
                bus: self.bus.clone(),
                finished,
                state: std::sync::Mutex::new(SubscriberLaneState {
                    queue: VecDeque::new(),
                    dropped: 0,
                    phase: LanePhase::Idle,
                    closing: false,
                    abort: false,
                    closed_reported: false,
                    ownership: Some(ownership),
                    done: Some(done),
                }),
            }));
            completions.push(done_rx);
        }
        *state = SubscriberState::Started {
            lanes,
            completions,
            executor,
        };
        Ok(())
    }

    /// Attempts to enqueue one shared event into every subscriber lane.
    ///
    /// The method does not wait for callbacks. A call before startup or after
    /// closure has no effect. A full lane drops the event only for that
    /// subscriber.
    pub(crate) fn emit_arc(&self, event: Arc<Event>) {
        let closed_subscribers = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started {
                lanes, executor, ..
            } = &*state
            else {
                return;
            };
            let mut closed_subscribers = Vec::new();
            for lane in lanes {
                match lane.enqueue(&event) {
                    EnqueueResult::Schedule => executor.schedule(Arc::clone(lane)),
                    EnqueueResult::Closed(name) => closed_subscribers.push(name),
                    EnqueueResult::Queued => {}
                }
            }
            closed_subscribers
        };

        for subscriber in closed_subscribers {
            self.bus
                .publish_lazy(|| Event::subscriber_overflow(subscriber, "closed"));
        }
    }

    /// Closes all lanes and drains them within one shared deadline.
    ///
    /// At the deadline, queued events are dropped. A callback already running
    /// can continue on its worker after this method returns. Later calls do
    /// nothing.
    pub(crate) async fn close(&self) {
        let (lanes, mut completions, executor) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            match std::mem::replace(&mut *state, SubscriberState::Closed) {
                SubscriberState::Pending(_) | SubscriberState::Closed => return,
                SubscriberState::Started {
                    lanes,
                    completions,
                    executor,
                } => (lanes, completions, executor),
            }
        };

        if lanes.is_empty() {
            return;
        }

        let idle_ownership: Vec<_> = lanes.iter().filter_map(|lane| lane.begin_close()).collect();
        for owned in idle_ownership {
            submit_owned_subscriber(owned);
        }

        let drained = if self.shutdown_timeout.is_zero() {
            false
        } else {
            tokio::time::timeout(self.shutdown_timeout, async {
                for completion in &mut completions {
                    let _ = completion.await;
                }
            })
            .await
            .is_ok()
        };

        if !drained {
            let mut ownership = Vec::new();
            let mut queued = Vec::new();
            for lane in &lanes {
                let (owned, events) = lane.abort();
                ownership.extend(owned);
                queued.push(events);
            }
            drop(queued);
            for owned in ownership {
                submit_owned_subscriber(owned);
            }
        }
        executor.shutdown(drained).await;
    }
}

/// Destroys a caught panic payload only on the active callback worker.
///
/// A blocking payload destructor keeps that callback worker and its ownership
/// reservation alive. It cannot extend the public shutdown deadline or become
/// uncharged. If destruction panics again, the nested payload and its charged
/// slot are retained permanently.
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

/// Runs one scheduling quantum from a subscriber's serial lane.
fn run_subscriber_quantum(lane: &SubscriberJob, executor: &Arc<ExecutorShared>) {
    let mut owned = {
        let mut state = lane.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.phase != LanePhase::Scheduled || state.abort {
            return;
        }
        state.phase = LanePhase::Running;
        state
            .ownership
            .take()
            .expect("a scheduled subscriber lane must retain its ownership")
    };

    for _ in 0..CALLBACK_QUANTUM {
        enum Next {
            Event(Arc<Event>),
            Overflow(u64),
            Finish,
            Idle,
        }
        let next = {
            let mut state = lane.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.abort {
                Next::Finish
            } else if let Some(event) = state.queue.pop_front() {
                Next::Event(event)
            } else if state.dropped != 0 {
                let dropped = std::mem::take(&mut state.dropped);
                Next::Overflow(dropped)
            } else if state.closing {
                Next::Finish
            } else {
                Next::Idle
            }
        };

        match next {
            Next::Event(event) => {
                if !invoke_subscriber(
                    &owned.subscriber,
                    event.as_ref(),
                    &lane.name,
                    &lane.bus,
                    &mut owned.cleanup,
                ) {
                    lane.finish_running(owned);
                    return;
                }
            }
            Next::Overflow(dropped) => {
                let event = Event::subscriber_overflow(Arc::clone(&lane.name), "full")
                    .with_dropped(dropped);
                if !invoke_subscriber(
                    &owned.subscriber,
                    &event,
                    &lane.name,
                    &lane.bus,
                    &mut owned.cleanup,
                ) {
                    lane.finish_running(owned);
                    return;
                }
            }
            Next::Finish => {
                lane.finish_running(owned);
                return;
            }
            Next::Idle => {
                let mut state = lane.state.lock().unwrap_or_else(|error| error.into_inner());
                if state.abort || state.closing {
                    drop(state);
                    lane.finish_running(owned);
                } else if state.queue.is_empty() && state.dropped == 0 {
                    state.ownership = Some(owned);
                    state.phase = LanePhase::Idle;
                } else {
                    state.ownership = Some(owned);
                    state.phase = LanePhase::Scheduled;
                    drop(state);
                    executor.enqueue(Arc::clone(lane));
                }
                return;
            }
        }
    }

    let mut state = lane.state.lock().unwrap_or_else(|error| error.into_inner());
    if state.abort || (state.closing && state.queue.is_empty() && state.dropped == 0) {
        drop(state);
        lane.finish_running(owned);
    } else {
        state.ownership = Some(owned);
        state.phase = LanePhase::Scheduled;
        drop(state);
        executor.enqueue(Arc::clone(lane));
    }
}

/// Calls user subscriber code behind its panic boundary.
///
/// Returns `false` when destroying a caught panic payload itself panics. That
/// lane then stops permanently. This prevents one subscriber from retaining a
/// new nested panic payload for every later event.
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
        // This callback already runs on a library-owned OS worker. Keep a
        // blocking destructor there instead of consuming supervisor-local destructor
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
    async fn partial_thread_spawn_failure_is_transactional_and_retryable() {
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

        let start_result =
            set.start_with(|_, _| Err(std::io::Error::other("injected subscriber spawn failure")));
        assert!(matches!(
            start_result,
            Err(RuntimeError::ThreadStartFailed {
                component: "subscriber_dispatch",
                ..
            })
        ));
        {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Pending(definitions) = &*state else {
                panic!("a failed launcher batch must restore pending startup")
            };
            assert_eq!(definitions.len(), 3);
        }
        set.start()
            .expect("a failed seed must permit an exact retry");
        {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started {
                lanes, executor, ..
            } = &*state
            else {
                panic!("the retry must commit a started subscriber set")
            };
            assert_eq!(lanes.len(), 3);
            assert_eq!(executor.shared.worker_count.load(Ordering::Acquire), 1);
        }
        set.close().await;
        drop(set);

        for _ in 0..3 {
            let destructor_thread = drops
                .recv_timeout(Duration::from_secs(2))
                .expect("every retried subscriber must clean up");
            assert_ne!(
                destructor_thread, caller,
                "retry cleanup cannot destroy a final subscriber on its caller"
            );
        }
        drop(wait_for_test_reservations(&source, 3));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn large_valid_queue_capacity_is_not_eagerly_allocated() {
        struct LargeCapacity;

        impl Subscribe for LargeCapacity {
            fn on_event(&self, _event: &Event) {}

            fn queue_capacity(&self) -> NonZeroUsize {
                NonZeroUsize::new(MAX_ASYNC_CAPACITY).expect("the structural maximum is non-zero")
            }
        }

        let set = SubscriberSet::new(vec![Arc::new(LargeCapacity)], Bus::new(8));
        set.start()
            .expect("a large logical queue bound must not require proportional allocation");
        set.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_lane_does_not_strand_an_already_ready_lane() {
        let (blocking, gate) = blocking_order_sub();
        let (healthy_count, healthy) = CountingSub::new(8);
        let set = SubscriberSet::new(
            vec![
                Arc::clone(&blocking) as Arc<dyn Subscribe>,
                healthy as Arc<dyn Subscribe>,
            ],
            Bus::new(64),
        );
        set.start()
            .expect("subscriber callback executor must start");
        set.emit_arc(ev("first"));

        let first_blocked = wait_for_gate(&gate, |state| state.entered).await;
        let healthy_ran = tokio::time::timeout(Duration::from_secs(2), async {
            while healthy_count.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();

        release_gate(&gate);
        set.close().await;
        assert!(first_blocked);
        assert!(
            healthy_ran,
            "a blocked seed worker must trigger elastic progress for a ready independent lane"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lazy_worker_spawn_failure_retries_without_another_emit() {
        let bus = Bus::new(64);
        let mut events = bus.subscribe();
        let (blocking, gate) = blocking_order_sub();
        let (healthy_count, healthy) = CountingSub::new(8);
        let set = SubscriberSet::new(
            vec![
                Arc::clone(&blocking) as Arc<dyn Subscribe>,
                healthy as Arc<dyn Subscribe>,
            ],
            bus,
        );
        set.start()
            .expect("subscriber callback executor must start");
        {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { executor, .. } = &*state else {
                panic!("subscriber callback executor must be started")
            };
            executor
                .shared
                .injected_spawn_failures
                .store(1, Ordering::Release);
        }

        set.emit_arc(ev("first"));
        assert!(wait_for_gate(&gate, |state| state.entered).await);
        let healthy_ran = tokio::time::timeout(Duration::from_secs(2), async {
            while healthy_count.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        release_gate(&gate);
        set.close().await;

        assert!(
            healthy_ran,
            "a failed lazy spawn must retry while ready work remains"
        );
        assert_eq!(
            count(&mut events, EventKind::RuntimeFailure),
            1,
            "one failure episode must publish one diagnostic"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn extra_callback_workers_retire_back_to_the_seed() {
        let (blocking, gate) = blocking_order_sub();
        let (count, healthy) = CountingSub::new(8);
        let set = SubscriberSet::new(
            vec![
                Arc::clone(&blocking) as Arc<dyn Subscribe>,
                healthy as Arc<dyn Subscribe>,
            ],
            Bus::new(64),
        );
        set.start()
            .expect("subscriber callback executor must start");
        set.emit_arc(ev("first"));
        assert!(wait_for_gate(&gate, |state| state.entered).await);
        tokio::time::timeout(Duration::from_secs(2), async {
            while count.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the elastic worker must service the healthy lane");
        release_gate(&gate);

        let executor = {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { executor, .. } = &*state else {
                panic!("subscriber callback executor must remain started")
            };
            Arc::clone(executor)
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while executor.shared.worker_count.load(Ordering::Acquire) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("elastic workers must retire to one persistent seed");
        set.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeated_growth_reaps_retired_worker_handles() {
        let (blocking, gate) = blocking_order_sub();
        let (healthy_count, healthy) = CountingSub::new(64);
        let set = SubscriberSet::new(
            vec![
                Arc::clone(&blocking) as Arc<dyn Subscribe>,
                healthy as Arc<dyn Subscribe>,
            ],
            Bus::new(64),
        );
        set.start()
            .expect("subscriber callback executor must start");
        let executor = {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { executor, .. } = &*state else {
                panic!("subscriber callback executor must be started")
            };
            Arc::clone(executor)
        };

        for iteration in 0..3_u64 {
            set.emit_arc(ev("first"));
            assert!(wait_for_gate(&gate, |state| state.entered).await);
            tokio::time::timeout(Duration::from_secs(2), async {
                while healthy_count.load(Ordering::Acquire) <= iteration {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the elastic worker must service the healthy lane");
            release_gate(&gate);
            tokio::time::timeout(Duration::from_secs(3), async {
                while executor.shared.worker_count.load(Ordering::Acquire) != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the extra worker must retire");

            if iteration != 2 {
                let mut state = gate.0.lock().unwrap_or_else(|error| error.into_inner());
                state.entered = false;
                state.released = false;
                state.finished = false;
            }
        }

        executor.shared.control.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let handles = executor
                    .shared
                    .workers
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .len();
                if handles == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retired callback worker handles must be reaped");
        set.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeated_start_and_close_cannot_lose_coordinator_stop() {
        for _ in 0..64 {
            let (_count, subscriber) = CountingSub::new(1);
            let set = SubscriberSet::new(vec![subscriber], Bus::new(8));
            set.start()
                .expect("subscriber callback executor must start");
            tokio::time::timeout(Duration::from_secs(1), set.close())
                .await
                .expect("coordinator stop is a retained cancellation signal");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn configured_subscribers_start_with_one_callback_worker() {
        let subscribers: Vec<Arc<dyn Subscribe>> = (0..64)
            .map(|_| CountingSub::new(1).1 as Arc<dyn Subscribe>)
            .collect();
        let set = SubscriberSet::new(subscribers, Bus::new(8));
        set.start()
            .expect("subscriber callback executor must start");
        let worker_count = {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { executor, .. } = &*state else {
                panic!("subscriber callback executor must be started")
            };
            executor.shared.worker_count.load(Ordering::Acquire)
        };
        assert_eq!(worker_count, 1);
        set.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_is_idempotent_and_close_drains_delivery() {
        let (count, subscriber) = CountingSub::new(8);
        let set = SubscriberSet::new(vec![subscriber], Bus::new(8));

        set.start()
            .expect("subscriber callback executor must start");
        set.start()
            .expect("subscriber callback executor must start");
        for _ in 0..3 {
            set.emit_arc(ev("started"));
        }
        tokio::time::timeout(Duration::from_secs(1), set.close())
            .await
            .expect("close must drain started subscriber workers");

        assert_eq!(count.load(Ordering::Relaxed), 3);
        set.start()
            .expect("subscriber callback executor must start");
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

        set.start()
            .expect("subscriber callback executor must start");
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
        set.start()
            .expect("subscriber callback executor must start");
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
        set.start()
            .expect("subscriber callback executor must start");

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
            set.start()
                .expect("subscriber callback executor must start");
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
                "subscriber callbacks must use a library-owned worker, not Tokio's blocking pool"
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
        set.start()
            .expect("subscriber callback executor must start");
        let worker_finished = {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { lanes, .. } = &*state else {
                panic!("subscriber worker must be started")
            };
            Arc::clone(
                &lanes
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
        set.start()
            .expect("subscriber callback executor must start");

        let close_result = tokio::time::timeout(Duration::from_millis(500), set.close()).await;
        let payload_destructor_ran = gate
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entered;
        let poisoned_slot_stays_charged = source.try_reserve().is_err();

        assert!(
            close_result.is_ok(),
            "a subscriber destructor panic payload cannot extend close"
        );
        assert!(
            !payload_destructor_ran,
            "the supervisor-local charged executor must retain, not destroy, a hostile destructor panic payload"
        );
        assert!(
            poisoned_slot_stays_charged,
            "a subscriber destructor panic must permanently consume its charged ownership slot"
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
        set.start()
            .expect("subscriber callback executor must start");
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
        set.start()
            .expect("subscriber callback executor must start");
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
        set.start()
            .expect("subscriber callback executor must start");
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
        set.start()
            .expect("subscriber callback executor must start");

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
        let set = Arc::new(
            SubscriberSet::from_test_source(vec![subscriber], bus, Duration::from_secs(1), &source)
                .expect("the isolated budget has one subscriber slot"),
        );
        set.start()
            .expect("subscriber callback executor must start");
        let worker_finished = {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { lanes, .. } = &*state else {
                panic!("the subscriber worker must be started")
            };
            Arc::clone(&lanes[0].finished)
        };

        set.emit_arc(ev("nested-drop"));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !worker_finished.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the poisoned subscriber worker must terminate");
        let emitters: Vec<_> = (0..8)
            .map(|_| {
                let set = Arc::clone(&set);
                std::thread::spawn(move || set.emit_arc(ev("after-worker-exit")))
            })
            .collect();
        for emitter in emitters {
            emitter.join().expect("closed-queue emitter must not panic");
        }
        tokio::time::timeout(Duration::from_secs(1), set.close())
            .await
            .expect("the poisoned subscriber worker must terminate");
        let poisoned_slot_stays_charged = source.try_reserve().is_err();
        let panicked = first(&mut events, EventKind::SubscriberPanicked)
            .expect("the callback panic must be reported");
        let closed = first(&mut events, EventKind::SubscriberOverflow)
            .expect("the closed subscriber queue must be reported once");

        assert_eq!(
            calls.load(Ordering::Acquire),
            1,
            "a nested panic-payload destructor failure must permanently stop that worker"
        );
        assert_eq!(panicked.task.as_deref(), Some("nested-drop-panic"));
        assert_eq!(closed.task.as_deref(), Some("nested-drop-panic"));
        assert_eq!(closed.reason.as_deref(), Some("closed"));
        assert_eq!(
            count(&mut events, EventKind::SubscriberOverflow),
            0,
            "concurrent closed-queue sends must publish only one diagnostic"
        );
        assert!(
            poisoned_slot_stays_charged,
            "a nested panic-payload destructor must permanently consume its charged ownership slot"
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
            set.start()
                .expect("subscriber callback executor must start");

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
        set.start()
            .expect("subscriber callback executor must start");

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
        set.start()
            .expect("subscriber callback executor must start");

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
