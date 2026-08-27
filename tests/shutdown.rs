//! Graceful-shutdown & run-completion integration tests.

mod common;

use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::process::Command;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::task::Poll;
use std::time::{Duration, Instant};

use common::*;
use taskvisor::prelude::*;

fn make_gated_cancel(
    started: Arc<tokio::sync::Notify>,
    cancellation_seen: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
) -> TaskRef {
    TaskFn::arc(move |ctx: TaskContext| {
        let started = Arc::clone(&started);
        let cancellation_seen = Arc::clone(&cancellation_seen);
        let release = Arc::clone(&release);
        async move {
            started.notify_one();
            ctx.cancelled().await;
            cancellation_seen.notify_one();
            release.notified().await;
            Err(TaskError::Canceled)
        }
    })
}

async fn assert_pending_once<F: Future>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("future completed before the expected ordering point"),
    })
    .await;
}

#[derive(Default)]
struct CallbackGateState {
    entered: bool,
    released: bool,
    finished: bool,
    watchdog_fired: bool,
}

type CallbackGate = Arc<(Mutex<CallbackGateState>, Condvar)>;

#[derive(Default)]
struct DestructorGateState {
    entered: bool,
    released: bool,
    finished: bool,
}

type DestructorGate = Arc<(Mutex<DestructorGateState>, Condvar)>;

struct BlockingDropTask {
    started: Arc<tokio::sync::Notify>,
    gate: DestructorGate,
}

#[derive(Debug)]
struct PanickingSourceDrop {
    dropped: Arc<AtomicBool>,
}

impl fmt::Display for PanickingSourceDrop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("panicking source destructor")
    }
}

impl std::error::Error for PanickingSourceDrop {}

impl Drop for PanickingSourceDrop {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
        panic!("injected source destructor panic");
    }
}

impl Task for BlockingDropTask {
    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            started.notify_one();
            std::future::pending::<()>().await;
            Ok(())
        })
    }
}

impl Drop for BlockingDropTask {
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

struct BlockingSubscriber {
    gate: CallbackGate,
}

impl Subscribe for BlockingSubscriber {
    fn on_event(&self, _event: &Event) {
        let (state, ready) = &*self.gate;
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        if state.entered {
            return;
        }

        state.entered = true;
        ready.notify_all();
        while !state.released {
            state = ready.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.finished = true;
        ready.notify_all();
    }

    fn name(&self) -> &str {
        "blocking-shutdown"
    }

    fn execution(&self) -> SubscriberExecution {
        SubscriberExecution::Dedicated
    }

    fn queue_capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(64).unwrap()
    }
}

struct BlockingSubscriberTlsDrop {
    gate: CallbackGate,
}

impl Drop for BlockingSubscriberTlsDrop {
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

thread_local! {
    static SUBSCRIBER_TLS_DROP: std::cell::RefCell<Option<BlockingSubscriberTlsDrop>> =
        const { std::cell::RefCell::new(None) };
}

struct TlsSubscriber {
    callback: Arc<BlockingSubscriber>,
    tls_gate: CallbackGate,
}

impl Subscribe for TlsSubscriber {
    fn on_event(&self, event: &Event) {
        SUBSCRIBER_TLS_DROP.with(|value| {
            value
                .borrow_mut()
                .get_or_insert_with(|| BlockingSubscriberTlsDrop {
                    gate: Arc::clone(&self.tls_gate),
                });
        });
        self.callback.on_event(event);
    }

    fn name(&self) -> &str {
        "tls-shutdown"
    }

    fn execution(&self) -> SubscriberExecution {
        SubscriberExecution::Dedicated
    }

    fn queue_capacity(&self) -> NonZeroUsize {
        self.callback.queue_capacity()
    }
}

struct ReleaseCallbackGates(Vec<CallbackGate>);

impl Drop for ReleaseCallbackGates {
    fn drop(&mut self) {
        for gate in &self.0 {
            release_callback(gate);
        }
    }
}

fn blocking_subscriber() -> (Arc<BlockingSubscriber>, CallbackGate) {
    let gate = Arc::new((Mutex::new(CallbackGateState::default()), Condvar::new()));
    let subscriber = Arc::new(BlockingSubscriber {
        gate: Arc::clone(&gate),
    });
    (subscriber, gate)
}

fn spawn_callback_watchdog(gate: CallbackGate) -> std::thread::JoinHandle<()> {
    spawn_callback_watchdog_with_timeout(gate, Duration::from_secs(2))
}

fn spawn_callback_watchdog_with_timeout(
    gate: CallbackGate,
    timeout: Duration,
) -> std::thread::JoinHandle<()> {
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
            .wait_timeout_while(state, timeout, |state| !state.released)
            .unwrap_or_else(|e| e.into_inner());
        if !state.released {
            state.watchdog_fired = true;
            state.released = true;
            ready.notify_all();
        }
    })
}

fn release_callback(gate: &CallbackGate) {
    let (state, ready) = &**gate;
    state.lock().unwrap_or_else(|e| e.into_inner()).released = true;
    ready.notify_all();
}

async fn wait_for_callback(
    gate: &CallbackGate,
    predicate: impl Fn(&CallbackGateState) -> bool,
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

async fn wait_for_destructor(
    gate: &DestructorGate,
    predicate: impl Fn(&DestructorGateState) -> bool,
) -> bool {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let matches = {
                let state = gate.0.lock().unwrap_or_else(|error| error.into_inner());
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

fn release_destructor(gate: &DestructorGate) {
    let (state, ready) = &**gate;
    state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .released = true;
    ready.notify_all();
}

fn served(grace: Duration) -> (SupervisorHandle, Arc<EventCollector>) {
    served_with_collector(SupervisorConfig::default().with_grace(grace))
}

#[tokio::test(flavor = "current_thread")]
async fn subscriber_deadline_bounds_explicit_shutdown() {
    let (subscriber, gate) = blocking_subscriber();
    let watchdog = spawn_callback_watchdog(Arc::clone(&gate));
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_subscriber_shutdown_timeout(Duration::from_millis(50))
        .with_subscribers(vec![subscriber as Arc<dyn Subscribe>])
        .build();
    let handle = supervisor.serve().expect("runtime startup");

    let add_result = handle
        .add(TaskSpec::restartable("subscriber-deadline", make_coop()))
        .execute()
        .await;
    let callback_entered = wait_for_callback(&gate, |state| state.entered).await;
    let mut shutdown_task = tokio::spawn(async move { handle.shutdown().await });
    let shutdown_result = tokio::time::timeout(Duration::from_secs(5), &mut shutdown_task).await;
    let callback_was_still_running = !gate.0.lock().unwrap_or_else(|e| e.into_inner()).finished;

    release_callback(&gate);
    let callback_finished = wait_for_callback(&gate, |state| state.finished).await;
    watchdog.join().expect("watchdog thread must not panic");
    if shutdown_result.is_err() {
        shutdown_task.abort();
        let _ = shutdown_task.await;
    }
    let watchdog_stayed_idle = !gate
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .watchdog_fired;

    assert!(add_result.is_ok(), "the cooperative task must be admitted");
    assert!(callback_entered, "the blocking callback must start first");
    assert!(
        matches!(shutdown_result, Ok(Ok(Ok(())))),
        "explicit shutdown must return after the subscriber deadline"
    );
    assert!(
        callback_was_still_running,
        "Taskvisor must stop waiting without stopping the blocking callback"
    );
    assert!(callback_finished, "cleanup must release the callback");
    assert!(
        watchdog_stayed_idle,
        "the test must beat its safety watchdog"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn subscriber_deadline_bounds_natural_run_completion() {
    let (subscriber, gate) = blocking_subscriber();
    let watchdog = spawn_callback_watchdog(Arc::clone(&gate));
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_subscriber_shutdown_timeout(Duration::from_millis(50))
        .with_subscribers(vec![subscriber as Arc<dyn Subscribe>])
        .build();
    let task_gate = Arc::new(tokio::sync::Notify::new());
    let task_gate_for_task = Arc::clone(&task_gate);
    let task = TaskFn::arc(move |_ctx: TaskContext| {
        let task_gate = Arc::clone(&task_gate_for_task);
        async move {
            task_gate.notified().await;
            Ok(())
        }
    });
    let run_supervisor = Arc::clone(&supervisor);
    let mut run_task = tokio::spawn(async move {
        run_supervisor
            .run(vec![TaskSpec::once("natural-deadline", task)])
            .await
    });

    let callback_entered = wait_for_callback(&gate, |state| state.entered).await;
    task_gate.notify_one();
    let run_result = tokio::time::timeout(Duration::from_secs(5), &mut run_task).await;
    let callback_was_still_running = !gate.0.lock().unwrap_or_else(|e| e.into_inner()).finished;

    release_callback(&gate);
    let callback_finished = wait_for_callback(&gate, |state| state.finished).await;
    watchdog.join().expect("watchdog thread must not panic");
    if run_result.is_err() {
        run_task.abort();
        let _ = run_task.await;
    }
    let watchdog_stayed_idle = !gate
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .watchdog_fired;

    assert!(callback_entered, "the blocking callback must start first");
    assert!(
        matches!(run_result, Ok(Ok(Ok(())))),
        "natural run completion must return after the subscriber deadline"
    );
    assert!(
        callback_was_still_running,
        "run must stop waiting without stopping the blocking callback"
    );
    assert!(callback_finished, "cleanup must release the callback");
    assert!(
        watchdog_stayed_idle,
        "the test must beat its safety watchdog"
    );
}

const SUBSCRIBER_TLS_DROP_CHILD: &str = "TASKVISOR_SUBSCRIBER_TLS_DROP_CHILD";

#[test]
fn subscriber_tls_teardown_does_not_block_current_thread_runtime() {
    if std::env::var_os(SUBSCRIBER_TLS_DROP_CHILD).is_some() {
        subscriber_tls_teardown_child();
        return;
    }

    let mut child = Command::new(std::env::current_exe().expect("the test binary path must exist"))
        .arg("--exact")
        .arg("subscriber_tls_teardown_does_not_block_current_thread_runtime")
        .arg("--nocapture")
        .env(SUBSCRIBER_TLS_DROP_CHILD, "1")
        .spawn()
        .expect("the isolated TLS destructor test process must start");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .expect("the TLS test child must be waitable")
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the TLS destructor test exceeded its external process watchdog");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        status.success(),
        "the isolated TLS regression failed: {status}"
    );
}

fn subscriber_tls_teardown_child() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the isolated Tokio runtime must build");
    let tls_gate = Arc::new((Mutex::new(CallbackGateState::default()), Condvar::new()));
    let (first_callback, first_gate) = blocking_subscriber();
    let (second_callback, second_gate) = blocking_subscriber();
    let _release_on_drop = ReleaseCallbackGates(vec![
        Arc::clone(&tls_gate),
        Arc::clone(&first_gate),
        Arc::clone(&second_gate),
    ]);
    let watchdog =
        spawn_callback_watchdog_with_timeout(Arc::clone(&tls_gate), Duration::from_secs(10));

    runtime.block_on(async {
        let subscribers: Vec<Arc<dyn Subscribe>> = vec![
            Arc::new(TlsSubscriber {
                callback: first_callback,
                tls_gate: Arc::clone(&tls_gate),
            }),
            Arc::new(TlsSubscriber {
                callback: second_callback,
                tls_gate: Arc::clone(&tls_gate),
            }),
        ];
        let supervisor = Supervisor::builder(SupervisorConfig::default())
            .with_subscriber_shutdown_timeout(Duration::from_millis(100))
            .with_subscribers(subscribers)
            .build();
        let handle = supervisor.serve().expect("runtime startup");
        let waiter = handle
            .add(TaskSpec::once("subscriber-tls", make_ok_once()))
            .watch()
            .execute()
            .await
            .expect("the warmup task must be admitted");
        assert!(matches!(waiter.wait().await, Ok(TaskOutcome::Completed)));
        assert!(
            wait_for_callback(&first_gate, |state| state.entered).await,
            "the first callback must hold a worker"
        );
        assert!(
            wait_for_callback(&second_gate, |state| state.entered).await,
            "the second callback must acquire a separate worker"
        );
        release_callback(&first_gate);
        release_callback(&second_gate);

        let shutdown_task = tokio::spawn(async move { handle.shutdown().await });
        assert!(
            wait_for_callback(&tls_gate, |state| state.entered).await,
            "a dedicated subscriber worker must enter its real TLS destructor during shutdown"
        );
        assert_subscriber_tls_blocked(&tls_gate, "before the heartbeat");
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
        })
        .await
        .expect("the current-thread heartbeat task must complete");
        assert_subscriber_tls_blocked(&tls_gate, "after the heartbeat");

        tokio::time::timeout(Duration::from_secs(5), shutdown_task)
            .await
            .expect("public shutdown must not join a dedicated subscriber worker")
            .expect("the public shutdown task must not panic")
            .expect("public shutdown must finish");
        assert_subscriber_tls_blocked(&tls_gate, "after public shutdown");
    });

    drop(runtime);
    assert_subscriber_tls_blocked(&tls_gate, "after Tokio runtime teardown");
    release_callback(&tls_gate);
    watchdog
        .join()
        .expect("the TLS safety watchdog must not panic");
    let (state, ready) = &*tls_gate;
    let state = state.lock().unwrap_or_else(|error| error.into_inner());
    let (state, _) = ready
        .wait_timeout_while(state, Duration::from_secs(5), |state| !state.finished)
        .unwrap_or_else(|error| error.into_inner());
    assert!(state.finished, "the released TLS destructor must finish");
    assert!(
        !state.watchdog_fired,
        "progress must precede watchdog release"
    );
}

fn assert_subscriber_tls_blocked(gate: &CallbackGate, boundary: &str) {
    let state = gate.0.lock().unwrap_or_else(|error| error.into_inner());
    assert!(state.entered, "TLS Drop must start {boundary}");
    assert!(
        !state.released && !state.finished && !state.watchdog_fired,
        "TLS Drop must remain blocked {boundary}; the safety watchdog must not release it"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_cooperative_returns_ok_emits_all_stopped_within_grace() {
    let (handle, collector) = served(Duration::from_secs(5));
    let id_c1 = handle
        .add(TaskSpec::restartable("c1", make_coop()))
        .execute()
        .await
        .unwrap();
    let id_c2 = handle
        .add(TaskSpec::restartable("c2", make_coop()))
        .execute()
        .await
        .unwrap();

    with_timeout(5, handle.shutdown())
        .await
        .expect("cooperative tasks drain within grace → Ok");

    let requested = collector
        .find(EventKind::ShutdownRequested)
        .expect("ShutdownRequested");
    let all_stopped = collector
        .find(EventKind::AllStoppedWithinGrace)
        .expect("AllStoppedWithinGrace");
    assert_eq!(collector.count(EventKind::GraceExceeded), 0);
    assert!(
        requested.seq < all_stopped.seq,
        "ShutdownRequested must precede AllStopped"
    );
    for (id, name) in [(id_c1, "c1"), (id_c2, "c2")] {
        assert_eq!(
            collector
                .by_id(id)
                .iter()
                .filter(|event| event.kind == EventKind::TaskRemoved)
                .count(),
            1,
            "shutdown must emit exactly one TaskRemoved for {name}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_shutdown_waiters_share_clean_result() {
    let (handle, collector) = served(Duration::from_secs(5));
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation_seen = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let id = handle
        .add(TaskSpec::restartable(
            "shared-clean",
            make_gated_cancel(
                Arc::clone(&started),
                Arc::clone(&cancellation_seen),
                Arc::clone(&release),
            ),
        ))
        .execute()
        .await
        .expect("the gated task must register");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("the gated task must start");

    let late = handle.clone();
    let mut first = Box::pin(handle.clone().shutdown());
    let mut second = Box::pin(handle.shutdown());
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::select! {
            result = &mut first => panic!("first shutdown returned before task release: {result:?}"),
            result = &mut second => panic!("second shutdown returned before task release: {result:?}"),
            _ = cancellation_seen.notified() => {}
        }
    })
    .await
    .expect("the shared owner must cancel the task");
    assert_pending_once(first.as_mut()).await;
    assert_pending_once(second.as_mut()).await;

    release.notify_one();
    let (first_result, second_result) = tokio::join!(first, second);
    assert!(first_result.is_ok(), "first result: {first_result:?}");
    assert!(second_result.is_ok(), "second result: {second_result:?}");
    assert!(
        late.shutdown().await.is_ok(),
        "a late caller must receive the cached clean result"
    );

    assert_eq!(collector.count(EventKind::ShutdownRequested), 1);
    assert_eq!(collector.count(EventKind::AllStoppedWithinGrace), 1);
    assert_eq!(collector.count(EventKind::GraceExceeded), 0);
    assert_eq!(
        collector
            .by_id(id)
            .into_iter()
            .filter(|event| event.kind == EventKind::TaskRemoved)
            .count(),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_shutdown_waiters_share_subscriber_drain() {
    let (subscriber, gate) = blocking_subscriber();
    let watchdog = spawn_callback_watchdog(Arc::clone(&gate));
    let supervisor =
        Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_secs(5)))
            .with_subscriber_shutdown_timeout(Duration::from_secs(5))
            .with_subscribers(vec![subscriber as Arc<dyn Subscribe>])
            .build();
    let handle = supervisor.serve().expect("runtime startup");
    handle
        .add(TaskSpec::restartable(
            "shared-subscriber-drain",
            make_coop(),
        ))
        .execute()
        .await
        .expect("the cooperative task must register");
    assert!(
        wait_for_callback(&gate, |state| state.entered).await,
        "the blocking callback must start"
    );

    let mut first = Box::pin(handle.clone().shutdown());
    let mut second = Box::pin(handle.shutdown());
    assert_pending_once(first.as_mut()).await;
    assert_pending_once(second.as_mut()).await;

    release_callback(&gate);
    let (first_result, second_result) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(first, second)
    })
    .await
    .expect("both callers must finish after subscriber drain");
    assert!(first_result.is_ok(), "first result: {first_result:?}");
    assert!(second_result.is_ok(), "second result: {second_result:?}");
    assert!(
        wait_for_callback(&gate, |state| state.finished).await,
        "the callback must finish before the shared result is returned"
    );

    watchdog.join().expect("watchdog thread must not panic");
    assert!(
        !gate
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .watchdog_fired,
        "the test must beat its safety watchdog"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_shutdown_waiters_share_grace_exceeded() {
    let grace = Duration::from_millis(50);
    let (handle, collector) = served(grace);
    let (stubborn_a, started_a) = make_stubborn();
    let (stubborn_b, started_b) = make_stubborn();
    handle
        .add(TaskSpec::once("shared-stuck-a", stubborn_a))
        .execute()
        .await
        .expect("first stubborn task must register");
    handle
        .add(TaskSpec::once("shared-stuck-b", stubborn_b))
        .execute()
        .await
        .expect("second stubborn task must register");
    wait_for_start("shared-stuck-a", &started_a).await;
    wait_for_start("shared-stuck-b", &started_b).await;

    let late = handle.clone();
    let first = handle.clone();
    let second = handle;
    let (first_result, second_result) = with_timeout(5, async move {
        tokio::join!(first.shutdown(), second.shutdown())
    })
    .await;

    let (first_grace, first_stuck) = match first_result {
        Err(RuntimeError::GraceExceeded { grace, stuck, .. }) => (grace, stuck),
        other => panic!("first caller must receive GraceExceeded, got {other:?}"),
    };
    let (second_grace, second_stuck) = match second_result {
        Err(RuntimeError::GraceExceeded { grace, stuck, .. }) => (grace, stuck),
        other => panic!("second caller must receive GraceExceeded, got {other:?}"),
    };
    assert_eq!(first_grace, grace);
    assert_eq!(second_grace, grace);
    assert_eq!(first_stuck, second_stuck, "callers need the same snapshot");
    let (late_grace, late_stuck) = match late.shutdown().await {
        Err(RuntimeError::GraceExceeded { grace, stuck, .. }) => (grace, stuck),
        other => panic!("late caller must receive GraceExceeded, got {other:?}"),
    };
    assert_eq!(late_grace, grace);
    assert_eq!(
        late_stuck, first_stuck,
        "late caller needs the cached snapshot"
    );

    let mut names: Vec<_> = first_stuck.iter().map(|name| name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["shared-stuck-a", "shared-stuck-b"]);
    assert_eq!(collector.count(EventKind::ShutdownRequested), 1);
    assert_eq!(collector.count(EventKind::GraceExceeded), 1);
    assert_eq!(collector.count(EventKind::AllStoppedWithinGrace), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_first_shutdown_waiter_does_not_cancel_owner() {
    let (handle, collector) = served(Duration::from_secs(5));
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation_seen = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    handle
        .add(TaskSpec::restartable(
            "dropped-shutdown-waiter",
            make_gated_cancel(
                Arc::clone(&started),
                Arc::clone(&cancellation_seen),
                Arc::clone(&release),
            ),
        ))
        .execute()
        .await
        .expect("the gated task must register");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("the gated task must start");

    let first_handle = handle.clone();
    let first_waiter = tokio::spawn(async move { first_handle.shutdown().await });
    tokio::time::timeout(Duration::from_secs(2), cancellation_seen.notified())
        .await
        .expect("the detached owner must start task cancellation");
    first_waiter.abort();
    let _ = first_waiter.await;

    let mut second = Box::pin(handle.shutdown());
    assert_pending_once(second.as_mut()).await;
    release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .expect("the second waiter must observe owner completion");
    assert!(result.is_ok(), "joined shutdown result: {result:?}");
    assert_eq!(collector.count(EventKind::ShutdownRequested), 1);
    assert_eq!(collector.count(EventKind::AllStoppedWithinGrace), 1);
    assert_eq!(collector.count(EventKind::GraceExceeded), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_only_shutdown_waiter_does_not_override_detached_graceful_cleanup() {
    let (handle, collector) = served(Duration::from_secs(5));
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation_seen = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let waiter = handle
        .add(TaskSpec::restartable(
            "only-dropped-shutdown-waiter",
            make_gated_cancel(
                Arc::clone(&started),
                Arc::clone(&cancellation_seen),
                Arc::clone(&release),
            ),
        ))
        .watch()
        .execute()
        .await
        .expect("the gated task must register");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("the gated task must start");

    let shutdown_waiter = tokio::spawn(async move { handle.shutdown().await });
    tokio::time::timeout(Duration::from_secs(2), cancellation_seen.notified())
        .await
        .expect("the detached owner must start task cancellation");
    shutdown_waiter.abort();
    let _ = shutdown_waiter.await;

    let mut outcome = Box::pin(waiter.wait());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut outcome)
            .await
            .is_err(),
        "last-owner Drop must not replace active graceful shutdown with zero-grace cleanup"
    );

    release.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(2), outcome)
        .await
        .expect("the detached graceful owner must finish")
        .expect("the watched task must keep its terminal outcome");
    assert!(matches!(outcome, TaskOutcome::Canceled));

    collector
        .wait_for(EventKind::AllStoppedWithinGrace, Duration::from_secs(2))
        .await
        .expect("detached cleanup must publish its graceful result");
    assert_eq!(collector.count(EventKind::GraceExceeded), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn run_and_handle_shutdown_share_one_operation() {
    let collector = EventCollector::new();
    let supervisor =
        Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_secs(5)))
            .with_subscribers(vec![collector.clone() as Arc<dyn Subscribe>])
            .build();
    let handle = supervisor.serve().expect("runtime startup");
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation_seen = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let task = make_gated_cancel(
        Arc::clone(&started),
        Arc::clone(&cancellation_seen),
        Arc::clone(&release),
    );

    let run_supervisor = Arc::clone(&supervisor);
    let run = tokio::spawn(async move {
        run_supervisor
            .run(vec![TaskSpec::restartable("run-shutdown-owner", task)])
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("the static task must start");

    let mut shutdown = Box::pin(handle.shutdown());
    tokio::select! {
        result = &mut shutdown => panic!("shutdown returned before task release: {result:?}"),
        _ = cancellation_seen.notified() => {}
    }
    release.notify_one();

    let shutdown_result = tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("handle shutdown must finish");
    let run_result = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("run must join shared shutdown")
        .expect("run task must not panic");
    assert!(shutdown_result.is_ok(), "shutdown: {shutdown_result:?}");
    assert!(run_result.is_ok(), "run: {run_result:?}");
    assert_eq!(collector.count(EventKind::ShutdownRequested), 1);
    assert_eq!(collector.count(EventKind::AllStoppedWithinGrace), 1);
    assert_eq!(collector.count(EventKind::GraceExceeded), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn run_joins_shutdown_that_started_first() {
    let collector = EventCollector::new();
    let supervisor =
        Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_secs(5)))
            .with_subscribers(vec![collector.clone() as Arc<dyn Subscribe>])
            .build();
    let handle = supervisor.serve().expect("runtime startup");
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation_seen = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    handle
        .add(TaskSpec::restartable(
            "shutdown-before-run",
            make_gated_cancel(
                Arc::clone(&started),
                Arc::clone(&cancellation_seen),
                Arc::clone(&release),
            ),
        ))
        .execute()
        .await
        .expect("the gated task must register");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("the gated task must start");

    let mut shutdown = Box::pin(handle.shutdown());
    tokio::select! {
        result = &mut shutdown => panic!("shutdown returned before task release: {result:?}"),
        _ = cancellation_seen.notified() => {}
    }

    let mut run = Box::pin(supervisor.run(vec![]));
    assert_pending_once(run.as_mut()).await;
    release.notify_one();
    let (shutdown_result, run_result) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(shutdown, run)
    })
    .await
    .expect("run and shutdown must finish together");

    assert!(shutdown_result.is_ok(), "shutdown: {shutdown_result:?}");
    assert!(run_result.is_ok(), "run: {run_result:?}");
    assert_eq!(collector.count(EventKind::ShutdownRequested), 1);
    assert_eq!(collector.count(EventKind::AllStoppedWithinGrace), 1);
    assert_eq!(collector.count(EventKind::GraceExceeded), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_stubborn_under_small_grace_returns_grace_exceeded_force_aborts() {
    let (handle, collector) = served(Duration::from_millis(200));
    let (stubborn, started) = make_stubborn();
    handle
        .add(TaskSpec::once("stubborn", stubborn))
        .execute()
        .await
        .unwrap();
    wait_for_start("stubborn", &started).await;

    match with_timeout(5, handle.shutdown()).await {
        Err(RuntimeError::GraceExceeded { grace, stuck, .. }) => {
            assert_eq!(grace, Duration::from_millis(200));
            assert!(stuck.iter().any(|n| &**n == "stubborn"));
        }
        other => panic!("expected GraceExceeded, got {other:?}"),
    }
    assert!(collector.find(EventKind::ShutdownRequested).is_some());
    assert!(collector.find(EventKind::GraceExceeded).is_some());
    assert_eq!(collector.count(EventKind::AllStoppedWithinGrace), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_task_destructor_cannot_extend_public_shutdown() {
    let grace = Duration::from_millis(20);
    let (handle, _collector) = served(grace);
    let started = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new((Mutex::new(DestructorGateState::default()), Condvar::new()));
    let task: TaskRef = Arc::new(BlockingDropTask {
        started: Arc::clone(&started),
        gate: Arc::clone(&gate),
    });

    handle
        .add(TaskSpec::once("blocking-task-drop", task))
        .execute()
        .await
        .expect("the blocking-drop task must register");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("the blocking-drop task must start");

    let shutdown = tokio::time::timeout(Duration::from_secs(1), handle.shutdown()).await;
    let destructor_entered = wait_for_destructor(&gate, |state| state.entered).await;

    release_destructor(&gate);
    let destructor_finished = wait_for_destructor(&gate, |state| state.finished).await;

    assert!(
        matches!(
            shutdown,
            Ok(Err(RuntimeError::GraceExceeded {
                grace: reported,
                ..
            })) if reported == grace
        ),
        "shutdown must report its configured deadline without waiting for Task::drop: {shutdown:?}"
    );
    assert!(
        destructor_entered,
        "terminal cleanup must hand the final task reference to destructor isolation"
    );
    assert!(
        destructor_finished,
        "the isolated destructor must finish after the test releases it"
    );
}

const PANICKING_SOURCE_DROP_CHILD: &str = "TASKVISOR_PANICKING_SOURCE_DROP_CHILD";

#[test]
fn panicking_error_source_destructor_cannot_break_public_shutdown() {
    if std::env::var_os(PANICKING_SOURCE_DROP_CHILD).is_some() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the isolated Tokio runtime must build");
        runtime.block_on(panicking_error_source_destructor_child());
        return;
    }

    let status = Command::new(std::env::current_exe().expect("the test binary path must exist"))
        .arg("--exact")
        .arg("panicking_error_source_destructor_cannot_break_public_shutdown")
        .arg("--nocapture")
        .env(PANICKING_SOURCE_DROP_CHILD, "1")
        .status()
        .expect("the isolated destructor-panic test process must start");
    assert!(
        status.success(),
        "the isolated destructor-panic regression failed: {status}"
    );
}

async fn panicking_error_source_destructor_child() {
    let (handle, _collector) = served(Duration::from_secs(1));
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let task_started = Arc::clone(&started);
    let source_dropped = Arc::clone(&dropped);
    let task = TaskFn::arc(move |ctx: TaskContext| {
        let started = Arc::clone(&task_started);
        let dropped = Arc::clone(&source_dropped);
        async move {
            started.notify_one();
            ctx.cancelled().await;
            Err(TaskError::fatal("terminal failure")
                .with_source(Box::new(PanickingSourceDrop { dropped })))
        }
    });

    handle
        .add(TaskSpec::once("panicking-source-drop", task))
        .execute()
        .await
        .expect("the source-drop task must register");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("the source-drop task must start");

    let shutdown = tokio::time::timeout(Duration::from_secs(1), handle.shutdown()).await;
    let source_drop_attempted = tokio::time::timeout(Duration::from_secs(1), async {
        while !dropped.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();

    assert!(
        matches!(shutdown, Ok(Ok(()))),
        "a panicking source destructor must not poison shutdown: {shutdown:?}"
    );
    assert!(
        source_drop_attempted,
        "the isolated executor must attempt the source destructor"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_empty_registry_returns_ok_all_stopped() {
    let (handle, collector) = served(SupervisorConfig::default().grace());

    with_timeout(5, handle.shutdown())
        .await
        .expect("empty registry drains instantly → Ok");

    assert!(collector.find(EventKind::ShutdownRequested).is_some());
    assert!(collector.find(EventKind::AllStoppedWithinGrace).is_some());
    assert_eq!(collector.count(EventKind::GraceExceeded), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_mixed_reports_only_stubborn_in_stuck() {
    let (handle, collector) = served(Duration::from_millis(500));
    let coop_started = Arc::new(tokio::sync::Notify::new());
    let task_started = Arc::clone(&coop_started);
    let coop = TaskFn::arc(move |ctx: TaskContext| {
        let started = Arc::clone(&task_started);
        async move {
            started.notify_one();
            ctx.cancelled().await;
            Ok(())
        }
    });
    let (stuck, stuck_started) = make_stubborn();
    handle
        .add(TaskSpec::restartable("coop", coop))
        .execute()
        .await
        .unwrap();
    handle
        .add(TaskSpec::once("stuck", stuck))
        .execute()
        .await
        .unwrap();
    wait_for_start("coop", &coop_started).await;
    wait_for_start("stuck", &stuck_started).await;

    match with_timeout(5, handle.shutdown()).await {
        Err(RuntimeError::GraceExceeded { stuck, .. }) => {
            assert!(stuck.iter().any(|n| &**n == "stuck"));
            assert!(
                !stuck.iter().any(|n| &**n == "coop"),
                "a cooperative task must not be reported as stuck"
            );
        }
        other => panic!("expected GraceExceeded, got {other:?}"),
    }
    assert!(collector.find(EventKind::GraceExceeded).is_some());
    assert_eq!(collector.count(EventKind::AllStoppedWithinGrace), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_zero_grace_force_terminates_stubborn_immediately() {
    let (handle, collector) = served(Duration::ZERO);
    let (stubborn, started) = make_stubborn();
    handle
        .add(TaskSpec::once("z", stubborn))
        .execute()
        .await
        .unwrap();
    wait_for_start("z", &started).await;

    match with_timeout(5, handle.shutdown()).await {
        Err(RuntimeError::GraceExceeded { grace, stuck, .. }) => {
            assert_eq!(grace, Duration::ZERO);
            assert!(stuck.iter().any(|n| &**n == "z"));
        }
        other => panic!("expected GraceExceeded, got {other:?}"),
    }
    assert!(collector.find(EventKind::GraceExceeded).is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn run_blocks_while_gated_task_alive_then_unblocks_on_completion() {
    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    let gate = Arc::new(tokio::sync::Notify::new());
    let started = Arc::new(tokio::sync::Notify::new());

    let g = gate.clone();
    let task_started = Arc::clone(&started);
    let task = TaskFn::arc(move |_ctx: TaskContext| {
        let g = g.clone();
        let started = Arc::clone(&task_started);
        async move {
            started.notify_one();
            g.notified().await;
            Ok(())
        }
    });

    let sup2 = sup.clone();
    let mut jh = tokio::spawn(async move { sup2.run(vec![TaskSpec::once("gated", task)]).await });

    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("the gated task must start");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut jh)
            .await
            .is_err(),
        "run() must remain pending while a registered task is alive"
    );

    gate.notify_one();
    with_timeout(5, jh)
        .await
        .expect("run() task should not panic")
        .expect("run returns Ok after the gated task completes");
}

#[tokio::test(flavor = "current_thread")]
async fn run_until_uses_application_owned_shutdown_future() {
    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation_seen = Arc::new(AtomicBool::new(false));
    let task_started = Arc::clone(&started);
    let task_cancellation_seen = Arc::clone(&cancellation_seen);
    let task = TaskFn::arc(move |ctx: TaskContext| {
        let started = Arc::clone(&task_started);
        let cancellation_seen = Arc::clone(&task_cancellation_seen);
        async move {
            started.notify_one();
            ctx.cancelled().await;
            cancellation_seen.store(true, Ordering::Release);
            Err(TaskError::Canceled)
        }
    });
    let (request_shutdown, shutdown_requested) = tokio::sync::oneshot::channel::<()>();
    let run_supervisor = Arc::clone(&supervisor);
    let run = tokio::spawn(async move {
        run_supervisor
            .run_until(vec![TaskSpec::once("run-until", task)], async move {
                let _ = shutdown_requested.await;
            })
            .await
    });

    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("the task must start before the application requests shutdown");
    request_shutdown
        .send(())
        .expect("the run_until shutdown future must still be live");
    tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("run_until must finish after its shutdown future")
        .expect("the run_until task must not panic")
        .expect("cooperative shutdown must succeed");

    assert!(
        cancellation_seen.load(Ordering::Acquire),
        "the application-owned future must start graceful task cancellation"
    );
}
