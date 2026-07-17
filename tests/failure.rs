//! Failure-mode integration tests: exit-code propagation and graceful cancellation.

mod common;

use std::num::NonZeroUsize;
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
async fn task_failure_exit_code_propagates_to_attempt_and_task_events() {
    for (name, expected_code) in [("fail-code", Some(7)), ("logical", None)] {
        let collector = run_to_completion(TaskSpec::once(make_fail(name, expected_code))).await;
        let finished = collector
            .wait_for(EventKind::TaskFinished, Duration::from_secs(2))
            .await
            .unwrap_or_else(|| panic!("{name}: TaskFinished was not observed"));
        let failed = collector
            .find(EventKind::AttemptFailed)
            .unwrap_or_else(|| panic!("{name}: AttemptFailed was not observed"));

        assert_eq!(failed.exit_code, expected_code, "{name}: AttemptFailed");
        assert_eq!(finished.exit_code, expected_code, "{name}: TaskFinished");
        assert_eq!(finished.outcome_kind, Some(TaskOutcomeKind::Failed));
        assert_eq!(failed.attempt, Some(1), "{name}: first attempt");
        assert!(
            finished.reason.is_some(),
            "{name}: diagnostic detail is retained"
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
        collector.any_reason_contains(EventKind::AttemptFailed, "panic"),
        "panic must surface as AttemptFailed with a panic reason"
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
                        && event.kind == EventKind::TaskFinished
                        && event.outcome_kind == Some(TaskOutcomeKind::Completed)
                })
            })
            .await,
        "task must finish normally after panics are retried"
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

    let finished = collector.find(EventKind::TaskFinished).unwrap();
    assert_eq!(finished.outcome_kind, Some(TaskOutcomeKind::Canceled));
    assert_eq!(
        finished.reason, None,
        "classification must not require reason text"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cooperative_cancellation_returning_ok_yields_succeeded_attempt_and_canceled_task() {
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
        assert!(by_id.iter().any(|e| e.kind == EventKind::AttemptSucceeded));
        assert!(by_id.iter().all(|e| e.kind != EventKind::AttemptFailed));
        assert!(by_id.iter().any(|e| {
            e.kind == EventKind::TaskFinished && e.outcome_kind == Some(TaskOutcomeKind::Canceled)
        }));
        assert!(by_id.iter().any(|e| e.kind == EventKind::TaskRemoved));

        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_returning_canceled_error_yields_canceled_attempt_and_task() {
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
            by_id.iter().any(|e| e.kind == EventKind::AttemptCanceled),
            "graceful cancellation must surface as AttemptCanceled"
        );
        assert!(
            by_id.iter().all(|e| e.kind != EventKind::AttemptSucceeded),
            "AttemptSucceeded is reserved for successful attempts"
        );
        assert!(by_id.iter().all(|e| e.kind != EventKind::AttemptFailed));
        assert!(by_id.iter().any(|e| {
            e.kind == EventKind::TaskFinished && e.outcome_kind == Some(TaskOutcomeKind::Canceled)
        }));
        assert!(by_id.iter().any(|e| e.kind == EventKind::TaskRemoved));

        let _ = handle.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_while_waiting_for_a_permit_finishes_without_an_attempt() {
    let (handle, collector) = served_with_collector(
        SupervisorConfig::default().with_max_concurrent(NonZeroUsize::new(1)),
    );
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let permit_owner: TaskRef = TaskFn::arc("permit-owner", {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        move |_ctx: TaskContext| {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            async move {
                started.notify_one();
                release.notified().await;
                Ok(())
            }
        }
    });

    handle.add(TaskSpec::once(permit_owner)).await.unwrap();
    started.notified().await;

    let (id, waiter) = handle
        .add_and_watch(TaskSpec::once(make_ok_once("permit-waiter")))
        .await
        .unwrap();
    assert!(handle.cancel(id).await.unwrap());
    assert!(matches!(
        waiter.wait().await.unwrap(),
        TaskOutcome::Canceled
    ));
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
        by_id
            .iter()
            .all(|event| event.kind != EventKind::AttemptStarting)
    );
    assert_eq!(
        by_id
            .iter()
            .filter(|event| {
                event.kind == EventKind::TaskFinished
                    && event.outcome_kind == Some(TaskOutcomeKind::Canceled)
            })
            .count(),
        1
    );

    release.notify_one();
    handle.shutdown().await.unwrap();
}
