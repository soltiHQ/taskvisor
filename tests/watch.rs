//! Integration tests for `add(...).watch().execute()` / `TaskWaiter`.

mod common;

use std::num::NonZeroU32;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use common::*;
use taskvisor::prelude::*;

fn served() -> SupervisorHandle {
    Supervisor::new(SupervisorConfig::default(), vec![])
        .serve()
        .expect("runtime startup")
}

#[derive(Default)]
struct FinalDropGate {
    entered: AtomicBool,
    released: AtomicBool,
    panicking: AtomicBool,
}

struct ReleaseFinalDrop(Arc<FinalDropGate>);

impl Drop for ReleaseFinalDrop {
    fn drop(&mut self) {
        self.0.released.store(true, Ordering::Release);
    }
}

struct PanickingFinalDropTask {
    gate: Arc<FinalDropGate>,
}

impl Task for PanickingFinalDropTask {
    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(async { Ok(()) })
    }
}

impl Drop for PanickingFinalDropTask {
    fn drop(&mut self) {
        self.gate.entered.store(true, Ordering::Release);
        while !self.gate.released.load(Ordering::Acquire) {
            std::thread::park_timeout(Duration::from_millis(1));
        }
        self.gate.panicking.store(true, Ordering::Release);
        panic!("final retained task destructor panicked");
    }
}

#[tokio::test]
async fn outcome_reason_is_byte_identical_to_the_event_reason() {
    let (handle, collector) = served_with_collector(SupervisorConfig::default());

    let spec = TaskSpec::restartable("drifter", make_fail(Some(9)))
        .with_backoff(fast_backoff())
        .with_max_retries(NonZeroU32::new(2).unwrap());
    let waiter = handle
        .add(spec)
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");
    let id = waiter.id();

    let outcome = with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored");

    assert!(
        collector
            .wait_until(Duration::from_secs(2), |events| {
                events
                    .iter()
                    .any(|event| event.id == Some(id) && event.kind == EventKind::TaskFinished)
            })
            .await
    );
    let event = collector
        .by_id(id)
        .into_iter()
        .find(|e| e.kind == EventKind::TaskFinished)
        .expect("TaskFinished event for the run");
    assert_eq!(event.outcome_kind, Some(TaskOutcomeKind::Failed));

    match outcome {
        TaskOutcome::Failed {
            reason, exit_code, ..
        } => {
            assert!(reason.contains("boom"));
            assert_eq!(exit_code, Some(9));
            assert_eq!(
                &*reason,
                event.reason.as_deref().expect("event carries a reason"),
                "TaskOutcome reason must be byte-identical to the TaskFinished reason"
            );
            assert_eq!(exit_code, event.exit_code, "exit_code must match too");
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn watched_add_variants_return_the_same_completed_contract() {
    let handle = served();

    let waiter = handle
        .add(TaskSpec::once("ok", make_ok_once()))
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");
    let id = waiter.id();
    assert_eq!(waiter.id(), id);

    let outcome = with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored");
    assert!(matches!(outcome, TaskOutcome::Completed));
    assert!(outcome.is_success());

    let waiter = handle
        .add(TaskSpec::once("try-ok", make_ok_once()))
        .watch()
        .fail_fast()
        .execute()
        .await
        .expect("the management queue has capacity");
    let id = waiter.id();
    assert_eq!(waiter.id(), id);
    assert!(matches!(
        with_timeout(5, waiter.wait()).await,
        Ok(TaskOutcome::Completed)
    ));

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn completed_outcome_precedes_a_panicking_final_task_destructor() {
    let handle = served();
    let gate = Arc::new(FinalDropGate::default());
    let release_on_failure = ReleaseFinalDrop(Arc::clone(&gate));
    let task: TaskRef = Arc::new(PanickingFinalDropTask {
        gate: Arc::clone(&gate),
    });

    let waiter = handle
        .add(TaskSpec::once("panicking-final-task-drop", task))
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");

    assert!(
        poll_until(Duration::from_secs(2), || {
            let gate = Arc::clone(&gate);
            async move { gate.entered.load(Ordering::Acquire) }
        })
        .await,
        "final task destruction must reach the deferred-cleanup worker"
    );

    let outcome = with_timeout(2, waiter.wait())
        .await
        .expect("the terminal outcome must not wait for final task destruction");
    assert!(matches!(outcome, TaskOutcome::Completed));
    assert!(
        !gate.panicking.load(Ordering::Acquire),
        "the outcome must be fixed before the final destructor is released"
    );

    gate.released.store(true, Ordering::Release);
    assert!(
        poll_until(Duration::from_secs(2), || {
            let gate = Arc::clone(&gate);
            async move { gate.panicking.load(Ordering::Acquire) }
        })
        .await,
        "the released final task destructor must reach its panic"
    );
    assert!(matches!(outcome, TaskOutcome::Completed));

    drop(release_on_failure);
    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn fatal_outcome_for_fatal_error() {
    let handle = served();

    let waiter = handle
        .add(TaskSpec::restartable("doomed", make_fatal(Some(137))))
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");

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
    let handle = served();

    let waiter = handle
        .add(TaskSpec::once("kaboom", make_panic()))
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");

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
    let handle = served();

    let liar: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Err(TaskError::Canceled) });
    let waiter = handle
        .add(TaskSpec::restartable("liar-watch", liar))
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");

    let outcome = with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored");
    assert!(
        matches!(outcome, TaskOutcome::Canceled),
        "a task returning Canceled without cancellation must resolve as Canceled, got {outcome:?}"
    );

    let _ = handle.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_drain_force_aborts_stubborn_watched_task() {
    let cfg = SupervisorConfig::default().with_grace(Duration::from_millis(150));
    let sup = Supervisor::new(cfg, vec![]);
    let handle = sup.serve().expect("runtime startup");

    let (stubborn, started) = make_stubborn();
    let waiter = handle
        .add(TaskSpec::once("stubborn-watch", stubborn))
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");
    wait_for_start("stubborn-watch", &started).await;

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

#[tokio::test(start_paused = true)]
async fn waiter_stays_pending_across_periodic_reruns() {
    let handle = served();

    let spec = TaskSpec::restartable("periodic-watch", make_ok_once()).with_restart(
        RestartPolicy::Always {
            interval: Some(Duration::from_millis(20)),
        },
    );
    let waiter = handle
        .add(spec)
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");
    let id = waiter.id();

    let pending = tokio::time::timeout(Duration::from_millis(200), waiter.wait()).await;
    assert!(
        pending.is_err(),
        "waiter must stay pending across successful Always re-runs"
    );

    let _ = handle.cancel(id).execute().await;
    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn cancelled_outcome_when_task_is_cancelled() {
    let handle = served();

    let waiter = handle
        .add(TaskSpec::restartable("coop", make_coop()))
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");
    let id = waiter.id();

    let removed = handle
        .cancel(id)
        .execute()
        .await
        .expect("cancel should not error");
    assert!(removed, "existing task must report removed=true");

    let outcome = with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored");
    assert!(matches!(outcome, TaskOutcome::Canceled));

    let _ = handle.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn force_aborted_outcome_for_noncooperative_task() {
    let cfg = SupervisorConfig::default().with_grace(Duration::from_millis(100));
    let sup = Supervisor::new(cfg, vec![]);
    let handle = sup.serve().expect("runtime startup");

    let (stubborn, started) = make_stubborn();
    let waiter = handle
        .add(TaskSpec::once("stubborn", stubborn))
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");
    let id = waiter.id();
    wait_for_start("stubborn", &started).await;

    assert!(
        handle
            .cancel(id)
            .execute()
            .await
            .expect("cancel should be accepted"),
        "plain cancel must wait through registry force-abort without a caller timeout"
    );

    let outcome = with_timeout(5, waiter.wait())
        .await
        .expect("waiter errored");
    assert!(matches!(outcome, TaskOutcome::ForceAborted));

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn duplicate_name_returns_already_exists_not_a_waiter() {
    let handle = served();

    let first = handle
        .add(TaskSpec::restartable("dup", make_coop()))
        .watch()
        .execute()
        .await;
    assert!(first.is_ok(), "first add must succeed");

    let second = handle
        .add(TaskSpec::restartable("dup", make_coop()))
        .watch()
        .execute()
        .await;
    assert!(
        matches!(second, Err(RuntimeError::TaskAlreadyExists { .. })),
        "duplicate add must surface TaskAlreadyExists, got {second:?}"
    );

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn shutdown_resolves_pending_waiters() {
    let handle = served();

    let waiter = handle
        .add(TaskSpec::restartable("worker", make_coop()))
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");

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
    let handle = served();

    let waiter = handle
        .add(TaskSpec::restartable("ignored", make_coop()))
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");
    let id = waiter.id();
    drop(waiter);

    assert!(
        poll_until(Duration::from_secs(2), || async {
            handle.is_alive("ignored").await
        })
        .await,
        "task must keep running after its waiter is dropped"
    );

    let removed = handle
        .cancel(id)
        .execute()
        .await
        .expect("cancel should not error");
    assert!(removed);

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn outcome_is_delivered_even_under_bus_lag() {
    let cfg =
        SupervisorConfig::default().with_bus_capacity(std::num::NonZeroUsize::new(2).unwrap());
    let sup = Supervisor::new(cfg, vec![]);
    let handle = sup.serve().expect("runtime startup");

    let spec = TaskSpec::restartable("noisy", make_fail(None))
        .with_backoff(fast_backoff())
        .with_max_retries(NonZeroU32::new(5).unwrap());
    let waiter = handle
        .add(spec)
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");

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
    let handle = served();

    let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async {
        Err(TaskError::fail_from(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        )))
    });

    let waiter = handle
        .add(TaskSpec::once("io-fail", task))
        .watch()
        .execute()
        .await
        .expect("watched add should succeed");

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
