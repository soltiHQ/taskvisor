//! Tests for submission admission and registry decisions.

use super::support::*;
use crate::controller::engine::state::{PendingSubmission, SlotPhase, SlotState};
use crate::controller::engine::{AdmissionResult, Controller, Submission};

#[tokio::test]
async fn stale_admission_ok_and_err_do_not_mutate_new_owner() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let stale_id = TaskId::next();
    let current_id = TaskId::next();
    let slot_arc = ctrl.get_or_create_slot("s");
    {
        let mut slot = slot_arc.lock().await;
        *slot = admitting_slot(current_id);
    }

    let mut operations = tracked_operations(&ctrl);
    ctrl.handle_admission_result(
        AdmissionResult {
            id: stale_id,
            slot_name: Arc::from("s"),
            decision: Ok(crate::core::RemovalCompletion::new()),
        },
        &mut operations,
    )
    .await;
    ctrl.handle_admission_result(
        AdmissionResult {
            id: stale_id,
            slot_name: Arc::from("s"),
            decision: Err(RuntimeError::ShuttingDown),
        },
        &mut operations,
    )
    .await;

    let slot = slot_arc.lock().await;
    assert_eq!(slot.owner_id(), Some(current_id));
    assert!(matches!(
        slot.phase(),
        SlotPhase::Admitting { owner, .. } if owner == current_id
    ));
    assert!(operations.completions.is_empty());
    assert!(operations.removals.is_empty());
}

#[tokio::test]
async fn explicit_slot_drop_if_running_rejects_before_pending_ownership() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let bus = Bus::new(64);
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), bus.clone());
    let mut events = bus.subscribe();
    let owner = TaskId::next();
    ctrl.state().slots.insert(
        Arc::from("busy-slot"),
        Arc::new(Mutex::new(running_slot(owner))),
    );
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let mut operations = tracked_operations(&ctrl);

    ctrl.handle_submission(
        Submission {
            id,
            owned: owned_controller_spec(
                ControllerSpec::drop_if_running(TaskSpec::once(
                    "busy-slot-task",
                    Arc::new(SpawnBombTask),
                ))
                .with_slot("busy-slot"),
            ),
            done: Some(done),
        },
        &mut operations,
    )
    .await;

    let outcome = receive_oneshot(outcome, "busy-rejection watcher").await;
    assert!(matches!(
        outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::SlotBusy,
            ..
        }
    ));
    let slot = ctrl.slot("busy-slot").expect("busy slot remains");
    let slot = slot.lock().await;
    assert_eq!(slot.owner_id(), Some(owner));
    assert!(slot.queue.is_empty());
    drop(slot);
    let drained = drain_events(&mut events);
    let rejections: Vec<_> = drained
        .iter()
        .filter(|event| event.kind == EventKind::ControllerRejected)
        .collect();
    assert_eq!(rejections.len(), 1);
    assert_rejection_parity(rejections[0], id, &outcome);
    assert!(
        drained
            .iter()
            .all(|event| event.kind != EventKind::RuntimeFailure)
    );
}

#[tokio::test]
async fn explicit_slot_queue_full_rejects_before_pending_ownership() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let bus = Bus::new(64);
    let config = ControllerConfig::new(NonZeroUsize::new(16).unwrap(), 1);
    let ctrl = Controller::new(config, supervisor.core(), bus.clone());
    let mut events = bus.subscribe();
    let owner = TaskId::next();
    let queued = TaskId::next();
    let mut state = running_slot(owner);
    state
        .queue
        .push_back(pending(queued, waiting_spec("existing-head")));
    ctrl.state()
        .slots
        .insert(Arc::from("full-slot"), Arc::new(Mutex::new(state)));
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let mut operations = tracked_operations(&ctrl);

    ctrl.handle_submission(
        Submission {
            id,
            owned: owned_controller_spec(
                ControllerSpec::queue(TaskSpec::once("full-slot-task", Arc::new(SpawnBombTask)))
                    .with_slot("full-slot"),
            ),
            done: Some(done),
        },
        &mut operations,
    )
    .await;

    let outcome = receive_oneshot(outcome, "queue-full watcher").await;
    assert!(matches!(
        outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::QueueFull,
            ..
        }
    ));
    let slot = ctrl.slot("full-slot").expect("full slot remains");
    let slot = slot.lock().await;
    assert_eq!(slot.owner_id(), Some(owner));
    assert_eq!(slot.queue.len(), 1);
    assert_eq!(slot.queue.front().map(|pending| pending.id), Some(queued));
    drop(slot);
    let drained = drain_events(&mut events);
    let rejections: Vec<_> = drained
        .iter()
        .filter(|event| event.kind == EventKind::ControllerRejected)
        .collect();
    assert_eq!(rejections.len(), 1);
    assert_rejection_parity(rejections[0], id, &outcome);
    assert!(
        drained
            .iter()
            .all(|event| event.kind != EventKind::RuntimeFailure)
    );
}

#[tokio::test]
async fn controller_slot_limit_rejects_submission_without_retaining_ownership() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let bus = Bus::new(64);
    let config = ControllerConfig::default().with_max_controller_slots(NonZeroUsize::new(1));
    let ctrl = Controller::new(config, supervisor.core(), bus.clone());
    let mut events = bus.subscribe();
    let existing_owner = TaskId::next();
    ctrl.state().slots.insert(
        Arc::from("existing-slot"),
        Arc::new(Mutex::new(running_slot(existing_owner))),
    );
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let mut operations = tracked_operations(&ctrl);

    ctrl.handle_submission(
        Submission {
            id,
            owned: owned_controller_spec(
                ControllerSpec::queue(TaskSpec::once("slot-limit-task", Arc::new(SpawnBombTask)))
                    .with_slot("new-slot"),
            ),
            done: Some(done),
        },
        &mut operations,
    )
    .await;

    let outcome = receive_oneshot(outcome, "slot-limit watcher").await;
    assert!(matches!(
        outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::ResourceLimit,
            ..
        }
    ));
    {
        let state = ctrl.state();
        assert_eq!(state.slots.len(), 1);
        assert!(state.slots.contains_key("existing-slot"));
        assert!(!state.slots.contains_key("new-slot"));
        assert!(state.watchers.is_empty());
        assert!(state.queued_slots.is_empty());
        assert!(state.capacity_pending.is_empty());
    }
    assert!(operations.capacity.is_empty());
    assert!(operations.admissions.is_empty());
    assert!(operations.removals.is_empty());

    let drained = drain_events(&mut events);
    let rejection = drained
        .iter()
        .find(|event| event.kind == EventKind::ControllerRejected && event.id == Some(id))
        .expect("slot-limit rejection event");
    assert_rejection_parity(rejection, id, &outcome);
    assert_eq!(
        rejection.rejection_kind,
        Some(crate::RejectionKind::ResourceLimit)
    );
}

#[tokio::test]
async fn controller_registry_commit_uses_stored_task_name() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), Bus::new(64));
    let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    let task_name: Arc<str> = Arc::from("stored-task-name");
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let mut operations = tracked_operations(&ctrl);

    handle_submission_fully(
        &ctrl,
        Submission {
            id,
            owned: owned_controller_spec(ControllerSpec::queue(TaskSpec::once(
                Arc::clone(&task_name),
                task,
            ))),
            done: Some(done),
        },
        &mut operations,
    )
    .await;

    assert_eq!(operations.admissions.len(), 1);
    assert!(operations.removals.is_empty());
    assert!(ctrl.state().watchers.is_empty());
    {
        let state = ctrl.state();
        let stored_name = state
            .slots
            .keys()
            .find(|name| name.as_ref() == task_name.as_ref())
            .expect("the implicit slot must retain the task name");
        assert!(Arc::ptr_eq(&task_name, stored_name));
    }
    let slot = ctrl
        .slot("stored-task-name")
        .expect("the stored task name must become the fallback slot");
    assert!(matches!(
        slot.lock().await.phase(),
        SlotPhase::Admitting { owner, .. } if owner == id
    ));

    drop(outcome);
    abort_and_drain(&mut operations.admissions).await;
    abort_and_drain(&mut operations.removals).await;
}

#[tokio::test]
async fn stored_names_apply_cross_slot_submissions_inline_in_fifo_order() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let bus = Bus::new(64);
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), bus.clone());
    let mut events = bus.subscribe();
    let mut operations = tracked_operations(&ctrl);
    let first = TaskId::next();
    let second = TaskId::next();

    for (id, name) in [(first, "inline-first"), (second, "inline-second")] {
        ctrl.handle_submission(
            Submission {
                id,
                owned: owned_controller_spec(ControllerSpec::queue(make_spec(name))),
                done: None,
            },
            &mut operations,
        )
        .await;
    }

    for (id, name) in [(first, "inline-first"), (second, "inline-second")] {
        let slot = ctrl
            .slot(name)
            .expect("the stored task name must create its implicit slot inline");
        assert!(matches!(
            slot.lock().await.phase(),
            SlotPhase::Admitting { owner, .. } if owner == id
        ));
    }
    assert_eq!(
        operations.admissions.len(),
        2,
        "one pending registry reply must not prevent the next slot from starting admission"
    );
    let submitted: Vec<_> = drain_events(&mut events)
        .into_iter()
        .filter(|event| event.kind == EventKind::ControllerSubmitted)
        .filter_map(|event| event.id)
        .collect();
    assert_eq!(submitted, vec![first, second]);

    abort_and_drain(&mut operations.admissions).await;
}

#[tokio::test]
async fn registry_precommit_shutdown_drop_panic_preserves_watcher_and_controller_state() {
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
            Arc::from("drop-panic-filler"),
            owned_task_spec(waiting_spec("drop-panic-filler")),
            None,
        )
        .expect("the filler must occupy the registry queue");
    assert_eq!(supervisor.core().registry_command_capacity(), 0);
    supervisor.core().close_registry_admission_for_test();

    let bus = Bus::new(64);
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), bus.clone());
    let mut events = bus.subscribe();
    let drops = Arc::new(AtomicUsize::new(0));
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let mut operations = tracked_operations(&ctrl);

    handle_submission_fully(
        &ctrl,
        Submission {
            id,
            owned: with_controller_panic_reporter(
                isolated_owned_controller_spec(
                    ControllerSpec::queue(TaskSpec::once(
                        "drop-panic-uncommitted",
                        Arc::new(PanickingDropTask {
                            drops: Arc::clone(&drops),
                        }),
                    ))
                    .with_slot("drop-panic-slot"),
                ),
                &ctrl.bus,
            ),
            done: Some(done),
        },
        &mut operations,
    )
    .await;

    assert!(
        poll_until(Duration::from_secs(2), || async {
            drops.load(Ordering::Acquire) == 1
        })
        .await,
        "deferred task destruction must complete"
    );
    let outcome = receive_oneshot(outcome, "pre-commit failure watcher").await;
    assert!(matches!(
        outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::AdmissionFailed,
            ..
        }
    ));
    assert!(ctrl.state().watchers.is_empty());
    assert!(ctrl.slot("drop-panic-slot").is_none());
    assert!(operations.admissions.is_empty());
    assert!(operations.removals.is_empty());

    let drained = drain_until_runtime_failure(&mut events, "injected task drop panic").await;
    let rejection = drained
        .iter()
        .find(|event| event.kind == EventKind::ControllerRejected)
        .expect("pre-commit failure must publish a rejection");
    assert_rejection_parity(rejection, id, &outcome);
    let drop_failure = drained
        .iter()
        .find(|event| {
            event.kind == EventKind::RuntimeFailure
                && event
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("injected task drop panic"))
        })
        .expect("panicking destructor must be isolated and diagnosed");
    assert_eq!(drop_failure.task.as_deref(), Some("controller"));
}

#[tokio::test]
async fn queued_precommit_failures_finish_before_panicking_task_drop() {
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
            Arc::from("queued-drop-filler"),
            owned_task_spec(waiting_spec("queued-drop-filler")),
            None,
        )
        .expect("the filler must occupy the registry queue");
    supervisor.core().close_registry_admission_for_test();

    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), Bus::new(64));
    let drops = Arc::new(AtomicUsize::new(0));
    let first = TaskId::next();
    let second = TaskId::next();
    let (first_done, first_outcome) = oneshot::channel();
    let (second_done, second_outcome) = oneshot::channel();
    {
        let mut state = ctrl.state();
        state.watchers.insert(first, first_done);
        state.watchers.insert(second, second_done);
    }
    let mut slot = SlotState::new();
    slot.queue.push_back(PendingSubmission::new(
        first,
        Arc::from("queued-drop-panic"),
        with_controller_panic_reporter(
            isolated_owned_task_spec(TaskSpec::once(
                "queued-drop-panic",
                Arc::new(PanickingDropTask {
                    drops: Arc::clone(&drops),
                }),
            )),
            &ctrl.bus,
        ),
    ));
    slot.queue
        .push_back(pending(second, waiting_spec("queued-after-drop-panic")));
    let slot_name = Arc::from("queued-drop-slot");
    let mut operations = tracked_operations(&ctrl);

    let deferred =
        ctrl.start_next_from_queue(supervisor.core(), &mut slot, &slot_name, &mut operations);

    assert!(slot.is_idle());
    assert!(slot.queue.is_empty());
    assert!(operations.admissions.is_empty());
    assert_eq!(deferred.len(), 2);
    assert!(matches!(
        receive_oneshot(first_outcome, "first queued pre-commit watcher").await,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::AdmissionFailed,
            ..
        }
    ));
    assert!(matches!(
        receive_oneshot(second_outcome, "second queued pre-commit watcher").await,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::AdmissionFailed,
            ..
        }
    ));
    assert!(ctrl.state().watchers.is_empty());
    assert_eq!(drops.load(Ordering::Acquire), 0);

    ctrl.drop_pending_submissions(deferred);
    assert!(
        poll_until(Duration::from_secs(2), || async {
            drops.load(Ordering::Acquire) == 1
        })
        .await,
        "deferred task destruction must complete"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn registry_reply_marks_slot_running_without_task_added() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let handle = sup.serve().expect("runtime startup");
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
        controller_bus.publish(Event::new(EventKind::AttemptStarting).with_task("noise"));
    }

    let reached_running = poll_until(Duration::from_secs(2), || async {
        let Some(slot) = ctrl.slot("s") else {
            return false;
        };
        let slot = slot.lock().await;
        slot.owner_id() == Some(id) && matches!(slot.phase(), SlotPhase::Running { .. })
    })
    .await;

    assert!(
        reached_running,
        "the direct registry reply must confirm admission without TaskAdded"
    );
    stop_controller_loop(token, runner).await;
    assert!(ctrl.state().slots.is_empty());

    let _ = handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_reply_frees_slot_without_task_add_failed() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let handle = sup.serve().expect("runtime startup");
    handle
        .add(waiting_spec("duplicate-reply"))
        .execute()
        .await
        .expect("the existing task must register");

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
    assert!(matches!(outcome, TaskOutcome::Rejected {
            kind: crate::RejectionKind::AlreadyExists,
            reason,
            ..
        } if reason.as_ref() == crate::reasons::ALREADY_EXISTS));
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.slot("s").is_none() && !ctrl.state().watchers.contains_key(&id)
        })
        .await,
        "the rejected admission must release its slot ownership"
    );
    assert!(
        ctrl.slot("s").is_none(),
        "an idle empty slot should be collected after registry rejection"
    );

    stop_controller_loop(token, runner).await;
    let _ = handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn queued_admission_skips_registry_rejected_head() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let handle = sup.serve().expect("runtime startup");
    handle
        .add(waiting_spec("queued-duplicate"))
        .execute()
        .await
        .expect("the existing task must register");
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(1));
    let slot_name: Arc<str> = Arc::from("s");
    let slot_arc = ctrl.get_or_create_slot(&slot_name);
    let duplicate_id = TaskId::next();
    let accepted_id = TaskId::next();
    let (duplicate_done, duplicate_outcome) = oneshot::channel();
    let (accepted_done, _accepted_outcome) = oneshot::channel();
    {
        let mut state = ctrl.state();
        state.watchers.insert(duplicate_id, duplicate_done);
        state.watchers.insert(accepted_id, accepted_done);
    }

    let mut operations = tracked_operations(&ctrl);
    {
        let mut slot = slot_arc.lock().await;
        slot.queue
            .push_back(pending(duplicate_id, waiting_spec("queued-duplicate")));
        slot.queue
            .push_back(pending(accepted_id, waiting_spec("queued-accepted")));
        let deferred =
            ctrl.start_next_from_queue(sup.core(), &mut slot, &slot_name, &mut operations);
        assert!(deferred.is_empty());
    }

    for _ in 0..2 {
        let result = tokio::time::timeout(Duration::from_secs(2), operations.admissions.next())
            .await
            .expect("registry admission reply must arrive")
            .expect("one admission must be in flight")
            .expect("admission waiter must not fail");
        ctrl.handle_admission_result(result, &mut operations).await;
    }

    let duplicate_outcome = duplicate_outcome
        .await
        .expect("registry must resolve the duplicate watcher");
    assert!(matches!(duplicate_outcome, TaskOutcome::Rejected {
            kind: crate::RejectionKind::AlreadyExists,
            reason,
            ..
        } if reason.as_ref() == crate::reasons::ALREADY_EXISTS));
    let slot = slot_arc.lock().await;
    assert_eq!(slot.owner_id(), Some(accepted_id));
    assert!(matches!(slot.phase(), SlotPhase::Running { .. }));
    assert!(slot.queue.is_empty());
    assert_ne!(slot.owner_id(), Some(duplicate_id));

    let _ = handle.shutdown().await;
}
