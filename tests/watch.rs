//! Integration tests for `add_and_watch` / `TaskWaiter`.

mod common;

use std::time::Duration;

use common::*;
use taskvisor::prelude::*;

const ADD_TIMEOUT: Duration = Duration::from_secs(1);

fn supervisor() -> (std::sync::Arc<Supervisor>, SupervisorHandle) {
    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = sup.serve();
    (sup, handle)
}

#[tokio::test]
async fn outcome_reason_is_byte_identical_to_the_event_reason() {
    use std::sync::Arc;

    let collector = EventCollector::new();
    let subs: Vec<Arc<dyn Subscribe>> = vec![collector.clone() as Arc<dyn Subscribe>];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);
    let handle = sup.serve();

    let spec = TaskSpec::restartable(make_fail("drifter", Some(9)))
        .with_backoff(fast_backoff())
        .with_max_retries(2);
    let (id, waiter) = handle
        .add_and_watch(spec, ADD_TIMEOUT)
        .await
        .expect("add_and_watch should succeed");

    let outcome = with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored");

    assert!(
        poll_until(Duration::from_secs(2), || async {
            collector
                .by_id(id)
                .iter()
                .any(|e| e.kind == EventKind::ActorExhausted)
        })
        .await
    );
    let event = collector
        .by_id(id)
        .into_iter()
        .find(|e| e.kind == EventKind::ActorExhausted)
        .expect("ActorExhausted event for the run");

    match outcome {
        TaskOutcome::Failed {
            reason, exit_code, ..
        } => {
            assert_eq!(
                &*reason,
                event.reason.as_deref().expect("event carries a reason"),
                "TaskOutcome reason must be byte-identical to the ActorExhausted reason"
            );
            assert_eq!(exit_code, event.exit_code, "exit_code must match too");
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn completed_outcome_for_successful_once_task() {
    let (_sup, handle) = supervisor();

    let (id, waiter) = handle
        .add_and_watch(TaskSpec::once(make_ok_once("ok")), ADD_TIMEOUT)
        .await
        .expect("add_and_watch should succeed");
    assert_eq!(waiter.id(), id);

    let outcome = with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored");
    assert!(matches!(outcome, TaskOutcome::Completed));
    assert!(outcome.is_success());

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn failed_outcome_carries_reason_and_exit_code() {
    let (_sup, handle) = supervisor();

    let spec = TaskSpec::restartable(make_fail("flaky", Some(7)))
        .with_backoff(fast_backoff())
        .with_max_retries(2);
    let (_id, waiter) = handle
        .add_and_watch(spec, ADD_TIMEOUT)
        .await
        .expect("add_and_watch should succeed");

    match with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored")
    {
        TaskOutcome::Failed {
            reason, exit_code, ..
        } => {
            assert!(
                reason.contains("max_retries_exceeded"),
                "reason must mention exhausted retries: {reason}"
            );
            assert_eq!(exit_code, Some(7), "exit code must survive to the outcome");
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn fatal_outcome_for_fatal_error() {
    let (_sup, handle) = supervisor();

    let (_id, waiter) = handle
        .add_and_watch(
            TaskSpec::restartable(make_fatal("doomed", Some(137))),
            ADD_TIMEOUT,
        )
        .await
        .expect("add_and_watch should succeed");

    match with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored")
    {
        TaskOutcome::Fatal {
            reason, exit_code, ..
        } => {
            assert!(
                reason.contains("unrecoverable"),
                "reason must carry the fatal message: {reason}"
            );
            assert_eq!(exit_code, Some(137));
        }
        other => panic!("expected Fatal, got {other:?}"),
    }

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn failed_outcome_after_task_panic_with_never_policy() {
    let (_sup, handle) = supervisor();

    let (_id, waiter) = handle
        .add_and_watch(TaskSpec::once(make_panic("kaboom")), ADD_TIMEOUT)
        .await
        .expect("add_and_watch should succeed");

    match with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored")
    {
        TaskOutcome::Failed { reason, .. } => {
            assert!(
                reason.contains("panic"),
                "reason must mention the panic: {reason}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn spurious_canceled_return_resolves_canceled_outcome() {
    let (_sup, handle) = supervisor();

    let liar: TaskRef = TaskFn::arc("liar-watch", |_ctx: TaskContext| async {
        Err(TaskError::Canceled)
    });
    let (_id, waiter) = handle
        .add_and_watch(TaskSpec::restartable(liar), ADD_TIMEOUT)
        .await
        .expect("add_and_watch should succeed");

    let outcome = with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored");
    assert!(
        matches!(outcome, TaskOutcome::Canceled),
        "a task returning Canceled without cancellation must resolve as Canceled, got {outcome:?}"
    );

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn shutdown_drain_force_aborts_stubborn_watched_task() {
    let cfg = SupervisorConfig {
        grace: Duration::from_millis(150),
        ..Default::default()
    };
    let sup = Supervisor::new(cfg, vec![]);
    let handle = sup.serve();

    let stubborn: TaskRef = TaskFn::arc("stubborn-watch", |_ctx: TaskContext| async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok(())
    });
    let (_id, waiter) = handle
        .add_and_watch(TaskSpec::once(stubborn), ADD_TIMEOUT)
        .await
        .expect("add_and_watch should succeed");

    let (shutdown_res, outcome) = tokio::join!(handle.shutdown(), with_timeout(5, waiter.wait()));
    assert!(
        shutdown_res.is_err(),
        "stubborn task must trip GraceExceeded"
    );
    assert!(
        matches!(outcome.expect("waiter errored"), TaskOutcome::ForceAborted),
        "the shutdown drain's force-abort must resolve the waiter as ForceAborted"
    );
}

#[tokio::test]
async fn waiter_stays_pending_across_periodic_reruns() {
    let (_sup, handle) = supervisor();

    let spec =
        TaskSpec::restartable(make_ok_once("periodic-watch")).with_restart(RestartPolicy::Always {
            interval: Some(Duration::from_millis(20)),
        });
    let (id, waiter) = handle
        .add_and_watch(spec, ADD_TIMEOUT)
        .await
        .expect("add_and_watch should succeed");

    let pending = tokio::time::timeout(Duration::from_millis(200), waiter.wait()).await;
    assert!(
        pending.is_err(),
        "waiter must stay pending across successful Always re-runs"
    );

    let _ = handle.cancel(id).await;
    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn cancelled_outcome_when_task_is_cancelled() {
    let (_sup, handle) = supervisor();

    let (id, waiter) = handle
        .add_and_watch(TaskSpec::restartable(make_coop("coop")), ADD_TIMEOUT)
        .await
        .expect("add_and_watch should succeed");

    let removed = handle.cancel(id).await.expect("cancel should not error");
    assert!(removed, "existing task must report removed=true");

    let outcome = with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored");
    assert!(matches!(outcome, TaskOutcome::Canceled));

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn force_aborted_outcome_for_noncooperative_task() {
    let cfg = SupervisorConfig {
        grace: Duration::from_millis(100),
        ..Default::default()
    };
    let sup = Supervisor::new(cfg, vec![]);
    let handle = sup.serve();

    let stubborn: TaskRef = TaskFn::arc("stubborn", |_ctx: TaskContext| async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok(())
    });
    let (id, waiter) = handle
        .add_and_watch(TaskSpec::once(stubborn), ADD_TIMEOUT)
        .await
        .expect("add_and_watch should succeed");

    handle.remove(id).expect("remove should be accepted");

    let outcome = with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored");
    assert!(matches!(outcome, TaskOutcome::ForceAborted));

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn duplicate_name_returns_already_exists_not_a_waiter() {
    let (_sup, handle) = supervisor();

    let first = handle
        .add_and_watch(TaskSpec::restartable(make_coop("dup")), ADD_TIMEOUT)
        .await;
    assert!(first.is_ok(), "first add must succeed");

    let second = handle
        .add_and_watch(TaskSpec::restartable(make_coop("dup")), ADD_TIMEOUT)
        .await;
    assert!(
        matches!(second, Err(RuntimeError::TaskAlreadyExists { .. })),
        "duplicate add must surface TaskAlreadyExists, got {second:?}"
    );

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn shutdown_resolves_pending_waiters() {
    let (_sup, handle) = supervisor();

    let (_id, waiter) = handle
        .add_and_watch(TaskSpec::restartable(make_coop("worker")), ADD_TIMEOUT)
        .await
        .expect("add_and_watch should succeed");

    handle
        .clone()
        .shutdown()
        .await
        .expect("shutdown should be Ok");

    let outcome = with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored");
    assert!(
        matches!(outcome, TaskOutcome::Canceled),
        "cooperative task must resolve as Canceled on shutdown, got {outcome:?}"
    );
}

#[tokio::test]
async fn dropping_waiter_does_not_affect_task() {
    let (_sup, handle) = supervisor();

    let (id, waiter) = handle
        .add_and_watch(TaskSpec::restartable(make_coop("ignored")), ADD_TIMEOUT)
        .await
        .expect("add_and_watch should succeed");
    drop(waiter);

    assert!(
        poll_until(Duration::from_secs(2), || async {
            handle.is_alive("ignored").await
        })
        .await,
        "task must keep running after its waiter is dropped"
    );

    let removed = handle.cancel(id).await.expect("cancel should not error");
    assert!(removed);

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn outcome_is_delivered_even_under_bus_lag() {
    let cfg = SupervisorConfig {
        bus_capacity: 2,
        ..Default::default()
    };
    let sup = Supervisor::new(cfg, vec![]);
    let handle = sup.serve();

    let spec = TaskSpec::restartable(make_fail("noisy", None))
        .with_backoff(fast_backoff())
        .with_max_retries(5);
    let (_id, waiter) = handle
        .add_and_watch(spec, ADD_TIMEOUT)
        .await
        .expect("add_and_watch should succeed");

    match with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored")
    {
        TaskOutcome::Failed { .. } => {}
        other => panic!("expected Failed despite bus lag, got {other:?}"),
    }

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn task_error_source_survives_end_to_end_to_the_outcome() {
    let (_sup, handle) = supervisor();

    let task: TaskRef = TaskFn::arc("io-fail", |_ctx: TaskContext| async {
        Err(TaskError::fail_from(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        )))
    });

    let (_id, waiter) = handle
        .add_and_watch(TaskSpec::once(task), ADD_TIMEOUT)
        .await
        .expect("add_and_watch should succeed");

    let outcome = with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored");

    let source = outcome
        .source()
        .expect("the task error's source must survive to the completion plane");
    let io = source
        .downcast_ref::<std::io::Error>()
        .expect("source must downcast back to the original io::Error");
    assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);

    let _ = handle.shutdown().await;
}
