//! Tests for draining accepted work and clearing controller state at shutdown.

use super::support::*;
use crate::controller::engine::state::PendingSubmission;
use crate::controller::engine::{
    AdmissionResult, CompletionResult, Controller, ControllerCommand, ControllerTask,
    IdentityOperation, Submission,
};

#[tokio::test]
async fn shutdown_finalizes_buffered_submission_as_rejected() {
    let bus = Bus::new(64);
    let mut events = bus.subscribe();
    let ctrl = make_controller(ControllerConfig::default(), bus);
    let implicit_name: Arc<str> = Arc::from("buffered");
    let explicit_task_name: Arc<str> = Arc::from("buffered-explicit-task");
    let explicit_slot: Arc<str> = Arc::from("buffered-explicit-slot");

    let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    let (implicit_id, implicit_waiter) = ctrl
        .handle()
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once(
            Arc::clone(&implicit_name),
            task,
        )))
        .await
        .expect("submission accepted into channel");
    let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    let (explicit_id, explicit_waiter) = ctrl
        .handle()
        .submit_and_watch(
            ControllerSpec::queue(TaskSpec::once(Arc::clone(&explicit_task_name), task))
                .with_slot(Arc::clone(&explicit_slot)),
        )
        .await
        .expect("explicit-slot submission accepted into channel");

    let mut rx = ctrl.take_command_receiver().expect("rx present");
    ctrl.finalize_pending_on_shutdown(&mut rx).await;
    drop(rx);

    for waiter in [implicit_waiter, explicit_waiter] {
        let outcome = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter must resolve, not hang")
            .expect("waiter must resolve to an outcome, not a dropped sender");
        assert!(
            matches!(outcome, TaskOutcome::Rejected { .. }),
            "a buffered submission on shutdown must resolve Rejected, got {outcome:?}"
        );
    }

    let rejected = drain_events(&mut events);
    let implicit_event = rejected
        .iter()
        .find(|event| event.kind == EventKind::ControllerRejected && event.id == Some(implicit_id))
        .expect("implicit-slot shutdown rejection must be published");
    assert!(Arc::ptr_eq(
        implicit_event
            .task
            .as_ref()
            .expect("implicit task name must become the event slot"),
        &implicit_name
    ));
    let explicit_event = rejected
        .iter()
        .find(|event| event.kind == EventKind::ControllerRejected && event.id == Some(explicit_id))
        .expect("explicit-slot shutdown rejection must be published");
    assert!(Arc::ptr_eq(
        explicit_event
            .task
            .as_ref()
            .expect("explicit slot must be present on the event"),
        &explicit_slot
    ));
    assert!(!Arc::ptr_eq(
        explicit_event
            .task
            .as_ref()
            .expect("explicit slot must be present on the event"),
        &explicit_task_name
    ));
}

#[tokio::test]
async fn outstanding_channel_permit_is_drained_with_explicit_terminal() {
    let bus = Bus::new(64);
    let mut events = bus.subscribe();
    let ctrl = make_controller(ControllerConfig::default(), bus);
    let permit = ctrl
        .tx
        .try_reserve()
        .expect("command capacity reserved before shutdown");
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let command = ControllerCommand::Submit(Box::new(Submission {
        id,
        owned: owned_controller_spec(
            ControllerSpec::queue(waiting_spec("active-shutdown-commit"))
                .with_slot("active-shutdown-commit-slot"),
        ),
        done: Some(done),
    }));
    let mut rx = ctrl.take_command_receiver().expect("receiver present");
    let mut finalize = Box::pin(ctrl.finalize_pending_on_shutdown(&mut rx));

    let completed_early = std::future::poll_fn(|context| {
        std::task::Poll::Ready(finalize.as_mut().poll(context).is_ready())
    })
    .await;
    assert!(ctrl.tx.is_closed());
    assert!(
        !completed_early,
        "shutdown must wait for a channel permit reserved before closure"
    );

    permit.send(command);
    tokio::time::timeout(Duration::from_secs(1), &mut finalize)
        .await
        .expect("shutdown must resume after the active commit sends");
    drop(finalize);

    let outcome = receive_oneshot(outcome, "active shutdown commit").await;
    assert!(matches!(
        &outcome,
        TaskOutcome::Rejected {
            kind: crate::events::RejectionKind::ControllerShuttingDown,
            reason,
        } if reason.as_ref() == crate::reasons::CONTROLLER_SHUTTING_DOWN
    ));
    let rejections: Vec<_> = drain_events(&mut events)
        .into_iter()
        .filter(|event| event.kind == EventKind::ControllerRejected && event.id == Some(id))
        .collect();
    assert_eq!(rejections.len(), 1);
    assert_rejection_parity(&rejections[0], id, &outcome);
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
}

#[tokio::test]
async fn shutdown_rejects_slot_queue_and_clears_controller_state() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let watched_id = TaskId::next();
    let unwatched_id = TaskId::next();
    let running_id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    ctrl.state().watchers.insert(watched_id, done);

    let slot = ctrl.get_or_create_slot("shutdown-slot");
    {
        let mut slot = slot.lock().await;
        *slot = running_slot(running_id);
        ctrl.push_queued(
            &mut slot,
            &Arc::from("shutdown-slot"),
            pending(watched_id, waiting_spec("watched-shutdown-queue")),
        );
        ctrl.push_queued(
            &mut slot,
            &Arc::from("shutdown-slot"),
            pending(unwatched_id, waiting_spec("plain-shutdown-queue")),
        );
    }

    ctrl.finalize_slot_state_on_shutdown().await;

    assert!(matches!(
        outcome.await,
        Ok(TaskOutcome::Rejected {
            kind: crate::RejectionKind::ControllerShuttingDown,
            reason,
            ..
        })
            if reason.as_ref() == crate::reasons::CONTROLLER_SHUTTING_DOWN
    ));
    {
        let state = ctrl.state();
        assert!(state.watchers.is_empty());
        assert!(state.slots.is_empty());
        assert!(state.queued_slots.is_empty());
        assert!(state.capacity_pending.is_empty());
    }
}

#[tokio::test]
async fn shutdown_rejects_capacity_waiting_admission() {
    let supervisor = Supervisor::new(
        crate::SupervisorConfig::default()
            .with_registry_queue_capacity(NonZeroUsize::new(1).unwrap()),
        vec![],
    );
    let filler_id = TaskId::next();
    let (_reply, _completion) = supervisor
        .core()
        .add_task_with_id_watched(
            filler_id,
            Arc::from("capacity-shutdown-filler"),
            owned_task_spec(waiting_spec("capacity-shutdown-filler")),
            None,
        )
        .expect("the filler must occupy the registry queue");
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), Bus::new(64));
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let mut operations = tracked_operations(&ctrl);

    handle_submission_fully(
        &ctrl,
        Submission {
            id,
            owned: owned_controller_spec(
                ControllerSpec::queue(waiting_spec("capacity-shutdown-target"))
                    .with_slot("capacity-shutdown-slot"),
            ),
            done: Some(done),
        },
        &mut operations,
    )
    .await;
    assert!(ctrl.state().capacity_pending.contains_key(&id));
    assert!(operations.admissions.is_empty());
    assert_eq!(operations.capacity.len(), 1);

    ctrl.finalize_slot_state_on_shutdown().await;
    assert!(matches!(
        receive_oneshot(outcome, "capacity-waiting shutdown outcome").await,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::ControllerShuttingDown,
            ..
        }
    ));
    {
        let state = ctrl.state();
        assert!(state.capacity_pending.is_empty());
        assert!(state.watchers.is_empty());
        assert!(state.slots.is_empty());
    }
    drop(operations);
}

#[tokio::test(flavor = "current_thread")]
async fn slot_shutdown_finishes_all_watchers_before_panicking_task_drop() {
    let ctrl = Arc::new(make_controller(ControllerConfig::default(), Bus::new(64)));
    let mut events = ctrl.bus.subscribe();
    let drops = Arc::new(AtomicUsize::new(0));
    let state_clean_at_drop = Arc::new(AtomicBool::new(false));
    let first = TaskId::next();
    let second = TaskId::next();
    let (first_done, first_outcome) = oneshot::channel();
    let (second_done, second_outcome) = oneshot::channel();
    {
        let mut state = ctrl.state();
        state.watchers.insert(first, first_done);
        state.watchers.insert(second, second_done);
    }

    let slot = ctrl.get_or_create_slot("slot-shutdown-drop-panic");
    {
        let mut slot = slot.lock().await;
        slot.queue.push_back(PendingSubmission::new(
            first,
            Arc::from("slot-shutdown-drop-panic"),
            with_controller_panic_reporter(
                isolated_owned_task_spec(TaskSpec::once(
                    "slot-shutdown-drop-panic",
                    Arc::new(ShutdownDropProbeTask {
                        controller: Arc::downgrade(&ctrl),
                        state_clean_at_drop: Arc::clone(&state_clean_at_drop),
                        drops: Arc::clone(&drops),
                    }),
                )),
                &ctrl.bus,
            ),
        ));
        slot.queue
            .push_back(pending(second, waiting_spec("slot-shutdown-after-panic")));
    }

    ctrl.finalize_slot_state_on_shutdown().await;

    let first_outcome = receive_oneshot(first_outcome, "first slot-shutdown watcher").await;
    let second_outcome = receive_oneshot(second_outcome, "second slot-shutdown watcher").await;
    for outcome in [&first_outcome, &second_outcome] {
        assert!(matches!(
            outcome,
            TaskOutcome::Rejected {
                kind: crate::RejectionKind::ControllerShuttingDown,
                ..
            }
        ));
    }
    assert!(
        poll_until(Duration::from_secs(2), || async {
            drops.load(Ordering::Acquire) == 1
        })
        .await,
        "deferred task destruction must complete"
    );
    assert!(
        state_clean_at_drop.load(Ordering::Acquire),
        "all controller watchers and slots must be finalized before user Drop"
    );
    assert!(ctrl.state().watchers.is_empty());
    assert!(ctrl.state().slots.is_empty());

    let drained = drain_until_runtime_failure(&mut events, "injected task drop panic").await;
    let first_event = drained
        .iter()
        .find(|event| event.kind == EventKind::ControllerRejected && event.id == Some(first))
        .expect("first slot-shutdown rejection event");
    let second_event = drained
        .iter()
        .find(|event| event.kind == EventKind::ControllerRejected && event.id == Some(second))
        .expect("second slot-shutdown rejection event");
    assert_rejection_parity(first_event, first, &first_outcome);
    assert_rejection_parity(second_event, second, &second_outcome);
    assert!(drained.iter().any(|event| {
        event.kind == EventKind::RuntimeFailure
            && event
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("injected task drop panic"))
    }));
}

#[tokio::test]
async fn slot_shutdown_is_not_blocked_by_a_blocking_task_destructor() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let gate = Arc::new((StdMutex::new(BlockingDropState::default()), Condvar::new()));
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    ctrl.state().watchers.insert(id, done);
    let slot_name: Arc<str> = Arc::from("blocking-drop-slot");
    let slot = ctrl.get_or_create_slot(&slot_name);
    {
        let mut slot = slot.lock().await;
        ctrl.push_queued(
            &mut slot,
            &slot_name,
            PendingSubmission::new(
                id,
                Arc::from("blocking-controller-drop"),
                owned_task_spec(TaskSpec::once(
                    "blocking-controller-drop",
                    Arc::new(BlockingDropTask {
                        gate: Arc::clone(&gate),
                    }),
                )),
            ),
        );
    }

    tokio::time::timeout(
        Duration::from_millis(200),
        ctrl.finalize_slot_state_on_shutdown(),
    )
    .await
    .expect("controller cleanup must not execute a blocking destructor inline");

    assert!(matches!(
        receive_oneshot(outcome, "blocking-drop shutdown watcher").await,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::ControllerShuttingDown,
            ..
        }
    ));
    {
        let state = ctrl.state();
        assert!(state.watchers.is_empty());
        assert!(state.slots.is_empty());
        assert!(state.queued_slots.is_empty());
    }

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if gate
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .entered
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the deferred executor must start the blocking destructor");
    {
        let mut state = gate.0.lock().unwrap_or_else(|error| error.into_inner());
        assert!(!state.released);
        assert!(!state.finished);
        state.released = true;
        gate.1.notify_all();
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if gate
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .finished
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the blocking destructor must finish after release");
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

    let mut rx = ctrl.take_command_receiver().expect("rx present");
    ctrl.finalize_pending_on_shutdown(&mut rx).await;

    assert!(matches!(
        reply_rx.await,
        Ok(Err(RuntimeError::ShuttingDown))
    ));
}

#[tokio::test]
async fn submit_after_shutdown_finalize_is_rejected_not_leaked() {
    let bus = Bus::new(64);
    let ctrl = make_controller(ControllerConfig::default(), bus);

    let mut rx = ctrl.take_command_receiver().expect("rx present");
    ctrl.finalize_pending_on_shutdown(&mut rx).await;

    let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    let result = ctrl
        .handle()
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once("late", task)).with_slot("s"))
        .await;

    assert!(matches!(result, Err(ControllerError::Closed)));
    drop(rx);
}

#[tokio::test]
async fn explicit_slot_shutdown_rejects_immediately() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let bus = Bus::new(64);
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), bus.clone());
    let mut events = bus.subscribe();
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let mut operations = tracked_operations(&ctrl);
    ctrl.mark_shutting_down();

    ctrl.handle_submission(
        Submission {
            id,
            owned: owned_controller_spec(
                ControllerSpec::queue(TaskSpec::once(
                    "explicit-shutdown-task",
                    Arc::new(SpawnBombTask),
                ))
                .with_slot("explicit-shutdown"),
            ),
            done: Some(done),
        },
        &mut operations,
    )
    .await;

    let outcome = receive_oneshot(outcome, "shutdown watcher").await;
    assert!(matches!(
        outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::ControllerShuttingDown,
            ..
        }
    ));
    let drained = drain_events(&mut events);
    let rejections: Vec<_> = drained
        .iter()
        .filter(|event| event.kind == EventKind::ControllerRejected)
        .collect();
    assert_eq!(rejections.len(), 1);
    assert_rejection_parity(rejections[0], id, &outcome);
    assert_eq!(rejections[0].task.as_deref(), Some("explicit-shutdown"));
    assert!(
        drained
            .iter()
            .all(|event| event.kind != EventKind::RuntimeFailure)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_slot_shutdown_while_waiting_for_lock_rechecks_shutdown() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let bus = Bus::new(64);
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), bus.clone());
    let mut events = bus.subscribe();
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let slot = ctrl.get_or_create_slot("locked-at-shutdown");
    let owner = TaskId::next();
    let mut slot_guard = slot.lock().await;
    *slot_guard = running_slot(owner);
    let mut operations = tracked_operations(&ctrl);
    let mut admission = Box::pin(
        ctrl.handle_submission(
            Submission {
                id,
                owned: owned_controller_spec(
                    ControllerSpec::queue(TaskSpec::once(
                        "locked-at-shutdown-task",
                        Arc::new(SpawnBombTask),
                    ))
                    .with_slot("locked-at-shutdown"),
                ),
                done: Some(done),
            },
            &mut operations,
        ),
    );

    std::future::poll_fn(|cx| match admission.as_mut().poll(cx) {
        std::task::Poll::Pending => std::task::Poll::Ready(()),
        std::task::Poll::Ready(()) => {
            panic!("submission must wait for the held explicit-slot lock")
        }
    })
    .await;
    ctrl.mark_shutting_down();
    drop(slot_guard);
    tokio::time::timeout(Duration::from_secs(2), admission)
        .await
        .expect("submission must resume after the explicit-slot lock is released");

    let outcome = receive_oneshot(outcome, "lock-wait shutdown watcher").await;
    assert!(matches!(
        outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::ControllerShuttingDown,
            ..
        }
    ));
    assert!(operations.admissions.is_empty());
    assert!(operations.removals.is_empty());
    assert!(ctrl.state().watchers.is_empty());
    let slot = ctrl
        .slot("locked-at-shutdown")
        .expect("the existing running slot remains owned");
    assert_eq!(slot.lock().await.owner_id(), Some(owner));

    let drained = drain_events(&mut events);
    let rejections: Vec<_> = drained
        .iter()
        .filter(|event| event.kind == EventKind::ControllerRejected)
        .collect();
    assert_eq!(rejections.len(), 1);
    assert_rejection_parity(rejections[0], id, &outcome);
    assert_eq!(rejections[0].task.as_deref(), Some("locked-at-shutdown"));
    assert!(
        drained
            .iter()
            .all(|event| event.kind != EventKind::RuntimeFailure)
    );
}

#[tokio::test]
async fn buffered_shutdown_drain_continues_after_task_drop_panic() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let mut events = ctrl.bus.subscribe();
    let drops = Arc::new(AtomicUsize::new(0));
    let first = TaskId::next();
    let second = TaskId::next();
    let (first_done, first_outcome) = oneshot::channel();
    let (second_done, second_outcome) = oneshot::channel();
    let (identity_reply, identity_result) = oneshot::channel();

    ctrl.tx
        .try_send(ControllerCommand::Submit(Box::new(Submission {
            id: first,
            owned: with_controller_panic_reporter(
                isolated_owned_controller_spec(
                    ControllerSpec::queue(TaskSpec::once(
                        "buffered-drop-panic",
                        Arc::new(PanickingDropTask {
                            drops: Arc::clone(&drops),
                        }),
                    ))
                    .with_slot("buffered-first"),
                ),
                &ctrl.bus,
            ),
            done: Some(first_done),
        })))
        .expect("first buffered submission");
    ctrl.tx
        .try_send(ControllerCommand::Submit(Box::new(Submission {
            id: second,
            owned: owned_controller_spec(
                ControllerSpec::queue(waiting_spec("buffered-after-drop-panic"))
                    .with_slot("buffered-second"),
            ),
            done: Some(second_done),
        })))
        .expect("second buffered submission");
    ctrl.tx
        .try_send(ControllerCommand::ManageIdentity {
            id: TaskId::next(),
            operation: IdentityOperation::Remove,
            reply: identity_reply,
        })
        .expect("buffered identity operation");

    let mut rx = ctrl.take_command_receiver().expect("rx present");
    ctrl.finalize_pending_on_shutdown(&mut rx).await;

    assert!(
        poll_until(Duration::from_secs(2), || async {
            drops.load(Ordering::Acquire) == 1
        })
        .await,
        "deferred task destruction must complete"
    );
    let first_outcome = receive_oneshot(first_outcome, "first shutdown rejection").await;
    let second_outcome = receive_oneshot(second_outcome, "second shutdown rejection").await;
    for outcome in [&first_outcome, &second_outcome] {
        assert!(matches!(
            outcome,
            TaskOutcome::Rejected {
                kind: crate::RejectionKind::ControllerShuttingDown,
                ..
            }
        ));
    }
    assert!(matches!(
        receive_oneshot(identity_result, "buffered shutdown identity reply").await,
        Err(RuntimeError::ShuttingDown)
    ));

    let drained = drain_until_runtime_failure(&mut events, "injected task drop panic").await;
    let first_event = drained
        .iter()
        .find(|event| event.kind == EventKind::ControllerRejected && event.id == Some(first))
        .expect("first shutdown event");
    let second_event = drained
        .iter()
        .find(|event| event.kind == EventKind::ControllerRejected && event.id == Some(second))
        .expect("second shutdown event");
    assert_rejection_parity(first_event, first, &first_outcome);
    assert_rejection_parity(second_event, second, &second_outcome);
    assert!(drained.iter().any(|event| {
        event.kind == EventKind::RuntimeFailure
            && event
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("injected task drop panic"))
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn public_shutdown_waits_for_controller_join_and_survives_a_dropped_waiter() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let _runtime_handle = sup.serve().expect("runtime startup");
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
        .execute()
        .await
        .expect("the blocking submission must enter the controller queue");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.tx.capacity() == ctrl.config.queue_capacity().get()
        })
        .await,
        "the controller must receive the command and block on the held slot lock"
    );

    let queued_waiter = handle
        .submit(
            ControllerSpec::queue(waiting_spec("buffered-during-shutdown"))
                .with_slot("buffered-during-shutdown"),
        )
        .watch()
        .execute()
        .await
        .expect("the watched command must be buffered behind the blocked handler");
    let second_waiter = handle
        .submit(ControllerSpec::queue(make_spec(
            "second-buffered-during-shutdown",
        )))
        .watch()
        .execute()
        .await
        .expect("the second watched command must remain buffered for shutdown drain");
    let identity_handle = handle.clone();
    let identity =
        tokio::spawn(async move { identity_handle.cancel(TaskId::next()).execute().await });
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
    assert!(ctrl.state().slots.is_empty());
    assert!(ctrl.state().watchers.is_empty());
    let queued_outcome = tokio::time::timeout(Duration::from_millis(50), queued_waiter.wait())
        .await
        .expect("the buffered watcher must already be settled")
        .expect("the buffered watched command must resolve before shutdown returns");
    assert!(matches!(
        queued_outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::ControllerShuttingDown,
            reason,
            ..
        }
            if reason.as_ref() == crate::reasons::CONTROLLER_SHUTTING_DOWN
    ));
    let panicking_outcome = tokio::time::timeout(Duration::from_millis(50), second_waiter.wait())
        .await
        .expect("the hostile buffered watcher must already be settled")
        .expect("the hostile buffered watcher must resolve as an outcome");
    assert!(matches!(
        panicking_outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::ControllerShuttingDown,
            reason,
            ..
        }
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

#[tokio::test]
async fn no_queue_advancement_after_shutdown_starts() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let handle = sup.serve().expect("runtime startup");
    let id = handle
        .add(waiting_spec("occupant"))
        .execute()
        .await
        .expect("task should register");
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));

    let mut queue = std::collections::VecDeque::new();
    queue.push_back(pending(TaskId::next(), waiting_spec("queued")));
    let mut slot = running_slot(id);
    slot.queue = queue;
    ctrl.state()
        .slots
        .insert(Arc::from("s"), Arc::new(Mutex::new(slot)));
    let mut operations = tracked_operations(&ctrl);
    ctrl.mark_shutting_down();
    ctrl.handle_completion_result(
        CompletionResult {
            id,
            slot_name: Arc::from("s"),
        },
        &mut operations,
    )
    .await;

    assert!(
        operations.admissions.is_empty(),
        "shutdown must prevent a queued admission from being scheduled"
    );
    assert!(
        sup.core().id_for_name("queued").await.is_none(),
        "controller must not start queued tasks once shutdown has been requested"
    );

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn admission_rejection_after_shutdown_does_not_advance_queue() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    let owner = TaskId::next();
    let queued = TaskId::next();
    let slot_name: Arc<str> = Arc::from("shutdown-admission-slot");
    let slot_arc = ctrl.get_or_create_slot(&slot_name);
    {
        let mut slot = slot_arc.lock().await;
        *slot = admitting_slot(owner);
        ctrl.push_queued(
            &mut slot,
            &slot_name,
            pending(queued, waiting_spec("queued-after-admission-rejection")),
        );
    }
    let mut operations = tracked_operations(&ctrl);
    ctrl.mark_shutting_down();

    ctrl.handle_admission_result(
        AdmissionResult {
            id: owner,
            slot_name: Arc::clone(&slot_name),
            decision: Err(RuntimeError::ShuttingDown),
        },
        &mut operations,
    )
    .await;

    {
        let slot = slot_arc.lock().await;
        assert!(slot.is_idle());
        assert_eq!(slot.queue.front().map(|pending| pending.id), Some(queued));
    }
    assert_eq!(
        ctrl.state().queued_slots.get(&queued),
        Some(&slot_name),
        "shutdown must retain the queue for shutdown finalization"
    );
    assert!(operations.capacity.is_empty());
    assert!(operations.admissions.is_empty());
    assert!(operations.completions.is_empty());
    assert!(operations.removals.is_empty());
    assert!(
        sup.core()
            .id_for_name("queued-after-admission-rejection")
            .await
            .is_none()
    );

    ctrl.finalize_slot_state_on_shutdown().await;
}
