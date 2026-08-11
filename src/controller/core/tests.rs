//! Tests for the controller engine and its cross-module invariants.

use super::*;
use crate::Supervisor;
use crate::TaskContext;
use crate::{BackoffPolicy, BoxTaskFuture, RestartPolicy, Task, TaskFn, TaskRef, TaskSpec};
use std::num::NonZeroUsize;
use std::sync::{
    Mutex as StdMutex, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
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

struct NameBombTask {
    calls: Arc<AtomicUsize>,
}

impl Task for NameBombTask {
    fn name(&self) -> &str {
        self.calls.fetch_add(1, Ordering::AcqRel);
        panic!("injected unexpected task name read")
    }

    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        panic!("a policy-rejected task must not spawn")
    }
}

struct PanickingDropTask {
    name: &'static str,
    drops: Arc<AtomicUsize>,
}

impl Task for PanickingDropTask {
    fn name(&self) -> &str {
        self.name
    }

    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(std::future::pending())
    }
}

impl Drop for PanickingDropTask {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::AcqRel);
        panic!("injected task drop panic")
    }
}

struct ShutdownDropProbeTask {
    controller: Weak<Controller>,
    state_clean_at_drop: Arc<AtomicBool>,
    drops: Arc<AtomicUsize>,
}

impl Task for ShutdownDropProbeTask {
    fn name(&self) -> &str {
        "slot-shutdown-drop-panic"
    }

    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(std::future::pending())
    }
}

impl Drop for ShutdownDropProbeTask {
    fn drop(&mut self) {
        let state_is_clean = self.controller.upgrade().is_some_and(|controller| {
            controller.watchers.is_empty()
                && controller.slots.is_empty()
                && controller.queued_slots.is_empty()
                && controller.capacity_pending.is_empty()
        });
        self.state_clean_at_drop
            .store(state_is_clean, Ordering::Release);
        self.drops.fetch_add(1, Ordering::AcqRel);
        panic!("injected task drop panic")
    }
}

struct SingleReadNameTask {
    calls: Arc<AtomicUsize>,
}

impl Task for SingleReadNameTask {
    fn name(&self) -> &str {
        assert_eq!(
            self.calls.fetch_add(1, Ordering::AcqRel),
            0,
            "controller admission must snapshot Task::name exactly once"
        );
        "single-read-name"
    }

    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(std::future::pending())
    }
}

fn make_spec(name: &str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, |_ctx: TaskContext| async { Ok(()) });
    TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
}

fn pending(id: TaskId, task_spec: TaskSpec) -> crate::controller::slot::PendingSubmission {
    let task_name = Arc::from(task_spec.name());
    crate::controller::slot::PendingSubmission::new(id, task_name, task_spec)
}

fn slot_arc_name() -> Arc<str> {
    Arc::from("s")
}

fn drain_events(events: &mut tokio::sync::broadcast::Receiver<Arc<Event>>) -> Vec<Arc<Event>> {
    let mut drained = Vec::new();
    while let Ok(event) = events.try_recv() {
        drained.push(event);
    }
    drained
}

fn assert_rejection_parity(event: &Event, id: TaskId, outcome: &TaskOutcome) {
    let TaskOutcome::Rejected { kind, reason, .. } = outcome else {
        panic!("expected a rejected task outcome, got {outcome:?}");
    };
    assert_eq!(event.kind, EventKind::ControllerRejected);
    assert_eq!(event.id, Some(id));
    assert_eq!(event.rejection_kind, Some(*kind));
    assert_eq!(event.outcome_kind, Some(crate::TaskOutcomeKind::Rejected));
    assert_eq!(event.reason.as_deref(), Some(reason.as_ref()));
}

async fn receive_oneshot<T>(receiver: oneshot::Receiver<T>, context: &str) -> T {
    tokio::time::timeout(Duration::from_secs(2), receiver)
        .await
        .unwrap_or_else(|_| panic!("{context} timed out"))
        .unwrap_or_else(|_| panic!("{context} sender was dropped"))
}

fn admitting_slot(owner: TaskId) -> SlotState {
    let mut slot = SlotState::new();
    assert!(slot.begin_admission(owner, Instant::now()));
    slot
}

fn running_slot(owner: TaskId) -> SlotState {
    let mut slot = admitting_slot(owner);
    assert_eq!(
        slot.confirm_admission(owner, Instant::now()),
        AdmissionTransition::Running
    );
    slot
}

fn terminating_slot(owner: TaskId) -> SlotState {
    let mut slot = running_slot(owner);
    assert_eq!(
        slot.request_replacement(Instant::now()),
        crate::controller::slot::ReplaceAction::RemoveNow(owner)
    );
    slot
}

async fn abort_and_drain<T: 'static>(workers: &mut JoinSet<T>) {
    workers.abort_all();
    while workers.join_next().await.is_some() {}
}

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
    assert_eq!(slot.queue.front().unwrap().task_spec.name(), "new-head");
    assert_eq!(slot.queue.back().unwrap().task_spec.name(), "tail");
    assert_eq!(displaced_spec.task_spec.name(), "old-head");
    assert!(!ctrl.queued_slots.contains_key(&displaced));
    assert_eq!(
        ctrl.queued_slots
            .get(&replacement)
            .map(|entry| Arc::clone(entry.value())),
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
    assert_eq!(slot.queue.front().unwrap().task_spec.name(), "v1");

    assert_eq!(
        ctrl.replace_head_or_push(&mut slot, &name, pending(TaskId::next(), make_spec("v2")),)
            .expect("v1 must be displaced")
            .task_spec
            .name(),
        "v1"
    );
    assert_eq!(
        ctrl.replace_head_or_push(&mut slot, &name, pending(TaskId::next(), make_spec("v3")),)
            .expect("v2 must be displaced")
            .task_spec
            .name(),
        "v2"
    );

    assert_eq!(slot.queue.len(), 1);
    assert_eq!(slot.queue.front().unwrap().task_spec.name(), "v3");
}

#[test]
fn queue_full_reason_respects_the_capacity_boundary() {
    let config = ControllerConfig::new(NonZeroUsize::new(16).unwrap(), 3);
    let ctrl = make_controller(config, Bus::new(64));

    for (depth, expected_rejection) in [(0, false), (2, false), (3, true), (10, true)] {
        assert_eq!(
            ctrl.queue_full_reason(depth).is_some(),
            expected_rejection,
            "unexpected decision at queue depth {depth}"
        );
    }
}

#[test]
fn queued_reverse_index_tracks_push_pop_and_position_removal() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let slot_name = slot_arc_name();
    let mut slot = SlotState::new();
    let first = TaskId::next();
    let second = TaskId::next();

    ctrl.push_queued(&mut slot, &slot_name, pending(first, make_spec("first")));
    ctrl.push_queued(&mut slot, &slot_name, pending(second, make_spec("second")));
    assert_eq!(
        ctrl.queued_slots
            .get(&first)
            .map(|entry| Arc::clone(entry.value())),
        Some(Arc::clone(&slot_name))
    );
    assert_eq!(
        ctrl.queued_slots
            .get(&second)
            .map(|entry| Arc::clone(entry.value())),
        Some(Arc::clone(&slot_name))
    );

    assert_eq!(
        ctrl.pop_queued_front(&mut slot).map(|pending| pending.id),
        Some(first)
    );
    assert!(!ctrl.queued_slots.contains_key(&first));
    assert_eq!(
        ctrl.remove_queued_at(&mut slot, 0)
            .map(|pending| pending.id),
        Some(second)
    );
    assert!(!ctrl.queued_slots.contains_key(&second));
    assert!(slot.queue.is_empty());
}

#[test]
fn get_or_create_slot_preserves_name_identity_and_initial_state() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));

    let slot_arc = ctrl.get_or_create_slot("my-slot");
    {
        let slot = slot_arc.blocking_lock();
        assert_eq!(slot.phase(), SlotPhase::Idle);
        assert!(slot.queue.is_empty());
    }

    assert!(
        Arc::ptr_eq(&slot_arc, &ctrl.get_or_create_slot("my-slot")),
        "the same slot name must return the same allocation"
    );
    assert!(
        !Arc::ptr_eq(&slot_arc, &ctrl.get_or_create_slot("other-slot")),
        "different slot names must not share state"
    );
}

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

    let mut admissions = JoinSet::new();
    ctrl.handle_completion_result(
        CompletionResult {
            id: owner,
            slot_name: Arc::from("s"),
        },
        &mut admissions,
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
    assert_eq!(admissions.len(), 1);
    abort_and_drain(&mut admissions).await;
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
async fn stale_admission_ok_and_err_do_not_mutate_new_owner() {
    let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
    let stale_id = TaskId::next();
    let current_id = TaskId::next();
    let slot_arc = ctrl.get_or_create_slot("s");
    {
        let mut slot = slot_arc.lock().await;
        *slot = admitting_slot(current_id);
    }

    let mut admissions = JoinSet::new();
    let mut completions = JoinSet::new();
    let mut removals = JoinSet::new();
    ctrl.handle_admission_result(
        AdmissionResult {
            id: stale_id,
            slot_name: Arc::from("s"),
            decision: AdmissionDecision::Registry(Ok(crate::core::RemovalCompletion::new())),
        },
        &mut admissions,
        &mut completions,
        &mut removals,
    )
    .await;
    ctrl.handle_admission_result(
        AdmissionResult {
            id: stale_id,
            slot_name: Arc::from("s"),
            decision: AdmissionDecision::Registry(Err(RuntimeError::ShuttingDown)),
        },
        &mut admissions,
        &mut completions,
        &mut removals,
    )
    .await;

    let slot = slot_arc.lock().await;
    assert_eq!(slot.owner_id(), Some(current_id));
    assert!(matches!(
        slot.phase(),
        SlotPhase::Admitting { owner, .. } if owner == current_id
    ));
    assert!(completions.is_empty());
    assert!(removals.is_empty());
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

    let mut admissions = JoinSet::new();
    for _ in 0..2 {
        ctrl.handle_completion_result(
            CompletionResult {
                id: completed_id,
                slot_name: Arc::from("s"),
            },
            &mut admissions,
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
        admissions.len(),
        1,
        "a duplicate completion must not commit the queued Add twice"
    );
    drop(slot);
    abort_and_drain(&mut admissions).await;
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

    let mut admissions = JoinSet::new();
    let mut completions = JoinSet::new();
    let mut removals = JoinSet::new();
    ctrl.handle_submission(
        Submission {
            id: replacement,
            spec: ControllerSpec::replace(waiting_spec("replacement-after-add-err")).with_slot("s"),
            done: None,
        },
        &mut admissions,
        &mut removals,
    )
    .await;
    assert!(removals.is_empty());

    ctrl.handle_admission_result(
        AdmissionResult {
            id: owner,
            slot_name: Arc::from("s"),
            decision: AdmissionDecision::Registry(Err(RuntimeError::TaskAlreadyExists {
                name: Arc::from("rejected-owner"),
            })),
        },
        &mut admissions,
        &mut completions,
        &mut removals,
    )
    .await;

    let slot = slot_arc.lock().await;
    assert_eq!(slot.owner_id(), Some(replacement));
    assert!(matches!(
        slot.phase(),
        SlotPhase::Admitting { owner, .. } if owner == replacement
    ));
    assert!(slot.queue.is_empty());
    assert_eq!(admissions.len(), 1);
    assert!(
        removals.is_empty(),
        "a rejected Add means there was no owner to remove"
    );
    drop(slot);
    abort_and_drain(&mut admissions).await;
    abort_and_drain(&mut completions).await;
    abort_and_drain(&mut removals).await;
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

    let mut admissions = JoinSet::new();
    let mut completions = JoinSet::new();
    let mut removals = JoinSet::new();
    let (first_done, first_outcome) = oneshot::channel();
    ctrl.handle_submission(
        Submission {
            id: first,
            spec: ControllerSpec::replace(waiting_spec("pending-replace-first")).with_slot("s"),
            done: Some(first_done),
        },
        &mut admissions,
        &mut removals,
    )
    .await;
    ctrl.handle_submission(
        Submission {
            id: latest,
            spec: ControllerSpec::replace(waiting_spec("pending-replace-latest")).with_slot("s"),
            done: None,
        },
        &mut admissions,
        &mut removals,
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
    assert!(removals.is_empty());

    for _ in 0..2 {
        ctrl.handle_admission_result(
            AdmissionResult {
                id: owner,
                slot_name: Arc::from("s"),
                decision: AdmissionDecision::Registry(Ok(crate::core::RemovalCompletion::new())),
            },
            &mut admissions,
            &mut completions,
            &mut removals,
        )
        .await;
    }

    let slot = slot_arc.lock().await;
    assert!(matches!(
        slot.phase(),
        SlotPhase::Terminating { owner: id, .. } if id == owner
    ));
    assert_eq!(slot.queue.front().map(|pending| pending.id), Some(latest));
    assert_eq!(completions.len(), 1, "duplicate Add Ok must be stale");
    assert_eq!(
        removals.len(),
        1,
        "only the first authoritative Add Ok may order removal"
    );
    drop(slot);
    abort_and_drain(&mut admissions).await;
    abort_and_drain(&mut completions).await;
    abort_and_drain(&mut removals).await;
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
    ctrl.finalize_pending_on_shutdown(&mut rx).await;
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
async fn try_submit_and_watch_is_fail_fast_and_preserves_watched_outcome() {
    let config = ControllerConfig::default().with_queue_capacity(NonZeroUsize::new(1).unwrap());
    let ctrl = make_controller(config, Bus::new(64));

    let task: TaskRef = TaskFn::arc("try-watched", |_ctx: TaskContext| async { Ok(()) });
    let (_id, waiter) = ctrl
        .handle()
        .try_submit_and_watch(ControllerSpec::queue(TaskSpec::once(task)).with_slot("s"))
        .expect("the watched submission must occupy the only command slot");
    assert!(matches!(
        ctrl.handle().try_submit_and_watch(
            ControllerSpec::queue(waiting_spec("try-watched-overflow")).with_slot("s")
        ),
        Err(ControllerError::Full)
    ));

    let mut rx = ctrl.rx.write().await.take().expect("rx present");
    ctrl.finalize_pending_on_shutdown(&mut rx).await;
    drop(rx);

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), waiter).await,
        Ok(Ok(TaskOutcome::Rejected { .. }))
    ));
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
    assert!(ctrl.watchers.is_empty());
    assert!(ctrl.slots.is_empty());
    assert!(ctrl.queued_slots.is_empty());
    assert!(ctrl.capacity_pending.is_empty());
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
            waiting_spec("capacity-shutdown-filler"),
            None,
        )
        .expect("the filler must occupy the registry queue");
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), Bus::new(64));
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let mut admissions = JoinSet::new();
    let mut removals = JoinSet::new();

    ctrl.handle_submission(
        Submission {
            id,
            spec: ControllerSpec::queue(waiting_spec("capacity-shutdown-target"))
                .with_slot("capacity-shutdown-slot"),
            done: Some(done),
        },
        &mut admissions,
        &mut removals,
    )
    .await;
    assert!(ctrl.capacity_pending.contains_key(&id));
    assert_eq!(admissions.len(), 1);

    ctrl.finalize_slot_state_on_shutdown().await;
    assert!(matches!(
        receive_oneshot(outcome, "capacity-waiting shutdown outcome").await,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::ControllerShuttingDown,
            ..
        }
    ));
    assert!(ctrl.capacity_pending.is_empty());
    assert!(ctrl.watchers.is_empty());
    assert!(ctrl.slots.is_empty());
    abort_and_drain(&mut admissions).await;
    abort_and_drain(&mut removals).await;
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
    ctrl.watchers.insert(first, first_done);
    ctrl.watchers.insert(second, second_done);

    let slot = ctrl.get_or_create_slot("slot-shutdown-drop-panic");
    {
        let mut slot = slot.lock().await;
        slot.queue
            .push_back(crate::controller::slot::PendingSubmission::new(
                first,
                Arc::from("slot-shutdown-drop-panic"),
                TaskSpec::once(Arc::new(ShutdownDropProbeTask {
                    controller: Arc::downgrade(&ctrl),
                    state_clean_at_drop: Arc::clone(&state_clean_at_drop),
                    drops: Arc::clone(&drops),
                })),
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
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(
        state_clean_at_drop.load(Ordering::Acquire),
        "all controller watchers and slots must be finalized before user Drop"
    );
    assert!(ctrl.watchers.is_empty());
    assert!(ctrl.slots.is_empty());

    let drained = drain_events(&mut events);
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
    ctrl.finalize_pending_on_shutdown(&mut rx).await;

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
async fn queued_identity_reply_survives_panicking_task_drop() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let bus = Bus::new(64);
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), bus.clone());
    let mut events = bus.subscribe();
    let drops = Arc::new(AtomicUsize::new(0));
    let id = TaskId::next();
    let slot_name: Arc<str> = Arc::from("identity-drop-panic-slot");
    let (done, outcome) = oneshot::channel();
    ctrl.watchers.insert(id, done);
    let slot = ctrl.get_or_create_slot(&slot_name);
    let mut slot_state = slot.lock().await;
    ctrl.push_queued(
        &mut slot_state,
        &slot_name,
        crate::controller::slot::PendingSubmission::new(
            id,
            Arc::from("identity-drop-panic-task"),
            TaskSpec::once(Arc::new(PanickingDropTask {
                name: "identity-drop-panic-task",
                drops: Arc::clone(&drops),
            })),
        ),
    );
    drop(slot_state);

    let (reply, result) = oneshot::channel();
    let mut admissions = JoinSet::new();
    let mut identity_operations = JoinSet::new();
    ctrl.handle_identity_operation(
        id,
        IdentityOperation::Remove,
        reply,
        &mut admissions,
        &mut identity_operations,
    )
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
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(identity_operations.is_empty());
    assert!(admissions.is_empty());
    assert!(ctrl.watchers.is_empty());
    assert!(ctrl.queued_slots.is_empty());
    assert!(ctrl.slots.get(&*slot_name).is_none());

    let drained = drain_events(&mut events);
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
    let mut admissions = JoinSet::new();

    let removed = tokio::time::timeout(
        Duration::from_millis(50),
        ctrl.remove_queued_submission(TaskId::next(), None, &mut admissions),
    )
    .await
    .expect("an unindexed ID must not inspect or wait for unrelated slots");

    assert!(!removed);
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
    ctrl.finalize_pending_on_shutdown(&mut rx).await;

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
        queued_slots: DashMap::new(),
        capacity_pending: DashMap::new(),
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
    assert_eq!(ev.kind, EventKind::RuntimeFailure);
    assert!(
        ev.reason.as_deref().unwrap_or_default().contains("boom 1"),
        "diagnostic must carry the panic message, got {:?}",
        ev.reason
    );
}

#[tokio::test]
async fn explicit_slot_shutdown_rejects_without_reading_task_name() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let bus = Bus::new(64);
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), bus.clone());
    let mut events = bus.subscribe();
    let calls = Arc::new(AtomicUsize::new(0));
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let mut admissions = JoinSet::new();
    let mut removals = JoinSet::new();
    ctrl.mark_shutting_down();

    ctrl.handle_submission(
        Submission {
            id,
            spec: ControllerSpec::queue(TaskSpec::once(Arc::new(NameBombTask {
                calls: Arc::clone(&calls),
            })))
            .with_slot("explicit-shutdown"),
            done: Some(done),
        },
        &mut admissions,
        &mut removals,
    )
    .await;

    assert_eq!(calls.load(Ordering::Acquire), 0);
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
async fn explicit_slot_shutdown_while_waiting_for_lock_skips_task_name() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let bus = Bus::new(64);
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), bus.clone());
    let mut events = bus.subscribe();
    let calls = Arc::new(AtomicUsize::new(0));
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let slot = ctrl.get_or_create_slot("locked-at-shutdown");
    let owner = TaskId::next();
    let mut slot_guard = slot.lock().await;
    *slot_guard = running_slot(owner);
    let mut admissions = JoinSet::new();
    let mut removals = JoinSet::new();
    let mut admission = Box::pin(
        ctrl.handle_submission(
            Submission {
                id,
                spec: ControllerSpec::queue(TaskSpec::once(Arc::new(NameBombTask {
                    calls: Arc::clone(&calls),
                })))
                .with_slot("locked-at-shutdown"),
                done: Some(done),
            },
            &mut admissions,
            &mut removals,
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

    assert_eq!(calls.load(Ordering::Acquire), 0);
    let outcome = receive_oneshot(outcome, "lock-wait shutdown watcher").await;
    assert!(matches!(
        outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::ControllerShuttingDown,
            ..
        }
    ));
    assert!(admissions.is_empty());
    assert!(removals.is_empty());
    assert!(ctrl.watchers.is_empty());
    let slot = ctrl
        .slots
        .get("locked-at-shutdown")
        .expect("the existing running slot remains owned")
        .clone();
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
async fn explicit_slot_drop_if_running_rejects_without_reading_task_name() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let bus = Bus::new(64);
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), bus.clone());
    let mut events = bus.subscribe();
    let owner = TaskId::next();
    ctrl.slots.insert(
        Arc::from("busy-slot"),
        Arc::new(Mutex::new(running_slot(owner))),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let mut admissions = JoinSet::new();
    let mut removals = JoinSet::new();

    ctrl.handle_submission(
        Submission {
            id,
            spec: ControllerSpec::drop_if_running(TaskSpec::once(Arc::new(NameBombTask {
                calls: Arc::clone(&calls),
            })))
            .with_slot("busy-slot"),
            done: Some(done),
        },
        &mut admissions,
        &mut removals,
    )
    .await;

    assert_eq!(calls.load(Ordering::Acquire), 0);
    let outcome = receive_oneshot(outcome, "busy-rejection watcher").await;
    assert!(matches!(
        outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::SlotBusy,
            ..
        }
    ));
    let slot = ctrl
        .slots
        .get("busy-slot")
        .expect("busy slot remains")
        .clone();
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
async fn explicit_slot_queue_full_rejects_without_reading_task_name() {
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
    ctrl.slots
        .insert(Arc::from("full-slot"), Arc::new(Mutex::new(state)));
    let calls = Arc::new(AtomicUsize::new(0));
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let mut admissions = JoinSet::new();
    let mut removals = JoinSet::new();

    ctrl.handle_submission(
        Submission {
            id,
            spec: ControllerSpec::queue(TaskSpec::once(Arc::new(NameBombTask {
                calls: Arc::clone(&calls),
            })))
            .with_slot("full-slot"),
            done: Some(done),
        },
        &mut admissions,
        &mut removals,
    )
    .await;

    assert_eq!(calls.load(Ordering::Acquire), 0);
    let outcome = receive_oneshot(outcome, "queue-full watcher").await;
    assert!(matches!(
        outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::QueueFull,
            ..
        }
    ));
    let slot = ctrl
        .slots
        .get("full-slot")
        .expect("full slot remains")
        .clone();
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

#[tokio::test(flavor = "current_thread")]
async fn task_name_panic_publishes_rejection_matching_waiter() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let bus = Bus::new(64);
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), bus.clone());
    let mut events = bus.subscribe();
    let mut admissions = JoinSet::new();
    let mut removals = JoinSet::new();

    for explicit_slot in [None, Some("explicit-name-panic")] {
        let id = TaskId::next();
        let (done, outcome) = oneshot::channel();
        let spec = ControllerSpec::queue(TaskSpec::once(Arc::new(PanickingNameTask)));
        let spec = if let Some(slot) = explicit_slot {
            spec.with_slot(slot)
        } else {
            spec
        };

        ctrl.handle_submission(
            Submission {
                id,
                spec,
                done: Some(done),
            },
            &mut admissions,
            &mut removals,
        )
        .await;

        let outcome = receive_oneshot(outcome, "task-name panic watcher").await;
        assert!(matches!(
            outcome,
            TaskOutcome::Rejected {
                kind: crate::RejectionKind::AdmissionFailed,
                ref reason,
                ..
            } if reason.as_ref() == crate::reasons::CONTROLLER_ADMISSION_INTERRUPTED
        ));
        let drained = drain_events(&mut events);
        let rejections: Vec<_> = drained
            .iter()
            .filter(|event| event.kind == EventKind::ControllerRejected)
            .collect();
        assert_eq!(rejections.len(), 1);
        assert_rejection_parity(rejections[0], id, &outcome);
        assert_eq!(rejections[0].task.as_deref(), explicit_slot);
        assert_eq!(
            drained
                .iter()
                .filter(|event| event.kind == EventKind::RuntimeFailure)
                .count(),
            1
        );
    }

    assert!(admissions.is_empty());
    assert!(removals.is_empty());
    assert!(ctrl.watchers.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn task_name_panic_rejects_watcher_and_controller_continues() {
    let supervisor = Supervisor::builder(crate::SupervisorConfig::default())
        .with_controller(ControllerConfig::default())
        .build();
    let handle = supervisor.serve();

    let hostile_specs = [
        ControllerSpec::queue(TaskSpec::once(Arc::new(PanickingNameTask))),
        ControllerSpec::queue(TaskSpec::once(Arc::new(PanickingNameTask)))
            .with_slot("explicit-slot"),
    ];
    for spec in hostile_specs {
        let (_id, waiter) = handle
            .submit_and_watch(spec)
            .await
            .expect("controller intake must accept the hostile submission");
        let outcome = tokio::time::timeout(Duration::from_secs(2), waiter.wait())
            .await
            .expect("a task-name panic must not leave the waiter pending")
            .expect("panic-safe admission must produce a typed outcome");
        assert!(matches!(
            outcome,
            TaskOutcome::Rejected {
                kind: crate::RejectionKind::AdmissionFailed,
                reason,
                ..
            } if reason.as_ref() == crate::reasons::CONTROLLER_ADMISSION_INTERRUPTED
        ));
    }

    let good: TaskRef = TaskFn::arc("after-name-panic", |_ctx: TaskContext| async { Ok(()) });
    let (_id, waiter) = handle
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once(good)))
        .await
        .expect("the controller loop must continue after the caught panic");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), waiter.wait()).await,
        Ok(Ok(TaskOutcome::Completed))
    ));

    handle
        .shutdown()
        .await
        .expect("panic-safe controller admission must shut down cleanly");
}

#[tokio::test]
async fn controller_registry_commit_reuses_snapshotted_task_name() {
    let supervisor = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), Bus::new(64));
    let calls = Arc::new(AtomicUsize::new(0));
    let task: TaskRef = Arc::new(SingleReadNameTask {
        calls: Arc::clone(&calls),
    });
    let id = TaskId::next();
    let (done, outcome) = oneshot::channel();
    let mut admissions = JoinSet::new();
    let mut removals = JoinSet::new();

    ctrl.handle_submission(
        Submission {
            id,
            spec: ControllerSpec::queue(TaskSpec::once(task)),
            done: Some(done),
        },
        &mut admissions,
        &mut removals,
    )
    .await;

    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(admissions.len(), 1);
    assert!(removals.is_empty());
    assert!(ctrl.watchers.is_empty());
    let slot = ctrl
        .slots
        .get("single-read-name")
        .expect("the snapshotted name must become the fallback slot")
        .clone();
    assert!(matches!(
        slot.lock().await.phase(),
        SlotPhase::Admitting { owner, .. } if owner == id
    ));

    drop(outcome);
    abort_and_drain(&mut admissions).await;
    abort_and_drain(&mut removals).await;
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
            waiting_spec("drop-panic-filler"),
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
    let mut admissions = JoinSet::new();
    let mut removals = JoinSet::new();

    ctrl.handle_submission(
        Submission {
            id,
            spec: ControllerSpec::queue(TaskSpec::once(Arc::new(PanickingDropTask {
                name: "drop-panic-uncommitted",
                drops: Arc::clone(&drops),
            })))
            .with_slot("drop-panic-slot"),
            done: Some(done),
        },
        &mut admissions,
        &mut removals,
    )
    .await;

    assert_eq!(drops.load(Ordering::Acquire), 1);
    let outcome = receive_oneshot(outcome, "pre-commit failure watcher").await;
    assert!(matches!(
        outcome,
        TaskOutcome::Rejected {
            kind: crate::RejectionKind::AdmissionFailed,
            ..
        }
    ));
    assert!(ctrl.watchers.is_empty());
    assert!(ctrl.slots.get("drop-panic-slot").is_none());
    assert!(admissions.is_empty());
    assert!(removals.is_empty());

    let drained = drain_events(&mut events);
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
            waiting_spec("transient-full-filler"),
            None,
        )
        .expect("the filler must occupy the registry queue");
    assert_eq!(supervisor.core().registry_command_capacity(), 0);

    let bus = Bus::new(64);
    let mut events = bus.subscribe();
    let ctrl = Controller::new(ControllerConfig::default(), supervisor.core(), bus);
    let token = CancellationToken::new();
    let runner = start_controller_loop(&ctrl, &token).await;
    let task: TaskRef = TaskFn::arc("transient-full-target", |_ctx: TaskContext| async {
        Ok(())
    });
    let (id, outcome) = ctrl
        .handle()
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once(task)).with_slot("transient-slot"))
        .await
        .expect("the controller command queue must accept the target");

    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.capacity_pending.contains_key(&id)
                && ctrl.watchers.contains_key(&id)
                && ctrl
                    .slots
                    .get("transient-slot")
                    .map(|entry| entry.clone())
                    .is_some_and(|slot| {
                        slot.try_lock().is_ok_and(|slot| {
                            matches!(slot.phase(), SlotPhase::Admitting { owner, .. } if owner == id)
                        })
                    })
        })
        .await,
        "registry backpressure must retain the payload and watcher in an admitting slot"
    );

    let runtime_handle = supervisor.serve();
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
            !ctrl.capacity_pending.contains_key(&id)
                && !ctrl.watchers.contains_key(&id)
                && ctrl.slots.get("transient-slot").is_none()
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
            waiting_spec("capacity-cancel-filler"),
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
            ctrl.capacity_pending.contains_key(&id)
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
    assert!(!ctrl.capacity_pending.contains_key(&id));
    assert!(!ctrl.watchers.contains_key(&id));
    assert!(ctrl.slots.get("capacity-cancel-slot").is_none());

    stop_controller_loop(token, runner).await;
    let runtime_handle = supervisor.serve();
    let _ = runtime_handle.shutdown().await;
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
            waiting_spec("capacity-replace-filler"),
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
            ctrl.capacity_pending.contains_key(&owner_id)
        })
        .await
    );

    let replacement: TaskRef =
        TaskFn::arc("capacity-replacement", |_ctx: TaskContext| async { Ok(()) });
    let (replacement_id, replacement_outcome) = ctrl
        .handle()
        .submit_and_watch(
            ControllerSpec::replace(TaskSpec::once(replacement)).with_slot("capacity-replace-slot"),
        )
        .await
        .expect("the replacement must enter controller intake");
    assert!(
        poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl
                .slots
                .get("capacity-replace-slot")
                .map(|entry| entry.clone())
            else {
                return false;
            };
            let slot = slot.lock().await;
            matches!(slot.phase(), SlotPhase::CancelPendingAdmission { owner, .. } if owner == owner_id)
                && slot.queue.front().map(|pending| pending.id) == Some(replacement_id)
        })
        .await,
        "Replace must remain queued behind the capacity-waiting owner"
    );

    let runtime_handle = supervisor.serve();
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
            ctrl.slots.get("capacity-replace-slot").is_none() && ctrl.capacity_pending.is_empty()
        })
        .await
    );

    stop_controller_loop(token, runner).await;
    let _ = runtime_handle.shutdown().await;
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
            waiting_spec("queued-drop-filler"),
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
    ctrl.watchers.insert(first, first_done);
    ctrl.watchers.insert(second, second_done);
    let mut slot = SlotState::new();
    slot.queue
        .push_back(crate::controller::slot::PendingSubmission::new(
            first,
            Arc::from("queued-drop-panic"),
            TaskSpec::once(Arc::new(PanickingDropTask {
                name: "queued-drop-panic",
                drops: Arc::clone(&drops),
            })),
        ));
    slot.queue
        .push_back(pending(second, waiting_spec("queued-after-drop-panic")));
    let mut admissions = JoinSet::new();
    let slot_name = Arc::from("queued-drop-slot");

    let deferred =
        ctrl.start_next_from_queue(supervisor.core(), &mut slot, &slot_name, &mut admissions);

    assert!(slot.is_idle());
    assert!(slot.queue.is_empty());
    assert!(admissions.is_empty());
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
    assert!(ctrl.watchers.is_empty());
    assert_eq!(drops.load(Ordering::Acquire), 0);

    ctrl.drop_pending_submissions(deferred).await;
    assert_eq!(drops.load(Ordering::Acquire), 1);
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
        .try_send(ControllerCommand::Submit(Submission {
            id: first,
            spec: ControllerSpec::queue(TaskSpec::once(Arc::new(PanickingDropTask {
                name: "buffered-drop-panic",
                drops: Arc::clone(&drops),
            })))
            .with_slot("buffered-first"),
            done: Some(first_done),
        }))
        .expect("first buffered submission");
    ctrl.tx
        .try_send(ControllerCommand::Submit(Submission {
            id: second,
            spec: ControllerSpec::queue(waiting_spec("buffered-after-drop-panic"))
                .with_slot("buffered-second"),
            done: Some(second_done),
        }))
        .expect("second buffered submission");
    ctrl.tx
        .try_send(ControllerCommand::ManageIdentity {
            id: TaskId::next(),
            operation: IdentityOperation::Remove,
            reply: identity_reply,
        })
        .expect("buffered identity operation");

    let mut rx = ctrl.rx.write().await.take().expect("rx present");
    ctrl.finalize_pending_on_shutdown(&mut rx).await;

    assert_eq!(drops.load(Ordering::Acquire), 1);
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

    let drained = drain_events(&mut events);
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
    assert!(ctrl.watchers.is_empty());
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
    let panicking_outcome =
        tokio::time::timeout(Duration::from_millis(50), panicking_waiter.wait())
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
async fn try_identity_operations_report_full_controller_command_queue() {
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
    assert!(matches!(
        handle.try_cancel(TaskId::next()).await,
        Err(RuntimeError::CommandQueueFull)
    ));
    assert!(matches!(
        handle
            .try_cancel_with_timeout(TaskId::next(), Duration::from_secs(1))
            .await,
        Err(RuntimeError::CommandQueueFull)
    ));

    let mut rx = ctrl.rx.write().await.take().expect("rx present");
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
            waiting_spec("registry-queue-filler"),
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
        handle.try_remove(TaskId::next()).await,
        Err(RuntimeError::CommandQueueFull)
    ));
    assert_eq!(
        sup.core().registry_command_capacity(),
        0,
        "a rejected fallback must not consume or replace the queued registry command"
    );
    assert!(matches!(
        handle.try_cancel(TaskId::next()).await,
        Err(RuntimeError::CommandQueueFull)
    ));
    assert!(matches!(
        handle
            .try_cancel_with_timeout(TaskId::next(), Duration::from_secs(1))
            .await,
        Err(RuntimeError::CommandQueueFull)
    ));

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
        controller_bus.publish(Event::new(EventKind::AttemptStarting).with_task("noise"));
    }

    let reached_running = poll_until(Duration::from_secs(2), || async {
        let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
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
    assert!(ctrl.slots.is_empty());

    let _ = handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_owner_progresses_under_continuously_ready_intake() {
    let sup = Supervisor::builder(crate::SupervisorConfig::default())
        .with_controller(
            ControllerConfig::default().with_queue_capacity(NonZeroUsize::new(4_096).unwrap()),
        )
        .build();
    let handle = sup.serve();

    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let owner: TaskRef = TaskFn::arc("starvation-owner", {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        move |_ctx| {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            async move {
                started.notify_one();
                release.notified().await;
                Ok(())
            }
        }
    });
    let (owner_id, owner_waiter) = handle
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once(owner)).with_slot("hot-slot"))
        .await
        .expect("the initial owner submission must enter the controller");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("the initial owner must start");

    let flood_task: TaskRef = TaskFn::arc("starvation-flood", |ctx| async move {
        ctx.cancelled().await;
        Ok(())
    });
    let flood_spec =
        ControllerSpec::drop_if_running(TaskSpec::once(flood_task)).with_slot("hot-slot");
    let stop = Arc::new(AtomicBool::new(false));
    let saw_full = Arc::new(AtomicBool::new(false));
    let producer_failed = Arc::new(AtomicBool::new(false));
    let mut producers = Vec::new();

    for _ in 0..4 {
        let producer_handle = handle.clone();
        let producer_spec = flood_spec.clone();
        let producer_stop = Arc::clone(&stop);
        let producer_saw_full = Arc::clone(&saw_full);
        let producer_failed = Arc::clone(&producer_failed);
        producers.push(std::thread::spawn(move || {
            while !producer_stop.load(Ordering::Relaxed) {
                match producer_handle.try_submit(producer_spec.clone()) {
                    Ok(_) => {}
                    Err(ControllerError::Full) => {
                        producer_saw_full.store(true, Ordering::Release);
                        std::hint::spin_loop();
                    }
                    Err(_) => {
                        producer_failed.store(true, Ordering::Release);
                        break;
                    }
                }
            }
        }));
    }

    let saturated = tokio::time::timeout(Duration::from_secs(2), async {
        while !saw_full.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();

    release.notify_one();
    let owner_outcome = tokio::time::timeout(Duration::from_secs(2), owner_waiter.wait()).await;
    let progressed = poll_until(Duration::from_secs(2), || async {
        let Some(snapshot) = handle.controller_snapshot().await else {
            return false;
        };
        snapshot
            .slot("hot-slot")
            .is_none_or(|slot| slot.owner_id != Some(owner_id))
    })
    .await;

    stop.store(true, Ordering::Release);
    let producers_joined = producers
        .into_iter()
        .all(|producer| producer.join().is_ok());
    let shutdown = handle.shutdown().await;

    assert!(
        saturated,
        "the producers must keep the command channel ready"
    );
    assert!(
        matches!(owner_outcome, Ok(Ok(TaskOutcome::Completed))),
        "the initial owner must complete normally"
    );
    assert!(
        progressed,
        "a ready completion result must advance the slot while intake remains saturated"
    );
    assert!(producers_joined, "all intake producers must exit cleanly");
    assert!(
        !producer_failed.load(Ordering::Acquire),
        "the controller must remain open during the saturation phase"
    );
    assert!(shutdown.is_ok(), "the supervisor must shut down cleanly");
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
            let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
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
            let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                return false;
            };
            let slot = slot.lock().await;
            matches!(slot.phase(), SlotPhase::CancelPendingAdmission { .. })
                && slot.queue.front().map(|pending| pending.id) == Some(replacement_id)
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
    let runtime_handle = sup.serve();
    let owner_id = runtime_handle
        .add(waiting_spec("replace-owner"))
        .await
        .expect("the owner must register");

    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    let slot_name: Arc<str> = Arc::from("s");
    let slot = running_slot(owner_id);
    ctrl.slots
        .insert(Arc::clone(&slot_name), Arc::new(Mutex::new(slot)));

    let filler_id = TaskId::next();
    let (filler_reply, _filler_completion) = sup
        .core()
        .add_task_with_id_watched(
            filler_id,
            Arc::from("replace-filler"),
            waiting_spec("replace-filler"),
            None,
        )
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
    assert!(matches!(slot.phase(), SlotPhase::Terminating { .. }));
    assert_eq!(
        slot.queue.front().map(|pending| pending.id),
        Some(second_id)
    );
    drop(slot);
    assert_eq!(
        removals.len(),
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
            slot.owner_id() == Some(owner_id) && matches!(slot.phase(), SlotPhase::Running { .. })
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
        poll_until(Duration::from_secs(2), || async {
            ctrl.queued_slots
                .get(&victim_id)
                .is_some_and(|entry| entry.value().as_ref() == "s")
        })
        .await,
        "queued admission must publish its reverse-index route"
    );

    assert!(
        handle
            .cancel(victim_id)
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
    assert!(!ctrl.queued_slots.contains_key(&victim_id));

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
    assert!(matches!(try_outcome, TaskOutcome::Rejected {
            kind: crate::RejectionKind::RemovedFromQueue,
            reason,
            ..
        } if reason.as_ref() == crate::reasons::REMOVED_FROM_QUEUE));

    let try_cancel_ran = Arc::clone(&victim_ran);
    let try_cancel_victim: TaskRef = TaskFn::arc("try-cancel-victim", move |_ctx: TaskContext| {
        let ran = Arc::clone(&try_cancel_ran);
        async move {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        }
    });
    let (try_cancel_id, try_cancel_waiter) = handle
        .submit_and_watch(ControllerSpec::queue(TaskSpec::once(try_cancel_victim)).with_slot("s"))
        .await
        .expect("the try-cancel victim must enter the controller channel");
    assert!(
        handle
            .try_cancel(try_cancel_id)
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
            .await
            .expect("the admitted owner must be cancelled")
    );
    assert!(
        poll_until(Duration::from_secs(2), || async {
            ctrl.slots.get("s").is_none()
        })
        .await,
        "the slot must settle after its owner completes"
    );
    assert!(
        !victim_ran.load(Ordering::SeqCst),
        "a queued submission claimed by cancel must never start"
    );
    assert!(ctrl.queued_slots.is_empty());

    stop_controller_loop(token, runner).await;
    let _ = runtime_handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn reliable_completion_reuses_task_name_without_task_removed() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let handle = sup.serve();
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
            slot.owner_id() == Some(first_id) && matches!(slot.phase(), SlotPhase::Running { .. })
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
            ctrl.slots.get("s").is_none()
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
            ctrl.slots.get("s").is_none() && !ctrl.watchers.contains_key(&id)
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
            .push_back(pending(duplicate_id, waiting_spec("queued-duplicate")));
        slot.queue
            .push_back(pending(accepted_id, waiting_spec("queued-accepted")));
        let deferred =
            ctrl.start_next_from_queue(sup.core(), &mut slot, &slot_name, &mut admissions);
        assert!(deferred.is_empty());
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

#[tokio::test]
async fn no_queue_advancement_after_shutdown_starts() {
    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let handle = sup.serve();
    let id = handle
        .add(waiting_spec("occupant"))
        .await
        .expect("task should register");
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));

    let mut queue = std::collections::VecDeque::new();
    queue.push_back(pending(TaskId::next(), waiting_spec("queued")));
    let mut slot = running_slot(id);
    slot.queue = queue;
    ctrl.slots
        .insert(Arc::from("s"), Arc::new(Mutex::new(slot)));
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

    assert!(
        admissions.is_empty(),
        "shutdown must prevent a queued admission from being scheduled"
    );
    assert!(
        sup.core().id_for_label("queued").await.is_none(),
        "controller must not start queued tasks once shutdown has been requested"
    );

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn snapshot_maps_every_internal_slot_phase_and_owner() {
    use crate::controller::{SlotStatusKind, slot::ReplaceAction};

    let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
    let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
    let admitting_id = TaskId::next();
    let cancel_pending_id = TaskId::next();
    let running_id = TaskId::next();
    let terminating_id = TaskId::next();
    let now = Instant::now();

    let mut admitting = SlotState::new();
    assert!(admitting.begin_admission(admitting_id, now - Duration::from_secs(4)));
    let mut cancel_pending = SlotState::new();
    assert!(cancel_pending.begin_admission(cancel_pending_id, now - Duration::from_secs(5)));
    assert_eq!(
        cancel_pending.request_replacement(now - Duration::from_secs(3)),
        ReplaceAction::WaitForAdmission
    );
    let mut running = SlotState::new();
    assert!(running.begin_admission(running_id, now - Duration::from_secs(5)));
    assert_eq!(
        running.confirm_admission(running_id, now - Duration::from_secs(2)),
        AdmissionTransition::Running
    );
    let mut terminating = SlotState::new();
    assert!(terminating.begin_admission(terminating_id, now - Duration::from_secs(5)));
    assert_eq!(
        terminating.confirm_admission(terminating_id, now - Duration::from_secs(4)),
        AdmissionTransition::Running
    );
    assert_eq!(
        terminating.request_replacement(now - Duration::from_secs(1)),
        ReplaceAction::RemoveNow(terminating_id)
    );

    let with_queue = |mut slot: SlotState, depth: usize| {
        for _ in 0..depth {
            slot.queue
                .push_back(pending(TaskId::next(), make_spec("snapshot-queued")));
        }
        slot
    };
    for (name, slot) in [
        ("terminating", with_queue(terminating, 4)),
        ("running", with_queue(running, 3)),
        ("idle", SlotState::new()),
        ("cancel-pending", with_queue(cancel_pending, 2)),
        ("admitting", with_queue(admitting, 1)),
    ] {
        ctrl.slots
            .insert(Arc::from(name), Arc::new(Mutex::new(slot)));
    }

    let snap = ctrl.snapshot().await;
    assert_eq!(snap.len(), 5);
    assert_eq!(snap.total_queued(), 10);
    assert_eq!(snap.running_count(), 1);
    assert_eq!(
        snap.slots
            .iter()
            .map(|slot| slot.slot.as_ref())
            .collect::<Vec<_>>(),
        [
            "admitting",
            "cancel-pending",
            "idle",
            "running",
            "terminating"
        ]
    );

    for (name, status, owner, queue_depth, minimum_age) in [
        ("idle", SlotStatusKind::Idle, None, 0, Duration::ZERO),
        (
            "admitting",
            SlotStatusKind::Admitting,
            Some(admitting_id),
            1,
            Duration::from_secs(4),
        ),
        (
            "cancel-pending",
            SlotStatusKind::Terminating,
            Some(cancel_pending_id),
            2,
            Duration::from_secs(3),
        ),
        (
            "running",
            SlotStatusKind::Running,
            Some(running_id),
            3,
            Duration::from_secs(2),
        ),
        (
            "terminating",
            SlotStatusKind::Terminating,
            Some(terminating_id),
            4,
            Duration::from_secs(1),
        ),
    ] {
        let view = snap.slot(name).expect("the inserted slot must be visible");
        assert_eq!(view.status, status, "wrong public status for {name}");
        assert_eq!(view.owner_id, owner, "wrong phase-owned id for {name}");
        assert_eq!(
            view.queue_depth, queue_depth,
            "wrong queue depth for {name}"
        );
        if status == SlotStatusKind::Idle {
            assert_eq!(view.status_for, Duration::ZERO);
        } else {
            assert!(
                view.status_for >= minimum_age,
                "wrong status timestamp selected for {name}: {:?}",
                view.status_for
            );
        }
    }
}

async fn poll_until<F, Fut>(within: Duration, mut cond: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + within;
    loop {
        if cond().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
