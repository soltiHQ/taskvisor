//! Failure-mode integration tests: exit-code propagation and graceful cancellation.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use taskvisor::prelude::*;

#[tokio::test(flavor = "current_thread")]
async fn fail_under_never_with_exit_code_propagates_to_exhausted() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

    let spec = TaskSpec::once(make_fail("fail-code", Some(7)));
    with_timeout(5, sup.run(vec![spec]))
        .await
        .expect("run() should return Ok");

    assert!(
        poll_until(Duration::from_secs(2), || async {
            collector.find(EventKind::ActorExhausted).is_some()
        })
        .await
    );

    let failed = collector.find(EventKind::TaskFailed).unwrap();
    let exhausted = collector.find(EventKind::ActorExhausted).unwrap();
    assert_eq!(failed.exit_code, Some(7));
    assert_eq!(exhausted.exit_code, Some(7));
    assert_eq!(failed.attempt, Some(1));
    assert!(
        !exhausted
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("max_retries_exceeded")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn logical_fail_without_code_yields_none_exit_code() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

    let spec = TaskSpec::once(make_fail("logical", None));
    with_timeout(5, sup.run(vec![spec]))
        .await
        .expect("run() should return Ok");

    assert!(
        poll_until(Duration::from_secs(2), || async {
            collector.find(EventKind::ActorExhausted).is_some()
        })
        .await
    );

    assert_eq!(
        collector.find(EventKind::TaskFailed).unwrap().exit_code,
        None
    );
    assert_eq!(
        collector.find(EventKind::ActorExhausted).unwrap().exit_code,
        None
    );
}

#[tokio::test(flavor = "current_thread")]
async fn panicking_task_is_reaped_and_run_returns() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

    let spec = TaskSpec::once(make_panic("boom"));
    with_timeout(5, sup.run(vec![spec]))
        .await
        .expect("run() should return Ok");

    assert!(
        poll_until(Duration::from_secs(2), || async {
            collector
                .by_label("boom")
                .iter()
                .any(|e| e.kind == EventKind::TaskRemoved)
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

    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

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
    with_timeout(5, sup.run(vec![spec]))
        .await
        .expect("run() should return Ok");

    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "panic must be retried per RestartPolicy::OnFailure"
    );
    assert!(
        poll_until(Duration::from_secs(2), || async {
            collector
                .by_label("flaky-panic")
                .iter()
                .any(|e| e.kind == EventKind::ActorExhausted)
        })
        .await,
        "actor must finish normally after panics are retried"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn task_returning_canceled_without_cancellation_is_reaped() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);

    // The task lies: returns Canceled while its token was never cancelled.
    // Worst case is Always, which would otherwise restart on any other return.
    let liar: TaskRef = TaskFn::arc("liar", |_ctx: TaskContext| async move {
        Err(TaskError::Canceled)
    });
    let spec = TaskSpec::restartable(liar).with_restart(RestartPolicy::Always { interval: None });

    with_timeout(5, sup.run(vec![spec]))
        .await
        .expect("run() should return Ok instead of leaking a dead actor");

    assert!(
        collector.any_reason_contains(EventKind::ActorExhausted, "task_returned_canceled"),
        "spurious Canceled must surface as ActorExhausted with an explicit reason"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cooperative_cancellation_returning_ok_yields_task_stopped() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_secs(5)))
        .with_subscribers(subs)
        .build();
    let handle = sup.serve();

    with_timeout(10, async {
        let id = handle
            .add(TaskSpec::restartable(make_coop("coop-ok")))
            .await
            .expect("add ok");

        assert!(handle.cancel(id).await.expect("cancel ok"));

        assert!(
            poll_until(Duration::from_secs(2), || async {
                collector
                    .by_id(id)
                    .iter()
                    .any(|e| e.kind == EventKind::TaskRemoved)
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
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::builder(SupervisorConfig::default().with_grace(Duration::from_secs(5)))
        .with_subscribers(subs)
        .build();
    let handle = sup.serve();

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
            poll_until(Duration::from_secs(2), || async {
                collector
                    .by_id(id)
                    .iter()
                    .any(|e| e.kind == EventKind::TaskRemoved)
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
