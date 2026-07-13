//! Graceful-shutdown & run-completion integration tests.

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::Poll;
use std::time::Duration;

use common::*;
use taskvisor::prelude::*;

/// Task that ignores cancellation and sleeps far longer than any test grace.
fn make_stubborn(name: &str) -> TaskRef {
    TaskFn::arc(name, |_ctx: TaskContext| async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(())
    })
}

fn make_gated_cancel(
    name: &str,
    started: Arc<tokio::sync::Notify>,
    cancellation_seen: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
) -> TaskRef {
    TaskFn::arc(name, move |ctx: TaskContext| {
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

    fn queue_capacity(&self) -> usize {
        64
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

fn served(grace: Duration) -> (SupervisorHandle, Arc<EventCollector>) {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::builder(SupervisorConfig::default().with_grace(grace))
        .with_subscribers(subs)
        .build();
    (sup.serve(), collector)
}

#[tokio::test(flavor = "current_thread")]
async fn subscriber_deadline_bounds_explicit_shutdown() {
    let (subscriber, gate) = blocking_subscriber();
    let watchdog = spawn_callback_watchdog(Arc::clone(&gate));
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_subscriber_shutdown_timeout(Duration::from_millis(50))
        .with_subscribers(vec![subscriber as Arc<dyn Subscribe>])
        .build();
    let handle = supervisor.serve();

    let add_result = handle
        .add(TaskSpec::restartable(make_coop("subscriber-deadline")))
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
    let task = TaskFn::arc("natural-deadline", move |_ctx: TaskContext| {
        let task_gate = Arc::clone(&task_gate_for_task);
        async move {
            task_gate.notified().await;
            Ok(())
        }
    });
    let run_supervisor = Arc::clone(&supervisor);
    let mut run_task =
        tokio::spawn(async move { run_supervisor.run(vec![TaskSpec::once(task)]).await });

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

#[tokio::test(flavor = "current_thread")]
async fn shutdown_cooperative_returns_ok_emits_all_stopped_within_grace() {
    let (handle, collector) = served(Duration::from_secs(5));
    handle
        .add(TaskSpec::restartable(make_coop("c1")))
        .await
        .unwrap();
    handle
        .add(TaskSpec::restartable(make_coop("c2")))
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
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_shutdown_waiters_share_clean_result() {
    let (handle, collector) = served(Duration::from_secs(5));
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation_seen = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let id = handle
        .add(TaskSpec::restartable(make_gated_cancel(
            "shared-clean",
            Arc::clone(&started),
            Arc::clone(&cancellation_seen),
            Arc::clone(&release),
        )))
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
    let handle = supervisor.serve();
    handle
        .add(TaskSpec::restartable(make_coop("shared-subscriber-drain")))
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
    handle
        .add(TaskSpec::once(make_stubborn("shared-stuck-a")))
        .await
        .expect("first stubborn task must register");
    handle
        .add(TaskSpec::once(make_stubborn("shared-stuck-b")))
        .await
        .expect("second stubborn task must register");

    let late = handle.clone();
    let first = handle.clone();
    let second = handle;
    let (first_result, second_result) = with_timeout(5, async move {
        tokio::join!(first.shutdown(), second.shutdown())
    })
    .await;

    let (first_grace, first_stuck) = match first_result {
        Err(RuntimeError::GraceExceeded { grace, stuck }) => (grace, stuck),
        other => panic!("first caller must receive GraceExceeded, got {other:?}"),
    };
    let (second_grace, second_stuck) = match second_result {
        Err(RuntimeError::GraceExceeded { grace, stuck }) => (grace, stuck),
        other => panic!("second caller must receive GraceExceeded, got {other:?}"),
    };
    assert_eq!(first_grace, grace);
    assert_eq!(second_grace, grace);
    assert_eq!(first_stuck, second_stuck, "callers need the same snapshot");
    let (late_grace, late_stuck) = match late.shutdown().await {
        Err(RuntimeError::GraceExceeded { grace, stuck }) => (grace, stuck),
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
        .add(TaskSpec::restartable(make_gated_cancel(
            "dropped-shutdown-waiter",
            Arc::clone(&started),
            Arc::clone(&cancellation_seen),
            Arc::clone(&release),
        )))
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
    let (_, waiter) = handle
        .add_and_watch(TaskSpec::restartable(make_gated_cancel(
            "only-dropped-shutdown-waiter",
            Arc::clone(&started),
            Arc::clone(&cancellation_seen),
            Arc::clone(&release),
        )))
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
        "last-owner Drop must not replace an active graceful shutdown with zero-grace cleanup"
    );

    release.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(2), outcome)
        .await
        .expect("the detached graceful owner must finish")
        .expect("the watched task must keep its terminal outcome");
    assert!(matches!(outcome, TaskOutcome::Canceled));

    tokio::time::timeout(Duration::from_secs(2), async {
        while collector.count(EventKind::AllStoppedWithinGrace) == 0 {
            tokio::task::yield_now().await;
        }
    })
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
    let handle = supervisor.serve();
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation_seen = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let task = make_gated_cancel(
        "run-shutdown-owner",
        Arc::clone(&started),
        Arc::clone(&cancellation_seen),
        Arc::clone(&release),
    );

    let run_supervisor = Arc::clone(&supervisor);
    let run =
        tokio::spawn(async move { run_supervisor.run(vec![TaskSpec::restartable(task)]).await });
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
    let handle = supervisor.serve();
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation_seen = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    handle
        .add(TaskSpec::restartable(make_gated_cancel(
            "shutdown-before-run",
            Arc::clone(&started),
            Arc::clone(&cancellation_seen),
            Arc::clone(&release),
        )))
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
    handle
        .add(TaskSpec::once(make_stubborn("stubborn")))
        .await
        .unwrap();

    match with_timeout(5, handle.shutdown()).await {
        Err(RuntimeError::GraceExceeded { grace, stuck }) => {
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
    handle
        .add(TaskSpec::restartable(make_coop("coop")))
        .await
        .unwrap();
    handle
        .add(TaskSpec::once(make_stubborn("stuck")))
        .await
        .unwrap();

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
    handle
        .add(TaskSpec::once(make_stubborn("z")))
        .await
        .unwrap();

    match with_timeout(5, handle.shutdown()).await {
        Err(RuntimeError::GraceExceeded { grace, stuck }) => {
            assert_eq!(grace, Duration::ZERO);
            assert!(stuck.iter().any(|n| &**n == "z"));
        }
        other => panic!("expected GraceExceeded, got {other:?}"),
    }
    assert!(collector.find(EventKind::GraceExceeded).is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_emits_task_removed_for_each_drained_task() {
    let (handle, collector) = served(Duration::from_secs(5));
    let id_a = handle
        .add(TaskSpec::restartable(make_coop("a")))
        .await
        .unwrap();
    let id_b = handle
        .add(TaskSpec::restartable(make_coop("b")))
        .await
        .unwrap();

    with_timeout(5, handle.shutdown())
        .await
        .expect("shutdown ok");

    assert!(
        collector
            .by_id(id_a)
            .iter()
            .any(|e| e.kind == EventKind::TaskRemoved),
        "missing TaskRemoved for task a"
    );
    assert!(
        collector
            .by_id(id_b)
            .iter()
            .any(|e| e.kind == EventKind::TaskRemoved),
        "missing TaskRemoved for task b"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn run_returns_ok_when_all_oneshots_complete_and_registry_empties() {
    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    let specs = vec![
        TaskSpec::once(make_ok_once("o1")),
        TaskSpec::once(make_ok_once("o2")),
        TaskSpec::once(make_ok_once("o3")),
    ];
    with_timeout(5, sup.run(specs))
        .await
        .expect("run returns Ok once registry empties");
}

#[tokio::test(flavor = "current_thread")]
async fn run_blocks_while_gated_task_alive_then_unblocks_on_completion() {
    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    let gate = Arc::new(tokio::sync::Notify::new());

    let g = gate.clone();
    let task = TaskFn::arc("gated", move |_ctx: TaskContext| {
        let g = g.clone();
        async move {
            g.notified().await;
            Ok(())
        }
    });

    let sup2 = sup.clone();
    let jh = tokio::spawn(async move { sup2.run(vec![TaskSpec::once(task)]).await });

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!jh.is_finished(), "run() must block while a task is alive");

    gate.notify_one();
    with_timeout(5, jh)
        .await
        .expect("run() task should not panic")
        .expect("run returns Ok after the gated task completes");
}
