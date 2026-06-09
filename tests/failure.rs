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
async fn cooperative_cancellation_returning_ok_yields_task_stopped() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::builder(SupervisorConfig {
        grace: Duration::from_secs(5),
        ..Default::default()
    })
    .with_subscribers(subs)
    .build();
    let handle = sup.serve();

    with_timeout(10, async {
        let id = handle
            .add_and_wait(
                TaskSpec::restartable(make_coop("coop-ok")),
                Duration::from_secs(1),
            )
            .await
            .expect("add_and_wait ok");

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
async fn cancellation_returning_canceled_error_still_task_stopped() {
    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::builder(SupervisorConfig {
        grace: Duration::from_secs(5),
        ..Default::default()
    })
    .with_subscribers(subs)
    .build();
    let handle = sup.serve();

    let task = TaskFn::arc("cancel-err", |ctx: CancellationToken| async move {
        ctx.cancelled().await;
        Err(TaskError::Canceled)
    });

    with_timeout(10, async {
        let id = handle
            .add_and_wait(TaskSpec::restartable(task), Duration::from_secs(1))
            .await
            .expect("add_and_wait ok");

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
        assert!(by_id.iter().all(|e| e.kind != EventKind::ActorExhausted));
        assert!(by_id.iter().all(|e| e.kind != EventKind::ActorDead));
        assert!(by_id.iter().any(|e| e.kind == EventKind::TaskRemoved));

        let _ = handle.shutdown().await;
    })
    .await;
}
