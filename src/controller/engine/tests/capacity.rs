//! Tests for registry capacity waiting and cancellation.

use super::support::*;
use crate::controller::engine::state::{CapacityPending, SlotPhase};
use crate::controller::engine::{Controller, Submission};

#[tokio::test]
async fn capacity_waiter_removal_cancels_pump_after_stale_slot_loss() {
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
            Arc::from("stale-slot-filler"),
            owned_task_spec(waiting_spec("stale-slot-filler")),
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
                ControllerSpec::queue(waiting_spec("stale-slot-target")).with_slot("stale-slot"),
            ),
            done: Some(done),
        },
        &mut operations,
    )
    .await;
    assert!(ctrl.state().capacity_pending.contains_key(&id));
    assert_eq!(operations.capacity.len(), 1);

    ctrl.state().slots.remove("stale-slot");
    assert!(
        ctrl.remove_queued_submission(id, Some("test_remove"), &mut operations)
            .await
    );
    assert!(operations.capacity.is_empty());
    assert!(!ctrl.state().capacity_pending.contains_key(&id));
    assert!(!ctrl.state().watchers.contains_key(&id));
    assert!(matches!(
        receive_oneshot(outcome, "stale capacity waiter removal").await,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::RemovedFromQueue,
            ..
        }
    ));
}

#[tokio::test]
async fn admission_capacity_rejection_rolls_back_pending_ownership() {
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
            Arc::from("admission-limit-filler"),
            owned_task_spec(waiting_spec("admission-limit-filler")),
            None,
        )
        .expect("the filler must occupy the registry queue");

    let bus = Bus::new(64);
    let mut events = bus.subscribe();
    let config = ControllerConfig::default().with_admission_capacity(NonZeroUsize::new(1).unwrap());
    let ctrl = Controller::new(config, supervisor.core(), bus);
    let first = TaskId::next();
    let second = TaskId::next();
    let (first_done, first_outcome) = oneshot::channel();
    let (second_done, second_outcome) = oneshot::channel();
    let mut operations = tracked_operations(&ctrl);

    ctrl.handle_submission(
        Submission {
            id: first,
            owned: owned_controller_spec(
                ControllerSpec::queue(waiting_spec("admission-limit-first"))
                    .with_slot("admission-limit-first-slot"),
            ),
            done: Some(first_done),
        },
        &mut operations,
    )
    .await;
    ctrl.handle_submission(
        Submission {
            id: second,
            owned: owned_controller_spec(
                ControllerSpec::queue(waiting_spec("admission-limit-second"))
                    .with_slot("admission-limit-second-slot"),
            ),
            done: Some(second_done),
        },
        &mut operations,
    )
    .await;

    let outcome = receive_oneshot(second_outcome, "admission-limit watcher").await;
    assert!(matches!(
        outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::ResourceLimit,
            ..
        }
    ));
    {
        let state = ctrl.state();
        assert_eq!(state.capacity_pending.len(), 1);
        assert!(state.capacity_pending.contains_key(&first));
        assert!(!state.capacity_pending.contains_key(&second));
        assert!(state.watchers.contains_key(&first));
        assert!(!state.watchers.contains_key(&second));
        assert!(state.slots.contains_key("admission-limit-first-slot"));
        assert!(!state.slots.contains_key("admission-limit-second-slot"));
    }
    assert_eq!(operations.capacity.len(), 1);
    let rejection = drain_events(&mut events)
        .into_iter()
        .find(|event| event.kind == EventKind::ControllerRejected && event.id == Some(second))
        .expect("admission-limit rejection event");
    assert_rejection_parity(&rejection, second, &outcome);

    ctrl.finalize_slot_state_on_shutdown().await;
    assert!(matches!(
        receive_oneshot(first_outcome, "retained admission shutdown").await,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::ControllerShuttingDown,
            ..
        }
    ));
    drop(operations);
}

#[tokio::test]
async fn stale_capacity_results_release_pending_ownership() {
    let bus = Bus::new(64);
    let mut events = bus.subscribe();
    let ctrl = make_controller(ControllerConfig::default(), bus);
    let lost = TaskId::next();
    let changed = TaskId::next();
    let current = TaskId::next();
    let lost_slot: Arc<str> = Arc::from("lost-capacity-slot");
    let changed_slot: Arc<str> = Arc::from("changed-capacity-slot");
    let (lost_done, lost_outcome) = oneshot::channel();
    let (changed_done, changed_outcome) = oneshot::channel();
    {
        let mut state = ctrl.state();
        state.watchers.insert(lost, lost_done);
        state.watchers.insert(changed, changed_done);
        state.capacity_pending.insert(
            lost,
            CapacityPending {
                slot_name: Arc::clone(&lost_slot),
                pending: pending(lost, waiting_spec("lost-capacity-task")),
            },
        );
        state.capacity_pending.insert(
            changed,
            CapacityPending {
                slot_name: Arc::clone(&changed_slot),
                pending: pending(changed, waiting_spec("changed-capacity-task")),
            },
        );
        state.slots.insert(
            Arc::clone(&changed_slot),
            Arc::new(Mutex::new(running_slot(current))),
        );
    }
    let mut operations = tracked_operations(&ctrl);

    for id in [lost, changed] {
        ctrl.handle_registry_capacity_result(id, Err(RuntimeError::ShuttingDown), &mut operations)
            .await;
    }

    let lost_outcome = receive_oneshot(lost_outcome, "lost capacity watcher").await;
    let changed_outcome = receive_oneshot(changed_outcome, "changed capacity watcher").await;
    for outcome in [&lost_outcome, &changed_outcome] {
        assert!(matches!(
            outcome,
            TaskOutcome::Rejected {
                kind: crate::RejectionKind::AdmissionFailed,
                ..
            }
        ));
    }
    {
        let state = ctrl.state();
        assert!(state.capacity_pending.is_empty());
        assert!(state.watchers.is_empty());
        assert!(!state.slots.contains_key(&*lost_slot));
    }
    let slot = ctrl
        .slot(&changed_slot)
        .expect("the changed owner must remain tracked");
    assert_eq!(slot.lock().await.owner_id(), Some(current));
    assert!(operations.capacity.is_empty());
    assert!(operations.admissions.is_empty());

    let drained = drain_events(&mut events);
    for (id, outcome) in [(lost, &lost_outcome), (changed, &changed_outcome)] {
        let rejection = drained
            .iter()
            .find(|event| event.kind == EventKind::ControllerRejected && event.id == Some(id))
            .expect("stale capacity rejection event");
        assert_rejection_parity(rejection, id, outcome);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn transient_registry_full_waits_then_admits_without_rejection() {
    let supervisor = Supervisor::new(
        crate::SupervisorConfig::default()
            .with_registry_queue_capacity(NonZeroUsize::new(1).unwrap()),
        vec![],
    );
    let filler_id = TaskId::next();
    let (filler_reply, _filler_completion) = supervisor
        .core()
        .add_task_with_id_watched(
            filler_id,
            Arc::from("transient-full-filler"),
            owned_task_spec(waiting_spec("transient-full-filler")),
            None,
        )
        .expect("the filler must occupy the registry queue");
    assert_eq!(supervisor.core().registry_command_capacity(), 0);

    let bus = Bus::new(64);
    let mut events = bus.subscribe();
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), bus);
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;
    let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    let (id, outcome) = ctrl
        .handle()
        .submit_and_watch(
            ControllerSpec::queue(TaskSpec::once("transient-full-target", task))
                .with_slot("transient-slot"),
        )
        .await
        .expect("the controller command queue must accept the target");

    assert!(
        poll_until(Duration::from_secs(2), || async {
            let retained = {
                let state = ctrl.state();
                state.capacity_pending.contains_key(&id) && state.watchers.contains_key(&id)
            };
            retained
                && ctrl.slot("transient-slot").is_some_and(|slot| {
                    slot.try_lock().is_ok_and(|slot| {
                        matches!(slot.phase(), SlotPhase::Admitting { owner, .. } if owner == id)
                    })
                })
        })
        .await,
        "registry backpressure must retain the payload and watcher in an admitting slot"
    );

    let runtime_handle = supervisor.serve().expect("runtime startup");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), filler_reply).await,
        Ok(Ok(Ok(())))
    ));
    assert!(matches!(
        receive_oneshot(outcome, "transient registry-full outcome").await,
        TaskOutcome::Completed
    ));
    assert!(
        poll_until(Duration::from_secs(2), || async {
            let released = {
                let state = ctrl.state();
                !state.capacity_pending.contains_key(&id) && !state.watchers.contains_key(&id)
            };
            released && ctrl.slot("transient-slot").is_none()
        })
        .await,
        "successful admission must release all controller-owned pending state"
    );
    assert!(
        !drain_events(&mut events)
            .iter()
            .any(|event| { event.kind == EventKind::ControllerRejected && event.id == Some(id) })
    );

    stop_controller_loop(token, runner).await;
    let _ = runtime_handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn capacity_waiting_admission_remains_removable_by_id() {
    let supervisor = Supervisor::new(
        crate::SupervisorConfig::default()
            .with_registry_queue_capacity(NonZeroUsize::new(1).unwrap()),
        vec![],
    );
    let filler_id = TaskId::next();
    let (_filler_reply, _filler_completion) = supervisor
        .core()
        .add_task_with_id_watched(
            filler_id,
            Arc::from("capacity-cancel-filler"),
            owned_task_spec(waiting_spec("capacity-cancel-filler")),
            None,
        )
        .expect("the filler must occupy the registry queue");

    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), Bus::new(64));
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;
    let (id, outcome) = ctrl
        .handle()
        .submit_and_watch(
            ControllerSpec::queue(waiting_spec("capacity-cancel-target"))
                .with_slot("capacity-cancel-slot"),
        )
        .await
        .expect("the controller command queue must accept the target");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.state().capacity_pending.contains_key(&id)
        })
        .await,
        "the target must be waiting for registry capacity"
    );

    assert!(
        ctrl.handle()
            .remove(id)
            .await
            .expect("capacity-waiting removal must complete")
    );
    assert!(matches!(
        receive_oneshot(outcome, "capacity-waiting removal outcome").await,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::RemovedFromQueue,
            ..
        }
    ));
    assert!(!ctrl.state().capacity_pending.contains_key(&id));
    assert!(!ctrl.state().watchers.contains_key(&id));
    assert!(ctrl.slot("capacity-cancel-slot").is_none());

    stop_controller_loop(token, runner).await;
    let runtime_handle = supervisor.serve().expect("runtime startup");
    let _ = runtime_handle.shutdown().await;
}
