//! Failure-mode integration tests: exit-code propagation and graceful cancellation.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use taskvisor::prelude::*;

async fn run_to_completion(spec: TaskSpec) -> Arc<EventCollector> {
    let (supervisor, collector) = supervisor_with_collector(SupervisorConfig::default());
    with_timeout(5, supervisor.run(vec![spec]))
        .await
        .expect("run() should return Ok");
    collector
}

#[tokio::test(flavor = "current_thread")]
async fn task_failure_exit_code_propagates_to_terminal_events() {
    for (name, expected_code) in [("fail-code", Some(7)), ("logical", None)] {
        let collector = run_to_completion(TaskSpec::once(make_fail(name, expected_code))).await;
        let exhausted = collector
            .wait_for(EventKind::ActorExhausted, Duration::from_secs(2))
            .await
            .unwrap_or_else(|| panic!("{name}: ActorExhausted was not observed"));
        let failed = collector
            .find(EventKind::TaskFailed)
            .unwrap_or_else(|| panic!("{name}: TaskFailed was not observed"));

        assert_eq!(failed.exit_code, expected_code, "{name}: TaskFailed");
        assert_eq!(exhausted.exit_code, expected_code, "{name}: exhausted");
        assert_eq!(failed.attempt, Some(1), "{name}: first attempt");
        assert!(
            !exhausted
                .reason
                .as_deref()
                .unwrap_or("")
                .contains("max_retries_exceeded"),
            "{name}: RestartPolicy::Never is not retry-budget exhaustion"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn panicking_task_is_reaped_and_run_returns() {
    let collector = run_to_completion(TaskSpec::once(make_panic("boom"))).await;

    assert!(
        collector
            .wait_until(Duration::from_secs(2), |events| {
                events.iter().any(|event| {
                    event.task.as_deref() == Some("boom") && event.kind == EventKind::TaskRemoved
                })
            })
            .await,
        "panicked task must be reaped (TaskRemoved published)"
    );
    assert!(
        collector.any_reason_contains(EventKind::TaskFailed, "panic"),
        "panic must surface as TaskFailed with a panic reason"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn panicking_task_restarts_per_policy_then_succeeds() {
    use std::sync::atomic::{AtomicU32, Ordering};

    let attempts = Arc::new(AtomicU32::new(0));
    let attempts2 = Arc::clone(&attempts);
    let flaky: TaskRef = TaskFn::arc("flaky-panic", move |_ctx: TaskContext| {
        let attempts = Arc::clone(&attempts2);
        async move {
            if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                panic!("transient panic");
            }
            Ok(())
        }
    });

    let spec = TaskSpec::restartable(flaky).with_backoff(fast_backoff());
    let collector = run_to_completion(spec).await;

    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "panic must be retried per RestartPolicy::OnFailure"
    );
    assert!(
        collector
            .wait_until(Duration::from_secs(2), |events| {
                events.iter().any(|event| {
                    event.task.as_deref() == Some("flaky-panic")
                        && event.kind == EventKind::ActorExhausted
                })
            })
            .await,
        "actor must finish normally after panics are retried"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn task_returning_canceled_without_cancellation_is_reaped() {
    // The task lies: returns Canceled while its token was never cancelled.
    // Worst case is Always, which would otherwise restart on any other return.
    let liar: TaskRef = TaskFn::arc("liar", |_ctx: TaskContext| async move {
        Err(TaskError::Canceled)
    });
    let spec = TaskSpec::restartable(liar).with_restart(RestartPolicy::Always { interval: None });

    let collector = run_to_completion(spec).await;

    assert!(
        collector.any_reason_contains(EventKind::ActorExhausted, "task_returned_canceled"),
        "spurious Canceled must surface as ActorExhausted with an explicit reason"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cooperative_cancellation_returning_ok_yields_task_stopped() {
    let (handle, collector) =
        served_with_collector(SupervisorConfig::default().with_grace(Duration::from_secs(5)));

    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable(make_coop("coop-ok")))
            .await
            .expect("add ok");

        assert!(handle.cancel(id).await.expect("cancel ok"));

        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events
                        .iter()
                        .any(|event| event.id == Some(id) && event.kind == EventKind::TaskRemoved)
                })
                .await
        );

        let by_id = collector.by_id(id);
        assert!(by_id.iter().any(|e| e.kind == EventKind::TaskStopped));
        assert!(by_id.iter().all(|e| e.kind != EventKind::TaskFailed));
        assert!(by_id.iter().all(|e| e.kind != EventKind::ActorDead));
        assert!(by_id.iter().any(|e| e.kind == EventKind::TaskRemoved));

        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_returning_canceled_error_yields_task_canceled() {
    let (handle, collector) =
        served_with_collector(SupervisorConfig::default().with_grace(Duration::from_secs(5)));

    let task = TaskFn::arc("cancel-err", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Err(TaskError::Canceled)
    });

    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable(task))
            .await
            .expect("add ok");

        assert!(handle.cancel(id).await.expect("cancel ok"));

        assert!(
            collector
                .wait_until(Duration::from_secs(2), |events| {
                    events
                        .iter()
                        .any(|event| event.id == Some(id) && event.kind == EventKind::TaskRemoved)
                })
                .await
        );

        let by_id = collector.by_id(id);
        assert!(
            by_id.iter().any(|e| e.kind == EventKind::TaskCanceled),
            "graceful cancellation must surface as TaskCanceled"
        );
        assert!(
            by_id.iter().all(|e| e.kind != EventKind::TaskStopped),
            "TaskStopped is reserved for successful attempts"
        );
        assert!(by_id.iter().all(|e| e.kind != EventKind::TaskFailed));
        assert!(by_id.iter().all(|e| e.kind != EventKind::ActorExhausted));
        assert!(by_id.iter().all(|e| e.kind != EventKind::ActorDead));
        assert!(by_id.iter().any(|e| e.kind == EventKind::TaskRemoved));

        let _ = handle.shutdown().await;
    })
    .await;
}
