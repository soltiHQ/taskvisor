//! Graceful-shutdown & run-completion integration tests.

mod common;

use std::sync::{Arc, Condvar, Mutex};
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
    let sup = Supervisor::builder(SupervisorConfig {
        grace,
        ..Default::default()
    })
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
    let (handle, collector) = served(SupervisorConfig::default().grace);

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
