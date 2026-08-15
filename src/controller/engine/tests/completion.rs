//! Tests for registry completion and physical owner release.

use super::support::*;
use crate::controller::engine::state::SlotPhase;
use crate::controller::engine::{CompletionResult, Controller, RemovalResult};

#[tokio::test]
async fn stale_completion_does_not_free_current_owner() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let current_id = TaskId::next();
    let stale_id = TaskId::next();
    let slot_arc = ctrl.get_or_create_slot("s");
    {
        let mut slot = slot_arc.lock().await;
        *slot = running_slot(current_id);
    }

    let mut operations = tracked_operations(&ctrl);
    ctrl.handle_completion_result(
        CompletionResult {
            id: stale_id,
            slot_name: Arc::from("s"),
        },
        &mut operations,
    )
    .await;

    let slot = slot_arc.lock().await;
    assert_eq!(slot.owner_id(), Some(current_id));
    assert!(matches!(slot.phase(), SlotPhase::Running { .. }));
}

#[tokio::test]
async fn removal_not_claimed_keeps_terminating_until_reliable_completion() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let bus = Bus::new(64);
    let mut events = bus.subscribe();
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), bus);
    let owner = TaskId::next();
    let queued = TaskId::next();
    let slot_arc = ctrl.get_or_create_slot("s");
    {
        let mut slot = slot_arc.lock().await;
        *slot = terminating_slot(owner);
        slot.queue
            .push_back(pending(queued, waiting_spec("after-unclaimed-removal")));
    }

    ctrl.handle_removal_result(RemovalResult {
        id: owner,
        slot_name: Arc::from("s"),
        decision: Ok(false),
    })
    .await;

    {
        let slot = slot_arc.lock().await;
        assert_eq!(slot.owner_id(), Some(owner));
        assert!(matches!(slot.phase(), SlotPhase::Terminating { .. }));
        assert_eq!(slot.queue.front().map(|pending| pending.id), Some(queued));
    }
    assert!(
        events.try_recv().is_err(),
        "Ok(false) is not a removal failure diagnostic"
    );

    let mut operations = tracked_operations(&ctrl);
    ctrl.handle_completion_result(
        CompletionResult {
            id: owner,
            slot_name: Arc::from("s"),
        },
        &mut operations,
    )
    .await;

    {
        let slot = slot_arc.lock().await;
        assert_eq!(slot.owner_id(), Some(queued));
        assert!(matches!(
            slot.phase(),
            SlotPhase::Admitting { owner, .. } if owner == queued
        ));
        assert!(slot.queue.is_empty());
    }
    assert_eq!(operations.admissions.len(), 1);
    abort_and_drain(&mut operations.admissions).await;
}

#[tokio::test]
async fn removal_error_preserves_owner_and_queue_and_emits_one_diagnostic() {
    let bus = Bus::new(64);
    let mut events = bus.subscribe();
    let ctrl = make_controller(ControllerConfig::default(), bus);
    let owner = TaskId::next();
    let queued = TaskId::next();
    let slot_arc = ctrl.get_or_create_slot("s");
    {
        let mut slot = slot_arc.lock().await;
        *slot = terminating_slot(owner);
        slot.queue
            .push_back(pending(queued, waiting_spec("after-failed-removal")));
    }

    ctrl.handle_removal_result(RemovalResult {
        id: owner,
        slot_name: Arc::from("s"),
        decision: Err(RuntimeError::CommandQueueFull),
    })
    .await;

    let event = events
        .try_recv()
        .expect("the current owner's removal error must be observable");
    assert_eq!(event.kind, EventKind::RuntimeFailure);
    assert_eq!(event.id, Some(owner));
    assert_eq!(event.task.as_deref(), Some("controller"));
    assert!(event.reason.as_deref().is_some_and(|reason| {
        reason.starts_with("remove_failed slot=s:") && reason.contains("queue is full")
    }));
    assert!(
        events.try_recv().is_err(),
        "one failed result must publish exactly one diagnostic"
    );

    let slot = slot_arc.lock().await;
    assert_eq!(slot.owner_id(), Some(owner));
    assert!(matches!(slot.phase(), SlotPhase::Terminating { .. }));
    assert_eq!(slot.queue.front().map(|pending| pending.id), Some(queued));
}

#[tokio::test]
async fn stale_removal_error_does_not_publish_or_mutate_new_owner() {
    let bus = Bus::new(64);
    let mut events = bus.subscribe();
    let ctrl = make_controller(ControllerConfig::default(), bus);
    let stale = TaskId::next();
    let current = TaskId::next();
    let queued = TaskId::next();
    let slot_arc = ctrl.get_or_create_slot("s");
    {
        let mut slot = slot_arc.lock().await;
        *slot = running_slot(current);
        slot.queue
            .push_back(pending(queued, waiting_spec("new-owner-queued")));
    }

    ctrl.handle_removal_result(RemovalResult {
        id: stale,
        slot_name: Arc::from("s"),
        decision: Err(RuntimeError::CommandQueueFull),
    })
    .await;

    assert!(events.try_recv().is_err());
    let slot = slot_arc.lock().await;
    assert_eq!(slot.owner_id(), Some(current));
    assert!(matches!(slot.phase(), SlotPhase::Running { .. }));
    assert_eq!(slot.queue.front().map(|pending| pending.id), Some(queued));
}

#[tokio::test]
async fn duplicate_completion_does_not_start_queued_owner_twice() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    let completed_id = TaskId::next();
    let next_id = TaskId::next();
    let slot_arc = ctrl.get_or_create_slot("s");
    {
        let mut slot = slot_arc.lock().await;
        *slot = running_slot(completed_id);
        slot.queue
            .push_back(pending(next_id, waiting_spec("duplicate-completion-next")));
    }

    let mut operations = tracked_operations(&ctrl);
    for _ in 0..2 {
        ctrl.handle_completion_result(
            CompletionResult {
                id: completed_id,
                slot_name: Arc::from("s"),
            },
            &mut operations,
        )
        .await;
    }

    let slot = slot_arc.lock().await;
    assert_eq!(slot.owner_id(), Some(next_id));
    assert!(matches!(
        slot.phase(),
        SlotPhase::Admitting { owner, .. } if owner == next_id
    ));
    assert!(slot.queue.is_empty());
    assert_eq!(
        operations.admissions.len(),
        1,
        "a duplicate completion must not commit the queued Add twice"
    );
    drop(slot);
    abort_and_drain(&mut operations.admissions).await;
}

#[tokio::test(flavor = "current_thread")]
async fn reliable_completion_reuses_task_name_without_task_removed() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let handle = sup.serve().expect("runtime startup");
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(1));
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;

    let log = Arc::new(StdMutex::new(Vec::new()));
    let (release, released) = oneshot::channel();
    let released = Arc::new(StdMutex::new(Some(released)));
    let first_log = Arc::clone(&log);
    let first_release = Arc::clone(&released);
    let first: TaskRef = TaskFn::arc(move |_ctx: TaskContext| {
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
    let second: TaskRef = TaskFn::arc(move |_ctx: TaskContext| {
        let log = Arc::clone(&second_log);
        async move {
            log.lock().expect("log lock poisoned").push("second");
            Ok(())
        }
    });

    let (first_id, first_outcome) = ctrl
        .handle()
        .submit_and_watch(
            ControllerSpec::queue(TaskSpec::once("same-runtime-name", first)).with_slot("s"),
        )
        .await
        .expect("the first submission must enter controller intake");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl.slot("s") else {
                return false;
            };
            let slot = slot.lock().await;
            slot.owner_id() == Some(first_id) && matches!(slot.phase(), SlotPhase::Running { .. })
        })
        .await,
        "the first task must own the slot before queueing the second"
    );

    let (second_id, second_outcome) = ctrl
        .handle()
        .submit_and_watch(
            ControllerSpec::queue(TaskSpec::once("same-runtime-name", second)).with_slot("s"),
        )
        .await
        .expect("the second submission must enter controller intake");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl.slot("s") else {
                return false;
            };
            slot.lock().await.queue.front().map(|pending| pending.id) == Some(second_id)
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
            ctrl.slot("s").is_none()
        })
        .await,
        "the empty slot must be collected after the second completion"
    );

    stop_controller_loop(token, runner).await;
    let _ = handle.shutdown().await;
}
