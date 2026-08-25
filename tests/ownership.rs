//! Runtime construction and public-owner Drop contracts.

mod common;

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use common::poll_until;
use common::{EventCollector, collector_subscribers, with_timeout};
use taskvisor::prelude::*;

struct NoopSubscriber;

impl Subscribe for NoopSubscriber {
    fn on_event(&self, _event: &Event) {}

    fn name(&self) -> &str {
        "noop"
    }
}

struct PanickingDropSubscriber;

impl Subscribe for PanickingDropSubscriber {
    fn on_event(&self, _event: &Event) {}

    fn name(&self) -> &str {
        "panicking-drop-subscriber"
    }
}

impl Drop for PanickingDropSubscriber {
    fn drop(&mut self) {
        panic!("injected final subscriber destructor panic");
    }
}

#[derive(Default)]
struct FinalDropState {
    entered: bool,
    released: bool,
}

type FinalDropGate = Arc<(Mutex<FinalDropState>, Condvar)>;

struct ReleaseFinalDrop(FinalDropGate);

impl Drop for ReleaseFinalDrop {
    fn drop(&mut self) {
        release_final_drop(&self.0);
    }
}

struct BlockingFinalDropTask {
    gate: FinalDropGate,
}

impl Task for BlockingFinalDropTask {
    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(async { Ok(()) })
    }
}

impl Drop for BlockingFinalDropTask {
    fn drop(&mut self) {
        let (state, changed) = &*self.gate;
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        state.entered = true;
        changed.notify_all();
        while !state.released {
            state = changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}

struct PanickingFinalDropTask;

impl Task for PanickingFinalDropTask {
    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(async { Ok(()) })
    }
}

impl Drop for PanickingFinalDropTask {
    fn drop(&mut self) {
        panic!("injected final Task destructor panic");
    }
}

fn wait_for_final_drop(gate: &FinalDropGate) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let (state, changed) = &**gate;
    let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
    while !state.entered {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!remaining.is_zero(), "the final destructor must start");
        let (next, timeout) = changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(|error| error.into_inner());
        state = next;
        assert!(
            !timeout.timed_out() || state.entered,
            "the final destructor must start"
        );
    }
}

fn release_final_drop(gate: &FinalDropGate) {
    let (state, changed) = &**gate;
    state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .released = true;
    changed.notify_all();
}

#[test]
fn build_with_subscribers_is_safe_outside_tokio() {
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(NoopSubscriber)];
    let supervisor = Supervisor::new(SupervisorConfig::default(), subscribers);

    assert_eq!(supervisor.runtime_config().bus_capacity().get(), 1024);
    let ownership = supervisor.ownership_snapshot();
    let configured = supervisor
        .runtime_config()
        .ownership_capacity()
        .expect("the default ownership limit is finite")
        .get();
    assert_eq!(ownership.configured_limit, Some(configured));
    assert_eq!(ownership.effective_limit, Some(configured));
    assert_eq!(ownership.available, Some(configured - 1));
    assert_eq!(ownership.in_use(), Some(1));
    assert_eq!(ownership.waiters, 0);
    assert!(ownership.admission_open);
    assert_eq!(ownership.cleanup_queued, 0);
    assert_eq!(ownership.cleanup_running, 0);
    drop(supervisor);
}

#[tokio::test(flavor = "current_thread")]
async fn direct_ownership_timeout_commits_nothing_and_capacity_is_reusable() {
    let collector = EventCollector::new();
    let config = SupervisorConfig::default()
        .try_with_ownership_capacity(2)
        .expect("the subscriber and one task each need one ownership unit");
    let supervisor = Supervisor::new(config, collector_subscribers(&collector));
    let handle = supervisor.serve().expect("runtime startup");
    let (holder_id, holder_waiter) = handle
        .add_and_watch(TaskSpec::once(
            "ownership-timeout-holder",
            common::make_coop(),
        ))
        .await
        .expect("the holder must consume the remaining ownership unit");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            handle.is_alive("ownership-timeout-holder").await
        })
        .await,
        "the holder must start before saturation is tested"
    );

    let error = handle
        .add_with_ownership_timeout(
            TaskSpec::once("ownership-timeout-add", common::make_ok_once()),
            Duration::ZERO,
        )
        .await
        .expect_err("a saturated direct add must time out");
    assert!(matches!(
        error,
        RuntimeError::OwnershipAdmissionTimeout { timeout, .. }
            if timeout == Duration::ZERO
    ));

    let error = handle
        .add_and_watch_with_ownership_timeout(
            TaskSpec::once("ownership-timeout-add-watched", common::make_ok_once()),
            Duration::ZERO,
        )
        .await
        .expect_err("a saturated watched add must time out");
    assert!(matches!(
        error,
        RuntimeError::OwnershipAdmissionTimeout { timeout, .. }
            if timeout == Duration::ZERO
    ));
    let saturated = handle.ownership_snapshot();
    assert_eq!(saturated.available, Some(0));
    assert_eq!(saturated.waiters, 0);
    let listed = handle.list().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, holder_id);
    assert_eq!(listed[0].1.as_ref(), "ownership-timeout-holder");

    assert!(handle.cancel(holder_id).await.expect("cancel holder"));
    assert!(matches!(
        with_timeout(2, holder_waiter.wait()).await,
        Ok(TaskOutcome::Canceled)
    ));
    assert!(
        poll_until(Duration::from_secs(2), || async {
            handle.ownership_snapshot().available == Some(1)
        })
        .await,
        "the holder cleanup must return its ownership unit"
    );

    let (_, marker_waiter) = handle
        .add_and_watch_with_ownership_timeout(
            TaskSpec::once("ownership-timeout-marker", common::make_ok_once()),
            Duration::ZERO,
        )
        .await
        .expect("an immediately available ownership unit must beat a zero deadline");
    assert!(matches!(
        with_timeout(2, marker_waiter.wait()).await,
        Ok(TaskOutcome::Completed)
    ));
    assert!(
        collector
            .wait_until(Duration::from_secs(2), |events| {
                events.iter().any(|event| {
                    event.kind == EventKind::TaskRemoved
                        && event.task.as_deref() == Some("ownership-timeout-marker")
                })
            })
            .await,
        "the marker event must flush earlier lifecycle events"
    );
    assert!(collector.by_label("ownership-timeout-add").is_empty());
    assert!(
        collector
            .by_label("ownership-timeout-add-watched")
            .is_empty()
    );

    handle.shutdown().await.expect("runtime shutdown");
}

#[tokio::test(flavor = "current_thread")]
async fn ownership_snapshot_explains_blocked_final_cleanup_and_parked_admission() {
    let config = SupervisorConfig::default()
        .try_with_ownership_capacity(1)
        .expect("the test capacity is valid");
    let supervisor = Supervisor::new(config, vec![]);
    let handle = supervisor.serve().expect("runtime startup");
    let gate = Arc::new((Mutex::new(FinalDropState::default()), Condvar::new()));
    let _release_on_failure = ReleaseFinalDrop(Arc::clone(&gate));
    let task: TaskRef = Arc::new(BlockingFinalDropTask {
        gate: Arc::clone(&gate),
    });
    let (_, waiter) = handle
        .add_and_watch(TaskSpec::once("blocked-final-cleanup", task))
        .await
        .expect("the task must be admitted");

    assert!(matches!(
        with_timeout(2, waiter.wait()).await,
        Ok(TaskOutcome::Completed)
    ));
    wait_for_final_drop(&gate);
    assert!(handle.list().await.is_empty());
    assert!(handle.alive_snapshot().await.is_empty());

    let blocked = handle.ownership_snapshot();
    assert_eq!(blocked.configured_limit, Some(1));
    assert_eq!(blocked.effective_limit, Some(1));
    assert_eq!(blocked.available, Some(0));
    assert_eq!(blocked.in_use(), Some(1));
    assert_eq!(blocked.cleanup_running, 1);
    assert_eq!(blocked.cleanup_queued, 0);
    assert!(blocked.admission_open);

    let waiting_handle = handle.clone();
    let waiting = tokio::spawn(async move {
        let task = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
        waiting_handle
            .add(TaskSpec::once("parked-behind-cleanup", task))
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle.ownership_snapshot().waiters != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the waiting add must become visible");

    waiting.abort();
    let _ = waiting.await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle.ownership_snapshot().waiters != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("canceling the add future must remove its ownership waiter");

    handle
        .clone()
        .shutdown()
        .await
        .expect("blocked final destruction must not extend public shutdown");
    let shut_down = supervisor.ownership_snapshot();
    assert!(!shut_down.admission_open);
    assert_eq!(shut_down.cleanup_running, 1);

    release_final_drop(&gate);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let drained = handle.ownership_snapshot();
            if drained.cleanup_running == 0 && drained.available == Some(1) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the snapshot must reflect completed final destruction");
}

#[tokio::test(flavor = "current_thread")]
async fn final_destructor_panic_retires_capacity_and_emits_one_typed_event() {
    let collector = EventCollector::new();
    let config = SupervisorConfig::default()
        .try_with_ownership_capacity(2)
        .expect("the subscriber and task each need one ownership unit");
    let supervisor = Supervisor::new(config, collector_subscribers(&collector));
    let handle = supervisor.serve().expect("runtime startup");
    let task: TaskRef = Arc::new(PanickingFinalDropTask);
    let (_, waiter) = handle
        .add_and_watch(TaskSpec::once("panicking-final-cleanup", task))
        .await
        .expect("the task must be admitted");

    assert!(matches!(
        with_timeout(2, waiter.wait()).await,
        Ok(TaskOutcome::Completed)
    ));
    let event = collector
        .wait_for(EventKind::OwnershipCapacityRetired, Duration::from_secs(2))
        .await
        .expect("retirement must emit a typed diagnostic");
    assert_eq!(event.task.as_deref(), Some("destructor_isolation"));
    assert_eq!(event.configured_capacity, Some(2));
    assert_eq!(event.effective_capacity, Some(1));
    assert_eq!(event.retired_units, Some(1));
    let ownership = handle.ownership_snapshot();
    assert_eq!(ownership.configured_limit, Some(2));
    assert_eq!(ownership.effective_limit, Some(1));
    assert_eq!(ownership.retired(), Some(1));
    assert_eq!(ownership.available, Some(0));
    assert_eq!(ownership.in_use(), Some(1));

    handle.shutdown().await.expect("runtime shutdown");
    assert_eq!(
        collector.count(EventKind::OwnershipCapacityRetired),
        1,
        "one failed cleanup batch must emit one retirement transition"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn final_subscriber_destructor_retirement_remains_visible_after_shutdown() {
    let config = SupervisorConfig::default()
        .try_with_ownership_capacity(1)
        .expect("the subscriber needs one ownership unit");
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(PanickingDropSubscriber)];
    let supervisor = Supervisor::new(config, subscribers);
    let handle = supervisor.serve().expect("runtime startup");

    assert_eq!(supervisor.ownership_snapshot().in_use(), Some(1));
    handle.shutdown().await.expect("runtime shutdown");

    let retired = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = supervisor.ownership_snapshot();
            if snapshot.effective_limit == Some(0) && snapshot.cleanup_running == 0 {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the final subscriber destructor must retire its ownership unit");
    assert_eq!(retired.configured_limit, Some(1));
    assert_eq!(retired.retired(), Some(1));
    assert_eq!(retired.available, Some(0));
    assert_eq!(retired.in_use(), Some(0));
    assert!(!retired.admission_open);
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
