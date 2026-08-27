//! Tests for replacement ordering during admission and owner retirement.

use super::support::*;
use crate::controller::engine::state::{SlotPhase, SlotState};
use crate::controller::engine::{AdmissionResult, Controller, Submission};

#[test]
fn replace_head_or_push_replaces_existing_head_and_rejects_displaced() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let mut rx = ctrl.bus.subscribe();
    let mut slot = SlotState::new();
    let displaced = TaskId::next();
    slot.queue
        .push_back(pending(displaced, make_spec("old-head")));
    slot.queue
        .push_back(pending(TaskId::next(), make_spec("tail")));

    let replacement = TaskId::next();
    let displaced_spec = ctrl
        .replace_head_or_push(
            &mut slot,
            &slot_arc_name(),
            pending(replacement, make_spec("new-head")),
        )
        .expect("the old queue head must be returned for deferred drop");

    assert_eq!(slot.queue.len(), 2, "queue depth should not grow");
    assert_eq!(slot.queue.front().unwrap().task_spec().name(), "new-head");
    assert_eq!(slot.queue.back().unwrap().task_spec().name(), "tail");
    assert_eq!(displaced_spec.task_spec().name(), "old-head");
    assert!(!ctrl.state().queued_slots.contains_key(&displaced));
    assert_eq!(
        ctrl.state().queued_slots.get(&replacement).cloned(),
        Some(slot_arc_name())
    );

    let ev = rx.try_recv().expect("displaced head must be rejected");
    assert_eq!(ev.kind, EventKind::ControllerRejected);
    assert_eq!(
        ev.rejection_kind,
        Some(crate::RejectionKind::SupersededByReplace)
    );
    assert_eq!(ev.id, Some(displaced));
    assert_eq!(
        ev.reason.as_deref(),
        Some(crate::reasons::SUPERSEDED_BY_REPLACE)
    );
}

#[test]
fn replace_head_or_push_appends_to_empty_then_keeps_only_the_latest_head() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let mut slot = SlotState::new();
    let name = slot_arc_name();
    assert!(
        ctrl.replace_head_or_push(&mut slot, &name, pending(TaskId::next(), make_spec("v1")),)
            .is_none()
    );
    assert_eq!(slot.queue.len(), 1);
    assert_eq!(slot.queue.front().unwrap().task_spec().name(), "v1");

    assert_eq!(
        ctrl.replace_head_or_push(&mut slot, &name, pending(TaskId::next(), make_spec("v2")),)
            .expect("v1 must be displaced")
            .task_spec()
            .name(),
        "v1"
    );
    assert_eq!(
        ctrl.replace_head_or_push(&mut slot, &name, pending(TaskId::next(), make_spec("v3")),)
            .expect("v2 must be displaced")
            .task_spec()
            .name(),
        "v2"
    );

    assert_eq!(slot.queue.len(), 1);
    assert_eq!(slot.queue.front().unwrap().task_spec().name(), "v3");
}

#[tokio::test]
async fn replace_pending_admission_then_add_err_starts_replacement_without_removal() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    let owner = TaskId::next();
    let replacement = TaskId::next();
    let slot_arc = ctrl.get_or_create_slot("s");
    {
        let mut slot = slot_arc.lock().await;
        *slot = admitting_slot(owner);
    }

    let mut operations = tracked_operations(&ctrl);
    handle_submission_fully(
        &ctrl,
        Submission {
            id: replacement,
            owned: owned_controller_spec(
                ControllerSpec::replace(waiting_spec("replacement-after-add-err")).with_slot("s"),
            ),
            done: None,
        },
        &mut operations,
    )
    .await;
    assert!(operations.removals.is_empty());

    ctrl.handle_admission_result(
        AdmissionResult {
            id: owner,
            slot_name: Arc::from("s"),
            decision: Err(RuntimeError::TaskAlreadyExists {
                name: Arc::from("rejected-owner"),
            }),
        },
        &mut operations,
    )
    .await;

    let slot = slot_arc.lock().await;
    assert_eq!(slot.owner_id(), Some(replacement));
    assert!(matches!(
        slot.phase(),
        SlotPhase::Admitting { owner, .. } if owner == replacement
    ));
    assert!(slot.queue.is_empty());
    assert_eq!(operations.admissions.len(), 1);
    assert!(
        operations.removals.is_empty(),
        "a rejected Add means there was no owner to remove"
    );
    drop(slot);
    abort_and_drain(&mut operations.admissions).await;
    abort_and_drain(&mut operations.completions).await;
    abort_and_drain(&mut operations.removals).await;
}

#[tokio::test]
async fn repeated_replace_while_admitting_is_latest_wins_with_one_removal_after_ok() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    let owner = TaskId::next();
    let first = TaskId::next();
    let latest = TaskId::next();
    let slot_arc = ctrl.get_or_create_slot("s");
    {
        let mut slot = slot_arc.lock().await;
        *slot = admitting_slot(owner);
    }

    let mut operations = tracked_operations(&ctrl);
    let (first_done, first_outcome) = oneshot::channel();
    handle_submission_fully(
        &ctrl,
        Submission {
            id: first,
            owned: owned_controller_spec(
                ControllerSpec::replace(waiting_spec("pending-replace-first")).with_slot("s"),
            ),
            done: Some(first_done),
        },
        &mut operations,
    )
    .await;
    handle_submission_fully(
        &ctrl,
        Submission {
            id: latest,
            owned: owned_controller_spec(
                ControllerSpec::replace(waiting_spec("pending-replace-latest")).with_slot("s"),
            ),
            done: None,
        },
        &mut operations,
    )
    .await;

    assert!(matches!(
        first_outcome.await,
        Ok(TaskOutcome::Rejected {
            kind: crate::RejectionKind::SupersededByReplace,
            reason,
            ..
        })
            if reason.as_ref() == crate::reasons::SUPERSEDED_BY_REPLACE
    ));

    {
        let slot = slot_arc.lock().await;
        assert!(matches!(
            slot.phase(),
            SlotPhase::CancelPendingAdmission { owner: id, .. } if id == owner
        ));
        assert_eq!(slot.queue.len(), 1);
        assert_eq!(slot.queue.front().map(|pending| pending.id), Some(latest));
    }
    assert!(operations.removals.is_empty());

    for _ in 0..2 {
        ctrl.handle_admission_result(
            AdmissionResult {
                id: owner,
                slot_name: Arc::from("s"),
                decision: Ok(crate::core::RemovalCompletion::new()),
            },
            &mut operations,
        )
        .await;
    }

    let slot = slot_arc.lock().await;
    assert!(matches!(
        slot.phase(),
        SlotPhase::Terminating { owner: id, .. } if id == owner
    ));
    assert_eq!(slot.queue.front().map(|pending| pending.id), Some(latest));
    assert_eq!(
        operations.completions.len(),
        1,
        "duplicate Add Ok must be stale"
    );
    assert_eq!(
        operations.removals.len(),
        1,
        "only the first authoritative Add Ok may order removal"
    );
    drop(slot);
    abort_and_drain(&mut operations.admissions).await;
    abort_and_drain(&mut operations.completions).await;
    abort_and_drain(&mut operations.removals).await;
}

#[tokio::test(flavor = "current_thread")]
async fn replace_remains_ordered_while_owner_waits_for_registry_capacity() {
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
            Arc::from("capacity-replace-filler"),
            owned_task_spec(waiting_spec("capacity-replace-filler")),
            None,
        )
        .expect("the filler must occupy the registry queue");

    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), Bus::new(64));
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;
    let (owner_id, owner_outcome) = ctrl
        .handle()
        .submit_and_watch(
            ControllerSpec::queue(waiting_spec("capacity-replace-owner"))
                .with_slot("capacity-replace-slot"),
        )
        .await
        .expect("the owner must enter controller intake");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.state().capacity_pending.contains_key(&owner_id)
        })
        .await
    );

    let replacement: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    let (replacement_id, replacement_outcome) = ctrl
        .handle()
        .submit_and_watch(
            ControllerSpec::replace(TaskSpec::once("capacity-replacement", replacement))
                .with_slot("capacity-replace-slot"),
        )
        .await
        .expect("the replacement must enter controller intake");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl.slot("capacity-replace-slot") else {
                return false;
            };
            let slot = slot.lock().await;
            matches!(slot.phase(), SlotPhase::CancelPendingAdmission { owner, .. } if owner == owner_id)
                && slot.queue.front().map(|pending| pending.id) == Some(replacement_id)
        })
        .await,
        "Replace must remain queued behind the capacity-waiting owner"
    );

    let runtime_handle = supervisor.serve().expect("runtime startup");
    assert!(matches!(
        receive_oneshot(owner_outcome, "capacity-waiting replaced owner").await,
        TaskOutcome::Canceled
    ));
    assert!(matches!(
        receive_oneshot(replacement_outcome, "capacity-waiting replacement").await,
        TaskOutcome::Completed
    ));
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.slot("capacity-replace-slot").is_none() && ctrl.state().capacity_pending.is_empty()
        })
        .await
    );

    stop_controller_loop(token, runner).await;
    let _ = runtime_handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn replace_is_processed_while_registry_reply_is_pending() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let controller_bus = Bus::new(64);
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), controller_bus);
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;

    let (first_id, first_outcome) = ctrl
        .handle()
        .submit_and_watch(ControllerSpec::queue(waiting_spec("pending-owner")).with_slot("s"))
        .await
        .expect("controller intake must accept the first submission");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl.slot("s") else {
                return false;
            };
            let slot = slot.lock().await;
            slot.owner_id() == Some(first_id) && matches!(slot.phase(), SlotPhase::Admitting { .. })
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
            let Some(slot) = ctrl.slot("s") else {
                return false;
            };
            let slot = slot.lock().await;
            matches!(slot.phase(), SlotPhase::CancelPendingAdmission { .. })
                && slot.queue.front().map(|pending| pending.id) == Some(replacement_id)
        })
        .await,
        "Replace must be processed without waiting for the first registry reply"
    );

    let handle = sup.serve().expect("runtime startup");
    let outcome = tokio::time::timeout(Duration::from_secs(2), first_outcome)
        .await
        .expect("the accepted owner must be removed")
        .expect("the registry must resolve the owner outcome");
    assert!(matches!(outcome, TaskOutcome::Canceled));

    assert!(
        poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl.slot("s") else {
                return false;
            };
            let slot = slot.lock().await;
            slot.owner_id() == Some(replacement_id)
                && matches!(slot.phase(), SlotPhase::Running { .. })
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
    let runtime_handle = sup.serve().expect("runtime startup");
    let owner_id = runtime_handle
        .add(waiting_spec("replace-owner"))
        .execute()
        .await
        .expect("the owner must register");

    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    let slot_name: Arc<str> = Arc::from("s");
    let slot = running_slot(owner_id);
    ctrl.state()
        .slots
        .insert(Arc::clone(&slot_name), Arc::new(Mutex::new(slot)));

    let filler_id = TaskId::next();
    let (filler_reply, _filler_completion) = sup
        .core()
        .add_task_with_id_watched(
            filler_id,
            Arc::from("replace-filler"),
            owned_task_spec(waiting_spec("replace-filler")),
            None,
        )
        .expect("the filler must occupy the registry queue");
    assert_eq!(sup.core().registry_command_capacity(), 0);

    let first_id = TaskId::next();
    let (first_done, first_outcome) = oneshot::channel();
    let first = Submission {
        id: first_id,
        owned: owned_controller_spec(
            ControllerSpec::replace(waiting_spec("replace-first")).with_slot("s"),
        ),
        done: Some(first_done),
    };
    let mut operations = tracked_operations(&ctrl);
    let mut first = Box::pin(ctrl.handle_submission(first, &mut operations));
    tokio::time::timeout(Duration::from_secs(1), first.as_mut())
        .await
        .expect("Replace must not wait for registry capacity");
    drop(first);
    assert_eq!(
        operations.removals.len(),
        1,
        "one owner removal must be tracked"
    );

    let second_id = TaskId::next();
    let second = Submission {
        id: second_id,
        owned: owned_controller_spec(
            ControllerSpec::replace(waiting_spec("replace-second")).with_slot("s"),
        ),
        done: None,
    };
    let mut second = Box::pin(ctrl.handle_submission(second, &mut operations));
    tokio::time::timeout(Duration::from_secs(1), second.as_mut())
        .await
        .expect("a newer Replace must stay responsive while removal is backpressured");
    drop(second);

    let slot = ctrl.slot("s").expect("the slot must remain tracked");
    let slot = slot.lock().await;
    assert!(matches!(slot.phase(), SlotPhase::Terminating { .. }));
    assert_eq!(
        slot.queue.front().map(|pending| pending.id),
        Some(second_id)
    );
    drop(slot);
    assert_eq!(
        operations.removals.len(),
        1,
        "repeated Replace must not enqueue duplicate owner removals"
    );
    assert!(matches!(
        first_outcome.await,
        Ok(TaskOutcome::Rejected {
            kind: crate::RejectionKind::SupersededByReplace,
            reason,
            ..
        })
            if reason.as_ref() == crate::reasons::SUPERSEDED_BY_REPLACE
    ));

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), filler_reply).await,
        Ok(Ok(Ok(())))
    ));
    let removal = tokio::time::timeout(Duration::from_secs(2), operations.removals.next())
        .await
        .expect("the owner removal must resume after registry capacity recovers")
        .expect("one removal waiter must exist")
        .expect("the removal waiter must not panic");
    assert_eq!(removal.id, owner_id);
    assert!(matches!(removal.decision, Ok(true)));

    let _ = runtime_handle.shutdown().await;
}
