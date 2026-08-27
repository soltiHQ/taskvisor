//! Implements the internal fan-out engine behind [`Subscribe`].
//!
//! [`SupervisorBuilder`](crate::SupervisorBuilder) creates a [`SubscriberSet`] from the configured subscribers.
//! Construction reserves ownership for the complete set before it reads subscriber metadata.
//! Runtime startup then creates one bounded lane per subscriber.
//! Shared lanes use one fixed worker; each dedicated lane uses its own fixed worker.
//!
//! ```text
//! SupervisorBuilder ──► SubscriberSet::from_reserved ──► pending definitions
//! runtime start ──────► SubscriberSet::start ──────────► shared + dedicated subscriber lanes
//!
//! runtime event relay
//!      │ Arc<Event>
//!      ▼
//! SubscriberSet::emit_arc
//!      ├── full lane for ordinary event ───────► count one lane drop
//!      ├── full lane for internal diagnostic ──► discard silently
//!      └── lane with room ─────────────────────► lane worker ────────► Subscribe::on_event
//! ```
//!
//! Fan-out performs one bounded enqueue attempt per subscriber and never waits for callbacks.
//! Each lane preserves FIFO order.
//! Shared lanes avoid one worker thread per subscriber.
//! A subscriber that requires blocking isolation can select a dedicated worker.
//! Ordinary drops in one full lane are counted and coalesced into a direct overflow callback after that lane catches up while it remains active.
//!
//! Callback unwinding is isolated with `catch_unwind`.
//! Taskvisor transfers its retained subscriber `Arc` to the supervisor's deferred-drop domain.
//! Shutdown closes all lanes and gives them one shared drain deadline.
//! A callback already running at that deadline may continue on a detached worker.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;

use crate::{
    BuildError, RuntimeError,
    core::{
        MAX_ASYNC_CAPACITY,
        deferred_drop::{DropBundle, DropReservation},
    },
    events::{Bus, Event},
    subscribers::{Subscribe, SubscriberExecution},
};

/// Test default for the shared subscriber drain deadline.
#[cfg(test)]
pub(crate) const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

const SHARED_CALLBACK_QUANTUM: usize = 64;

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
    dedicated_ready: Option<std::sync::Condvar>,
}

type SubscriberJob = Arc<SubscriberLane>;

struct SharedQueueState {
    ready: VecDeque<SubscriberJob>,
    closed: bool,
}

struct SharedQueue {
    state: std::sync::Mutex<SharedQueueState>,
    available: std::sync::Condvar,
}

struct SubscriberWorkerHandle {
    _thread: std::thread::JoinHandle<()>,
}

struct SubscriberWorkers {
    shared: Option<Arc<SharedQueue>>,
    handles: Vec<SubscriberWorkerHandle>,
}

enum SubscriberWorkerLaunch {
    Shared {
        queue: Arc<SharedQueue>,
        runtime: tokio::runtime::Handle,
    },
    Dedicated {
        lane: SubscriberJob,
        runtime: tokio::runtime::Handle,
    },
}

fn spawn_subscriber_worker(
    index: usize,
    receiver: std::sync::mpsc::Receiver<SubscriberWorkerLaunch>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("taskvisor-subscriber-{index}"))
        .spawn(move || {
            if let Ok(launch) = receiver.recv() {
                contain_thread_unwind(move || match launch {
                    SubscriberWorkerLaunch::Shared { queue, runtime } => {
                        run_shared_worker(queue, runtime);
                    }
                    SubscriberWorkerLaunch::Dedicated { lane, runtime } => {
                        run_dedicated_worker(lane, runtime);
                    }
                });
            }
        })
}

/// Contains an escaping panic payload on its callback worker thread.
/// A nested panic payload from its destructor is leaked instead of entering the detached thread handle.
fn contain_thread_unwind(run: impl FnOnce()) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
        && let Err(nested) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
    {
        std::mem::forget(nested);
    }
}

/// Snapshot metadata and charged ownership retained until startup.
struct SubscriberDefinition {
    name: Arc<str>,
    capacity: usize,
    execution: SubscriberExecution,
    ownership: OwnedSubscriber,
}

/// Callback ownership installed before any subscriber metadata is read.
///
/// Field order keeps the final charged reference in the cleanup bundle while unwinding drops the callback reference.
struct OwnedSubscriber {
    subscriber: Arc<dyn Subscribe>,
    cleanup: DropBundle,
}

/// Mutually exclusive subscriber startup, delivery, and shutdown states.
enum SubscriberState {
    /// Metadata is ready, but the callback workers have not started.
    Pending(Vec<SubscriberDefinition>),
    /// Per-subscriber lanes and their selected callback workers are active.
    Started {
        lanes: Vec<SubscriberJob>,
        completions: Vec<oneshot::Receiver<()>>,
        workers: SubscriberWorkers,
    },
    /// Delivery is closed and later startup is disabled.
    Closed,
}

enum EnqueueResult {
    Queued,
    ScheduleShared,
    Closed(Arc<str>),
}

impl SharedQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(SharedQueueState {
                ready: VecDeque::new(),
                closed: false,
            }),
            available: std::sync::Condvar::new(),
        })
    }

    fn schedule(&self, lane: SubscriberJob) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return;
        }
        state.ready.push_back(lane);
        self.available.notify_one();
    }

    fn next(&self) -> Option<SubscriberJob> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        while state.ready.is_empty() && !state.closed {
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        state.ready.pop_front()
    }

    fn close(&self) {
        let abandoned = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.closed = true;
            self.available.notify_all();
            std::mem::take(&mut state.ready)
        };
        drop(abandoned);
    }
}

impl SubscriberWorkers {
    fn schedule_shared(&self, lane: SubscriberJob) {
        self.shared
            .as_ref()
            .expect("a shared subscriber lane has one fixed shared worker")
            .schedule(lane);
    }

    fn detach(self) {
        drop(self);
    }
}

impl Drop for SubscriberWorkers {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            shared.close();
        }
        drop(std::mem::take(&mut self.handles));
    }
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
            if let Some(ready) = &self.dedicated_ready {
                ready.notify_one();
            } else {
                return EnqueueResult::ScheduleShared;
            }
        }
        EnqueueResult::Queued
    }

    fn notify_dedicated(&self) {
        if let Some(ready) = &self.dedicated_ready {
            ready.notify_all();
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
            self.notify_dedicated();
            drop(state);
            if let Some(done) = done {
                let _ = done.send(());
            }
            ownership
        } else {
            self.notify_dedicated();
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
            self.notify_dedicated();
            drop(state);
            if let Some(done) = done {
                let _ = done.send(());
            }
            (ownership, queued)
        } else {
            self.notify_dedicated();
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

fn finish_panicked_lane(lane: &SubscriberJob, payload: Box<dyn std::any::Any + Send>) {
    let message = extract_panic_info(&payload);
    contain_thread_unwind(|| drop(payload));
    lane.fail_running_after_unwind();
    lane.bus.publish_lazy(|| {
        Event::runtime_failure(
            "subscriber_dispatch",
            format!("subscriber callback worker panicked: {message}"),
        )
    });
}

fn run_shared_worker(queue: Arc<SharedQueue>, runtime: tokio::runtime::Handle) {
    let _runtime = runtime.enter();
    while let Some(lane) = queue.next() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_shared_quantum(&lane, &queue);
        }));
        if let Err(payload) = result {
            finish_panicked_lane(&lane, payload);
        }
    }
}

fn run_dedicated_worker(lane: SubscriberJob, runtime: tokio::runtime::Handle) {
    let _runtime = runtime.enter();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_dedicated_lane(&lane);
    }));
    if let Err(payload) = result {
        finish_panicked_lane(&lane, payload);
    }
}

fn run_dedicated_lane(lane: &SubscriberJob) {
    let ready = lane
        .dedicated_ready
        .as_ref()
        .expect("a dedicated subscriber lane has its own condition variable");
    loop {
        let mut owned = {
            let mut state = lane.state.lock().unwrap_or_else(|error| error.into_inner());
            while state.phase == LanePhase::Idle {
                state = ready.wait(state).unwrap_or_else(|error| error.into_inner());
            }
            match state.phase {
                LanePhase::Scheduled => {
                    state.phase = LanePhase::Running;
                    state
                        .ownership
                        .take()
                        .expect("a scheduled subscriber lane must retain its ownership")
                }
                LanePhase::Finished => return,
                LanePhase::Idle | LanePhase::Running => {
                    unreachable!("a dedicated subscriber worker owns one serial lane")
                }
            }
        };

        loop {
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
                    Next::Overflow(std::mem::take(&mut state.dropped))
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
                        return;
                    }
                    if state.queue.is_empty() && state.dropped == 0 {
                        state.ownership = Some(owned);
                        state.phase = LanePhase::Idle;
                        break;
                    }
                    drop(state);
                }
            }
        }
    }
}

/// A busy shared lane yields after at most `SHARED_CALLBACK_QUANTUM` callbacks.
fn run_shared_quantum(lane: &SubscriberJob, shared: &Arc<SharedQueue>) {
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

    for _ in 0..SHARED_CALLBACK_QUANTUM {
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
                Next::Overflow(std::mem::take(&mut state.dropped))
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
                    shared.schedule(Arc::clone(lane));
                }
                return;
            }
        }
    }

    let mut state = lane.state.lock().unwrap_or_else(|error| error.into_inner());
    let empty = state.queue.is_empty() && state.dropped == 0;
    if state.abort || (state.closing && empty) {
        drop(state);
        lane.finish_running(owned);
    } else if empty {
        state.ownership = Some(owned);
        state.phase = LanePhase::Idle;
    } else {
        state.ownership = Some(owned);
        state.phase = LanePhase::Scheduled;
        drop(state);
        shared.schedule(Arc::clone(lane));
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
/// The runtime event relay calls [`emit_arc`](Self::emit_arc).
/// Callback workers consume each lane in FIFO order on library-owned OS threads.
/// This keeps user callbacks outside Tokio's async and blocking pools.
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
    /// Creates a test set with snapshotted metadata and no active callback workers.
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

    /// Inactive set backed by a complete ownership-reservation batch.
    ///
    /// The caller acquires one reservation per subscriber before this method reads
    /// [`Subscribe::name`], [`Subscribe::queue_capacity`], or [`Subscribe::execution`].
    /// All values are stored for the lifetime of the lane.
    ///
    /// # Errors
    ///
    /// - [`BuildError::CapacityTooLarge`] when a subscriber queue exceeds Taskvisor's structural async-capacity limit.
    ///
    /// # Panics
    ///
    /// Reservation-count mismatch.
    /// A panic from any metadata method reaches the caller with ownership already transferred to deferred-drop isolation.
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
                    let execution = owned.subscriber.execution();
                    (name, capacity, execution)
                }));
                match metadata {
                    Ok((name, capacity, execution)) if capacity <= MAX_ASYNC_CAPACITY => {
                        Ok(SubscriberDefinition {
                            name,
                            capacity,
                            execution,
                            ownership: owned,
                        })
                    }
                    Ok((_name, capacity, _execution)) => Err(BuildError::CapacityTooLarge {
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

    /// Ownership slots charged to these subscribers.
    pub(crate) fn ownership_slots(&self) -> usize {
        self.ownership_slots
    }

    /// Callback-lane startup with a shared worker when needed and one worker per dedicated lane.
    ///
    /// This operation is idempotent.
    /// A closed set cannot be started again.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::TokioRuntimeUnavailable`] outside a Tokio runtime;
    /// - [`RuntimeError::ThreadStartFailed`] when a required callback worker cannot start.
    pub(crate) fn start(&self) -> Result<(), RuntimeError> {
        self.start_with(spawn_subscriber_worker)
    }

    fn start_with(
        &self,
        mut spawn_worker: impl FnMut(
            usize,
            std::sync::mpsc::Receiver<SubscriberWorkerLaunch>,
        ) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let definitions = match &mut *state {
            SubscriberState::Pending(definitions) => definitions,
            SubscriberState::Started { .. } | SubscriberState::Closed => return Ok(()),
        };
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| RuntimeError::TokioRuntimeUnavailable)?;
        if definitions.is_empty() {
            *state = SubscriberState::Started {
                lanes: Vec::new(),
                completions: Vec::new(),
                workers: SubscriberWorkers {
                    shared: None,
                    handles: Vec::new(),
                },
            };
            return Ok(());
        }

        let shared_needed = definitions
            .iter()
            .any(|definition| definition.execution == SubscriberExecution::Shared);
        let dedicated_workers = definitions
            .iter()
            .filter(|definition| definition.execution == SubscriberExecution::Dedicated)
            .count();
        let worker_count = usize::from(shared_needed) + dedicated_workers;
        let mut launchers = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let (launch, receiver) = std::sync::mpsc::channel();
            match spawn_worker(index, receiver) {
                Ok(thread) => launchers.push((launch, thread)),
                Err(source) => {
                    drop(launch);
                    let threads: Vec<_> = launchers
                        .into_iter()
                        .map(|(launch, thread)| {
                            drop(launch);
                            thread
                        })
                        .collect();
                    for thread in threads {
                        contain_thread_unwind(|| drop(thread.join()));
                    }
                    return Err(RuntimeError::ThreadStartFailed {
                        component: "subscriber_dispatch",
                        source,
                    });
                }
            }
        }

        let definitions = std::mem::take(definitions);
        let mut lanes = Vec::with_capacity(definitions.len());
        let mut completions = Vec::with_capacity(definitions.len());
        let mut launchers = launchers.into_iter();
        let mut handles = Vec::with_capacity(worker_count);
        let shared = shared_needed.then(SharedQueue::new);
        if let Some(queue) = &shared {
            let (launch, thread) = launchers
                .next()
                .expect("the shared subscriber worker was started before commit");
            launch
                .send(SubscriberWorkerLaunch::Shared {
                    queue: Arc::clone(queue),
                    runtime: runtime.clone(),
                })
                .expect("a subscriber launcher waits for exactly one committed worker");
            handles.push(SubscriberWorkerHandle { _thread: thread });
        }

        for definition in definitions {
            let SubscriberDefinition {
                name,
                capacity,
                execution,
                ownership,
            } = definition;
            let finished = Arc::new(AtomicBool::new(false));
            let (done, done_rx) = oneshot::channel();
            let lane = Arc::new(SubscriberLane {
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
                dedicated_ready: (execution == SubscriberExecution::Dedicated)
                    .then(std::sync::Condvar::new),
            });
            if execution == SubscriberExecution::Dedicated {
                let (launch, thread) = launchers
                    .next()
                    .expect("each dedicated subscriber worker was started before commit");
                launch
                    .send(SubscriberWorkerLaunch::Dedicated {
                        lane: Arc::clone(&lane),
                        runtime: runtime.clone(),
                    })
                    .expect("a subscriber launcher waits for exactly one committed worker");
                handles.push(SubscriberWorkerHandle { _thread: thread });
            }
            lanes.push(lane);
            completions.push(done_rx);
        }
        debug_assert!(launchers.next().is_none());
        *state = SubscriberState::Started {
            lanes,
            completions,
            workers: SubscriberWorkers { shared, handles },
        };
        Ok(())
    }

    /// One non-blocking fan-out attempt to every active subscriber lane.
    ///
    /// The method does not wait for callbacks.
    /// A call before startup or after closure has no effect.
    /// A full lane drops the event only for that subscriber.
    pub(crate) fn emit_arc(&self, event: Arc<Event>) {
        let closed_subscribers = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { lanes, workers, .. } = &*state else {
                return;
            };
            let mut closed_subscribers = Vec::new();
            for lane in lanes {
                match lane.enqueue(&event) {
                    EnqueueResult::ScheduleShared => {
                        workers.schedule_shared(Arc::clone(lane));
                    }
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

    /// Shared-deadline drain and closure for every subscriber lane.
    ///
    /// At the deadline, queued events are dropped.
    /// A callback already running can continue on its worker after this method returns.
    /// Later calls do nothing.
    pub(crate) async fn close(&self) {
        let (lanes, mut completions, workers) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            match std::mem::replace(&mut *state, SubscriberState::Closed) {
                SubscriberState::Pending(_) | SubscriberState::Closed => return,
                SubscriberState::Started {
                    lanes,
                    completions,
                    workers,
                } => (lanes, completions, workers),
            }
        };

        if lanes.is_empty() {
            workers.detach();
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
        workers.detach();
    }
}

impl Drop for SubscriberSet {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        let SubscriberState::Started { lanes, workers, .. } =
            std::mem::replace(state, SubscriberState::Closed)
        else {
            return;
        };

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
        workers.detach();
    }
}

/// Panic-payload destruction stays on the active callback worker.
///
/// A blocking payload destructor keeps that callback worker and its ownership reservation alive.
/// It cannot extend the public shutdown deadline or become uncharged.
/// If destruction panics again, the nested payload and its charged slot are retained permanently.
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

/// Panic boundary for one subscriber callback.
///
/// `false` means destruction of the caught panic payload also panicked.
/// The caller then stops that lane permanently.
/// This prevents one subscriber from retaining a new nested panic payload for every later event.
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
    ) -> Vec<DropReservation> {
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
        assert_eq!(definitions[0].execution, SubscriberExecution::Shared);
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn metadata_panic_keeps_current_and_unvisited_subscribers_isolated() {
        for panic_behavior in [
            MetadataBehavior::PanicCapacity,
            MetadataBehavior::PanicName,
            MetadataBehavior::PanicExecution,
        ] {
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
                    behavior: MetadataBehavior::Dedicated,
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

        let start_result = set.start_with(|index, receiver| {
            if index == 1 {
                Err(std::io::Error::other("injected subscriber spawn failure"))
            } else {
                spawn_subscriber_worker(index, receiver)
            }
        });
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
            .expect("a failed worker batch must permit an exact retry");
        {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { lanes, workers, .. } = &*state else {
                panic!("the retry must commit a started subscriber set")
            };
            assert_eq!(lanes.len(), 3);
            assert!(workers.shared.is_none());
            assert_eq!(workers.handles.len(), 3);
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
        set.start().expect("subscriber callback workers must start");
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
            "a dedicated blocked lane must not occupy the fixed shared worker"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_shared_lane_does_not_delay_a_dedicated_lane() {
        let (blocking, gate) = shared_blocking_sub();
        let dedicated_second = Arc::new(AtomicUsize::new(0));
        let dedicated = Arc::new(DedicatedTaskCounter {
            task: "second",
            count: Arc::clone(&dedicated_second),
        });
        let set = SubscriberSet::new(vec![blocking, dedicated], Bus::new(64));
        set.start().expect("hybrid subscriber workers must start");

        set.emit_arc(ev("first"));
        assert!(wait_for_gate(&gate, |state| state.entered).await);
        set.emit_arc(ev("second"));
        let dedicated_ran = tokio::time::timeout(Duration::from_secs(2), async {
            while dedicated_second.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();

        release_gate(&gate);
        set.close().await;
        assert!(
            dedicated_ran,
            "a blocked shared callback must not delay a dedicated subscriber lane"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shared_worker_yields_a_busy_lane_after_one_finite_quantum() {
        let first_gate = Arc::new((Mutex::new(BlockingGateState::default()), Condvar::new()));
        let next_quantum_gate =
            Arc::new((Mutex::new(BlockingGateState::default()), Condvar::new()));
        let busy = Arc::new(SharedQuantumSub {
            first_gate: Arc::clone(&first_gate),
            next_quantum_gate: Arc::clone(&next_quantum_gate),
            calls: AtomicUsize::new(0),
        });
        let (healthy_count, healthy) = CountingSub::new(SHARED_CALLBACK_QUANTUM * 2);
        let set = SubscriberSet::new(vec![busy, healthy], Bus::new(SHARED_CALLBACK_QUANTUM * 2));
        set.start()
            .expect("the shared subscriber worker must start");

        set.emit_arc(ev("first"));
        assert!(wait_for_gate(&first_gate, |state| state.entered).await);
        for index in 1..(SHARED_CALLBACK_QUANTUM * 2) {
            set.emit_arc(ev(&format!("queued-{index}")));
        }

        release_gate(&first_gate);
        let next_quantum_entered = wait_for_gate(&next_quantum_gate, |state| state.entered).await;
        let healthy_calls_before_busy_resumed = healthy_count.load(Ordering::Acquire);
        release_gate(&next_quantum_gate);
        set.close().await;

        assert!(
            next_quantum_entered,
            "the busy shared lane must resume for its next quantum"
        );
        assert_eq!(
            healthy_calls_before_busy_resumed, SHARED_CALLBACK_QUANTUM as u64,
            "one ready shared lane must run before a busy lane receives its next quantum"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shared_quantum_does_not_requeue_a_lane_that_just_became_empty() {
        let first_gate = Arc::new((Mutex::new(BlockingGateState::default()), Condvar::new()));
        let unused_next_quantum_gate =
            Arc::new((Mutex::new(BlockingGateState::default()), Condvar::new()));
        let busy = Arc::new(SharedQuantumSub {
            first_gate: Arc::clone(&first_gate),
            next_quantum_gate: unused_next_quantum_gate,
            calls: AtomicUsize::new(0),
        });
        let (next, next_gate) = shared_blocking_sub();
        let set = SubscriberSet::new(vec![busy, next], Bus::new(SHARED_CALLBACK_QUANTUM));
        set.start()
            .expect("the shared subscriber worker must start");

        set.emit_arc(ev("first"));
        assert!(wait_for_gate(&first_gate, |state| state.entered).await);
        for index in 1..SHARED_CALLBACK_QUANTUM {
            set.emit_arc(ev(&format!("queued-{index}")));
        }

        release_gate(&first_gate);
        let next_entered = wait_for_gate(&next_gate, |state| state.entered).await;
        let ready_is_empty = {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { workers, .. } = &*state else {
                panic!("the subscriber worker must remain started")
            };
            workers
                .shared
                .as_ref()
                .expect("both lanes use the shared worker")
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .ready
                .is_empty()
        };
        release_gate(&next_gate);
        set.close().await;

        assert!(next_entered, "the already-ready second lane must run");
        assert!(
            ready_is_empty,
            "an empty lane must return to Idle instead of consuming another scheduler turn"
        );
    }

    #[test]
    fn thread_entry_contains_capture_and_escaping_payload_cleanup() {
        struct CaptureDrop(std_mpsc::Sender<(&'static str, std::thread::ThreadId)>);
        impl Drop for CaptureDrop {
            fn drop(&mut self) {
                let _ = self.0.send(("capture", std::thread::current().id()));
            }
        }
        struct EscapingPayload(std_mpsc::Sender<(&'static str, std::thread::ThreadId)>);
        impl Drop for EscapingPayload {
            fn drop(&mut self) {
                let _ = self.0.send(("payload", std::thread::current().id()));
                std::panic::panic_any(PanicPayloadWithPanickingDrop);
            }
        }

        let (dropped_on, drops) = std_mpsc::channel();
        let capture = CaptureDrop(dropped_on.clone());
        let entry = move || {
            let _capture = capture;
            std::panic::panic_any(EscapingPayload(dropped_on));
        };
        let worker = std::thread::spawn(move || contain_thread_unwind(entry));
        let worker_id = worker.thread().id();
        let result = worker.join();
        let returned_normally = result.is_ok();
        contain_thread_unwind(|| drop(result));
        assert!(
            returned_normally,
            "the native thread packet must contain Ok(())"
        );
        assert_eq!(
            drops.try_iter().collect::<Vec<_>>(),
            [("capture", worker_id), ("payload", worker_id)]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeated_start_and_close_wakes_the_idle_shared_worker() {
        for _ in 0..64 {
            let (_count, subscriber) = CountingSub::new(1);
            let set = SubscriberSet::new(vec![subscriber], Bus::new(8));
            set.start()
                .expect("the shared subscriber worker must start");
            tokio::time::timeout(Duration::from_secs(1), set.close())
                .await
                .expect("closing the last idle lane must wake the shared worker");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn configured_shared_subscribers_start_one_fixed_callback_worker() {
        let subscribers: Vec<Arc<dyn Subscribe>> = (0..8)
            .map(|_| CountingSub::new(1).1 as Arc<dyn Subscribe>)
            .collect();
        let set = SubscriberSet::new(subscribers, Bus::new(8));
        set.start()
            .expect("the shared subscriber worker must start");
        {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { lanes, workers, .. } = &*state else {
                panic!("subscriber workers must be started")
            };
            assert_eq!(lanes.len(), 8);
            assert!(lanes.iter().all(|lane| lane.dedicated_ready.is_none()));
            assert!(workers.shared.is_some());
            assert_eq!(workers.handles.len(), 1);
            assert_eq!(
                workers.handles[0]._thread.thread().name(),
                Some("taskvisor-subscriber-0")
            );
        }
        set.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dedicated_subscribers_add_exactly_one_worker_each() {
        struct DedicatedNoop;

        impl Subscribe for DedicatedNoop {
            fn on_event(&self, _event: &Event) {}

            fn execution(&self) -> SubscriberExecution {
                SubscriberExecution::Dedicated
            }
        }

        let shared = CountingSub::new(1).1 as Arc<dyn Subscribe>;
        let subscribers: Vec<Arc<dyn Subscribe>> = vec![
            shared,
            Arc::new(DedicatedNoop),
            Arc::new(DedicatedNoop),
            Arc::new(DedicatedNoop),
        ];
        let set = SubscriberSet::new(subscribers, Bus::new(8));
        set.start().expect("hybrid subscriber workers must start");
        {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { lanes, workers, .. } = &*state else {
                panic!("subscriber workers must be started")
            };
            assert_eq!(lanes.len(), 4);
            assert_eq!(
                lanes
                    .iter()
                    .filter(|lane| lane.dedicated_ready.is_some())
                    .count(),
                3
            );
            assert!(workers.shared.is_some());
            assert_eq!(workers.handles.len(), 4);
        }
        set.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_is_idempotent_and_close_drains_delivery() {
        let (count, subscriber) = CountingSub::new(8);
        let set = SubscriberSet::new(vec![subscriber], Bus::new(8));

        set.start().expect("subscriber callback workers must start");
        set.start().expect("subscriber callback workers must start");
        for _ in 0..3 {
            set.emit_arc(ev("started"));
        }
        tokio::time::timeout(Duration::from_secs(1), set.close())
            .await
            .expect("close must drain started subscriber workers");

        assert_eq!(count.load(Ordering::Relaxed), 3);
        set.start().expect("subscriber callback workers must start");
        set.close().await;
        assert!(matches!(
            *set.state.lock().unwrap_or_else(|e| e.into_inner()),
            SubscriberState::Closed
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn normal_lane_close_releases_subscriber_ownership() {
        let source = crate::core::deferred_drop::TestReservationSource::new(1);
        let (_count, subscriber) = CountingSub::new(8);
        let set = SubscriberSet::from_test_source(
            vec![subscriber],
            Bus::new(8),
            Duration::from_secs(1),
            &source,
        )
        .expect("the isolated budget has one subscriber slot");

        set.start().expect("subscriber callback workers must start");
        set.close().await;

        let released = tokio::time::timeout(Duration::from_secs(1), source.reserve())
            .await
            .expect("clean lane close must release ownership")
            .expect("clean subscriber destruction keeps admission open");
        drop(released);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_started_set_wakes_its_idle_worker_without_joining() {
        let source = crate::core::deferred_drop::TestReservationSource::new(1);
        let (_count, subscriber) = CountingSub::new(8);
        let set = SubscriberSet::from_test_source(
            vec![subscriber],
            Bus::new(8),
            Duration::from_secs(1),
            &source,
        )
        .expect("the isolated budget has one subscriber slot");

        set.start().expect("the subscriber worker must start");
        let lane = {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { lanes, .. } = &*state else {
                panic!("the subscriber lane must be started")
            };
            Arc::downgrade(&lanes[0])
        };

        drop(set);

        tokio::time::timeout(Duration::from_secs(1), async {
            while lane.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping a started set must wake and detach its idle worker");
        let released = tokio::time::timeout(Duration::from_secs(1), source.reserve())
            .await
            .expect("dropping a started set must release idle lane ownership")
            .expect("clean subscriber destruction keeps admission open");
        drop(released);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn aborting_close_closes_the_shared_queue_and_releases_its_worker() {
        let (subscriber, gate) = shared_blocking_sub();
        let set = Arc::new(SubscriberSet::new_with_shutdown_timeout(
            vec![subscriber],
            Bus::new(8),
            Duration::from_secs(30),
        ));
        set.start()
            .expect("the shared subscriber worker must start");
        let shared = {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { workers, .. } = &*state else {
                panic!("the subscriber worker must be started")
            };
            Arc::downgrade(
                workers
                    .shared
                    .as_ref()
                    .expect("the default subscriber uses the shared worker"),
            )
        };

        let watchdog = spawn_gate_watchdog(Arc::clone(&gate));
        set.emit_arc(ev("block"));
        assert!(wait_for_gate(&gate, |state| state.entered).await);

        let close_set = Arc::clone(&set);
        let close_task = tokio::spawn(async move { close_set.close().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if matches!(
                    *set.state.lock().unwrap_or_else(|error| error.into_inner()),
                    SubscriberState::Closed
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("close must transfer the worker bundle into its future");

        close_task.abort();
        let close_error = close_task
            .await
            .expect_err("the blocked close task must be canceled");
        assert!(close_error.is_cancelled());
        assert!(
            shared.upgrade().is_some(),
            "the running callback still retains the shared worker queue"
        );

        release_gate(&gate);
        assert!(wait_for_gate(&gate, |state| state.finished).await);
        tokio::time::timeout(Duration::from_secs(1), async {
            while shared.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("canceling close must close the queue and let its shared worker exit");
        watchdog
            .join()
            .expect("the callback watchdog must not panic");
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
        set.start().expect("subscriber callback workers must start");
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

    struct SharedBlockingSub {
        gate: BlockingGate,
    }

    struct DedicatedTaskCounter {
        task: &'static str,
        count: Arc<AtomicUsize>,
    }

    impl Subscribe for DedicatedTaskCounter {
        fn on_event(&self, event: &Event) {
            if event.task.as_deref() == Some(self.task) {
                self.count.fetch_add(1, Ordering::Release);
            }
        }

        fn execution(&self) -> SubscriberExecution {
            SubscriberExecution::Dedicated
        }
    }

    struct SharedQuantumSub {
        first_gate: BlockingGate,
        next_quantum_gate: BlockingGate,
        calls: AtomicUsize,
    }

    impl Subscribe for SharedQuantumSub {
        fn on_event(&self, _event: &Event) {
            let call = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
            let gate = if call == 1 {
                Some(&self.first_gate)
            } else if call == SHARED_CALLBACK_QUANTUM + 1 {
                Some(&self.next_quantum_gate)
            } else {
                None
            };
            let Some(gate) = gate else {
                return;
            };
            let (state, ready) = &**gate;
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            state.entered = true;
            ready.notify_all();
            while !state.released {
                state = ready.wait(state).unwrap_or_else(|error| error.into_inner());
            }
            state.finished = true;
            ready.notify_all();
        }

        fn queue_capacity(&self) -> NonZeroUsize {
            NonZeroUsize::new(SHARED_CALLBACK_QUANTUM * 2).expect("test capacity is non-zero")
        }
    }

    impl Subscribe for SharedBlockingSub {
        fn on_event(&self, _event: &Event) {
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

        fn queue_capacity(&self) -> NonZeroUsize {
            NonZeroUsize::new(8).expect("test capacity is non-zero")
        }
    }

    fn shared_blocking_sub() -> (Arc<SharedBlockingSub>, BlockingGate) {
        let gate = Arc::new((Mutex::new(BlockingGateState::default()), Condvar::new()));
        (
            Arc::new(SharedBlockingSub {
                gate: Arc::clone(&gate),
            }),
            gate,
        )
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

        fn execution(&self) -> SubscriberExecution {
            SubscriberExecution::Dedicated
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
        Dedicated,
        PanicName,
        PanicCapacity,
        PanicExecution,
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
                MetadataBehavior::Ready
                | MetadataBehavior::Dedicated
                | MetadataBehavior::PanicName
                | MetadataBehavior::PanicExecution => {}
            }
            NonZeroUsize::new(8).expect("test capacity is non-zero")
        }

        fn execution(&self) -> SubscriberExecution {
            match self.behavior {
                MetadataBehavior::Dedicated => SubscriberExecution::Dedicated,
                MetadataBehavior::PanicExecution => panic!("subscriber execution panic"),
                _ => SubscriberExecution::Shared,
            }
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
    async fn blocking_callback_keeps_runtime_responsive_and_close_waits_for_it() {
        let (sub, first_gate) = blocking_order_sub();
        let set = Arc::new(SubscriberSet::new(
            vec![Arc::clone(&sub) as Arc<dyn Subscribe>],
            Bus::new(64),
        ));
        set.start().expect("subscriber callback workers must start");

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
            set.start().expect("subscriber callback workers must start");
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
        set.start().expect("subscriber callback workers must start");
        let lane_finished = {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { lanes, .. } = &*state else {
                panic!("subscriber lane must be started")
            };
            Arc::clone(
                &lanes
                    .first()
                    .expect("the test configures one subscriber lane")
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
        let lane_was_still_running = !lane_finished.load(Ordering::Acquire);
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
        let lane_finished_after_release = tokio::time::timeout(Duration::from_secs(1), async {
            while !lane_finished.load(Ordering::Acquire) {
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
            .expect("the returned callback must release subscriber ownership")
            .expect("clean subscriber destruction keeps admission open");
        drop(released);

        assert!(first_entered, "the first callback must start before close");
        assert!(
            close_result.is_ok(),
            "zero subscriber shutdown timeout must return immediately"
        );
        assert!(
            lane_was_still_running,
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
            "the detached lane must release queued events before close returns"
        );
        assert!(
            ownership_stayed_charged,
            "detached callback ownership must remain charged until the callback returns"
        );
        assert!(first_finished, "cleanup must release the running callback");
        assert!(
            lane_finished_after_release,
            "the detached lane must finish after its running callback returns"
        );
        assert_eq!(
            seen,
            ["first"],
            "releasing the detached callback cannot revive queued events"
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
        set.start().expect("subscriber callback workers must start");

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
            "the charged subscriber callback worker must retain, not destroy, a hostile destructor panic payload"
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
        set.start().expect("subscriber callback workers must start");
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
        set.start().expect("subscriber callback workers must start");
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
        set.start().expect("subscriber callback workers must start");
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
        set.start().expect("subscriber callback workers must start");

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
    async fn panic_payload_destructor_panic_stops_only_that_shared_lane() {
        struct HealthySharedProbe {
            calls: AtomicUsize,
            callback_thread: Mutex<Option<std::thread::ThreadId>>,
        }

        impl Subscribe for HealthySharedProbe {
            fn on_event(&self, _event: &Event) {
                *self
                    .callback_thread
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(std::thread::current().id());
                self.calls.fetch_add(1, Ordering::Release);
            }
        }

        let source = crate::core::deferred_drop::TestReservationSource::new(2);
        let bus = Bus::new(64);
        let mut events = bus.subscribe();
        let calls = Arc::new(AtomicUsize::new(0));
        let panicking: Arc<dyn Subscribe> = Arc::new(NestedDropPanicSub {
            calls: Arc::clone(&calls),
        });
        let healthy = Arc::new(HealthySharedProbe {
            calls: AtomicUsize::new(0),
            callback_thread: Mutex::new(None),
        });
        let set = Arc::new(
            SubscriberSet::from_test_source(
                vec![panicking, Arc::clone(&healthy) as Arc<dyn Subscribe>],
                bus,
                Duration::from_secs(1),
                &source,
            )
            .expect("the isolated budget has two subscriber slots"),
        );
        set.start().expect("subscriber callback workers must start");
        let (lane_finished, shared_thread) = {
            let state = set.state.lock().unwrap_or_else(|error| error.into_inner());
            let SubscriberState::Started { lanes, workers, .. } = &*state else {
                panic!("the shared subscriber worker must be started")
            };
            assert_eq!(workers.handles.len(), 1);
            (
                Arc::clone(&lanes[0].finished),
                workers.handles[0]._thread.thread().id(),
            )
        };

        set.emit_arc(ev("nested-drop"));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !lane_finished.load(Ordering::Acquire)
                || healthy.calls.load(Ordering::Acquire) == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the poisoned lane must finish and the healthy shared lane must run");
        let emitters: Vec<_> = (0..8)
            .map(|_| {
                let set = Arc::clone(&set);
                std::thread::spawn(move || set.emit_arc(ev("after-lane-exit")))
            })
            .collect();
        for emitter in emitters {
            emitter.join().expect("closed-queue emitter must not panic");
        }
        tokio::time::timeout(Duration::from_secs(1), set.close())
            .await
            .expect("the healthy shared lane must close cleanly");
        let callback_thread = *healthy
            .callback_thread
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let clean_slot = tokio::time::timeout(Duration::from_secs(1), source.reserve())
            .await
            .expect("the healthy subscriber must release its ownership slot")
            .expect("healthy subscriber cleanup keeps admission open");
        let poisoned_slot_stays_charged = source.try_reserve().is_err();
        drop(clean_slot);
        let panicked = first(&mut events, EventKind::SubscriberPanicked)
            .expect("the callback panic must be reported");
        let closed = first(&mut events, EventKind::SubscriberOverflow)
            .expect("the closed subscriber queue must be reported once");

        assert_eq!(
            calls.load(Ordering::Acquire),
            1,
            "a nested panic-payload destructor failure must permanently stop only that lane"
        );
        assert_eq!(healthy.calls.load(Ordering::Acquire), 9);
        assert_eq!(
            callback_thread,
            Some(shared_thread),
            "the same physical shared worker must continue with the healthy lane"
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
            set.start().expect("subscriber callback workers must start");

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
        set.start().expect("subscriber callback workers must start");

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
        set.start().expect("subscriber callback workers must start");

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
