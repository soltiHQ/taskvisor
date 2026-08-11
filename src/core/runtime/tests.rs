use std::{
    future::Future,
    num::NonZeroUsize,
    pin::Pin,
    sync::{Arc, Condvar, Mutex, atomic::Ordering},
    task::Poll,
    time::Duration,
};

use tokio::{
    sync::{broadcast, mpsc, oneshot},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use super::{CoreSettings, SupervisorCore, shutdown_workflow::ShutdownTrigger};
use crate::{
    BoxTaskFuture, Task, TaskContext, TaskFn, TaskRef,
    core::{
        SupervisorConfig, TaskDefaults,
        deferred_drop::{OwnedTask, test_reservation},
        registry::{AddBatchItem, Registry, RemoveReplyRx},
    },
    error::RuntimeError,
    events::{Bus, Event, EventKind, TryRecvError},
    identity::TaskId,
    subscribers::{Subscribe, SubscriberSet},
    tasks::TaskSpec,
};

#[derive(Default)]
struct BlockingTaskNameState {
    entered: bool,
    released: bool,
}

struct BlockingTaskName {
    gate: Arc<(Mutex<BlockingTaskNameState>, Condvar)>,
}

impl Task for BlockingTaskName {
    fn name(&self) -> &str {
        let (state, changed) = &*self.gate;
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        state.entered = true;
        changed.notify_all();
        while !state.released {
            state = changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        "blocking-direct-task-name"
    }

    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(std::future::pending())
    }
}

async fn wait_for_blocking_name(gate: &Arc<(Mutex<BlockingTaskNameState>, Condvar)>) {
    timeout(Duration::from_secs(2), async {
        loop {
            if gate
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .entered
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the fixed metadata worker must enter Task::name");
}

fn release_blocking_name(gate: &Arc<(Mutex<BlockingTaskNameState>, Condvar)>) {
    let (state, changed) = &**gate;
    state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .released = true;
    changed.notify_all();
}

struct RecordingSub {
    seen: Arc<Mutex<Vec<Event>>>,
    changed: tokio::sync::Notify,
}
impl RecordingSub {
    fn new() -> (Arc<Self>, Arc<Mutex<Vec<Event>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                seen: Arc::clone(&seen),
                changed: tokio::sync::Notify::new(),
            }),
            seen,
        )
    }

    async fn wait_for(&self, predicate: impl Fn(&Event) -> bool) {
        loop {
            let changed = self.changed.notified();
            if self.seen.lock().unwrap().iter().any(&predicate) {
                return;
            }
            changed.await;
        }
    }
}
impl Subscribe for RecordingSub {
    fn on_event(&self, e: &Event) {
        self.seen.lock().unwrap().push(e.clone());
        self.changed.notify_one();
    }
    fn name(&self) -> &str {
        "recorder"
    }
    fn queue_capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(8192).expect("the test subscriber queue is non-zero")
    }
}

struct NoopSub;

impl Subscribe for NoopSub {
    fn on_event(&self, _event: &Event) {}

    fn name(&self) -> &str {
        "noop"
    }

    fn queue_capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(64).expect("the test subscriber queue is non-zero")
    }
}

fn core(cfg: SupervisorConfig) -> Arc<SupervisorCore> {
    core_with_subs(cfg, Vec::new())
}

fn owned_task(spec: TaskSpec) -> OwnedTask<TaskSpec> {
    let retained = Arc::clone(spec.task());
    OwnedTask::new(spec, retained, test_reservation())
}

fn core_with_subs(
    cfg: SupervisorConfig,
    subs: Vec<Arc<dyn crate::subscribers::Subscribe>>,
) -> Arc<SupervisorCore> {
    let task_defaults = TaskDefaults::default();
    let bus = Bus::new(cfg.bus_capacity().get());
    let subs = Arc::new(SubscriberSet::new(subs, bus.clone()));
    let token = CancellationToken::new();
    let (cmd_tx, cmd_rx) = mpsc::channel(cfg.registry_queue_capacity().get());
    let registry = Registry::new(
        bus.clone(),
        token.clone(),
        None,
        cfg.grace(),
        task_defaults.clone(),
        cfg.max_registered_tasks(),
        cmd_rx,
    );
    SupervisorCore::new_internal(
        CoreSettings::new(cfg, task_defaults),
        bus,
        subs,
        registry,
        token,
        cmd_tx,
    )
}

async fn assert_pending_once<F: Future>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("future completed before the expected ordering point"),
    })
    .await;
}

fn core_with_full_command_queue() -> (Arc<SupervisorCore>, RemoveReplyRx) {
    let cfg =
        SupervisorConfig::default().with_registry_queue_capacity(NonZeroUsize::new(1).unwrap());
    let core = core(cfg);
    let filler_reply = core
        .enqueue_remove(TaskId::next(), None)
        .expect("the filler must occupy the only command queue slot");
    (core, filler_reply)
}

async fn start_and_release_command_queue(core: &SupervisorCore, filler_reply: RemoveReplyRx) {
    core.start();
    assert!(matches!(
        timeout(Duration::from_secs(2), filler_reply)
            .await
            .expect("the filler reply must resolve"),
        Ok(Ok(false))
    ));
}

#[derive(Clone, Copy, Debug)]
enum ManagementOperation {
    RemoveId,
    RemoveLabel,
    CancelId,
    CancelLabel,
    CancelIdWithTimeout,
    CancelLabelWithTimeout,
}

impl ManagementOperation {
    const ALL: [Self; 6] = [
        Self::RemoveId,
        Self::RemoveLabel,
        Self::CancelId,
        Self::CancelLabel,
        Self::CancelIdWithTimeout,
        Self::CancelLabelWithTimeout,
    ];

    async fn execute(self, core: &SupervisorCore, id: TaskId) -> Result<bool, RuntimeError> {
        match self {
            Self::RemoveId => core.remove(id).await,
            Self::RemoveLabel => core.remove_by_label(Arc::from("missing")).await,
            Self::CancelId => core.cancel(id).await,
            Self::CancelLabel => core.cancel_by_label(Arc::from("missing")).await,
            Self::CancelIdWithTimeout => core.cancel_with_timeout(id, Duration::ZERO).await,
            Self::CancelLabelWithTimeout => {
                core.cancel_by_label_with_timeout(Arc::from("missing"), Duration::ZERO)
                    .await
            }
        }
    }

    async fn try_execute(self, core: &SupervisorCore, id: TaskId) -> Result<bool, RuntimeError> {
        match self {
            Self::RemoveId => core.try_remove(id).await,
            Self::RemoveLabel => core.try_remove_by_label(Arc::from("missing")).await,
            Self::CancelId => core.try_cancel(id).await,
            Self::CancelLabel => core.try_cancel_by_label(Arc::from("missing")).await,
            Self::CancelIdWithTimeout => core.try_cancel_with_timeout(id, Duration::ZERO).await,
            Self::CancelLabelWithTimeout => {
                core.try_cancel_by_label_with_timeout(Arc::from("missing"), Duration::ZERO)
                    .await
            }
        }
    }

    fn publishes_identity_request(self) -> bool {
        matches!(self, Self::RemoveId)
    }
}

struct ControlledCancellationTask {
    task: TaskRef,
    cancellation_seen: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

fn controlled_cancellation_task(label: &'static str) -> ControlledCancellationTask {
    let cancellation_seen = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let seen_by_task = Arc::clone(&cancellation_seen);
    let release_by_task = Arc::clone(&release);
    let task = TaskFn::arc(label, move |ctx: TaskContext| {
        let cancellation_seen = Arc::clone(&seen_by_task);
        let release = Arc::clone(&release_by_task);
        async move {
            ctx.cancelled().await;
            cancellation_seen.notify_one();
            release.notified().await;
            Ok(())
        }
    });

    ControlledCancellationTask {
        task,
        cancellation_seen,
        release,
    }
}

fn signal_setup_source(result: Result<(), RuntimeError>) -> std::io::Error {
    match result {
        Err(RuntimeError::SignalSetupFailed { source }) => source,
        other => panic!("expected SignalSetupFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn subscriber_listener_reports_bus_lag_as_overflow() {
    let (recorder, _seen) = RecordingSub::new();

    let cfg = SupervisorConfig::default().with_bus_capacity(NonZeroUsize::new(2).unwrap());
    let core = core_with_subs(cfg, vec![recorder.clone()]);
    core.start();

    for i in 0..500 {
        core.bus
            .publish(Event::new(EventKind::AttemptStarting).with_task(format!("f{i}")));
    }

    let saw_lag = timeout(
        Duration::from_secs(2),
        recorder.wait_for(|event| {
            event.kind == EventKind::SubscriberOverflow
                && event.dropped.is_some_and(|dropped| dropped > 0)
                && event
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.starts_with("lagged("))
        }),
    )
    .await
    .is_ok();

    let _ = core.shutdown().await;
    assert!(
        saw_lag,
        "subscriber_listener must report bus lag with its typed dropped count"
    );
}

#[tokio::test]
async fn drain_pending_delivers_retained_tail_after_a_lag_gap() {
    let (recorder, seen) = RecordingSub::new();

    let bus = Bus::new(2);
    let mut rx = bus.take_receiver();
    let set = Arc::new(SubscriberSet::new(vec![recorder], bus.clone()));
    set.start();

    for i in 0..5 {
        bus.publish(Event::new(EventKind::AttemptStarting).with_task(format!("t{i}")));
    }

    SupervisorCore::drain_pending(&mut rx, &set).await;
    set.close().await;

    let delivered = seen.lock().unwrap();
    let newest = delivered
        .iter()
        .position(|event| {
            event.kind == EventKind::AttemptStarting && event.task.as_deref() == Some("t4")
        })
        .expect("newest retained event must reach subscribers despite a lag gap");
    let overflow: Vec<_> = delivered
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind == EventKind::SubscriberOverflow)
        .collect();
    assert_eq!(
        overflow.len(),
        1,
        "bounded shutdown drain must coalesce its lag accounting"
    );
    assert_eq!(overflow[0].1.dropped, Some(3));
    assert!(
        overflow[0].0 > newest,
        "shutdown drain must prioritize retained real events over its lag diagnostic"
    );
    assert!(
        delivered.iter().any(|event| {
            event.kind == EventKind::AttemptStarting && event.task.as_deref() == Some("t3")
        }),
        "the full retained tail must fit inside the fixed ring-sized drain budget"
    );
}

#[tokio::test]
async fn shutdown_relay_drain_has_a_fixed_work_budget() {
    let (recorder, seen) = RecordingSub::new();
    let capacity = super::event_relay::SHUTDOWN_RELAY_DRAIN_LIMIT + 2;
    let bus = Bus::new(capacity);
    let mut rx = bus.take_receiver();
    let set = Arc::new(SubscriberSet::new(vec![recorder], bus.clone()));
    set.start();

    for index in 0..capacity {
        bus.publish(Event::new(EventKind::AttemptStarting).with_task(format!("drain-{index}")));
    }

    SupervisorCore::drain_pending(&mut rx, &set).await;
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    bus.publish(Event::new(EventKind::AttemptStarting).with_task("late-after-relay-close"));
    assert!(
        matches!(rx.try_recv(), Err(TryRecvError::Empty)),
        "publication after the bounded relay drain must not repopulate the ring"
    );
    set.close().await;

    let delivered = seen.lock().unwrap_or_else(|error| error.into_inner());
    let real_events = delivered
        .iter()
        .filter(|event| event.kind == EventKind::AttemptStarting)
        .count();
    assert_eq!(
        real_events,
        super::event_relay::SHUTDOWN_RELAY_DRAIN_LIMIT,
        "shutdown relay work must not scale past its fixed event budget"
    );
    let overflow = delivered
        .iter()
        .find(|event| event.kind == EventKind::SubscriberOverflow)
        .expect("discarded retained events must be represented by one diagnostic");
    assert_eq!(overflow.dropped, Some(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuous_event_publication_cannot_extend_relay_shutdown() {
    let cfg = SupervisorConfig::default()
        .with_bus_capacity(NonZeroUsize::new(8).expect("the test bus is bounded"));
    let core = core_with_subs(cfg, vec![Arc::new(NoopSub)]);
    core.start();

    let bus = core.bus.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_publisher = Arc::clone(&stop);
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let publisher = std::thread::spawn(move || {
        let mut published = 0_u64;
        while !stop_publisher.load(Ordering::Acquire) {
            bus.publish_lazy(|| Event::new(EventKind::AttemptStarting));
            published = published.saturating_add(1);
            if published == 1_024 {
                let _ = started_tx.send(());
            }
        }
    });

    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the publisher must establish a continuous event flood");

    let shutdown = timeout(Duration::from_secs(2), core.shutdown()).await;
    stop.store(true, Ordering::Release);
    publisher
        .join()
        .expect("the event publisher thread must stop cleanly");

    assert!(
        matches!(shutdown, Ok(Ok(()))),
        "continuous publication must not extend relay shutdown: {shutdown:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_start_waiters_return_only_after_runtime_is_ready() {
    let core = core(SupervisorConfig::default());
    let startup = core
        .startup_gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let runtime = tokio::runtime::Handle::current();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let mut threads = Vec::new();

    for _ in 0..2 {
        let core = Arc::clone(&core);
        let runtime = runtime.clone();
        let ready = ready_tx.clone();
        let done = done_tx.clone();
        threads.push(std::thread::spawn(move || {
            let _runtime = runtime.enter();
            ready.send(()).expect("test receiver is alive");
            core.start();
            done.send(()).expect("test receiver is alive");
        }));
    }
    drop(ready_tx);
    drop(done_tx);

    for _ in 0..2 {
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("both start callers must reach the readiness gate");
    }
    assert!(!core.started.load(Ordering::Acquire));
    assert!(done_rx.try_recv().is_err());

    drop(startup);
    for _ in 0..2 {
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("both start callers must return after startup completes");
    }
    for thread in threads {
        thread.join().expect("start caller must not panic");
    }

    assert!(core.started.load(Ordering::Acquire));
    assert!(
        core.subscriber_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none(),
        "a runtime without subscribers must not allocate an event-relay task"
    );
    core.shutdown().await.expect("ready runtime must shut down");
}

#[tokio::test]
async fn natural_completion_publishes_all_stopped_within_grace() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let (recorder, seen) = RecordingSub::new();
    let core = core_with_subs(SupervisorConfig::default(), vec![recorder]);

    let task: TaskRef = TaskFn::arc("done", |_ctx: TaskContext| async move { Ok(()) });
    let res = timeout(Duration::from_secs(5), core.run(vec![TaskSpec::once(task)])).await;
    assert!(
        matches!(res, Ok(Ok(()))),
        "natural completion must return Ok, got {res:?}"
    );

    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|e| e.kind == EventKind::AllStoppedWithinGrace),
        "natural-completion success must publish a terminal verdict (AllStoppedWithinGrace)"
    );
}

#[tokio::test]
async fn run_is_single_shot() {
    let core = core(SupervisorConfig::default());

    let first = timeout(Duration::from_secs(5), core.run(vec![])).await;
    assert!(
        matches!(first, Ok(Ok(()))),
        "first run must succeed, got {first:?}"
    );

    let second = core.run(vec![]).await;
    assert!(
        matches!(second, Err(RuntimeError::AlreadyRunning)),
        "second run() must return AlreadyRunning, got {second:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_cancels_a_saturated_dynamic_ownership_wait() {
    let source = crate::core::deferred_drop::TestReservationSource::new(1);
    let held = source.try_reserve().expect("the test source has one slot");
    let core = core(SupervisorConfig::default());
    core.set_ownership_source_for_test(source);
    core.start();

    let task = TaskFn::arc("saturated-dynamic-add", |_ctx| async { Ok(()) });
    let mut add = Box::pin(core.add_task(TaskSpec::once(task)));
    assert_pending_once(add.as_mut()).await;

    timeout(Duration::from_secs(2), core.shutdown())
        .await
        .expect("shutdown must not wait for ownership capacity")
        .expect("the empty runtime must shut down cleanly");
    assert!(matches!(
        timeout(Duration::from_secs(1), add).await,
        Ok(Err(RuntimeError::ShuttingDown))
    ));
    drop(held);
}

#[tokio::test(flavor = "current_thread")]
async fn saturated_static_batch_fails_fast_without_consuming_run_lifecycle() {
    let source = crate::core::deferred_drop::TestReservationSource::new(1);
    let held = source.try_reserve().expect("the test source has one slot");
    let core = core(SupervisorConfig::default());
    core.set_ownership_source_for_test(source);
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let task_runs = Arc::clone(&runs);
    let task: crate::TaskRef = TaskFn::arc("saturated-static-run", move |_ctx| {
        task_runs.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    });
    let rejected = timeout(
        Duration::from_secs(2),
        core.run(vec![TaskSpec::once(Arc::clone(&task))]),
    )
    .await
    .expect("static ownership admission must be fail-fast");
    assert!(matches!(
        rejected,
        Err(RuntimeError::ResourceLimitReached {
            resource: crate::core::deferred_drop::OWNERSHIP_RESOURCE,
            limit: crate::core::deferred_drop::OWNERSHIP_CAPACITY,
        })
    ));
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    drop(held);
    timeout(Duration::from_secs(2), core.run(vec![TaskSpec::once(task)]))
        .await
        .expect("a corrected retry must finish")
        .expect("pre-start ownership rejection must not consume run");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_direct_task_name_cannot_hold_registry_capacity_or_extend_shutdown() {
    let _metadata_guard = crate::core::task_metadata::blocking_test_guard().await;
    let config = SupervisorConfig::default()
        .with_registry_queue_capacity(NonZeroUsize::new(1).expect("non-zero test capacity"));
    let core = core(config);
    core.start();

    let existing = TaskFn::arc("metadata-unrelated-direct", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Err(crate::TaskError::Canceled)
    });
    let existing_id = core
        .add_task(TaskSpec::once(existing))
        .await
        .expect("the unrelated task must register");

    let gate = Arc::new((Mutex::new(BlockingTaskNameState::default()), Condvar::new()));
    let mut blocked_add = Box::pin(core.try_add_task(TaskSpec::once(Arc::new(BlockingTaskName {
        gate: Arc::clone(&gate),
    }))));
    assert_pending_once(blocked_add.as_mut()).await;
    wait_for_blocking_name(&gate).await;

    let unrelated_cancel = timeout(Duration::from_secs(2), core.cancel(existing_id)).await;
    let shutdown = timeout(Duration::from_secs(2), core.shutdown()).await;

    // Release before asserting so a failed regression cannot occupy a global
    // metadata worker for the remainder of the test process.
    release_blocking_name(&gate);
    let blocked_result = timeout(Duration::from_secs(2), blocked_add).await;

    assert!(
        matches!(unrelated_cancel, Ok(Ok(true))),
        "a blocked try_add name must not hold the registry queue: {unrelated_cancel:?}"
    );
    assert!(
        matches!(shutdown, Ok(Ok(()))),
        "a blocked direct Task::name must not extend shared shutdown: {shutdown:?}"
    );
    assert!(matches!(
        blocked_result,
        Ok(Err(RuntimeError::ShuttingDown))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_static_task_name_cannot_extend_shared_shutdown() {
    let _metadata_guard = crate::core::task_metadata::blocking_test_guard().await;
    let core = core(SupervisorConfig::default());
    // `serve()` starts these listeners before exposing a public shutdown
    // handle; starting explicitly reproduces that public static-plus-handle path.
    core.start();
    let gate = Arc::new((Mutex::new(BlockingTaskNameState::default()), Condvar::new()));
    let mut run = Box::pin(core.run(vec![TaskSpec::once(Arc::new(BlockingTaskName {
        gate: Arc::clone(&gate),
    }))]));
    assert_pending_once(run.as_mut()).await;
    wait_for_blocking_name(&gate).await;

    let shutdown = timeout(Duration::from_secs(2), core.shutdown()).await;
    release_blocking_name(&gate);
    let run_result = timeout(Duration::from_secs(2), run).await;

    assert!(
        matches!(shutdown, Ok(Ok(()))),
        "static Task::name must run outside the Tokio shutdown path: {shutdown:?}"
    );
    assert!(matches!(run_result, Ok(Ok(()))));
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_static_batch_is_rejected_before_task_metadata() {
    use std::sync::atomic::AtomicUsize;

    struct CountedName {
        calls: Arc<AtomicUsize>,
        name: &'static str,
    }

    impl Task for CountedName {
        fn name(&self) -> &str {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.name
        }

        fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
            unreachable!("an oversized static batch must not spawn")
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let core = core(SupervisorConfig::default().with_max_registered_tasks(NonZeroUsize::new(1)));
    let tasks = ["oversized-a", "oversized-b"]
        .into_iter()
        .map(|name| {
            TaskSpec::once(Arc::new(CountedName {
                calls: Arc::clone(&calls),
                name,
            }) as TaskRef)
        })
        .collect();

    let result = core.run(tasks).await;
    assert!(matches!(
        result,
        Err(RuntimeError::ResourceLimitReached {
            resource: "registered_tasks",
            limit: 1,
        })
    ));
    assert_eq!(
        calls.load(Ordering::Acquire),
        0,
        "a deterministically oversized batch must not invoke Task::name"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn static_batch_that_cannot_fit_beside_subscribers_fails_without_waiting() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let task_calls = Arc::clone(&calls);
    let task: TaskRef = TaskFn::arc("ownership-self-deadlock", move |_ctx| {
        task_calls.fetch_add(1, Ordering::AcqRel);
        async { Ok(()) }
    });
    let core = core_with_subs(
        SupervisorConfig::default().with_max_registered_tasks(None),
        vec![Arc::new(NoopSub)],
    );
    let tasks = (0..crate::core::deferred_drop::OWNERSHIP_CAPACITY)
        .map(|_| TaskSpec::once(Arc::clone(&task)))
        .collect();

    let result = timeout(Duration::from_secs(1), core.run(tasks))
        .await
        .expect("an impossible self-owned batch must fail without waiting");
    assert!(matches!(
        result,
        Err(RuntimeError::ResourceLimitReached {
            resource: crate::core::deferred_drop::OWNERSHIP_RESOURCE,
            limit: crate::core::deferred_drop::OWNERSHIP_CAPACITY,
        })
    ));
    assert_eq!(
        calls.load(Ordering::Acquire),
        0,
        "an impossible ownership batch must be rejected before task execution"
    );
}

#[tokio::test]
async fn task_name_panic_does_not_consume_static_run_lifecycle() {
    struct PanickingName;

    impl crate::Task for PanickingName {
        fn name(&self) -> &str {
            panic!("injected static task-name panic")
        }

        fn spawn(&self, _ctx: TaskContext) -> crate::BoxTaskFuture {
            unreachable!("a task with invalid metadata must never be admitted")
        }
    }

    let core = core(SupervisorConfig::default());
    let bad: TaskRef = Arc::new(PanickingName);
    let first = crate::core::panic_guard::guarded(core.run(vec![TaskSpec::once(bad)])).await;
    assert!(
        first.is_err(),
        "the injected metadata panic must cross the static-run panic boundary"
    );

    let good: TaskRef = TaskFn::arc("after-name-panic", |_ctx| async { Ok(()) });
    let second = timeout(Duration::from_secs(5), core.run(vec![TaskSpec::once(good)]))
        .await
        .expect("a valid retry must not hang");
    assert_eq!(second.map_err(|error| error.as_label()), Ok(()));
}

#[tokio::test]
async fn shutdown_panic_still_runs_cleanup_before_caching_result() {
    let (recorder, seen) = RecordingSub::new();
    let core = core_with_subs(SupervisorConfig::default(), vec![recorder]);
    core.start();

    let result = core.join_shutdown(ShutdownTrigger::PanicForTest).await;
    assert!(
        matches!(result, Err(RuntimeError::ShuttingDown)),
        "a shutdown panic must become the shared fallback result: {result:?}"
    );
    assert!(core.runtime_token.is_cancelled());
    assert!(
        core.subscriber_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none(),
        "the subscriber listener must be joined before publishing the result"
    );

    let delivered_before_probe = seen.lock().unwrap().len();
    core.subs.emit_arc(Arc::new(
        Event::new(EventKind::AttemptStarting).with_task("closed-probe"),
    ));
    assert_eq!(
        seen.lock().unwrap().len(),
        delivered_before_probe,
        "subscriber channels must be closed before publishing the result"
    );
    assert!(
        matches!(core.shutdown().await, Err(RuntimeError::ShuttingDown)),
        "late callers must receive the cached fallback result"
    );
}

#[tokio::test]
async fn listener_join_failures_mark_shutdown_unclean() {
    for listener in ["registry", "subscriber"] {
        let core = if listener == "subscriber" {
            let (subscriber, _seen) = RecordingSub::new();
            core_with_subs(SupervisorConfig::default(), vec![subscriber])
        } else {
            core(SupervisorConfig::default())
        };
        core.start();
        match listener {
            "registry" => core.registry.abort_listener_for_test(),
            "subscriber" => core.abort_subscriber_listener_for_test(),
            _ => unreachable!(),
        }
        tokio::task::yield_now().await;

        let result = timeout(Duration::from_secs(2), core.shutdown())
            .await
            .unwrap_or_else(|_| panic!("shutdown hung after the {listener} listener failed"));
        assert!(
            matches!(result, Err(RuntimeError::ShuttingDown)),
            "a failed {listener} join must not be cached as clean: {result:?}"
        );
    }
}

#[tokio::test]
async fn add_is_rejected_once_shutting_down() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let core = core(SupervisorConfig::default());
    core.start();

    let early: TaskRef = TaskFn::arc("early", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    assert!(core.add_task(TaskSpec::restartable(early)).await.is_ok());

    core.mark_shutting_down();

    let late: TaskRef = TaskFn::arc("late", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    let res = core.add_task(TaskSpec::restartable(late)).await;
    assert!(
        matches!(res, Err(RuntimeError::ShuttingDown)),
        "add() after shutdown began must be rejected, got {res:?}"
    );

    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_fence_processes_committed_add_before_drain() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskError, TaskFn, TaskRef};

    let cfg = SupervisorConfig::default()
        .with_grace(Duration::from_secs(1))
        .with_registry_queue_capacity(NonZeroUsize::new(1).unwrap());
    let core = core(cfg);
    let accepted_id = TaskId::next();
    let accepted: TaskRef =
        TaskFn::arc("accepted-before-shutdown", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Err(TaskError::Canceled)
        });
    let (outcome, outcome_rx) = oneshot::channel();
    let (_, add_reply) = core
        .enqueue_add_task(accepted_id, TaskSpec::restartable(accepted), Some(outcome))
        .await
        .expect("the Add command must be committed before shutdown starts");

    let mut shutdown = Box::pin(core.shutdown());
    assert_pending_once(shutdown.as_mut()).await;
    assert!(core.is_shutting_down());

    let late_runs = Arc::new(AtomicUsize::new(0));
    let late_runs_by_task = Arc::clone(&late_runs);
    let late: TaskRef = TaskFn::arc("rejected-after-shutdown", move |_ctx: TaskContext| {
        late_runs_by_task.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    });
    assert!(matches!(
        core.add_task(TaskSpec::once(late)).await,
        Err(RuntimeError::ShuttingDown)
    ));

    core.start();
    assert!(matches!(
        timeout(Duration::from_secs(2), add_reply)
            .await
            .expect("the accepted Add must receive its registry reply"),
        Ok(Ok(()))
    ));
    timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("shutdown must pass the fence and finish")
        .expect("the accepted cooperative task must drain cleanly");
    timeout(Duration::from_secs(2), outcome_rx)
        .await
        .expect("the accepted watched task must receive a terminal outcome")
        .expect("the registry must keep the watched outcome sender");

    assert!(!core.contains_id(accepted_id).await);
    assert_eq!(late_runs.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_fence_processes_whole_committed_batch_before_drain() {
    use crate::{TaskContext, TaskError, TaskFn, TaskRef};

    let cfg = SupervisorConfig::default()
        .with_grace(Duration::from_secs(1))
        .with_registry_queue_capacity(NonZeroUsize::new(1).unwrap());
    let core = core(cfg);
    let mut events = core.bus.subscribe();
    let mut ids = Vec::new();
    let mut items = Vec::new();
    for label in ["batch-before-shutdown-a", "batch-before-shutdown-b"] {
        let task: TaskRef = TaskFn::arc(label, |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Err(TaskError::Canceled)
        });
        let id = TaskId::next();
        ids.push(id);
        items.push(AddBatchItem {
            id,
            label: Arc::from(label),
            owned: owned_task(TaskSpec::restartable(task)),
        });
    }
    let batch_reply = core
        .enqueue_add_batch_wait(items)
        .await
        .expect("the whole batch must commit before shutdown starts");

    let mut shutdown = Box::pin(core.shutdown());
    assert_pending_once(shutdown.as_mut()).await;
    core.start();

    assert!(matches!(
        timeout(Duration::from_secs(2), batch_reply)
            .await
            .expect("the committed batch reply must resolve"),
        Ok(Ok(()))
    ));
    timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("shutdown must pass the batch fence")
        .expect("the accepted batch must drain cleanly");
    assert!(core.registry.list().await.is_empty());

    let observed: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    for id in ids {
        assert!(
            observed
                .iter()
                .any(|event| { event.id == Some(id) && event.kind == EventKind::TaskAdded })
        );
        assert!(
            observed
                .iter()
                .any(|event| { event.id == Some(id) && event.kind == EventKind::TaskRemoved })
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn committed_duplicate_batch_keeps_its_error_during_shutdown() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskFn, TaskRef};

    let core = core(SupervisorConfig::default());
    let mut events = core.bus.subscribe();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut items = Vec::new();
    for label in ["shutdown-peer", "shutdown-duplicate", "shutdown-duplicate"] {
        let runs = Arc::clone(&runs);
        let task: TaskRef = TaskFn::arc(label, move |_ctx: TaskContext| {
            runs.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        items.push(AddBatchItem {
            id: TaskId::next(),
            label: Arc::from(label),
            owned: owned_task(TaskSpec::once(task)),
        });
    }
    let batch_reply = core
        .enqueue_add_batch_wait(items)
        .await
        .expect("the duplicate batch must commit before shutdown starts");

    let mut shutdown = Box::pin(core.shutdown());
    assert_pending_once(shutdown.as_mut()).await;
    core.start();

    let batch_result = timeout(
        Duration::from_secs(2),
        SupervisorCore::await_add_batch_reply(batch_reply),
    )
    .await
    .expect("the committed duplicate batch must receive its decision");
    assert!(matches!(
        batch_result,
        Err(RuntimeError::TaskAlreadyExists { name })
            if name.as_ref() == "shutdown-duplicate"
    ));
    timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("explicit shutdown must finish after the batch decision")
        .expect("the rejected batch leaves an empty clean runtime");
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    let observed: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    assert_eq!(
        observed
            .iter()
            .filter(|event| event.kind == EventKind::TaskAddFailed)
            .count(),
        3
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| event.kind == EventKind::TaskAdded)
            .count(),
        0
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| event.kind == EventKind::ShutdownRequested)
            .count(),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn backpressured_batch_loses_whole_admission_race_to_shutdown() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskFn, TaskRef};

    let cfg = SupervisorConfig::default()
        .with_grace(Duration::from_secs(1))
        .with_registry_queue_capacity(NonZeroUsize::new(1).unwrap());
    let core = core(cfg);
    let mut events = core.bus.subscribe();
    let filler_reply = core
        .enqueue_remove(TaskId::next(), None)
        .expect("the filler must occupy the command queue");
    let runs = Arc::new(AtomicUsize::new(0));
    let mut ids = Vec::new();
    let mut items = Vec::new();
    for label in ["batch-after-shutdown-a", "batch-after-shutdown-b"] {
        let runs = Arc::clone(&runs);
        let task: TaskRef = TaskFn::arc(label, move |_ctx: TaskContext| {
            runs.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });
        let id = TaskId::next();
        ids.push(id);
        items.push(AddBatchItem {
            id,
            label: Arc::from(label),
            owned: owned_task(TaskSpec::once(task)),
        });
    }

    let mut batch = Box::pin(core.enqueue_add_batch_wait(items));
    assert_pending_once(batch.as_mut()).await;
    let mut shutdown = Box::pin(core.shutdown());
    assert_pending_once(shutdown.as_mut()).await;
    core.start();

    timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("the fence must not wait for the backpressured batch")
        .expect("the empty runtime must shut down cleanly");
    assert!(matches!(
        timeout(Duration::from_secs(2), filler_reply)
            .await
            .expect("the filler reply must resolve"),
        Ok(Ok(false))
    ));
    assert!(matches!(
        timeout(Duration::from_secs(2), batch)
            .await
            .expect("the whole batch must wake after admission closes"),
        Err(RuntimeError::ShuttingDown)
    ));
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    while let Ok(event) = events.try_recv() {
        if let Some(id) = event.id {
            assert!(
                !ids.contains(&id) || event.kind != EventKind::TaskAddRequested,
                "a batch rejected behind the admission gate must stay silent"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unpolled_backpressured_add_does_not_block_shutdown_fence() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskFn, TaskRef};

    let cfg = SupervisorConfig::default()
        .with_grace(Duration::from_secs(1))
        .with_registry_queue_capacity(NonZeroUsize::new(1).unwrap());
    let core = core(cfg);
    let mut events = core.bus.subscribe();
    let filler_reply = core
        .enqueue_remove(TaskId::next(), None)
        .expect("the filler must occupy the command queue");

    let rejected_id = TaskId::next();
    let runs = Arc::new(AtomicUsize::new(0));
    let runs_by_task = Arc::clone(&runs);
    let rejected: TaskRef = TaskFn::arc("backpressured-at-shutdown", move |_ctx: TaskContext| {
        runs_by_task.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    });
    let mut add = Box::pin(core.enqueue_add_task_wait(rejected_id, TaskSpec::once(rejected), None));
    assert_pending_once(add.as_mut()).await;

    let mut shutdown = Box::pin(core.shutdown());
    assert_pending_once(shutdown.as_mut()).await;
    assert!(core.is_shutting_down());

    core.start();
    timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("the control fence must not wait for the backpressured Add")
        .expect("an empty registry must shut down cleanly");
    assert!(matches!(
        timeout(Duration::from_secs(2), filler_reply)
            .await
            .expect("the filler must receive its registry reply"),
        Ok(Ok(false))
    ));
    assert!(matches!(
        timeout(Duration::from_secs(2), add)
            .await
            .expect("the backpressured Add must wake after admission closes"),
        Err((RuntimeError::ShuttingDown, None))
    ));

    assert_eq!(runs.load(Ordering::SeqCst), 0);
    while let Ok(event) = events.try_recv() {
        assert!(
            event.id != Some(rejected_id) || event.kind != EventKind::TaskAddRequested,
            "an Add rejected behind the admission gate must stay silent"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn confirmed_add_waits_for_capacity_and_registry_reply() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let (core, filler_reply) = core_with_full_command_queue();

    let task: TaskRef = TaskFn::arc("backpressured-add", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    let mut add = Box::pin(core.add_task(TaskSpec::restartable(task)));
    assert_pending_once(add.as_mut()).await;
    assert!(core.id_for_label("backpressured-add").await.is_none());

    start_and_release_command_queue(&core, filler_reply).await;
    let id = timeout(Duration::from_secs(2), add)
        .await
        .expect("add must wake after capacity is released")
        .expect("registry must accept the task");
    assert!(core.contains_id(id).await);

    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn confirmed_management_operations_wait_for_capacity_and_registry_reply() {
    for operation in ManagementOperation::ALL {
        let (core, filler_reply) = core_with_full_command_queue();
        let mut events = core.bus.subscribe();
        let id = TaskId::next();
        let mut request = Box::pin(operation.execute(&core, id));

        assert_pending_once(request.as_mut()).await;
        assert!(
            std::iter::from_fn(|| events.try_recv().ok()).all(|event| {
                event.id != Some(id) || event.kind != EventKind::TaskRemoveRequested
            }),
            "{operation:?} must stay invisible before queue admission"
        );

        start_and_release_command_queue(&core, filler_reply).await;
        assert!(
            !timeout(Duration::from_secs(2), request)
                .await
                .unwrap_or_else(|_| panic!("{operation:?} did not wake after capacity release"))
                .unwrap_or_else(|error| panic!("{operation:?} registry reply failed: {error}")),
            "{operation:?} must report an unknown target as absent"
        );

        if operation.publishes_identity_request() {
            assert!(
                std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
                    event.id == Some(id) && event.kind == EventKind::TaskRemoveRequested
                }),
                "{operation:?} must publish its request after queue admission"
            );
        }

        core.shutdown().await.expect("test runtime must shut down");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn try_management_operations_wait_for_registry_decision_after_admission() {
    for operation in ManagementOperation::ALL {
        let core = core(SupervisorConfig::default());
        let mut request = Box::pin(operation.try_execute(&core, TaskId::next()));
        assert_pending_once(request.as_mut()).await;

        core.start();
        assert!(
            !timeout(Duration::from_secs(2), request)
                .await
                .unwrap_or_else(|_| panic!("{operation:?} did not wait for registry processing"))
                .unwrap_or_else(|error| panic!("{operation:?} registry reply failed: {error}")),
            "{operation:?} must report an unknown target as absent"
        );

        core.shutdown().await.expect("test runtime must shut down");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn try_cancel_by_label_with_timeout_bounds_terminal_completion() {
    let core = core(SupervisorConfig::default());
    core.start();

    let controlled = controlled_cancellation_task("timed-label");
    let id = core
        .add_task(TaskSpec::restartable(controlled.task))
        .await
        .expect("the task must be registered");

    match core
        .try_cancel_by_label_with_timeout(Arc::from("timed-label"), Duration::ZERO)
        .await
    {
        Err(RuntimeError::TaskTerminationTimeout {
            id: timed_id,
            timeout,
        }) => {
            assert_eq!(timed_id, id);
            assert_eq!(timeout, Duration::ZERO);
        }
        other => panic!("expected a terminal-completion timeout, got {other:?}"),
    }
    timeout(
        Duration::from_secs(2),
        controlled.cancellation_seen.notified(),
    )
    .await
    .expect("the timed-out caller must leave label cancellation running");

    controlled.release.notify_one();
    timeout(Duration::from_secs(2), core.registry.wait_until_empty())
        .await
        .expect("the task must finish after release");
    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn remove_by_label_orders_after_an_already_queued_add() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let core = core(SupervisorConfig::default());
    let mut events = core.bus.subscribe();
    let release = Arc::new(tokio::sync::Notify::new());
    let task_release = Arc::clone(&release);
    let task: TaskRef = TaskFn::arc("ordered-label", move |_ctx: TaskContext| {
        let release = Arc::clone(&task_release);
        async move {
            release.notified().await;
            Ok(())
        }
    });
    let id = TaskId::next();
    let (_, add_reply) = core
        .enqueue_add_task(id, TaskSpec::restartable(task), None)
        .await
        .expect("the Add command must enter the queue first");

    let mut remove = Box::pin(core.remove_by_label(Arc::from("ordered-label")));
    assert_pending_once(remove.as_mut()).await;
    core.start();

    assert!(matches!(
        timeout(Duration::from_secs(2), add_reply)
            .await
            .expect("Add reply must resolve"),
        Ok(Ok(()))
    ));
    assert!(
        timeout(Duration::from_secs(2), remove)
            .await
            .expect("label Remove must resolve")
            .expect("label Remove must receive a registry reply"),
        "the label lookup must happen after the queued Add is committed"
    );
    assert!(std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
        event.kind == EventKind::TaskRemoveRequested
            && event.id == Some(id)
            && event.task.as_deref() == Some("ordered-label")
    }));

    release.notify_one();
    timeout(Duration::from_secs(2), core.registry.wait_until_empty())
        .await
        .expect("the removed task must finish");
    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_by_label_orders_after_an_already_queued_add() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let core = core(SupervisorConfig::default());
    let mut events = core.bus.subscribe();
    let task: TaskRef = TaskFn::arc("ordered-cancel-label", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    let id = TaskId::next();
    let (_, add_reply) = core
        .enqueue_add_task(id, TaskSpec::restartable(task), None)
        .await
        .expect("the Add command must enter the queue first");

    let mut cancel = Box::pin(core.cancel_by_label(Arc::from("ordered-cancel-label")));
    assert_pending_once(cancel.as_mut()).await;
    core.start();

    assert!(matches!(
        timeout(Duration::from_secs(2), add_reply)
            .await
            .expect("Add reply must resolve"),
        Ok(Ok(()))
    ));
    assert!(
        timeout(Duration::from_secs(2), cancel)
            .await
            .expect("label Cancel must resolve after terminal cleanup")
            .expect("label Cancel must receive a registry reply"),
        "the label lookup must happen after the queued Add is committed"
    );
    assert!(std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
        event.kind == EventKind::TaskRemoveRequested
            && event.id == Some(id)
            && event.task.as_deref() == Some("ordered-cancel-label")
            && event.reason.as_deref() == Some("manual_cancel")
    }));
    assert!(!core.contains_id(id).await);

    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn backpressured_remove_returns_shutting_down_without_request_event() {
    let (core, filler_reply) = core_with_full_command_queue();
    let mut events = core.bus.subscribe();
    let remove_id = TaskId::next();
    let mut remove = Box::pin(core.remove(remove_id));
    assert_pending_once(remove.as_mut()).await;

    core.runtime_token.cancel();
    core.start();
    assert!(matches!(
        timeout(Duration::from_secs(2), remove)
            .await
            .expect("closing the queue must wake Remove"),
        Err(RuntimeError::ShuttingDown)
    ));
    let _ = timeout(Duration::from_secs(2), filler_reply)
        .await
        .expect("the buffered filler must still resolve");
    core.registry.join_listener().await;
    while let Ok(event) = events.try_recv() {
        assert!(
            event.id != Some(remove_id) || event.kind != EventKind::TaskRemoveRequested,
            "a Remove rejected before enqueue must not publish TaskRemoveRequested"
        );
    }

    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn backpressured_add_returns_shutting_down_when_queue_closes() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskFn, TaskRef};

    let (core, filler_reply) = core_with_full_command_queue();

    let runs = Arc::new(AtomicUsize::new(0));
    let task_runs = Arc::clone(&runs);
    let task: TaskRef = TaskFn::arc("closed-while-waiting", move |_ctx: TaskContext| {
        task_runs.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    });
    let mut add = Box::pin(core.add_task(TaskSpec::once(task)));
    assert_pending_once(add.as_mut()).await;

    core.runtime_token.cancel();
    core.start();
    assert!(matches!(
        timeout(Duration::from_secs(2), add)
            .await
            .expect("closing the queue must wake the waiting Add"),
        Err(RuntimeError::ShuttingDown)
    ));
    let _ = timeout(Duration::from_secs(2), filler_reply)
        .await
        .expect("the buffered filler must still resolve");
    core.registry.join_listener().await;
    assert!(core.id_for_label("closed-while-waiting").await.is_none());
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn try_add_reports_full_without_event_or_task_start() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskFn, TaskRef};

    let (core, filler_reply) = core_with_full_command_queue();
    let mut events = core.bus.subscribe();

    let runs = Arc::new(AtomicUsize::new(0));
    let task_runs = Arc::clone(&runs);
    let task: TaskRef = TaskFn::arc("try-add-full", move |_ctx: TaskContext| {
        task_runs.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    });
    assert!(matches!(
        core.try_add_task(TaskSpec::once(task)).await,
        Err(RuntimeError::CommandQueueFull)
    ));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(core.id_for_label("try-add-full").await.is_none());
    while let Ok(event) = events.try_recv() {
        assert!(
            event.kind != EventKind::TaskAddRequested
                || event.task.as_deref() != Some("try-add-full"),
            "an Add rejected before enqueue must not publish TaskAddRequested"
        );
    }

    start_and_release_command_queue(&core, filler_reply).await;
    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn try_add_waits_for_registry_decision_after_admission() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let core = core(SupervisorConfig::default());
    let task: TaskRef = TaskFn::arc("try-add-confirmed", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    let mut add = Box::pin(core.try_add_task(TaskSpec::restartable(task)));
    assert_pending_once(add.as_mut()).await;
    assert!(core.id_for_label("try-add-confirmed").await.is_none());

    core.start();
    let id = timeout(Duration::from_secs(2), add)
        .await
        .expect("try_add must resolve after the registry processes its command")
        .expect("registry must accept the task");
    assert!(core.contains_id(id).await);

    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn try_add_watched_returns_waiter_after_registry_admission() {
    use crate::{TaskContext, TaskFn, TaskOutcome, TaskRef};

    let core = core(SupervisorConfig::default());
    let task: TaskRef = TaskFn::arc("try-add-watched", |_ctx: TaskContext| async { Ok(()) });
    let mut add = Box::pin(core.try_add_task_watched(TaskSpec::once(task)));
    assert_pending_once(add.as_mut()).await;

    core.start();
    let (_id, outcome) = timeout(Duration::from_secs(2), add)
        .await
        .expect("try_add_and_watch must resolve after registry processing")
        .expect("registry must accept the watched task");
    assert!(matches!(
        timeout(Duration::from_secs(2), outcome).await,
        Ok(Ok(TaskOutcome::Completed))
    ));

    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_add_before_enqueue_rolls_back_admission() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskFn, TaskRef};

    let (core, filler_reply) = core_with_full_command_queue();

    let runs = Arc::new(AtomicUsize::new(0));
    let task_runs = Arc::clone(&runs);
    let task: TaskRef = TaskFn::arc("dropped-before-enqueue", move |_ctx: TaskContext| {
        task_runs.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    });
    let mut add = Box::pin(core.add_task(TaskSpec::once(task)));
    assert_pending_once(add.as_mut()).await;
    drop(add);

    start_and_release_command_queue(&core, filler_reply).await;
    assert!(core.id_for_label("dropped-before-enqueue").await.is_none());
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_add_after_enqueue_does_not_roll_command_back() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let core = core(SupervisorConfig::default());
    let (started_tx, started_rx) = oneshot::channel();
    let started_tx = Arc::new(Mutex::new(Some(started_tx)));
    let task_started = Arc::clone(&started_tx);
    let task: TaskRef = TaskFn::arc("dropped-after-enqueue", move |ctx: TaskContext| {
        let task_started = Arc::clone(&task_started);
        async move {
            if let Some(tx) = task_started.lock().unwrap().take() {
                let _ = tx.send(());
            }
            ctx.cancelled().await;
            Ok(())
        }
    });

    let mut events = core.bus.subscribe();
    let mut add = Box::pin(core.add_task(TaskSpec::once(task)));
    timeout(Duration::from_secs(2), async {
        loop {
            assert_pending_once(add.as_mut()).await;
            let mut committed = false;
            while let Ok(event) = events.try_recv() {
                if event.kind == EventKind::TaskAddRequested
                    && event.task.as_deref() == Some("dropped-after-enqueue")
                {
                    committed = true;
                    break;
                }
            }
            if committed {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the add must cross its documented command-acceptance boundary");
    drop(add);

    core.start();
    timeout(Duration::from_secs(2), started_rx)
        .await
        .expect("the queued task must start after its caller is dropped")
        .expect("the task must signal start");
    assert!(core.id_for_label("dropped-after-enqueue").await.is_some());

    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn bounded_command_queue_reports_full_and_recovers_capacity() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskFn, TaskOutcome, TaskRef};

    let (core, filler_reply) = core_with_full_command_queue();
    let mut events = core.bus.subscribe();

    let runs = Arc::new(AtomicUsize::new(0));
    let rejected_runs = Arc::clone(&runs);
    let rejected: TaskRef = TaskFn::arc("queue-full-add", move |_ctx: TaskContext| {
        rejected_runs.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    });
    let rejected_id = TaskId::next();
    let (outcome, outcome_rx) = oneshot::channel();
    let full_add = core
        .enqueue_add_task(rejected_id, TaskSpec::once(rejected), Some(outcome))
        .await;
    match full_add {
        Err((RuntimeError::CommandQueueFull, Some(returned))) => {
            returned
                .send(TaskOutcome::Rejected {
                    kind: crate::RejectionKind::AdmissionFailed,
                    reason: Arc::from("command_queue_full"),
                })
                .expect("the full command must return its outcome sender");
        }
        other => panic!("second command must report CommandQueueFull, got {other:?}"),
    }
    assert!(matches!(
        outcome_rx.await,
        Ok(TaskOutcome::Rejected { reason, .. }) if reason.as_ref() == "command_queue_full"
    ));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(!core.contains_id(rejected_id).await);
    while let Ok(event) = events.try_recv() {
        assert!(
            event.id != Some(rejected_id) || event.kind != EventKind::TaskAddRequested,
            "a command rejected before enqueue must not publish TaskAddRequested"
        );
    }

    let watched_runs = Arc::clone(&runs);
    let watched: TaskRef = TaskFn::arc("queue-full-watched-add", move |_ctx: TaskContext| {
        watched_runs.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    });
    assert!(matches!(
        core.try_add_task_watched(TaskSpec::once(watched)).await,
        Err(RuntimeError::CommandQueueFull)
    ));
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    for operation in ManagementOperation::ALL {
        let rejected_id = TaskId::next();
        assert!(
            matches!(
                operation.try_execute(&core, rejected_id).await,
                Err(RuntimeError::CommandQueueFull)
            ),
            "{operation:?} must fail fast when the command queue is full"
        );
        assert!(
            std::iter::from_fn(|| events.try_recv().ok()).all(|event| {
                event.id != Some(rejected_id) || event.kind != EventKind::TaskRemoveRequested
            }),
            "{operation:?} rejected before enqueue must not publish a request"
        );
    }

    start_and_release_command_queue(&core, filler_reply).await;

    let accepted: TaskRef = TaskFn::arc("capacity-recovered", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    let accepted_id = TaskId::next();
    let (_, accepted_reply) = core
        .enqueue_add_task(accepted_id, TaskSpec::restartable(accepted), None)
        .await
        .expect("capacity must recover after the filler is received");
    assert!(matches!(
        timeout(Duration::from_secs(2), accepted_reply)
            .await
            .expect("accepted add reply must resolve"),
        Ok(Ok(()))
    ));
    assert!(core.contains_id(accepted_id).await);

    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn static_run_batch_uses_one_queue_slot_with_lagged_observer() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskFn, TaskRef};

    let cfg = SupervisorConfig::default()
        .with_bus_capacity(NonZeroUsize::new(1).unwrap())
        .with_registry_queue_capacity(NonZeroUsize::new(1).unwrap());
    let core = core(cfg);
    let mut stale_events = core.bus.subscribe();
    for index in 0..4 {
        core.bus
            .publish(Event::new(EventKind::AttemptStarting).with_task(format!("noise-{index}")));
    }
    assert!(matches!(
        stale_events.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(_))
    ));
    let runs = Arc::new(AtomicUsize::new(0));
    let tasks = (0..4)
        .map(|index| {
            let runs = Arc::clone(&runs);
            let task: TaskRef = TaskFn::arc(format!("static-{index}"), move |_ctx: TaskContext| {
                runs.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            });
            TaskSpec::once(task)
        })
        .collect();

    timeout(Duration::from_secs(2), core.run(tasks))
        .await
        .expect("static run must not block on its bounded initial queue")
        .expect("static run must not fail when its batch exceeds queue capacity");
    assert_eq!(runs.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor = "current_thread")]
async fn closed_command_queue_returns_shutting_down_and_watcher() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{TaskContext, TaskFn, TaskOutcome, TaskRef};

    let core = core(SupervisorConfig::default());
    core.start();
    core.runtime_token.cancel();
    timeout(Duration::from_secs(2), core.registry.join_listener())
        .await
        .expect("registry listener must stop");
    let mut events = core.bus.subscribe();

    let remove_id = TaskId::next();
    assert!(matches!(
        core.remove(remove_id).await,
        Err(RuntimeError::ShuttingDown)
    ));
    while let Ok(event) = events.try_recv() {
        assert!(
            event.id != Some(remove_id) || event.kind != EventKind::TaskRemoveRequested,
            "a remove rejected by a closed queue must not publish TaskRemoveRequested"
        );
    }

    let runs = Arc::new(AtomicUsize::new(0));
    let rejected_runs = Arc::clone(&runs);
    let task: TaskRef = TaskFn::arc("closed-command", move |_ctx: TaskContext| {
        rejected_runs.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    });
    let (outcome, outcome_rx) = oneshot::channel();
    match core
        .enqueue_add_task(TaskId::next(), TaskSpec::once(task), Some(outcome))
        .await
    {
        Err((RuntimeError::ShuttingDown, Some(returned))) => {
            returned
                .send(TaskOutcome::Rejected {
                    kind: crate::RejectionKind::ControllerShuttingDown,
                    reason: Arc::from("shutting_down"),
                })
                .expect("closed queue must return its outcome sender");
        }
        other => panic!("closed command queue must return ShuttingDown, got {other:?}"),
    }
    assert!(matches!(
        outcome_rx.await,
        Ok(TaskOutcome::Rejected { reason, .. }) if reason.as_ref() == "shutting_down"
    ));
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    let _ = core.shutdown().await;
}

#[cfg(feature = "controller")]
#[tokio::test]
async fn add_task_with_id_watched_returns_watcher_on_failure() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let core = core(SupervisorConfig::default());
    core.mark_shutting_down(); // close the admission gate so the add fails

    let (tx, rx) = tokio::sync::oneshot::channel();
    let task: TaskRef = TaskFn::arc("x", |_ctx: TaskContext| async { Ok(()) });

    let res = core.add_task_with_id_watched(
        TaskId::next(),
        Arc::from("x"),
        owned_task(TaskSpec::once(task)),
        Some(tx),
    );
    match res {
        Err(uncommitted) => {
            let crate::core::UncommittedWatchedAdd {
                error,
                label,
                owned,
                done,
            } = *uncommitted;
            assert!(matches!(error, RuntimeError::ShuttingDown));
            assert_eq!(&*label, "x");
            assert_eq!(owned.value.name(), "x");
            let returned = done.expect("the watcher must be returned with the task spec");
            returned
                .send(crate::TaskOutcome::Rejected {
                    kind: crate::RejectionKind::AdmissionFailed,
                    reason: Arc::from("rejected"),
                })
                .expect("returned watcher must still be live");
            assert!(matches!(rx.await, Ok(crate::TaskOutcome::Rejected { .. })));
        }
        Ok(_) => panic!("add must hand the watcher and task spec back on failure"),
    }
}

#[cfg(feature = "controller")]
#[tokio::test]
async fn controller_completion_waits_for_registry_membership_cleanup() {
    use crate::{TaskContext, TaskFn, TaskRef};

    let core = core(SupervisorConfig::default());
    core.start();
    let id = TaskId::next();
    let task: TaskRef = TaskFn::arc("completion-cleanup", |_ctx: TaskContext| async { Ok(()) });
    let (reply, completion) = core
        .add_task_with_id_watched(
            id,
            Arc::from("completion-cleanup"),
            owned_task(TaskSpec::once(task)),
            None,
        )
        .expect("controller Add must enter the registry queue");

    assert!(matches!(reply.await, Ok(Ok(()))));
    timeout(Duration::from_secs(2), completion.wait_physical())
        .await
        .expect("controller completion must arrive after terminal cleanup");
    assert!(
        !core.contains_id(id).await,
        "completion must mean the registry id is gone"
    );

    let replacement: TaskRef =
        TaskFn::arc("completion-cleanup", |_ctx: TaskContext| async { Ok(()) });
    core.add_task(TaskSpec::once(replacement))
        .await
        .expect("completion must mean the registry label can be reused");

    let _ = core.shutdown().await;
}

#[tokio::test]
async fn signal_setup_error_surfaces_as_runtime_error_not_shutdown() {
    let core = core(SupervisorConfig::default());
    core.start();
    let mut rx = core.bus.subscribe();

    let err = std::io::Error::other("signal registration failed");
    let out = core
        .join_shutdown(ShutdownTrigger::SignalSetupFailed(Arc::new(err)))
        .await;

    assert!(
        matches!(out, Err(RuntimeError::SignalSetupFailed { .. })),
        "a signal-setup error must surface as SignalSetupFailed, got {out:?}"
    );

    let mut saw_shutdown = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev.kind, EventKind::ShutdownRequested) {
            saw_shutdown = true;
        }
    }
    assert!(
        !saw_shutdown,
        "a signal-setup error must NOT masquerade as a shutdown request"
    );
}

#[tokio::test(start_paused = true)]
async fn signal_setup_error_bounds_a_preexisting_removal_reporter() {
    let core = core(SupervisorConfig::default().with_grace(Duration::from_secs(60)));
    core.start();

    let controlled = controlled_cancellation_task("signal-setup-removing");
    let id = core
        .add_task(TaskSpec::once(Arc::clone(&controlled.task)))
        .await
        .expect("the stubborn task must register");
    let cancel_core = Arc::clone(&core);
    let cancel = tokio::spawn(async move { cancel_core.cancel(id).await });
    timeout(
        Duration::from_millis(10),
        controlled.cancellation_seen.notified(),
    )
    .await
    .expect("the prior cancellation must create a removal reporter");

    let original = std::io::Error::other("bounded signal setup failure");
    let result = timeout(
        Duration::from_millis(10),
        core.join_shutdown(ShutdownTrigger::SignalSetupFailed(Arc::new(original))),
    )
    .await
    .expect("signal-setup cleanup must not wait for the reporter's original grace");
    let source = signal_setup_source(result);
    assert_eq!(source.to_string(), "bounded signal setup failure");
    assert!(
        matches!(
            cancel.await.expect("the cancellation task must join"),
            Ok(true)
        ),
        "the prior cancellation must resolve after forced reporter cleanup"
    );

    let cached = signal_setup_source(core.shutdown().await);
    assert_eq!(cached.to_string(), "bounded signal setup failure");
}

#[tokio::test]
async fn signal_setup_error_keeps_custom_source_for_late_callers() {
    #[derive(Debug)]
    struct Marker;

    impl std::fmt::Display for Marker {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("custom signal marker")
        }
    }

    impl std::error::Error for Marker {}

    fn contains_marker(error: &(dyn std::error::Error + 'static)) -> bool {
        let mut current = Some(error);
        while let Some(error) = current {
            if error.downcast_ref::<Marker>().is_some() {
                return true;
            }
            current = error.source();
        }
        false
    }

    let core = core(SupervisorConfig::default());
    core.start();
    let original = std::io::Error::new(std::io::ErrorKind::PermissionDenied, Marker);

    let first = signal_setup_source(
        core.join_shutdown(ShutdownTrigger::SignalSetupFailed(Arc::new(original)))
            .await,
    );
    let late = signal_setup_source(core.shutdown().await);

    for source in [&first, &late] {
        assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(source.to_string(), "custom signal marker");
        assert!(
            contains_marker(source),
            "the original custom source must remain in the error chain"
        );
    }
}

#[tokio::test]
async fn signal_setup_error_keeps_raw_os_code_for_late_callers() {
    let core = core(SupervisorConfig::default());
    core.start();
    let original = std::io::Error::from_raw_os_error(2);

    let first = signal_setup_source(
        core.join_shutdown(ShutdownTrigger::SignalSetupFailed(Arc::new(original)))
            .await,
    );
    let late = signal_setup_source(core.shutdown().await);
    assert_eq!(first.raw_os_error(), Some(2));
    assert_eq!(late.raw_os_error(), Some(2));
}

#[tokio::test]
async fn real_signal_publishes_shutdown_requested() {
    let core = core(SupervisorConfig::default());
    core.start();
    let mut rx = core.bus.subscribe();

    let out = core.join_shutdown(ShutdownTrigger::Requested).await;
    assert!(out.is_ok(), "a real signal drains gracefully: {out:?}");

    let mut saw_shutdown = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev.kind, EventKind::ShutdownRequested) {
            saw_shutdown = true;
        }
    }
    assert!(saw_shutdown, "a real signal must publish ShutdownRequested");
}

#[tokio::test]
async fn cancel_uses_registry_completion_when_event_bus_lags() {
    use tokio::sync::broadcast::error::TryRecvError;

    let cfg = SupervisorConfig::default().with_bus_capacity(NonZeroUsize::new(1).unwrap());
    let core = core(cfg);
    core.start();

    let controlled = controlled_cancellation_task("laggy-cancel");
    let id = core
        .add_task(TaskSpec::restartable(controlled.task))
        .await
        .expect("add accepted");

    let mut stale_events = core.bus.subscribe();
    let receiver_count = core.bus.receiver_count();
    let mut cancel = Box::pin(core.cancel(id));
    tokio::select! {
        result = &mut cancel => panic!("cancel returned before actor termination: {result:?}"),
        _ = controlled.cancellation_seen.notified() => {}
    }
    assert_eq!(
        core.bus.receiver_count(),
        receiver_count,
        "cancel must not create a correctness receiver on the event bus"
    );
    assert_pending_once(cancel.as_mut()).await;

    for _ in 0..16 {
        core.bus
            .publish(Event::new(EventKind::AttemptStarting).with_task("noise"));
    }
    assert!(
        matches!(stale_events.try_recv(), Err(TryRecvError::Lagged(_))),
        "the observer must lag in this regression setup"
    );

    controlled.release.notify_one();
    assert!(
        timeout(Duration::from_secs(2), cancel)
            .await
            .expect("cancel must finish after terminal cleanup")
            .expect("cancel must receive a registry result")
    );
    assert!(
        !core.contains_id(id).await,
        "terminal completion must follow registry state cleanup"
    );

    let _ = core.shutdown().await;
}
