//! Runtime construction and public-owner Drop contracts.

mod common;

use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "controller")]
use common::poll_until;
use common::with_timeout;
use taskvisor::prelude::*;

struct NoopSubscriber;

impl Subscribe for NoopSubscriber {
    fn on_event(&self, _event: &Event) {}

    fn name(&self) -> &str {
        "noop"
    }
}

#[test]
fn build_with_subscribers_is_safe_outside_tokio() {
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(NoopSubscriber)];
    let supervisor = Supervisor::new(SupervisorConfig::default(), subscribers);

    assert_eq!(supervisor.runtime_config().bus_capacity().get(), 1024);
    drop(supervisor);
}

#[cfg(feature = "controller")]
#[test]
fn build_with_controller_is_safe_outside_tokio() {
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_subscribers(vec![Arc::new(NoopSubscriber)])
        .with_controller(ControllerConfig::default())
        .build();

    drop(supervisor);
}

#[test]
fn failed_serve_outside_tokio_does_not_poison_start() {
    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    assert!(
        matches!(
            supervisor.serve(),
            Err(RuntimeError::TokioRuntimeUnavailable)
        ),
        "serve outside Tokio must return its typed startup error"
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let handle = supervisor
            .serve()
            .expect("retry inside Tokio must start the runtime");
        handle
            .shutdown()
            .await
            .expect("retry inside Tokio must work");
    });
}

#[test]
fn dropping_last_public_owners_after_tokio_runtime_destruction_does_not_panic() {
    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let handle = runtime.block_on(async {
        let handle = supervisor.serve().expect("runtime startup");
        let task = TaskFn::arc(|_ctx| async {
            std::future::pending::<()>().await;
            Ok(())
        });
        handle
            .add(TaskSpec::once("runtime-destroyed-before-owners", task))
            .await
            .expect("the task must be registered before runtime destruction");
        handle
    });

    drop(runtime);
    let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(handle);
        drop(supervisor);
    }));
    assert!(
        dropped.is_ok(),
        "last-owner fallback must not require an ambient Tokio runtime"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_one_public_owner_keeps_other_owners_alive() {
    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    let first = supervisor.serve().expect("runtime startup");
    let second = first.clone();

    drop(supervisor);
    drop(first);

    let task = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    let (_, waiter) = second
        .add_and_watch(TaskSpec::once("owner-still-live", task))
        .await
        .expect("remaining handle must keep runtime open");
    assert!(matches!(
        with_timeout(2, waiter.wait()).await,
        Ok(TaskOutcome::Completed)
    ));
    second.shutdown().await.expect("shutdown must join");
}

#[tokio::test(flavor = "current_thread")]
async fn temporary_supervisor_transfers_ownership_to_serve_handle() {
    let handle = Supervisor::builder(SupervisorConfig::default())
        .build()
        .serve()
        .expect("runtime startup");
    let task = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    let (_, waiter) = handle
        .add_and_watch(TaskSpec::once("temporary-owner", task))
        .await
        .expect("serve handle must retain the public lease");

    assert!(matches!(
        with_timeout(2, waiter.wait()).await,
        Ok(TaskOutcome::Completed)
    ));
    handle.shutdown().await.expect("shutdown must join");
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_last_owner_cancels_a_running_task_without_blocking() {
    let started = Arc::new(tokio::sync::Notify::new());
    let canceled = Arc::new(tokio::sync::Notify::new());
    let task_started = Arc::clone(&started);
    let task_canceled = Arc::clone(&canceled);
    let task = TaskFn::arc(move |ctx: TaskContext| {
        let started = Arc::clone(&task_started);
        let canceled = Arc::clone(&task_canceled);
        async move {
            started.notify_one();
            ctx.cancelled().await;
            canceled.notify_one();
            Ok(())
        }
    });

    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = supervisor.serve().expect("runtime startup");
    handle
        .add(TaskSpec::once("last-owner-cancel", task))
        .await
        .expect("task must be admitted");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("task must start before the last owner is dropped");

    drop(supervisor);
    drop(handle);

    tokio::time::timeout(Duration::from_secs(2), canceled.notified())
        .await
        .expect("last-owner Drop must propagate cancellation");
}

#[tokio::test(flavor = "current_thread")]
async fn watched_task_resolves_after_last_owner_drop() {
    let started = Arc::new(tokio::sync::Notify::new());
    let task_started = Arc::clone(&started);
    let task = TaskFn::arc(move |_ctx: TaskContext| {
        let started = Arc::clone(&task_started);
        async move {
            started.notify_one();
            std::future::pending::<()>().await;
            Ok(())
        }
    });
    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = supervisor.serve().expect("runtime startup");
    let (_, waiter) = handle
        .add_and_watch(TaskSpec::once("abandoned-watcher", task))
        .await
        .expect("task must be admitted");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("task must start before owners are dropped");

    drop(supervisor);
    drop(handle);

    assert!(matches!(
        with_timeout(2, waiter.wait()).await,
        Ok(TaskOutcome::Canceled | TaskOutcome::ForceAborted)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_shutdown_keeps_its_result_while_other_owners_drop() {
    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = supervisor.serve().expect("runtime startup");
    let other = handle.clone();
    let task = TaskFn::arc(|ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    handle
        .add(TaskSpec::once("shutdown-owner", task))
        .await
        .expect("task must be admitted");

    let shutdown = tokio::spawn(async move { handle.shutdown().await });
    drop(supervisor);
    drop(other);

    assert!(matches!(with_timeout(2, shutdown).await, Ok(Ok(()))));
}

#[cfg(feature = "controller")]
#[tokio::test(flavor = "current_thread")]
async fn last_owner_drop_rejects_queued_controller_work() {
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_controller(ControllerConfig::default())
        .build();
    let handle = supervisor.serve().expect("runtime startup");
    let owner = TaskFn::arc(|ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    handle
        .submit(
            ControllerSpec::queue(TaskSpec::once("drop-slot-owner", owner)).with_slot("drop-slot"),
        )
        .await
        .expect("slot owner must be submitted");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            handle.controller_snapshot().await.is_some_and(|snapshot| {
                snapshot.slots.iter().any(|slot| {
                    slot.slot.as_ref() == "drop-slot" && slot.status == SlotStatusKind::Running
                })
            })
        })
        .await,
        "first submission must own the slot"
    );

    let queued = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    let (_, waiter) = handle
        .submit_and_watch(
            ControllerSpec::queue(TaskSpec::once("drop-slot-queued", queued))
                .with_slot("drop-slot"),
        )
        .await
        .expect("queued submission must enter controller intake");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            handle.controller_snapshot().await.is_some_and(|snapshot| {
                snapshot
                    .slots
                    .iter()
                    .any(|slot| slot.slot.as_ref() == "drop-slot" && slot.queue_depth == 1)
            })
        })
        .await,
        "second submission must be queued before owners drop"
    );

    drop(supervisor);
    drop(handle);

    assert!(matches!(
        with_timeout(2, waiter.wait()).await,
        Ok(TaskOutcome::Rejected {
            kind: RejectionKind::ControllerShuttingDown,
            ..
        })
    ));
}
