//! Tests for the controller engine and its cross-module invariants.

use super::*;
use crate::Supervisor;
use crate::TaskContext;
use crate::{BackoffPolicy, BoxTaskFuture, RestartPolicy, Task, TaskFn, TaskRef, TaskSpec};
use std::num::NonZeroUsize;
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

struct PanickingNameTask;

impl Task for PanickingNameTask {
    fn name(&self) -> &str {
        panic!("injected task name panic")
    }

    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(async { Ok(()) })
    }
}

fn make_spec(name: &str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, |_ctx: TaskContext| async { Ok(()) });
    TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
}

fn slot_arc_name() -> Arc<str> {
    Arc::from("s")
}

#[test]
fn replace_head_or_push_into_empty_queue() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let mut slot = SlotState::new();
    ctrl.replace_head_or_push(
        &mut slot,
        &slot_arc_name(),
        TaskId::next(),
        make_spec("first"),
    );

    assert_eq!(slot.queue.len(), 1);
    assert_eq!(slot.queue.front().unwrap().1.name(), "first");
}

#[test]
fn replace_head_or_push_replaces_existing_head_and_rejects_displaced() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let mut rx = ctrl.bus.subscribe();
    let mut slot = SlotState::new();
    let displaced = TaskId::next();
    slot.queue.push_back((displaced, make_spec("old-head")));
    slot.queue.push_back((TaskId::next(), make_spec("tail")));

    ctrl.replace_head_or_push(
        &mut slot,
        &slot_arc_name(),
        TaskId::next(),
        make_spec("new-head"),
    );

    assert_eq!(slot.queue.len(), 2, "queue depth should not grow");
    assert_eq!(slot.queue.front().unwrap().1.name(), "new-head");
    assert_eq!(slot.queue.back().unwrap().1.name(), "tail");

    let ev = rx.try_recv().expect("displaced head must be rejected");
    assert_eq!(ev.kind, EventKind::ControllerRejected);
    assert_eq!(ev.id, Some(displaced));
    assert_eq!(
        ev.reason.as_deref(),
        Some(crate::reasons::SUPERSEDED_BY_REPLACE)
    );
}

#[test]
fn replace_head_multiple_times_keeps_depth_1() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let mut slot = SlotState::new();
    let name = slot_arc_name();
    ctrl.replace_head_or_push(&mut slot, &name, TaskId::next(), make_spec("v1"));
    ctrl.replace_head_or_push(&mut slot, &name, TaskId::next(), make_spec("v2"));
    ctrl.replace_head_or_push(&mut slot, &name, TaskId::next(), make_spec("v3"));

    assert_eq!(slot.queue.len(), 1);
    assert_eq!(slot.queue.front().unwrap().1.name(), "v3");
}

#[test]
fn reject_if_full_returns_false_below_capacity() {
    let bus = Bus::new(64);
    let config = ControllerConfig::new(NonZeroUsize::new(16).unwrap(), 3);
    let ctrl = make_controller(config, bus);
    assert!(!ctrl.reject_if_full("slot", TaskId::next(), 0));
    assert!(!ctrl.reject_if_full("slot", TaskId::next(), 2));
}

#[test]
fn reject_if_full_returns_true_at_capacity() {
    let bus = Bus::new(64);
    let config = ControllerConfig::new(NonZeroUsize::new(16).unwrap(), 3);
    let ctrl = make_controller(config, bus);
    assert!(ctrl.reject_if_full("slot", TaskId::next(), 3));
    assert!(ctrl.reject_if_full("slot", TaskId::next(), 10));
}

#[test]
fn get_or_create_slot_creates_idle_slot() {
    let bus = Bus::new(64);
    let ctrl = make_controller(ControllerConfig::default(), bus);

    let slot_arc = ctrl.get_or_create_slot("my-slot");
    let slot = slot_arc.blocking_lock();
    assert_eq!(slot.status, SlotStatus::Idle);
    assert!(slot.queue.is_empty());
}

#[test]
fn get_or_create_slot_returns_same_arc() {
    let bus = Bus::new(64);
    let ctrl = make_controller(ControllerConfig::default(), bus);

    let s1 = ctrl.get_or_create_slot("x");
    let s2 = ctrl.get_or_create_slot("x");
    assert!(Arc::ptr_eq(&s1, &s2), "same slot name must return same Arc");
}

#[test]
fn get_or_create_slot_different_names_different_arcs() {
    let bus = Bus::new(64);
    let ctrl = make_controller(ControllerConfig::default(), bus);

    let s1 = ctrl.get_or_create_slot("a");
    let s2 = ctrl.get_or_create_slot("b");
    assert!(!Arc::ptr_eq(&s1, &s2));
}

#[tokio::test]
async fn stale_completion_does_not_free_current_owner() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let current_id = TaskId::next();
    let stale_id = TaskId::next();
    let slot_arc = ctrl.get_or_create_slot("s");
    {
        let mut slot = slot_arc.lock().await;
        slot.status = SlotStatus::Running {
            started_at: Instant::now(),
        };
        slot.running_id = Some(current_id);
    }
    ctrl.running.insert(current_id, Arc::from("s"));

    let mut admissions = JoinSet::new();
    ctrl.handle_completion_result(
        CompletionResult {
            id: stale_id,
            slot_name: Arc::from("s"),
        },
        &mut admissions,
    )
    .await;

    let slot = slot_arc.lock().await;
    assert_eq!(slot.running_id, Some(current_id));
    assert!(matches!(slot.status, SlotStatus::Running { .. }));
    assert!(ctrl.running.contains_key(&current_id));
}

#[tokio::test]
async fn shutdown_finalizes_buffered_submission_as_rejected() {
    let bus = Bus::new(64);
    let ctrl = make_controller(ControllerConfig::default(), bus);

    let task: TaskRef = TaskFn::arc("buffered", |_ctx: TaskContext| async { Ok(()) });
    let (_id, waiter) = ctrl
        .handle()
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once(task)).with_slot("s"))
        .await
        .expect("submission accepted into channel");

    let mut rx = ctrl.rx.write().await.take().expect("rx present");
    ctrl.finalize_pending_on_shutdown(&mut rx);
    drop(rx);

    let outcome = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("waiter must resolve, not hang")
        .expect("waiter must resolve to an outcome, not a dropped sender");
    assert!(
        matches!(outcome, TaskOutcome::Rejected { .. }),
        "a buffered submission on shutdown must resolve Rejected, got {outcome:?}"
    );
}

#[tokio::test]
async fn shutdown_rejects_slot_queue_and_clears_controller_state() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let watched_id = TaskId::next();
    let unwatched_id = TaskId::next();
    let running_id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    ctrl.watchers.insert(watched_id, done);

    let slot = ctrl.get_or_create_slot("shutdown-slot");
    {
        let mut slot = slot.lock().await;
        slot.status = SlotStatus::Running {
            started_at: Instant::now(),
        };
        slot.running_id = Some(running_id);
        slot.queue
            .push_back((watched_id, waiting_spec("watched-shutdown-queue")));
        slot.queue
            .push_back((unwatched_id, waiting_spec("plain-shutdown-queue")));
    }
    ctrl.running.insert(running_id, Arc::from("shutdown-slot"));

    ctrl.finalize_slot_state_on_shutdown().await;

    assert!(matches!(
        outcome.await,
        Ok(TaskOutcome::Rejected { reason })
            if reason.as_ref() == crate::reasons::CONTROLLER_SHUTTING_DOWN
    ));
    assert!(ctrl.watchers.is_empty());
    assert!(ctrl.slots.is_empty());
    assert!(ctrl.running.is_empty());
}

#[tokio::test]
async fn shutdown_resolves_buffered_removal_reply() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let (reply, reply_rx) = oneshot::channel();
    ctrl.tx
        .try_send(ControllerCommand::ManageIdentity {
            id: TaskId::next(),
            operation: IdentityOperation::Cancel,
            reply,
        })
        .expect("the controller command channel has capacity");

    let mut rx = ctrl.rx.write().await.take().expect("rx present");
    ctrl.finalize_pending_on_shutdown(&mut rx);

    assert!(matches!(
        reply_rx.await,
        Ok(Err(RuntimeError::ShuttingDown))
    ));
}

#[tokio::test]
async fn aborted_identity_worker_sends_explicit_shutdown_reply() {
    let (reply, reply_rx) = oneshot::channel();
    let (started, started_rx) = oneshot::channel();
    let mut workers = JoinSet::new();
    workers.spawn(async move {
        let _reply = IdentityReply::new(reply);
        let _ = started.send(());
        std::future::pending::<()>().await;
    });
    tokio::time::timeout(Duration::from_secs(1), started_rx)
        .await
        .expect("the identity worker must start")
        .expect("the identity worker must signal start");

    workers.abort_all();
    Controller::drain_workers(&mut workers).await;

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), reply_rx).await,
        Ok(Ok(Err(RuntimeError::ShuttingDown)))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn controller_task_join_can_resume_after_a_dropped_waiter() {
    let (release, released) = oneshot::channel::<()>();
    let task = Arc::new(ControllerTask::new(tokio::spawn(async move {
        let _ = released.await;
    })));
    let bus = Bus::new(8);

    let first_task = Arc::clone(&task);
    let first_bus = bus.clone();
    let first = tokio::spawn(async move { first_task.join(&first_bus).await });
    assert!(
        poll_until(Duration::from_secs(1), || async { task.state_is_locked() }).await,
        "the first waiter must own the shared join state"
    );
    first.abort();
    let _ = first.await;
    assert!(
        poll_until(Duration::from_secs(1), || async { !task.state_is_locked() }).await,
        "aborting the first waiter must release the shared join state"
    );

    let second_task = Arc::clone(&task);
    let second_bus = bus.clone();
    let second = tokio::spawn(async move { second_task.join(&second_bus).await });
    assert!(
        poll_until(Duration::from_secs(1), || async { task.state_is_locked() }).await,
        "the second waiter must resume ownership of the stored JoinHandle"
    );
    assert!(
        !second.is_finished(),
        "the stored JoinHandle must remain pending after the first waiter is dropped"
    );

    release.send(()).expect("the controller task is waiting");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), second).await,
        Ok(Ok(true))
    ));
    assert!(task.is_joined().await);
}

#[tokio::test]
async fn submit_after_shutdown_finalize_is_rejected_not_leaked() {
    let bus = Bus::new(64);
    let ctrl = make_controller(ControllerConfig::default(), bus);

    let mut rx = ctrl.rx.write().await.take().expect("rx present");
    ctrl.finalize_pending_on_shutdown(&mut rx);

    let task: TaskRef = TaskFn::arc("late", |_ctx: TaskContext| async { Ok(()) });
    let result = ctrl
        .handle()
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once(task)).with_slot("s"))
        .await;

    assert!(
        result.is_err(),
        "a submission after shutdown finalization must be rejected, not handed a doomed waiter"
    );
    drop(rx);
}

fn make_controller(config: ControllerConfig, bus: Bus) -> Controller {
    let (tx, rx) = mpsc::channel(config.queue_capacity().get());
    Controller {
        config,
        supervisor: Weak::new(),
        bus,
        shutdown_token: CancellationToken::new(),
        slots: DashMap::new(),
        running: DashMap::new(),
        watchers: DashMap::new(),
        tx,
        rx: RwLock::new(Some(rx)),
        shutting_down: std::sync::atomic::AtomicBool::new(false),
        task: OnceLock::new(),
    }
}

#[tokio::test]
async fn guarded_converts_panic_to_diagnostic_and_survives() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let mut rx = ctrl.bus.subscribe();

    let _ = ctrl.guarded("unit", async { panic!("boom {}", 1) }).await;

    let ev = rx
        .try_recv()
        .expect("a panicking work-unit must publish a diagnostic");
    assert_eq!(ev.kind, EventKind::ControllerRejected);
    assert!(
        ev.reason.as_deref().unwrap_or_default().contains("boom 1"),
        "diagnostic must carry the panic message, got {:?}",
        ev.reason
    );
}

#[tokio::test]
async fn minimum_queue_capacity_is_supported() {
    let sup = Supervisor::builder(crate::SupervisorConfig::default())
        .with_controller(
            ControllerConfig::default()
                .with_queue_capacity(NonZeroUsize::new(1).unwrap())
                .with_max_slot_queue(1),
        )
        .build();
    let handle = sup.serve();

    let task: TaskRef = TaskFn::arc("minimum-capacity", |_ctx: TaskContext| async { Ok(()) });
    handle
        .submit(ControllerSpec::queue(TaskSpec::once(task)))
        .await
        .expect("submission must work with the minimum non-zero capacity");

    let _ = handle.shutdown().await;
}

fn waiting_spec(name: &'static str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    TaskSpec::restartable(task)
}

async fn start_controller_loop(
    ctrl: &Arc<Controller>,
    token: &CancellationToken,
) -> tokio::task::JoinHandle<Result<(), ControllerError>> {
    let runner_ctrl = Arc::clone(ctrl);
    let runner_token = token.clone();
    let runner = tokio::spawn(async move { runner_ctrl.run_inner(runner_token).await });

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if ctrl.rx.read().await.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controller loop must take its command receiver");
    runner
}

async fn stop_controller_loop(
    token: CancellationToken,
    runner: tokio::task::JoinHandle<Result<(), ControllerError>>,
) {
    token.cancel();
    tokio::time::timeout(Duration::from_secs(1), runner)
        .await
        .expect("controller loop must stop after cancellation")
        .expect("controller loop task must not panic")
        .expect("controller loop must exit cleanly");
}

#[tokio::test(flavor = "current_thread")]
async fn public_shutdown_waits_for_controller_join_and_survives_a_dropped_waiter() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let _runtime_handle = sup.serve();
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    sup.core().attach_controller(&ctrl);
    ctrl.run();
    ctrl.run();

    let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
        .with_controller(Some(Arc::clone(&ctrl)));
    let slot = ctrl.get_or_create_slot("blocked-shutdown-slot");
    let slot_guard = slot.lock().await;

    handle
        .submit(
            ControllerSpec::queue(waiting_spec("blocked-shutdown-task"))
                .with_slot("blocked-shutdown-slot"),
        )
        .await
        .expect("the blocking submission must enter the controller queue");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.tx.capacity() == ctrl.config.queue_capacity().get()
        })
        .await,
        "the controller must receive the command and block on the held slot lock"
    );

    let (_queued_id, queued_waiter) = handle
        .submit_and_watch(
            ControllerSpec::queue(waiting_spec("buffered-during-shutdown"))
                .with_slot("buffered-during-shutdown"),
        )
        .await
        .expect("the watched command must be buffered behind the blocked handler");
    let (_panicking_id, panicking_waiter) = handle
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once(Arc::new(
            PanickingNameTask,
        ))))
        .await
        .expect("the hostile watched command must remain buffered for shutdown drain");
    let identity_handle = handle.clone();
    let identity = tokio::spawn(async move { identity_handle.cancel(TaskId::next()).await });
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.tx.capacity() == ctrl.config.queue_capacity().get() - 3
        })
        .await,
        "all later commands must remain buffered before shutdown"
    );

    let first_handle = handle.clone();
    let first_shutdown = tokio::spawn(async move { first_handle.shutdown().await });
    assert!(
        poll_until(Duration::from_secs(2), || async {
            sup.core().is_shutting_down()
        })
        .await,
        "shared runtime shutdown must start"
    );
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.task.get().is_some_and(ControllerTask::state_is_locked)
        })
        .await,
        "the shared shutdown owner must reach the controller join"
    );
    assert!(
        !first_shutdown.is_finished(),
        "public shutdown must wait for the blocked controller loop"
    );

    first_shutdown.abort();
    let _ = first_shutdown.await;

    let second_shutdown = tokio::spawn(async move { handle.shutdown().await });
    tokio::task::yield_now().await;
    assert!(
        !second_shutdown.is_finished(),
        "dropping one shutdown waiter must not detach the shared controller join"
    );

    drop(slot_guard);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), second_shutdown).await,
        Ok(Ok(Ok(())))
    ));
    assert!(ctrl.is_joined().await);
    assert!(ctrl.slots.is_empty());
    assert!(ctrl.running.is_empty());
    assert!(ctrl.watchers.is_empty());
    let queued_outcome = tokio::time::timeout(Duration::from_millis(50), queued_waiter.wait())
        .await
        .expect("the buffered watcher must already be settled")
        .expect("the buffered watched command must resolve before shutdown returns");
    assert!(matches!(
        queued_outcome,
        TaskOutcome::Rejected { reason }
            if reason.as_ref() == crate::reasons::CONTROLLER_SHUTTING_DOWN
    ));
    let panicking_outcome =
        tokio::time::timeout(Duration::from_millis(50), panicking_waiter.wait())
            .await
            .expect("the hostile buffered watcher must already be settled")
            .expect("the hostile buffered watcher must resolve as an outcome");
    assert!(matches!(
        panicking_outcome,
        TaskOutcome::Rejected { reason }
            if reason.as_ref() == crate::reasons::CONTROLLER_SHUTTING_DOWN
    ));
    assert!(identity.is_finished());
    assert!(matches!(
        identity.await,
        Ok(Err(RuntimeError::ShuttingDown))
    ));

    let late = ctrl
        .handle()
        .try_submit(ControllerSpec::queue(waiting_spec("late-after-join")));
    assert!(matches!(late, Err(ControllerError::Closed)));
}

#[tokio::test(flavor = "current_thread")]
async fn natural_run_waits_for_controller_join() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    sup.core().attach_controller(&ctrl);
    ctrl.run();

    let slot = ctrl.get_or_create_slot("blocked-natural-slot");
    let slot_guard = slot.lock().await;
    ctrl.handle()
        .submit(
            ControllerSpec::queue(waiting_spec("blocked-natural-task"))
                .with_slot("blocked-natural-slot"),
        )
        .await
        .expect("the blocking submission must enter controller intake");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.tx.capacity() == ctrl.config.queue_capacity().get()
        })
        .await,
        "the controller must block on the held slot before natural shutdown"
    );

    let run_sup = Arc::clone(&sup);
    let run = tokio::spawn(async move { run_sup.run(vec![]).await });
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.task.get().is_some_and(ControllerTask::state_is_locked)
        })
        .await,
        "natural shutdown must reach the shared controller join"
    );
    assert!(
        !run.is_finished(),
        "natural run must not return while the controller loop is blocked"
    );

    drop(slot_guard);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), run).await,
        Ok(Ok(Ok(())))
    ));
    assert!(ctrl.is_joined().await);
}

#[tokio::test(flavor = "current_thread")]
async fn accepted_cancel_continues_after_caller_future_is_dropped() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let runtime_handle = sup.serve();
    let id = runtime_handle
        .add(waiting_spec("dropped-cancel-caller"))
        .await
        .expect("the direct task must register");

    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
        .with_controller(Some(Arc::clone(&ctrl)));

    let mut cancel = Box::pin(handle.cancel(id));
    std::future::poll_fn(|cx| match cancel.as_mut().poll(cx) {
        std::task::Poll::Pending => std::task::Poll::Ready(()),
        std::task::Poll::Ready(result) => {
            panic!("cancel must wait for the stopped controller loop, got {result:?}")
        }
    })
    .await;
    drop(cancel);

    assert_eq!(
        ctrl.tx.capacity(),
        ControllerConfig::default().queue_capacity().get() - 1,
        "the cancel command must be accepted before its caller is dropped"
    );

    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;
    assert!(
        poll_until(Duration::from_secs(2), || async {
            handle
                .list()
                .await
                .iter()
                .all(|(task_id, _)| *task_id != id)
        })
        .await,
        "the controller must complete registry fallback without the public caller"
    );

    stop_controller_loop(token, runner).await;
    let _ = runtime_handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn try_remove_reports_full_controller_command_queue() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let runtime_handle = sup.serve();
    let ctrl = Controller::new(
        ControllerConfig::default().with_queue_capacity(NonZeroUsize::new(1).unwrap()),
        sup.core(),
        Bus::new(64),
    );
    let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
        .with_controller(Some(Arc::clone(&ctrl)));

    ctrl.handle()
        .try_submit(ControllerSpec::queue(waiting_spec("controller-queue-filler")).with_slot("s"))
        .expect("the filler must occupy the controller command queue");

    assert!(matches!(
        handle.try_remove(TaskId::next()).await,
        Err(RuntimeError::CommandQueueFull)
    ));

    let mut rx = ctrl.rx.write().await.take().expect("rx present");
    ctrl.finalize_pending_on_shutdown(&mut rx);
    drop(rx);
    let _ = runtime_handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn try_remove_propagates_full_registry_queue_after_controller_admission() {
    let sup = Supervisor::new(
        crate::SupervisorConfig::default()
            .with_registry_queue_capacity(NonZeroUsize::new(1).unwrap()),
        vec![],
    );
    let filler_id = TaskId::next();
    let (_filler_reply, _filler_completion) = sup
        .core()
        .add_task_with_id_watched(filler_id, waiting_spec("registry-queue-filler"), None)
        .expect("the filler must occupy the registry queue");
    assert_eq!(sup.core().registry_command_capacity(), 0);

    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
        .with_controller(Some(Arc::clone(&ctrl)));
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;

    assert!(matches!(
        handle.try_remove(TaskId::next()).await,
        Err(RuntimeError::CommandQueueFull)
    ));
    assert_eq!(
        sup.core().registry_command_capacity(),
        0,
        "a rejected fallback must not consume or replace the queued registry command"
    );

    stop_controller_loop(token, runner).await;
}

#[tokio::test(flavor = "current_thread")]
async fn identity_operation_limit_preserves_command_backpressure() {
    let sup = Supervisor::new(
        crate::SupervisorConfig::default().with_grace(Duration::from_secs(2)),
        vec![],
    );
    let runtime_handle = sup.serve();

    let task_started = Arc::new(AtomicBool::new(false));
    let started = Arc::clone(&task_started);
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&cancellation_observed);
    let (release, released) = oneshot::channel();
    let released = Arc::new(StdMutex::new(Some(released)));
    let task_release = Arc::clone(&released);
    let task: TaskRef = TaskFn::arc("bounded-identity-owner", move |ctx: TaskContext| {
        let started = Arc::clone(&started);
        let observed = Arc::clone(&observed);
        let released = task_release
            .lock()
            .expect("release lock poisoned")
            .take()
            .expect("the task runs once");
        async move {
            started.store(true, Ordering::SeqCst);
            ctx.cancelled().await;
            observed.store(true, Ordering::SeqCst);
            let _ = released.await;
            Ok(())
        }
    });
    let owner_id = runtime_handle
        .add(TaskSpec::once(task))
        .await
        .expect("the direct task must register");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            task_started.load(Ordering::SeqCst)
        })
        .await,
        "the direct task body must start before cancellation"
    );

    let ctrl = Controller::new(
        ControllerConfig::default().with_queue_capacity(NonZeroUsize::new(1).unwrap()),
        sup.core(),
        Bus::new(64),
    );
    let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
        .with_controller(Some(Arc::clone(&ctrl)));
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;

    let cancel_handle = handle.clone();
    let cancel = tokio::spawn(async move {
        cancel_handle
            .cancel_with_timeout(owner_id, Duration::from_secs(10))
            .await
    });
    assert!(
        poll_until(Duration::from_secs(2), || async {
            cancellation_observed.load(Ordering::SeqCst)
        })
        .await,
        "the first identity operation must remain in flight"
    );

    let buffered_ran = Arc::new(AtomicBool::new(false));
    let ran = Arc::clone(&buffered_ran);
    let buffered: TaskRef = TaskFn::arc("buffered-after-identity", move |_ctx| {
        let ran = Arc::clone(&ran);
        async move {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        }
    });
    handle
        .submit(ControllerSpec::queue(TaskSpec::once(buffered)).with_slot("buffered"))
        .await
        .expect("one later command must fit in the bounded controller queue");

    assert!(matches!(
        handle.try_submit(
            ControllerSpec::queue(waiting_spec("overflow-after-identity")).with_slot("overflow"),
        ),
        Err(ControllerError::Full)
    ));
    assert!(
        !buffered_ran.load(Ordering::SeqCst),
        "the controller must not drain commands past its in-flight identity limit"
    );

    release.send(()).expect("the task is waiting for release");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), cancel).await,
        Ok(Ok(Ok(true)))
    ));
    assert!(
        poll_until(Duration::from_secs(2), || async {
            buffered_ran.load(Ordering::SeqCst)
        })
        .await,
        "the buffered command must resume after identity cleanup"
    );

    stop_controller_loop(token, runner).await;
    let _ = runtime_handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn registry_reply_marks_slot_running_without_task_added() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let handle = sup.serve();
    let controller_bus = Bus::new(1);
    let ctrl = Controller::new(
        ControllerConfig::default(),
        sup.core(),
        controller_bus.clone(),
    );
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;

    let id = ctrl
        .handle()
        .submit(ControllerSpec::queue(waiting_spec("reply-admitted")).with_slot("s"))
        .await
        .expect("controller intake must accept the submission");
    for _ in 0..16 {
        controller_bus.publish(Event::new(EventKind::TaskStarting).with_task("noise"));
    }

    let reached_running = poll_until(Duration::from_secs(2), || async {
        let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
            return false;
        };
        let slot = slot.lock().await;
        slot.running_id == Some(id) && matches!(slot.status, SlotStatus::Running { .. })
    })
    .await;

    assert!(
        reached_running,
        "the direct registry reply must confirm admission without TaskAdded"
    );
    assert_eq!(
        ctrl.running.get(&id).as_deref().map(AsRef::as_ref),
        Some("s")
    );

    stop_controller_loop(token, runner).await;
    assert!(ctrl.running.is_empty());
    assert!(ctrl.slots.is_empty());

    let _ = handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn replace_is_processed_while_registry_reply_is_pending() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    // Keep TaskRemoved off the controller bus; the reliable completion must advance Replace.
    let controller_bus = Bus::new(64);
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), controller_bus);
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;

    // The registry is not started yet, so the first committed Add reply stays pending.
    let (first_id, first_outcome) = ctrl
        .handle()
        .submit_and_watch(ControllerSpec::queue(waiting_spec("pending-owner")).with_slot("s"))
        .await
        .expect("controller intake must accept the first submission");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                return false;
            };
            let slot = slot.lock().await;
            slot.running_id == Some(first_id) && matches!(slot.status, SlotStatus::Admitting { .. })
        })
        .await,
        "the first Add must remain in flight until the registry starts"
    );

    let replacement_id = ctrl
        .handle()
        .submit(ControllerSpec::replace(waiting_spec("pending-replacement")).with_slot("s"))
        .await
        .expect("controller intake must accept Replace");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                return false;
            };
            let slot = slot.lock().await;
            matches!(slot.status, SlotStatus::Terminating { .. })
                && slot.queue.front().map(|(id, _)| *id) == Some(replacement_id)
        })
        .await,
        "Replace must be processed without waiting for the first registry reply"
    );

    let handle = sup.serve();
    let outcome = tokio::time::timeout(Duration::from_secs(2), first_outcome)
        .await
        .expect("the accepted owner must be removed")
        .expect("the registry must resolve the owner outcome");
    assert!(matches!(outcome, TaskOutcome::Canceled));

    assert!(
        poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                return false;
            };
            let slot = slot.lock().await;
            slot.running_id == Some(replacement_id)
                && matches!(slot.status, SlotStatus::Running { .. })
        })
        .await,
        "the replacement must start from reliable completion without TaskRemoved"
    );

    stop_controller_loop(token, runner).await;
    let _ = handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn replace_stays_responsive_under_registry_backpressure() {
    let sup = Supervisor::new(
        crate::SupervisorConfig::default()
            .with_registry_queue_capacity(NonZeroUsize::new(1).unwrap()),
        vec![],
    );
    let runtime_handle = sup.serve();
    let owner_id = runtime_handle
        .add(waiting_spec("replace-owner"))
        .await
        .expect("the owner must register");

    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    let slot_name: Arc<str> = Arc::from("s");
    ctrl.slots.insert(
        Arc::clone(&slot_name),
        Arc::new(Mutex::new(SlotState {
            status: SlotStatus::Running {
                started_at: Instant::now(),
            },
            running_id: Some(owner_id),
            queue: std::collections::VecDeque::new(),
        })),
    );
    ctrl.running.insert(owner_id, Arc::clone(&slot_name));

    // On a current-thread runtime the registry cannot consume this command until this test
    // yields, so it occupies the only queue slot while both Replace commands are handled.
    let filler_id = TaskId::next();
    let (filler_reply, _filler_completion) = sup
        .core()
        .add_task_with_id_watched(filler_id, waiting_spec("replace-filler"), None)
        .expect("the filler must occupy the registry queue");
    assert_eq!(sup.core().registry_command_capacity(), 0);

    let first_id = TaskId::next();
    let (first_done, first_outcome) = oneshot::channel();
    let first = Submission {
        id: first_id,
        spec: ControllerSpec::replace(waiting_spec("replace-first")).with_slot("s"),
        done: Some(first_done),
    };
    let mut admissions = JoinSet::new();
    let mut removals = JoinSet::new();
    let mut first = Box::pin(ctrl.handle_submission(first, &mut admissions, &mut removals));
    std::future::poll_fn(|cx| match first.as_mut().poll(cx) {
        std::task::Poll::Ready(()) => std::task::Poll::Ready(()),
        std::task::Poll::Pending => {
            panic!("Replace must not wait inside the controller loop for registry capacity")
        }
    })
    .await;
    drop(first);
    assert_eq!(removals.len(), 1, "one owner removal must be tracked");

    let second_id = TaskId::next();
    let second = Submission {
        id: second_id,
        spec: ControllerSpec::replace(waiting_spec("replace-second")).with_slot("s"),
        done: None,
    };
    let mut second = Box::pin(ctrl.handle_submission(second, &mut admissions, &mut removals));
    std::future::poll_fn(|cx| match second.as_mut().poll(cx) {
        std::task::Poll::Ready(()) => std::task::Poll::Ready(()),
        std::task::Poll::Pending => {
            panic!("a newer Replace must stay responsive while removal is backpressured")
        }
    })
    .await;
    drop(second);

    let slot = ctrl
        .slots
        .get("s")
        .map(|entry| entry.clone())
        .expect("the slot must remain tracked");
    let slot = slot.lock().await;
    assert!(matches!(slot.status, SlotStatus::Terminating { .. }));
    assert_eq!(slot.queue.front().map(|(id, _)| *id), Some(second_id));
    drop(slot);
    assert_eq!(
        removals.len(),
        1,
        "repeated Replace must not enqueue duplicate owner removals"
    );
    assert!(matches!(
        first_outcome.await,
        Ok(TaskOutcome::Rejected { reason })
            if reason.as_ref() == crate::reasons::SUPERSEDED_BY_REPLACE
    ));

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), filler_reply).await,
        Ok(Ok(Ok(())))
    ));
    let removal = tokio::time::timeout(Duration::from_secs(2), removals.join_next())
        .await
        .expect("the owner removal must resume after registry capacity recovers")
        .expect("one removal waiter must exist")
        .expect("the removal waiter must not panic");
    assert_eq!(removal.id, owner_id);
    assert!(matches!(removal.decision, Ok(true)));

    let _ = runtime_handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn queued_cancel_is_ordered_without_runtime_bus_events() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let runtime_handle = sup.serve();
    // Runtime lifecycle and removal events cannot reach this controller.
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(1));
    let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
        .with_controller(Some(Arc::clone(&ctrl)));
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;

    let owner_id = handle
        .submit(ControllerSpec::queue(waiting_spec("cancel-owner")).with_slot("s"))
        .await
        .expect("the owner submission must enter the controller");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                return false;
            };
            let slot = slot.lock().await;
            slot.running_id == Some(owner_id) && matches!(slot.status, SlotStatus::Running { .. })
        })
        .await,
        "the first task must own the slot"
    );

    let victim_ran = Arc::new(AtomicBool::new(false));
    let ran = Arc::clone(&victim_ran);
    let victim: TaskRef = TaskFn::arc("cancel-victim", move |_ctx: TaskContext| {
        let ran = Arc::clone(&ran);
        async move {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        }
    });
    let (victim_id, waiter) = handle
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once(victim)).with_slot("s"))
        .await
        .expect("the queued submission must enter the controller channel");

    assert!(
        handle
            .cancel(victim_id)
            .await
            .expect("ordered queued cancellation must succeed"),
        "the first cancellation caller must claim the queued submission"
    );
    let outcome = waiter.wait().await.expect("the queued waiter must resolve");
    assert!(
        matches!(outcome, TaskOutcome::Rejected { reason } if reason.as_ref() == crate::reasons::REMOVED_FROM_QUEUE)
    );

    let try_ran = Arc::clone(&victim_ran);
    let try_victim: TaskRef = TaskFn::arc("try-remove-victim", move |_ctx: TaskContext| {
        let ran = Arc::clone(&try_ran);
        async move {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        }
    });
    let (try_id, try_waiter) = handle
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once(try_victim)).with_slot("s"))
        .await
        .expect("the second queued submission must enter the controller channel");
    assert!(
        handle
            .try_remove(try_id)
            .await
            .expect("the ordered controller channel has capacity"),
        "try_remove must claim queued controller work"
    );
    let try_outcome = try_waiter
        .wait()
        .await
        .expect("the try_remove waiter must resolve");
    assert!(
        matches!(try_outcome, TaskOutcome::Rejected { reason } if reason.as_ref() == crate::reasons::REMOVED_FROM_QUEUE)
    );

    assert!(
        handle
            .cancel(owner_id)
            .await
            .expect("the admitted owner must be cancelled")
    );
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.slots.get("s").is_none()
                && !ctrl.running.contains_key(&owner_id)
                && !ctrl.running.contains_key(&victim_id)
                && !ctrl.running.contains_key(&try_id)
        })
        .await,
        "the slot must settle after its owner completes"
    );
    assert!(
        !victim_ran.load(Ordering::SeqCst),
        "a queued submission claimed by cancel must never start"
    );

    stop_controller_loop(token, runner).await;
    let _ = runtime_handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn reliable_completion_reuses_task_name_without_task_removed() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let handle = sup.serve();
    // Registry lifecycle events cannot reach this controller.
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(1));
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;

    let log = Arc::new(StdMutex::new(Vec::new()));
    let (release, released) = oneshot::channel();
    let released = Arc::new(StdMutex::new(Some(released)));
    let first_log = Arc::clone(&log);
    let first_release = Arc::clone(&released);
    let first: TaskRef = TaskFn::arc("same-runtime-name", move |_ctx: TaskContext| {
        let released = first_release
            .lock()
            .expect("release lock poisoned")
            .take()
            .expect("the first task runs once");
        let log = Arc::clone(&first_log);
        async move {
            let _ = released.await;
            log.lock().expect("log lock poisoned").push("first");
            Ok(())
        }
    });
    let second_log = Arc::clone(&log);
    let second: TaskRef = TaskFn::arc("same-runtime-name", move |_ctx: TaskContext| {
        let log = Arc::clone(&second_log);
        async move {
            log.lock().expect("log lock poisoned").push("second");
            Ok(())
        }
    });

    let (first_id, first_outcome) = ctrl
        .handle()
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once(first)).with_slot("s"))
        .await
        .expect("the first submission must enter controller intake");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                return false;
            };
            let slot = slot.lock().await;
            slot.running_id == Some(first_id) && matches!(slot.status, SlotStatus::Running { .. })
        })
        .await,
        "the first task must own the slot before queueing the second"
    );

    let (second_id, second_outcome) = ctrl
        .handle()
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once(second)).with_slot("s"))
        .await
        .expect("the second submission must enter controller intake");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                return false;
            };
            slot.lock().await.queue.front().map(|(id, _)| *id) == Some(second_id)
        })
        .await,
        "the second task must wait behind the first"
    );

    release.send(()).expect("the first task is waiting");
    let first_outcome = tokio::time::timeout(Duration::from_secs(2), first_outcome)
        .await
        .expect("the first outcome must arrive")
        .expect("the registry must send the first outcome");
    let second_outcome = tokio::time::timeout(Duration::from_secs(2), second_outcome)
        .await
        .expect("reliable completion must start the queued task")
        .expect("the registry must send the second outcome");
    assert!(matches!(first_outcome, TaskOutcome::Completed));
    assert!(matches!(second_outcome, TaskOutcome::Completed));
    assert_eq!(
        log.lock().expect("log lock poisoned").as_slice(),
        ["first", "second"]
    );
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.slots.get("s").is_none()
                && !ctrl.running.contains_key(&first_id)
                && !ctrl.running.contains_key(&second_id)
        })
        .await,
        "the empty slot must be collected after the second completion"
    );

    stop_controller_loop(token, runner).await;
    let _ = handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_reply_frees_slot_without_task_add_failed() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let handle = sup.serve();
    handle
        .add(waiting_spec("duplicate-reply"))
        .await
        .expect("the existing task must register");

    // The controller listens to a different bus, so TaskAddFailed cannot drive its state.
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(1));
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;
    let (id, outcome) = ctrl
        .handle()
        .submit_and_watch(ControllerSpec::queue(waiting_spec("duplicate-reply")).with_slot("s"))
        .await
        .expect("controller intake must accept the duplicate");

    let outcome = tokio::time::timeout(Duration::from_secs(2), outcome)
        .await
        .expect("registry rejection must resolve the watcher")
        .expect("registry must send a rejected outcome");
    assert!(
        matches!(outcome, TaskOutcome::Rejected { reason } if reason.as_ref() == crate::reasons::ALREADY_EXISTS)
    );
    assert!(
        poll_until(Duration::from_secs(2), || async {
            !ctrl.running.contains_key(&id) && !ctrl.watchers.contains_key(&id)
        })
        .await,
        "the rejected admission must release its slot ownership"
    );
    assert!(
        ctrl.slots.get("s").is_none(),
        "an idle empty slot should be collected after registry rejection"
    );

    stop_controller_loop(token, runner).await;
    let _ = handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn queued_admission_skips_registry_rejected_head() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let handle = sup.serve();
    handle
        .add(waiting_spec("queued-duplicate"))
        .await
        .expect("the existing task must register");
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(1));
    let slot_name: Arc<str> = Arc::from("s");
    let slot_arc = ctrl.get_or_create_slot(&slot_name);
    let duplicate_id = TaskId::next();
    let accepted_id = TaskId::next();
    let (duplicate_done, duplicate_outcome) = oneshot::channel();
    let (accepted_done, _accepted_outcome) = oneshot::channel();
    ctrl.watchers.insert(duplicate_id, duplicate_done);
    ctrl.watchers.insert(accepted_id, accepted_done);

    let mut admissions = JoinSet::new();
    let mut completions = JoinSet::new();
    let mut removals = JoinSet::new();
    {
        let mut slot = slot_arc.lock().await;
        slot.queue
            .push_back((duplicate_id, waiting_spec("queued-duplicate")));
        slot.queue
            .push_back((accepted_id, waiting_spec("queued-accepted")));
        ctrl.start_next_from_queue(sup.core(), &mut slot, &slot_name, &mut admissions);
    }

    for _ in 0..2 {
        let result = tokio::time::timeout(Duration::from_secs(2), admissions.join_next())
            .await
            .expect("registry admission reply must arrive")
            .expect("one admission must be in flight")
            .expect("admission waiter must not fail");
        ctrl.handle_admission_result(result, &mut admissions, &mut completions, &mut removals)
            .await;
    }

    let duplicate_outcome = duplicate_outcome
        .await
        .expect("registry must resolve the duplicate watcher");
    assert!(
        matches!(duplicate_outcome, TaskOutcome::Rejected { reason } if reason.as_ref() == crate::reasons::ALREADY_EXISTS)
    );
    let slot = slot_arc.lock().await;
    assert_eq!(slot.running_id, Some(accepted_id));
    assert!(matches!(slot.status, SlotStatus::Running { .. }));
    assert!(slot.queue.is_empty());
    drop(slot);
    assert!(!ctrl.running.contains_key(&duplicate_id));
    assert_eq!(
        ctrl.running.get(&accepted_id).as_deref().map(AsRef::as_ref),
        Some("s")
    );

    let _ = handle.shutdown().await;
}

async fn sup_with_live_task() -> (Arc<Supervisor>, crate::core::SupervisorHandle, TaskId) {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let handle = sup.serve();
    let task: TaskRef = TaskFn::arc("occupant", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    let id = handle
        .add(TaskSpec::restartable(task))
        .await
        .expect("task should register");
    (sup, handle, id)
}

#[tokio::test]
async fn no_queue_advancement_after_shutdown_starts() {
    let (sup, handle, id) = sup_with_live_task().await;
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));

    let queued: TaskRef = TaskFn::arc("queued", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((TaskId::next(), TaskSpec::restartable(queued)));
    ctrl.slots.insert(
        Arc::from("s"),
        Arc::new(Mutex::new(SlotState {
            status: SlotStatus::Running {
                started_at: Instant::now(),
            },
            running_id: Some(id),
            queue,
        })),
    );
    ctrl.running.insert(id, Arc::from("s"));
    let mut admissions = JoinSet::new();
    ctrl.mark_shutting_down();
    ctrl.handle_completion_result(
        CompletionResult {
            id,
            slot_name: Arc::from("s"),
        },
        &mut admissions,
    )
    .await;

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        sup.core().id_for_label("queued").await.is_none(),
        "controller must not start queued tasks once shutdown has been requested"
    );

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn replace_supersedes_in_same_slot() {
    let sup = Supervisor::builder(crate::SupervisorConfig::default())
        .with_controller(ControllerConfig::default())
        .build();
    let handle = sup.serve();

    let mk = |name: &'static str| -> ControllerSpec {
        let task: TaskRef = TaskFn::arc(name, |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        ControllerSpec::replace(TaskSpec::restartable(task)).with_slot("s")
    };

    handle.submit(mk("run-1")).await.unwrap();
    handle.submit(mk("run-2")).await.unwrap();

    let superseded = poll_until(std::time::Duration::from_secs(3), || async {
        let alive = handle.snapshot().await;
        alive.iter().any(|n| &**n == "run-2") && alive.iter().all(|n| &**n != "run-1")
    })
    .await;
    assert!(
        superseded,
        "Replace must supersede run-1 with run-2 in the shared slot, not run both"
    );

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn snapshot_reports_status_running_and_queue_depth() {
    use crate::controller::SlotStatusKind;

    let (sup, handle, id) = sup_with_live_task().await;
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));

    let queued: TaskRef = TaskFn::arc("queued", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((TaskId::next(), TaskSpec::restartable(queued)));
    ctrl.slots.insert(
        Arc::from("s"),
        Arc::new(Mutex::new(SlotState {
            status: SlotStatus::Running {
                started_at: Instant::now(),
            },
            running_id: Some(id),
            queue,
        })),
    );

    let snap = ctrl.snapshot().await;
    assert_eq!(snap.len(), 1, "one slot tracked");
    assert_eq!(snap.running_count(), 1);
    assert_eq!(snap.total_queued(), 1);

    let view = snap.slot("s").expect("slot 's' must be present");
    assert_eq!(view.status, SlotStatusKind::Running);
    assert_eq!(view.queue_depth, 1);
    assert_eq!(view.running, Some(id));

    let _ = handle.shutdown().await;
}

async fn poll_until<F, Fut>(within: std::time::Duration, mut cond: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if cond().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
