//! Tests for ordered identity operations and registry fallback.

use super::support::*;
use crate::controller::engine::state::{PendingSubmission, SlotPhase};
use crate::controller::engine::{
    Controller, IdentityOperation, IdentityReply, OperationSet, TrackedOperations,
};

#[tokio::test]
async fn buffered_identity_paths_receive_explicit_shutdown_reply() {
    let config = ControllerConfig::default();
    let queue_capacity = config.queue_capacity().get();
    let bus = Bus::new(64);
    let mut events = bus.subscribe();
    let ctrl = make_controller(config, bus);
    let waiting_handle = ctrl.handle();
    let fail_fast_handle = waiting_handle.clone();
    let late_handle = waiting_handle.clone();
    let mut waiting = Box::pin(waiting_handle.remove(TaskId::next()));
    let mut fail_fast = Box::pin(fail_fast_handle.try_cancel(TaskId::next()));

    std::future::poll_fn(|context| match waiting.as_mut().poll(context) {
        std::task::Poll::Pending => std::task::Poll::Ready(()),
        std::task::Poll::Ready(result) => {
            panic!("waiting identity command must await its reply, got {result:?}")
        }
    })
    .await;
    std::future::poll_fn(|context| match fail_fast.as_mut().poll(context) {
        std::task::Poll::Pending => std::task::Poll::Ready(()),
        std::task::Poll::Ready(result) => {
            panic!("fail-fast identity command must await its reply, got {result:?}")
        }
    })
    .await;
    assert_eq!(ctrl.tx.capacity(), queue_capacity - 2);

    let mut rx = ctrl.take_command_receiver().expect("receiver present");
    ctrl.finalize_pending_on_shutdown(&mut rx).await;

    assert!(matches!(waiting.await, Err(RuntimeError::ShuttingDown)));
    assert!(matches!(fail_fast.await, Err(RuntimeError::ShuttingDown)));
    assert!(matches!(
        late_handle.remove(TaskId::next()).await,
        Err(RuntimeError::ShuttingDown)
    ));
    assert!(matches!(
        late_handle.try_cancel(TaskId::next()).await,
        Err(RuntimeError::ShuttingDown)
    ));
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
    assert!(
        drain_events(&mut events)
            .iter()
            .all(|event| event.kind != EventKind::ControllerRejected),
        "identity shutdown replies must not publish submission rejection events"
    );
}

#[tokio::test]
async fn aborted_identity_operation_sends_explicit_shutdown_reply() {
    let (reply, reply_rx) = oneshot::channel();
    let (started, started_rx) = oneshot::channel();
    let mut operations = OperationSet::new();
    TrackedOperations::push(&operations, async move {
        let _reply = IdentityReply::new(reply);
        let _ = started.send(());
        std::future::pending::<()>().await;
    });
    let mut next = Box::pin(operations.next());
    std::future::poll_fn(|cx| match next.as_mut().poll(cx) {
        std::task::Poll::Pending => std::task::Poll::Ready(()),
        std::task::Poll::Ready(_) => panic!("the identity operation must remain pending"),
    })
    .await;
    drop(next);
    tokio::time::timeout(Duration::from_secs(1), started_rx)
        .await
        .expect("the identity operation must start")
        .expect("the identity operation must signal start");

    operations.clear();

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), reply_rx).await,
        Ok(Ok(Err(RuntimeError::ShuttingDown)))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn queued_identity_reply_survives_panicking_task_drop() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let bus = Bus::new(64);
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), bus.clone());
    let mut events = bus.subscribe();
    let drops = Arc::new(AtomicUsize::new(0));
    let id = TaskId::next();
    let slot_name: Arc<str> = Arc::from("identity-drop-panic-slot");
    let (done, outcome) = oneshot::channel();
    ctrl.state().watchers.insert(id, done);
    let slot = ctrl.get_or_create_slot(&slot_name);
    let mut slot_state = slot.lock().await;
    ctrl.push_queued(
        &mut slot_state,
        &slot_name,
        PendingSubmission::new(
            id,
            Arc::from("identity-drop-panic-task"),
            with_controller_panic_reporter(
                isolated_owned_task_spec(TaskSpec::once(
                    "identity-drop-panic-task",
                    Arc::new(PanickingDropTask {
                        drops: Arc::clone(&drops),
                    }),
                )),
                &ctrl.bus,
            ),
        ),
    );
    drop(slot_state);

    let (reply, result) = oneshot::channel();
    let mut operations = tracked_operations(&ctrl);
    ctrl.handle_identity_operation(id, IdentityOperation::Remove, reply, &mut operations)
        .await;

    assert!(matches!(
        receive_oneshot(result, "queued identity result").await,
        Ok(true)
    ));
    let outcome = receive_oneshot(outcome, "queued identity watcher").await;
    assert!(matches!(
        outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::RemovedFromQueue,
            ..
        }
    ));
    assert!(
        poll_until(Duration::from_secs(2), || async {
            drops.load(Ordering::Acquire) == 1
        })
        .await,
        "deferred task destruction must complete"
    );
    assert!(operations.identity_operations.is_empty());
    assert!(operations.admissions.is_empty());
    assert!(ctrl.state().watchers.is_empty());
    assert!(ctrl.state().queued_slots.is_empty());
    assert!(ctrl.slot(&slot_name).is_none());

    let drained = drain_until_runtime_failure(&mut events, "injected task drop panic").await;
    let rejections: Vec<_> = drained
        .iter()
        .filter(|event| event.kind == EventKind::ControllerRejected && event.id == Some(id))
        .collect();
    assert_eq!(rejections.len(), 1);
    assert_rejection_parity(rejections[0], id, &outcome);
    assert!(drained.iter().any(|event| {
        event.kind == EventKind::RuntimeFailure
            && event
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("injected task drop panic"))
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_identity_does_not_wait_for_unrelated_slot_lock() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let slot_name = slot_arc_name();
    let slot = ctrl.get_or_create_slot(&slot_name);
    let _slot_guard = slot.lock().await;
    let mut operations = tracked_operations(&ctrl);

    let removed = tokio::time::timeout(
        Duration::from_millis(50),
        ctrl.remove_queued_submission(TaskId::next(), None, &mut operations),
    )
    .await
    .expect("an unindexed ID must not inspect or wait for unrelated slots");

    assert!(!removed);
}

#[tokio::test(flavor = "current_thread")]
async fn accepted_cancel_continues_after_caller_future_is_dropped() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let runtime_handle = sup.serve().expect("runtime startup");
    let id = runtime_handle
        .add(waiting_spec("dropped-cancel-caller"))
        .execute()
        .await
        .expect("the direct task must register");

    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
        .with_controller(Some(Arc::clone(&ctrl)));

    let mut cancel = Box::pin(handle.cancel(id).execute());
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
async fn try_identity_operations_report_full_controller_command_queue() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let runtime_handle = sup.serve().expect("runtime startup");
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
        handle.remove(TaskId::next()).fail_fast().execute().await,
        Err(RuntimeError::CommandQueueFull)
    ));
    assert!(matches!(
        handle.cancel(TaskId::next()).fail_fast().execute().await,
        Err(RuntimeError::CommandQueueFull)
    ));
    assert!(matches!(
        handle
            .cancel(TaskId::next())
            .fail_fast()
            .termination_timeout(Duration::from_secs(1))
            .execute()
            .await,
        Err(RuntimeError::CommandQueueFull)
    ));

    let mut rx = ctrl.take_command_receiver().expect("rx present");
    ctrl.finalize_pending_on_shutdown(&mut rx).await;
    drop(rx);
    let _ = runtime_handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn try_identity_operations_propagate_full_registry_queue_after_controller_admission() {
    let sup = Supervisor::new(
        crate::SupervisorConfig::default()
            .with_registry_queue_capacity(NonZeroUsize::new(1).unwrap()),
        vec![],
    );
    let filler_id = TaskId::next();
    let (_filler_reply, _filler_completion) = sup
        .core()
        .add_task_with_id_watched(
            filler_id,
            Arc::from("registry-queue-filler"),
            owned_task_spec(waiting_spec("registry-queue-filler")),
            None,
        )
        .expect("the filler must occupy the registry queue");
    assert_eq!(sup.core().registry_command_capacity(), 0);

    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
        .with_controller(Some(Arc::clone(&ctrl)));
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;

    assert!(matches!(
        handle.remove(TaskId::next()).fail_fast().execute().await,
        Err(RuntimeError::CommandQueueFull)
    ));
    assert_eq!(
        sup.core().registry_command_capacity(),
        0,
        "a rejected fallback must not consume or replace the queued registry command"
    );
    assert!(matches!(
        handle.cancel(TaskId::next()).fail_fast().execute().await,
        Err(RuntimeError::CommandQueueFull)
    ));
    assert!(matches!(
        handle
            .cancel(TaskId::next())
            .fail_fast()
            .termination_timeout(Duration::from_secs(1))
            .execute()
            .await,
        Err(RuntimeError::CommandQueueFull)
    ));

    stop_controller_loop(token, runner).await;
}

#[tokio::test(flavor = "current_thread")]
async fn identity_operation_limit_rejects_excess_fallback_without_blocking_submissions() {
    let sup = Supervisor::new(
        crate::SupervisorConfig::default().with_grace(Duration::from_secs(2)),
        vec![],
    );
    let runtime_handle = sup.serve().expect("runtime startup");

    let task_started = Arc::new(AtomicBool::new(false));
    let started = Arc::clone(&task_started);
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&cancellation_observed);
    let (release, released) = oneshot::channel();
    let released = Arc::new(StdMutex::new(Some(released)));
    let task_release = Arc::clone(&released);
    let task: TaskRef = TaskFn::arc(move |ctx: TaskContext| {
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
        .add(TaskSpec::once("bounded-identity-owner", task))
        .execute()
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
        ControllerConfig::default()
            .with_queue_capacity(NonZeroUsize::new(1).unwrap())
            .with_identity_operation_capacity(NonZeroUsize::new(1).unwrap()),
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
            .cancel(owner_id)
            .termination_timeout(Duration::from_secs(10))
            .execute()
            .await
    });
    assert!(
        poll_until(Duration::from_secs(2), || async {
            cancellation_observed.load(Ordering::SeqCst)
        })
        .await,
        "the first identity operation must remain in flight"
    );

    assert!(matches!(
        handle.remove(TaskId::next()).execute().await,
        Err(RuntimeError::ResourceLimitReached {
            resource: "controller_identity_operations",
            limit: 1,
        })
    ));

    let buffered_ran = Arc::new(AtomicBool::new(false));
    let ran = Arc::clone(&buffered_ran);
    let buffered: TaskRef = TaskFn::arc(move |_ctx| {
        let ran = Arc::clone(&ran);
        async move {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        }
    });
    handle
        .submit(
            ControllerSpec::queue(TaskSpec::once("buffered-after-identity", buffered))
                .with_slot("buffered"),
        )
        .execute()
        .await
        .expect("a later submission must cross the independent command budget");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            buffered_ran.load(Ordering::SeqCst)
        })
        .await,
        "identity-operation saturation must not block a later submission"
    );

    release.send(()).expect("the task is waiting for release");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), cancel).await,
        Ok(Ok(Ok(true)))
    ));
    stop_controller_loop(token, runner).await;
    let _ = runtime_handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn queued_cancel_is_ordered_without_runtime_bus_events() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let runtime_handle = sup.serve().expect("runtime startup");
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(1));
    let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
        .with_controller(Some(Arc::clone(&ctrl)));
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;

    let owner_id = handle
        .submit(ControllerSpec::queue(waiting_spec("cancel-owner")).with_slot("s"))
        .execute()
        .await
        .expect("the owner submission must enter the controller");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl.slot("s") else {
                return false;
            };
            let slot = slot.lock().await;
            slot.owner_id() == Some(owner_id) && matches!(slot.phase(), SlotPhase::Running { .. })
        })
        .await,
        "the first task must own the slot"
    );

    let victim_ran = Arc::new(AtomicBool::new(false));
    let ran = Arc::clone(&victim_ran);
    let victim: TaskRef = TaskFn::arc(move |_ctx: TaskContext| {
        let ran = Arc::clone(&ran);
        async move {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        }
    });
    let waiter = handle
        .submit(ControllerSpec::queue(TaskSpec::once("cancel-victim", victim)).with_slot("s"))
        .watch()
        .execute()
        .await
        .expect("the queued submission must enter the controller channel");
    let victim_id = waiter.id();
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.state()
                .queued_slots
                .get(&victim_id)
                .is_some_and(|slot| slot.as_ref() == "s")
        })
        .await,
        "queued admission must publish its reverse-index route"
    );

    assert!(
        handle
            .cancel(victim_id)
            .execute()
            .await
            .expect("ordered queued cancellation must succeed"),
        "the first cancellation caller must claim the queued submission"
    );
    let outcome = waiter.wait().await.expect("the queued waiter must resolve");
    assert!(matches!(outcome, TaskOutcome::Rejected {
            kind: crate::RejectionKind::RemovedFromQueue,
            reason,
            ..
        } if reason.as_ref() == crate::reasons::REMOVED_FROM_QUEUE));
    assert!(!ctrl.state().queued_slots.contains_key(&victim_id));

    let try_ran = Arc::clone(&victim_ran);
    let try_victim: TaskRef = TaskFn::arc(move |_ctx: TaskContext| {
        let ran = Arc::clone(&try_ran);
        async move {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        }
    });
    let try_waiter = handle
        .submit(
            ControllerSpec::queue(TaskSpec::once("try-remove-victim", try_victim)).with_slot("s"),
        )
        .watch()
        .execute()
        .await
        .expect("the second queued submission must enter the controller channel");
    let try_id = try_waiter.id();
    assert!(
        handle
            .remove(try_id)
            .fail_fast()
            .execute()
            .await
            .expect("the ordered controller channel has capacity"),
        "try_remove must claim queued controller work"
    );
    let try_outcome = try_waiter
        .wait()
        .await
        .expect("the try_remove waiter must resolve");
    assert!(matches!(try_outcome, TaskOutcome::Rejected {
            kind: crate::RejectionKind::RemovedFromQueue,
            reason,
            ..
        } if reason.as_ref() == crate::reasons::REMOVED_FROM_QUEUE));

    let try_cancel_ran = Arc::clone(&victim_ran);
    let try_cancel_victim: TaskRef = TaskFn::arc(move |_ctx: TaskContext| {
        let ran = Arc::clone(&try_cancel_ran);
        async move {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        }
    });
    let try_cancel_waiter = handle
        .submit(
            ControllerSpec::queue(TaskSpec::once("try-cancel-victim", try_cancel_victim))
                .with_slot("s"),
        )
        .watch()
        .execute()
        .await
        .expect("the try-cancel victim must enter the controller channel");
    let try_cancel_id = try_cancel_waiter.id();
    assert!(
        handle
            .cancel(try_cancel_id)
            .fail_fast()
            .execute()
            .await
            .expect("the ordered controller channel has capacity"),
        "try_cancel must claim queued controller work"
    );
    let try_cancel_outcome = try_cancel_waiter
        .wait()
        .await
        .expect("the try_cancel waiter must resolve");
    assert!(matches!(try_cancel_outcome, TaskOutcome::Rejected {
            kind: crate::RejectionKind::RemovedFromQueue,
            reason,
            ..
        } if reason.as_ref() == crate::reasons::REMOVED_FROM_QUEUE));

    assert!(
        handle
            .cancel(owner_id)
            .execute()
            .await
            .expect("the admitted owner must be cancelled")
    );
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.slot("s").is_none()
        })
        .await,
        "the slot must settle after its owner completes"
    );
    assert!(
        !victim_ran.load(Ordering::SeqCst),
        "a queued submission claimed by cancel must never start"
    );
    assert!(ctrl.state().queued_slots.is_empty());

    stop_controller_loop(token, runner).await;
    let _ = runtime_handle.shutdown().await;
}
