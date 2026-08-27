//! Common runtime, admission, and synchronization fixtures for benchmark workloads.
//!
//! Keep setup and ownership reset outside measured sections unless the case names them explicitly.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use criterion::Criterion;
use taskvisor::{
    BackoffPolicy, Event, EventKind, RestartPolicy, Subscribe, SupervisorConfig, SupervisorHandle,
    TaskContext, TaskFn, TaskOutcome, TaskOutcomeKind, TaskRef, TaskSpec, TaskWaiter,
};
use tokio::runtime::Runtime;
use tokio::sync::Notify;

pub const WATCHDOG: Duration = Duration::from_secs(10);
pub const EVENT_CAPACITY: usize = 16_384;
pub const OWNERSHIP_CAPACITY: usize = 1_024;

pub type RtFactory = fn() -> Runtime;

pub const RUNTIMES: [(&str, RtFactory); 2] = [
    ("current_thread", rt_current_thread),
    ("multi_thread", rt_multi_thread),
];

pub fn rt_current_thread() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
}

pub fn rt_multi_thread() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("four-worker runtime")
}

pub fn criterion() -> Criterion {
    Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
}

pub fn bench_config() -> SupervisorConfig {
    SupervisorConfig::default()
        .with_bus_capacity(NonZeroUsize::new(EVENT_CAPACITY).unwrap())
        .with_max_registered_tasks(NonZeroUsize::new(OWNERSHIP_CAPACITY))
        .with_ownership_capacity(NonZeroUsize::new(OWNERSHIP_CAPACITY))
        .with_grace(Duration::from_secs(5))
}

pub fn instant_task(name: impl Into<Arc<str>>) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    TaskSpec::new(
        name,
        task,
        RestartPolicy::Never,
        BackoffPolicy::default(),
        None,
    )
}

pub async fn expect_within<F: Future>(label: &str, future: F) -> F::Output {
    tokio::time::timeout(WATCHDOG, future)
        .await
        .unwrap_or_else(|_| panic!("benchmark timed out while waiting for {label}"))
}

pub async fn expect_completed(waiter: TaskWaiter) {
    let outcome = expect_within("a completed task", waiter.wait())
        .await
        .expect("task outcome channel closed");
    assert!(
        matches!(outcome, TaskOutcome::Completed),
        "expected Completed, got {outcome:?}"
    );
}

pub async fn expect_canceled(waiter: TaskWaiter) {
    let outcome = expect_within("a canceled task", waiter.wait())
        .await
        .expect("task outcome channel closed");
    assert!(
        matches!(outcome, TaskOutcome::Canceled),
        "expected Canceled, got {outcome:?}"
    );
}

pub async fn complete_batch(handle: &SupervisorHandle, tasks: Vec<TaskSpec>) {
    expect_within("a watched task batch", async {
        let mut waiters = Vec::with_capacity(tasks.len());
        for task in tasks {
            let (_, waiter) = handle
                .add_and_watch(task)
                .await
                .expect("batch admission failed");
            waiters.push(waiter);
        }
        for waiter in waiters {
            let outcome = waiter.wait().await.expect("batch outcome channel closed");
            assert!(
                matches!(outcome, TaskOutcome::Completed),
                "expected Completed, got {outcome:?}"
            );
        }
    })
    .await;
}

pub async fn wait_for_ownership(handle: &SupervisorHandle, retained_units: usize) {
    expect_within("ownership capacity and deferred cleanup to reset", async {
        loop {
            let snapshot = handle.ownership_snapshot();
            assert_eq!(snapshot.retired(), Some(0), "ownership capacity retired");
            if snapshot.in_use() == Some(retained_units)
                && snapshot.waiters == 0
                && snapshot.cleanup_queued == 0
                && snapshot.cleanup_running == 0
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

pub async fn warm_runtime(handle: &SupervisorHandle, retained_units: usize) {
    let (_, waiter) = expect_within(
        "warmup admission",
        handle.add_and_watch(instant_task("warmup")),
    )
    .await
    .expect("warmup admission failed");
    expect_completed(waiter).await;
    wait_for_ownership(handle, retained_units).await;
}

pub struct AsyncFlag {
    set: AtomicBool,
    changed: Notify,
}

impl AsyncFlag {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            set: AtomicBool::new(false),
            changed: Notify::new(),
        })
    }

    pub fn mark(&self) {
        self.set.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    pub async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.set.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
}

/// An observable monotonic counter for benchmark setup and finite task loops.
pub struct AsyncCounter {
    value: AtomicUsize,
    changed: Notify,
}

impl AsyncCounter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            value: AtomicUsize::new(0),
            changed: Notify::new(),
        })
    }

    pub fn increment(&self) -> usize {
        let value = self.value.fetch_add(1, Ordering::AcqRel) + 1;
        self.changed.notify_waiters();
        value
    }

    pub fn load(&self) -> usize {
        self.value.load(Ordering::Acquire)
    }

    pub async fn wait_for(&self, expected: usize) {
        expect_within("an observable counter threshold", async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.load() >= expected {
                    return;
                }
                changed.await;
            }
        })
        .await;
    }
}

/// Blocks only a dedicated callback or destructor worker, never a Tokio worker.
pub struct BlockingGate {
    released: Mutex<bool>,
    changed: Condvar,
    entered: Arc<AsyncFlag>,
    waiting: AtomicBool,
    timed_out: AtomicBool,
}

impl BlockingGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            released: Mutex::new(false),
            changed: Condvar::new(),
            entered: AsyncFlag::new(),
            waiting: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
        })
    }

    pub fn wait(&self) {
        let released = self.released.lock().expect("gate lock poisoned");
        self.waiting.store(true, Ordering::Release);
        self.entered.mark();
        let (released, _) = self
            .changed
            .wait_timeout_while(released, WATCHDOG, |released| !*released)
            .expect("gate lock poisoned while waiting");
        let was_released = *released;
        self.timed_out.store(!was_released, Ordering::Release);
        self.waiting.store(false, Ordering::Release);
        drop(released);
        assert!(was_released, "benchmark did not release its worker gate");
    }

    pub async fn wait_until_blocked(&self) {
        expect_within("a dedicated worker to enter the gate", self.entered.wait()).await;
        self.assert_blocked();
    }

    pub fn assert_blocked(&self) {
        self.assert_not_timed_out();
        assert!(self.waiting.load(Ordering::Acquire), "worker left its gate");
        assert!(
            !*self.released.lock().expect("gate lock poisoned"),
            "worker gate was already released"
        );
    }

    pub fn assert_not_timed_out(&self) {
        assert!(
            !self.timed_out.load(Ordering::Acquire),
            "worker gate timed out; this measurement is invalid"
        );
    }

    pub fn release(&self) {
        *self.released.lock().expect("gate lock poisoned") = true;
        self.changed.notify_all();
    }
}

/// Releases a worker even when a benchmark assertion unwinds.
pub struct ReleaseOnDrop(Arc<BlockingGate>);

impl ReleaseOnDrop {
    pub fn new(gate: Arc<BlockingGate>) -> Self {
        Self(gate)
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Counts the named event and exposes loss/failure diagnostics to the benchmark.
pub struct EventCounter {
    name: &'static str,
    kind: EventKind,
    count: AtomicUsize,
    dropped: AtomicU64,
    failures: AtomicUsize,
    changed: Notify,
}

impl EventCounter {
    pub fn new(name: &'static str, kind: EventKind) -> Arc<Self> {
        Arc::new(Self {
            name,
            kind,
            count: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            failures: AtomicUsize::new(0),
            changed: Notify::new(),
        })
    }

    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Acquire)
    }

    pub fn assert_healthy(&self) {
        self.assert_no_failures();
        assert_eq!(self.dropped(), 0, "{} lost lifecycle events", self.name);
    }

    pub fn assert_no_failures(&self) {
        assert_eq!(
            self.failures.load(Ordering::Acquire),
            0,
            "{} observed a runtime or task failure",
            self.name
        );
    }

    pub async fn wait_for_count(&self, expected: usize) {
        expect_within("the expected subscriber events", async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                self.assert_no_failures();
                if self.count() >= expected {
                    return;
                }
                changed.await;
            }
        })
        .await;
    }

    pub async fn wait_for_overflow_after(&self, previous: u64) {
        expect_within("a subscriber overflow diagnostic", async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                self.assert_no_failures();
                if self.dropped() > previous {
                    return;
                }
                changed.await;
            }
        })
        .await;
    }
}

impl Subscribe for EventCounter {
    fn on_event(&self, event: &Event) {
        if event.kind == self.kind {
            if self.kind == EventKind::TaskFinished
                && event.outcome_kind != Some(TaskOutcomeKind::Completed)
            {
                self.failures.fetch_add(1, Ordering::Release);
            } else {
                self.count.fetch_add(1, Ordering::Release);
            }
            self.changed.notify_waiters();
        } else if event.kind == EventKind::SubscriberOverflow {
            self.dropped
                .fetch_add(event.dropped.unwrap_or(1).max(1), Ordering::Release);
            self.changed.notify_waiters();
        } else if matches!(
            event.kind,
            EventKind::RuntimeFailure
                | EventKind::SubscriberPanicked
                | EventKind::OwnershipCapacityRetired
        ) {
            self.failures.fetch_add(1, Ordering::Release);
            self.changed.notify_waiters();
        }
    }

    fn name(&self) -> &str {
        self.name
    }

    fn queue_capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(EVENT_CAPACITY).unwrap()
    }
}
